//! The C-ABI transport: the `alpha2_plugin_v1` table and the host table's functions.

use std::ffi::c_void;

use alpha2_plugin_abi::{ABI_MAJOR, HostV1, PluginV1};

use crate::{Extractor, Host, Runtime};

/// The function table for `E`, which [`crate::export_extractor`] puts in a static.
#[must_use]
pub const fn table<E: Extractor>() -> PluginV1 {
    PluginV1 {
        abi: ABI_MAJOR,
        describe: describe::<E>,
        create: create::<E>,
        destroy: destroy::<E>,
        begin: begin::<E>,
        pass: pass::<E>,
        video: video::<E>,
        audio: audio::<E>,
        end: end::<E>,
    }
}

/// Borrows `len` items at `ptr`; empty for a null or zero-length pointer.
///
/// # Safety
/// A non-null `ptr` points to `len` initialized items that outlive the borrow.
unsafe fn borrowed<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: the caller guarantees `len` items at `ptr`.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// # Safety
/// `instance` came from `create::<E>` and has not been destroyed.
unsafe fn runtime<'a, E: Extractor>(instance: *mut c_void) -> &'a mut Runtime<E> {
    // SAFETY: the host passes back the pointer `create` returned, from one thread at a time.
    unsafe { &mut *instance.cast::<Runtime<E>>() }
}

unsafe extern "C" fn describe<E: Extractor>(len: *mut usize) -> *const u8 {
    if !len.is_null() {
        // SAFETY: the host passes a writable length.
        unsafe { len.write(E::DESCRIPTOR.len()) };
    }
    E::DESCRIPTOR.as_ptr()
}

unsafe extern "C" fn create<E: Extractor>(host: *const HostV1) -> *mut c_void {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the host passes its table, valid until `destroy`.
    let table = unsafe { *host };
    if table.abi != ABI_MAJOR {
        return std::ptr::null_mut();
    }
    std::panic::catch_unwind(|| Box::into_raw(Box::new(Runtime::<E>::new(Host { table }))))
        .map_or(std::ptr::null_mut(), <*mut Runtime<E>>::cast)
}

unsafe extern "C" fn destroy<E: Extractor>(instance: *mut c_void) {
    if !instance.is_null() {
        // SAFETY: `instance` came from `create::<E>` and is destroyed once.
        let instance = unsafe { Box::from_raw(instance.cast::<Runtime<E>>()) };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(instance)));
    }
}

unsafe extern "C" fn begin<E: Extractor>(
    instance: *mut c_void,
    context: *const u8,
    len: usize,
) -> i32 {
    // SAFETY: the ABI's rules for the instance and the borrowed context.
    unsafe { runtime::<E>(instance).begin(borrowed(context, len)) }
}

unsafe extern "C" fn pass<E: Extractor>(instance: *mut c_void, pass: u32, direction: u32) -> i32 {
    // SAFETY: the ABI's rules for the instance.
    unsafe { runtime::<E>(instance).pass(pass, direction) }
}

unsafe extern "C" fn video<E: Extractor>(
    instance: *mut c_void,
    time: i64,
    width: u32,
    height: u32,
    format: u32,
    data: *const u8,
    len: usize,
) -> i32 {
    // SAFETY: the ABI's rules for the instance and the borrowed pixels.
    unsafe { runtime::<E>(instance).video(time, width, height, format, borrowed(data, len)) }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn audio<E: Extractor>(
    instance: *mut c_void,
    start: i64,
    start_sample: i64,
    frames: u32,
    rate: u32,
    channels: u32,
    samples: *const f32,
    count: usize,
) -> i32 {
    // SAFETY: the ABI's rules for the instance and the borrowed samples.
    unsafe {
        runtime::<E>(instance).audio(
            start,
            start_sample,
            frames,
            rate,
            channels,
            borrowed(samples, count),
        )
    }
}

unsafe extern "C" fn end<E: Extractor>(instance: *mut c_void) -> i32 {
    // SAFETY: the ABI's rules for the instance.
    unsafe { runtime::<E>(instance).end() }
}

#[cfg(not(target_arch = "wasm32"))]
impl Host {
    pub(crate) fn emit(&self, records: &[u8]) -> i32 {
        // SAFETY: the host's table is valid until `destroy`; the records are borrowed for the call.
        unsafe { (self.table.emit)(self.table.context, records.as_ptr(), records.len()) }
    }

    pub(crate) fn fail(&self, message: &str) {
        // SAFETY: as for `emit`.
        unsafe { (self.table.fail)(self.table.context, message.as_ptr(), message.len()) }
    }

    pub(crate) fn write_log(&self, level: u32, message: &str) {
        // SAFETY: as for `emit`.
        unsafe { (self.table.log)(self.table.context, level, message.as_ptr(), message.len()) }
    }

    pub(crate) fn read_resource(&self, name: &str) -> Option<Vec<u8>> {
        let mut len = 0;
        // SAFETY: as for `emit`; the returned bytes stay valid until `destroy`.
        unsafe {
            let bytes =
                (self.table.resource)(self.table.context, name.as_ptr(), name.len(), &raw mut len);
            (!bytes.is_null()).then(|| borrowed(bytes, len).to_vec())
        }
    }
}
