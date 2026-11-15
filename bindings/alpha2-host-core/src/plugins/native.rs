//! Runs native plugins, C-ABI libraries loaded by the platform loader, through their
//! `alpha2_plugin_v1` table and the host table they call back through. A library runs with the
//! process's rights, as allowed by the caller. Each library is loaded once per process, keyed by
//! its artifact's SHA-256, and never unloaded, since threads it started may still run its code.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{offset_of, size_of};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use alpha2_engine::extract::AudioInput;
use alpha2_plugin_abi::{ABI_MAJOR, HostV1, NATIVE_ENTRY, PluginV1};

use super::{
    Call, Collected, LogBudget, MAX_FAIL_BYTES, MAX_LOG_BYTES, OutputCap, PluginError, Transport,
    bounded_text, codes,
};

struct State {
    collected: Collected,
    caps: Vec<OutputCap>,
    resources: HashMap<String, Vec<u8>>,
    log: LogBudget,
}

/// Runs a callback's body, answering `failed` instead of letting a panic cross the C boundary.
fn guarded<T>(failed: T, body: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(failed)
}

pub(super) struct NativeTransport {
    table: PluginV1,
    instance: *mut c_void,
    // Boxed so the context pointer in the host table stays put; dropped after the instance.
    host: Box<(HostV1, RefCell<State>)>,
}

/// Libraries this process loaded, by artifact SHA-256.
static LOADED: Mutex<Option<HashMap<String, &'static libloading::Library>>> = Mutex::new(None);

/// The library of the artifact pinned by `sha256`, loaded from the absolute form of the path
/// `prepare` gives on the first use in this process. The lock is held across the load, so two runs
/// never load one artifact twice.
fn library(
    sha256: &str,
    prepare: impl FnOnce() -> Result<PathBuf, PluginError>,
) -> Result<&'static libloading::Library, PluginError> {
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);
    let loaded = loaded.get_or_insert_with(HashMap::new);
    if let Some(library) = loaded.get(sha256) {
        return Ok(library);
    }
    let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
    // A bare name would go through the loader's search path instead of naming the checked file.
    let path = std::path::absolute(prepare()?)
        .map_err(|error| failed(format!("the library path: {error}")))?;
    // SAFETY: loading runs the library's initializers with the process's rights, which the
    // caller allowed for this plugin.
    let library = unsafe { libloading::Library::new(&path) }
        .map_err(|error| failed(format!("{}: {error}", path.display())))?;
    let library: &'static libloading::Library = Box::leak(Box::new(library));
    loaded.insert(sha256.to_owned(), library);
    Ok(library)
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
    guarded(alpha2_plugin_abi::status::FAILED, || {
        let State {
            collected, caps, ..
        } = &mut *state.borrow_mut();
        collected.emit(batch, caps)
    })
}

unsafe extern "C" fn resource(
    context: *mut c_void,
    name: *const u8,
    name_len: usize,
    len: *mut usize,
) -> *const u8 {
    // SAFETY: the ABI's rules for the context and the name, borrowed for the call.
    let (state, name) = unsafe { (state(context), borrowed(name, name_len)) };
    guarded(std::ptr::null(), || {
        let state = state.borrow();
        match std::str::from_utf8(name)
            .ok()
            .and_then(|name| state.resources.get(name))
        {
            Some(bytes) if !len.is_null() => {
                // SAFETY: the plugin passes a writable length; the bytes live until the instance
                // is destroyed, since resources are never replaced.
                unsafe { len.write(bytes.len()) };
                bytes.as_ptr()
            }
            _ => std::ptr::null(),
        }
    })
}

unsafe extern "C" fn fail(context: *mut c_void, message: *const u8, len: usize) {
    // SAFETY: the ABI's rules for the context and the message, borrowed for the call; only the
    // kept prefix is read.
    let (state, message) = unsafe {
        (
            state(context),
            borrowed(message, len.min(MAX_FAIL_BYTES + 1)),
        )
    };
    guarded((), || {
        state.borrow_mut().collected.failure = Some(bounded_text(message, MAX_FAIL_BYTES));
    });
}

