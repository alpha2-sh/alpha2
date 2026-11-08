//! An in-process host for plugin tests: it drives a native plugin table as a loader would and
//! keeps what the plugin emitted, decoded.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use alpha2_plugin_abi::records::{Record, decode};
use alpha2_plugin_abi::{ABI_MAJOR, HostV1, PluginV1, status};

/// What the plugin sent the host.
#[derive(Debug, Default)]
pub struct Received {
    /// Every record, in order.
    pub records: Vec<Record>,
    /// The last message passed to `fail`.
    pub failure: Option<String>,
    /// Every log call.
    pub logs: Vec<(u32, String)>,
    /// Calls to `emit` the host refused because their batch did not decode.
    pub refused: usize,
}

struct State {
    received: Received,
    resources: HashMap<String, Vec<u8>>,
}

/// A plugin instance on a test host.
pub struct Instance {
    table: PluginV1,
    handle: *mut c_void,
    // Boxed so the host table's context pointer stays put.
    host: Box<(HostV1, RefCell<State>)>,
}

/// Borrows `len` bytes at `ptr`.
///
/// # Safety
/// As `std::slice::from_raw_parts`, except that a null or empty pointer gives an empty slice.
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: the plugin passes `len` bytes at `ptr` for the call.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// # Safety
/// `context` is the `RefCell<State>` this module put in the host table.
unsafe fn state<'a>(context: *mut c_void) -> &'a RefCell<State> {
    // SAFETY: as documented.
    unsafe { &*context.cast::<RefCell<State>>() }
}

unsafe extern "C" fn emit(context: *mut c_void, records: *const u8, len: usize) -> i32 {
    // SAFETY: the ABI's rules for the context and borrowed records.
    let (state, batch) = unsafe { (state(context), bytes(records, len)) };
    let mut state = state.borrow_mut();
    match decode(batch) {
        Ok(records) => {
            state.received.records.extend(records);
            status::OK
        }
        Err(error) => {
            state.received.refused += 1;
            state.received.failure = Some(error.to_string());
            status::FAILED
        }
    }
}

unsafe extern "C" fn resource(
    context: *mut c_void,
    name: *const u8,
    name_len: usize,
    len: *mut usize,
) -> *const u8 {
    // SAFETY: the ABI's rules for the context and the borrowed name.
    let (state, name) = unsafe { (state(context), bytes(name, name_len)) };
    let state = state.borrow();
    let found = std::str::from_utf8(name)
        .ok()
        .and_then(|name| state.resources.get(name));
    match found {
        Some(bytes) => {
            // SAFETY: the plugin passes a writable length; the bytes live as long as the host.
            unsafe { len.write(bytes.len()) };
            bytes.as_ptr()
        }
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn fail(context: *mut c_void, message: *const u8, len: usize) {
    // SAFETY: the ABI's rules for the context and the borrowed message.
    let (state, message) = unsafe { (state(context), bytes(message, len)) };
    state.borrow_mut().received.failure = Some(String::from_utf8_lossy(message).into_owned());
}

unsafe extern "C" fn log(context: *mut c_void, level: u32, message: *const u8, len: usize) {
    // SAFETY: the ABI's rules for the context and the borrowed message.
    let (state, message) = unsafe { (state(context), bytes(message, len)) };
    let message = String::from_utf8_lossy(message).into_owned();
    state.borrow_mut().received.logs.push((level, message));
}

impl Instance {
    /// Creates an instance of the plugin whose `alpha2_plugin_v1` returned `table`, offering
    /// `resources`.
    ///
    /// # Safety
    /// `table` is what a plugin's `alpha2_plugin_v1` returned.
    ///
    /// # Panics
    /// When the table is null, of another ABI, or `create` returns null.
    #[must_use]
    pub unsafe fn new(table: *const PluginV1, resources: HashMap<String, Vec<u8>>) -> Self {
        assert!(!table.is_null(), "null plugin table");
        // SAFETY: the caller passes the static table a plugin's entry returned.
        let table = unsafe { *table };
        assert_eq!(table.abi, ABI_MAJOR, "plugin table of another ABI");
        let state = RefCell::new(State {
            received: Received::default(),
            resources,
        });
        let mut host = Box::new((
            HostV1 {
                abi: ABI_MAJOR,
                context: std::ptr::null_mut(),
                emit,
                resource,
                fail,
                log,
            },
            state,
        ));
        host.0.context = (&raw const host.1).cast_mut().cast();
        // SAFETY: the host table outlives the instance, which `Drop` destroys first.
        let handle = unsafe { (table.create)(&raw const host.0) };
        assert!(!handle.is_null(), "create returned null");
        Self {
            table,
            handle,
            host,
        }
    }

    /// The descriptor `describe` returns.
    ///
    /// # Panics
    /// When it is not UTF-8.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut len = 0;
        // SAFETY: `describe` returns static bytes of the length it writes.
        let descriptor = unsafe { bytes((self.table.describe)(&raw mut len), len) };
        String::from_utf8(descriptor.to_vec()).expect("descriptor is UTF-8")
    }

    /// `begin` with `context` as JSON.
    pub fn begin(&mut self, context: &str) -> i32 {
        // SAFETY: a live instance; the context is borrowed for the call.
        unsafe { (self.table.begin)(self.handle, context.as_ptr(), context.len()) }
    }

    /// `pass`.
    pub fn pass(&mut self, pass: u32, direction: u32) -> i32 {
        // SAFETY: a live instance.
        unsafe { (self.table.pass)(self.handle, pass, direction) }
    }

    /// `video`.
    pub fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> i32 {
        // SAFETY: a live instance; the pixels are borrowed for the call.
        unsafe {
            (self.table.video)(
                self.handle,
                time,
                width,
                height,
                format,
                data.as_ptr(),
                data.len(),
            )
        }
    }

    /// `audio`.
    pub fn audio(
        &mut self,
        start: i64,
        start_sample: i64,
        frames: u32,
        rate: u32,
        channels: u32,
        samples: &[f32],
    ) -> i32 {
        // SAFETY: a live instance; the samples are borrowed for the call.
        unsafe {
            (self.table.audio)(
                self.handle,
                start,
                start_sample,
                frames,
                rate,
                channels,
                samples.as_ptr(),
                samples.len(),
            )
        }
    }

    /// `end`.
    pub fn end(&mut self) -> i32 {
        // SAFETY: a live instance.
        unsafe { (self.table.end)(self.handle) }
    }

    /// What the plugin has sent so far, taken out of the host.
    pub fn take(&mut self) -> Received {
        std::mem::take(&mut self.host.1.borrow_mut().received)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the instance is live and destroyed once.
        unsafe { (self.table.destroy)(self.handle) }
    }
}
