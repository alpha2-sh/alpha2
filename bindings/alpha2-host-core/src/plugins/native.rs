//! Native plugins: a C-ABI library loaded by the platform loader, its `alpha2_plugin_v1` table,
//! and the host table it calls back through. The library runs with the process's rights.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;

use alpha2_engine::extract::AudioInput;
use alpha2_plugin_abi::{ABI_MAJOR, HostV1, NATIVE_ENTRY, PluginV1};

use super::{Call, Collected, PluginError, Transport, codes};

struct State {
    collected: Collected,
    resources: HashMap<String, Vec<u8>>,
}

pub(super) struct NativeTransport {
    table: PluginV1,
    instance: *mut c_void,
    // Boxed so the context pointer in the host table stays put; dropped after the instance.
    host: Box<(HostV1, RefCell<State>)>,
    // Dropped last: the table and instance point into it.
    library: Option<libloading::Library>,
}

/// Borrows `len` items at `ptr`; empty for a null or zero-length pointer.
///
/// # Safety
/// A non-null `ptr` points to `len` initialized items that outlive the borrow.
unsafe fn borrowed<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: as the caller guarantees.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// # Safety
/// `context` is the `RefCell<State>` [`NativeTransport::new`] put in the host table.
unsafe fn state<'a>(context: *mut c_void) -> &'a RefCell<State> {
    // SAFETY: as documented; the box outlives every call.
    unsafe { &*context.cast::<RefCell<State>>() }
}

unsafe extern "C" fn emit(context: *mut c_void, records: *const u8, len: usize) -> i32 {
    // SAFETY: the ABI's rules for the context and the records, borrowed for the call.
    let (state, batch) = unsafe { (state(context), borrowed(records, len)) };
    state.borrow_mut().collected.emit(batch)
}

unsafe extern "C" fn resource(
    context: *mut c_void,
    name: *const u8,
    name_len: usize,
    len: *mut usize,
) -> *const u8 {
    // SAFETY: the ABI's rules for the context and the name, borrowed for the call.
    let (state, name) = unsafe { (state(context), borrowed(name, name_len)) };
    let state = state.borrow();
    match std::str::from_utf8(name)
        .ok()
        .and_then(|name| state.resources.get(name))
    {
        Some(bytes) if !len.is_null() => {
            // SAFETY: the plugin passes a writable length; the bytes live until the instance is
            // destroyed, since resources are never replaced.
            unsafe { len.write(bytes.len()) };
            bytes.as_ptr()
        }
        _ => std::ptr::null(),
    }
}

unsafe extern "C" fn fail(context: *mut c_void, message: *const u8, len: usize) {
    // SAFETY: the ABI's rules for the context and the message, borrowed for the call.
    let (state, message) = unsafe { (state(context), borrowed(message, len)) };
    state.borrow_mut().collected.failure = Some(String::from_utf8_lossy(message).into_owned());
}

unsafe extern "C" fn log(_context: *mut c_void, _level: u32, message: *const u8, len: usize) {
    // SAFETY: the message is borrowed for the call.
    let message = unsafe { borrowed(message, len) };
    super::log_line(&String::from_utf8_lossy(message));
}

impl NativeTransport {
    pub(super) fn new(
        path: &Path,
        resources: HashMap<String, Vec<u8>>,
    ) -> Result<Self, PluginError> {
        let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
        // SAFETY: loading runs the library's initializers with the process's rights, which the
        // caller allowed for this plugin.
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|error| failed(format!("{}: {error}", path.display())))?;
        // SAFETY: the ABI types the entry as `extern "C" fn() -> *const PluginV1`.
        let table = unsafe {
            let entry = library
                .get::<unsafe extern "C" fn() -> *const PluginV1>(NATIVE_ENTRY)
                .map_err(|error| {
                    failed(format!("the library exports no alpha2_plugin_v1: {error}"))
                })?;
            entry()
        };
        if table.is_null() {
            return Err(failed("alpha2_plugin_v1 returned null".to_owned()));
        }
        // SAFETY: a non-null table is static while the library is loaded.
        let table = unsafe { *table };
        if table.abi != ABI_MAJOR {
            return Err(PluginError::new(
                codes::ABI_UNSUPPORTED,
                format!("the library implements alpha2-plugin/{}", table.abi),
            ));
        }
        let mut host = Box::new((
            HostV1 {
                abi: ABI_MAJOR,
                context: std::ptr::null_mut(),
                emit,
                resource,
                fail,
                log,
            },
            RefCell::new(State {
                collected: Collected::default(),
                resources,
            }),
        ));
        host.0.context = (&raw const host.1).cast_mut().cast();
        // SAFETY: the host table outlives the instance, which `Drop` destroys first.
        let instance = unsafe { (table.create)(&raw const host.0) };
        if instance.is_null() {
            return Err(failed("the plugin could not create an instance".to_owned()));
        }
        Ok(Self {
            table,
            instance,
            host,
            library: Some(library),
        })
    }

    fn outcome(&self, status: i32) -> Call {
        self.host.1.borrow_mut().collected.outcome(status)
    }
}

impl Transport for NativeTransport {
    fn describe(&mut self) -> Result<String, PluginError> {
        let mut len = 0;
        // SAFETY: `describe` returns static bytes of the length it writes.
        let bytes = unsafe { borrowed((self.table.describe)(&raw mut len), len) };
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            PluginError::new(codes::DESCRIPTOR_MISMATCH, "the descriptor is not UTF-8")
        })
    }

    fn begin(&mut self, context: &[u8]) -> Call {
        // SAFETY: a live instance; the context is borrowed for the call.
        let status = unsafe { (self.table.begin)(self.instance, context.as_ptr(), context.len()) };
        self.outcome(status)
    }

    fn pass(&mut self, pass: u32, direction: u32) -> Call {
        // SAFETY: a live instance.
        let status = unsafe { (self.table.pass)(self.instance, pass, direction) };
        self.outcome(status)
    }

    fn video(&mut self, time: i64, width: u32, height: u32, format: u32, data: &[u8]) -> Call {
        // SAFETY: a live instance; the pixels are borrowed for the call.
        let status = unsafe {
            (self.table.video)(
                self.instance,
                time,
                width,
                height,
                format,
                data.as_ptr(),
                data.len(),
            )
        };
        self.outcome(status)
    }

    fn audio(&mut self, block: &AudioInput) -> Call {
        // SAFETY: a live instance; the samples are borrowed for the call.
        let status = unsafe {
            (self.table.audio)(
                self.instance,
                block.start.ticks(),
                block.start_sample,
                block.frames,
                block.rate,
                block.channels,
                block.samples.as_ptr(),
                block.samples.len(),
            )
        };
        self.outcome(status)
    }

    fn end(&mut self) -> Call {
        // SAFETY: a live instance.
        let status = unsafe { (self.table.end)(self.instance) };
        self.outcome(status)
    }
}

impl Drop for NativeTransport {
    fn drop(&mut self) {
        // SAFETY: the instance is live and destroyed once, before the library unloads.
        unsafe { (self.table.destroy)(self.instance) };
        drop(self.library.take());
    }
}