unsafe extern "C" fn log(context: *mut c_void, _level: u32, message: *const u8, len: usize) {
    // SAFETY: the ABI's rules for the context and the message, borrowed for the call; only the
    // kept prefix is read.
    let (state, message) = unsafe {
        (
            state(context),
            borrowed(message, len.min(MAX_LOG_BYTES + 1)),
        )
    };
    guarded((), || state.borrow_mut().log.log(message));
}

// Both tables are a few pointers long.
#[allow(clippy::cast_possible_truncation)]
const HOST_BYTES: u32 = size_of::<HostV1>() as u32;
#[allow(clippy::cast_possible_truncation)]
const PLUGIN_BYTES: u32 = size_of::<PluginV1>() as u32;

/// Entries in `PluginV1`, the pointers after `abi` and `size`.
const ENTRIES: usize =
    (size_of::<PluginV1>() - offset_of!(PluginV1, describe)) / size_of::<*const c_void>();

/// A copy of the plugin's table, after its `abi` and `size` are read and every entry is checked.
///
/// # Safety
/// A non-null `table` is valid for reads of the `size` bytes it states, at least its header.
pub(super) unsafe fn checked(table: *const PluginV1) -> Result<PluginV1, PluginError> {
    let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
    if table.is_null() {
        return Err(failed("alpha2_plugin_v1 returned null".to_owned()));
    }
    // SAFETY: every `/1` table starts with `abi` and `size`.
    let (abi, size) = unsafe {
        (
            (&raw const (*table).abi).read(),
            (&raw const (*table).size).read(),
        )
    };
    if abi != ABI_MAJOR {
        return Err(PluginError::new(
            codes::ABI_UNSUPPORTED,
            format!("the library implements alpha2-plugin/{abi}"),
        ));
    }
    if size < PLUGIN_BYTES {
        return Err(failed(format!(
            "the plugin table is {size} bytes; alpha2-plugin/1 needs {PLUGIN_BYTES}"
        )));
    }
    // SAFETY: the table holds at least this build's `PluginV1`, whose entries are pointers from
    // `describe` on.
    let entries = unsafe {
        std::slice::from_raw_parts(
            (&raw const (*table).describe).cast::<*const c_void>(),
            ENTRIES,
        )
    };
    if entries.iter().any(|entry| entry.is_null()) {
        return Err(failed("the plugin table has a null entry".to_owned()));
    }
    // SAFETY: the bytes are a `PluginV1` with every entry set.
    Ok(unsafe { table.read() })
}

impl NativeTransport {
    /// An instance of the artifact pinned by `sha256`; `prepare` gives the file to load when this
    /// process has not loaded it yet.
    pub(super) fn new(
        sha256: &str,
        prepare: impl FnOnce() -> Result<PathBuf, PluginError>,
        resources: HashMap<String, Vec<u8>>,
        caps: Vec<OutputCap>,
    ) -> Result<Self, PluginError> {
        let failed = |why: String| PluginError::new(codes::LOAD_FAILED, why);
        let library = library(sha256, prepare)?;
        // SAFETY: the ABI types the entry as `extern "C" fn() -> *const PluginV1`; a null symbol
        // reads as `None`.
        let entry = unsafe {
            library.get::<Option<unsafe extern "C" fn() -> *const PluginV1>>(NATIVE_ENTRY)
        }
        .map_err(|error| failed(format!("the library exports no alpha2_plugin_v1: {error}")))?;
        let entry = (*entry).ok_or_else(|| failed("alpha2_plugin_v1 is null".to_owned()))?;
        // SAFETY: the entry takes nothing and returns a static table or null.
        let table = unsafe { entry() };
        // SAFETY: a non-null table is static, and the library is never unloaded.
        let table = unsafe { checked(table) }?;
        let mut host = Box::new((
            HostV1 {
                abi: ABI_MAJOR,
                size: HOST_BYTES,
                context: std::ptr::null_mut(),
                emit,
                resource,
                fail,
                log,
            },
            RefCell::new(State {
                collected: Collected::default(),
                caps,
                resources,
                log: LogBudget::default(),
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
        // SAFETY: the instance is live and destroyed once; the library stays loaded.
        unsafe { (self.table.destroy)(self.instance) };
    }
}
