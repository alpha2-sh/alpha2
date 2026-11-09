//! The wasm transport, with the `alpha2_*` exports and the `alpha2` imports.

use std::alloc::Layout;
use std::cell::UnsafeCell;

use alpha2_plugin_abi::{ABI_MAJOR, status};

use crate::{Extractor, Host, Runtime};

#[link(wasm_import_module = "alpha2")]
unsafe extern "C" {
    #[link_name = "emit"]
    fn host_emit(ptr: *const u8, len: usize) -> i32;
    #[link_name = "resource_size"]
    fn host_resource_size(name: *const u8, name_len: usize) -> i64;
    #[link_name = "resource_read"]
    fn host_resource_read(name: *const u8, name_len: usize, dst: *mut u8, dst_len: usize) -> i32;
    #[link_name = "fail"]
    fn host_fail(ptr: *const u8, len: usize);
    #[link_name = "log"]
    fn host_log(level: u32, ptr: *const u8, len: usize);
}

/// The one instance of a module, which serves one run on one thread.
pub struct Slot<E>(UnsafeCell<Option<Runtime<E>>>);

// SAFETY: wasm32 plugins are built without threads, so the slot is never shared across threads.
unsafe impl<E> Sync for Slot<E> {}

impl<E> Slot<E> {
    /// An empty slot.
    #[must_use]
    pub const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }
}

impl<E> Default for Slot<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Extractor> Slot<E> {
    #[allow(clippy::mut_from_ref)]
    fn runtime(&self) -> &mut Runtime<E> {
        // SAFETY: single-threaded, and no host import calls back into the module, so no other
        // borrow is alive.
        unsafe { (*self.0.get()).get_or_insert_with(|| Runtime::new(Host {})) }
    }
}

fn address(value: i32) -> usize {
    value.cast_unsigned() as usize
}

/// Borrows `len` items at the linear-memory offset `ptr`.
fn borrowed<'a, T>(ptr: i32, len: i32) -> &'a [T] {
    if ptr == 0 || len <= 0 {
        return &[];
    }
    // SAFETY: the host wrote `len` items at `ptr`, memory it allocated with `alpha2_alloc`
    // for this call.
    unsafe {
        std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance(address(ptr)),
            address(len),
        )
    }
}

/// `alpha2_abi`.
#[must_use]
pub fn abi() -> i32 {
    ABI_MAJOR.cast_signed()
}

/// `alpha2_alloc`: zero bytes get a non-null dangling offset.
#[must_use]
pub fn alloc(size: i32, align: i32) -> i32 {
    let Ok(layout) = Layout::from_size_align(address(size), address(align)) else {
        return 0;
    };
    if layout.size() == 0 {
        return align;
    }
    // SAFETY: the layout has a nonzero size.
    let ptr = unsafe { std::alloc::alloc(layout) };
    i32::try_from(ptr.expose_provenance()).unwrap_or(0)
}

/// `alpha2_free`.
pub fn free(ptr: i32, size: i32, align: i32) {
    let Ok(layout) = Layout::from_size_align(address(size), address(align)) else {
        return;
    };
    if ptr != 0 && layout.size() != 0 {
        // SAFETY: the host frees what `alpha2_alloc` returned for this layout, once.
        unsafe { std::alloc::dealloc(std::ptr::with_exposed_provenance_mut(address(ptr)), layout) }
    }
}

/// `alpha2_describe`.
#[must_use]
pub fn describe<E: Extractor>() -> i64 {
    let offset = E::DESCRIPTOR.as_ptr().expose_provenance() as u64;
    let len = E::DESCRIPTOR.len() as u64;
    (offset | len << 32).cast_signed()
}

/// `alpha2_begin`.
pub fn begin<E: Extractor>(slot: &Slot<E>, ptr: i32, len: i32) -> i32 {
    slot.runtime().begin(borrowed(ptr, len))
}

/// `alpha2_pass`.
pub fn pass<E: Extractor>(slot: &Slot<E>, pass: i32, direction: i32) -> i32 {
    slot.runtime()
        .pass(pass.cast_unsigned(), direction.cast_unsigned())
}

/// `alpha2_video`.
pub fn video<E: Extractor>(
    slot: &Slot<E>,
    time: i64,
    width: i32,
    height: i32,
    format: i32,
    ptr: i32,
    len: i32,
) -> i32 {
    slot.runtime().video(
        time,
        width.cast_unsigned(),
        height.cast_unsigned(),
        format.cast_unsigned(),
        borrowed(ptr, len),
    )
}

/// `alpha2_audio`.
#[allow(clippy::too_many_arguments)]
pub fn audio<E: Extractor>(
    slot: &Slot<E>,
    start: i64,
    start_sample: i64,
    frames: i32,
    rate: i32,
    channels: i32,
    ptr: i32,
    count: i32,
) -> i32 {
    slot.runtime().audio(
        start,
        start_sample,
        frames.cast_unsigned(),
        rate.cast_unsigned(),
        channels.cast_unsigned(),
        borrowed(ptr, count),
    )
}

/// `alpha2_end`.
pub fn end<E: Extractor>(slot: &Slot<E>) -> i32 {
    slot.runtime().end()
}

// The same methods as the native host's, which reads its table through `self`.
#[allow(clippy::unused_self)]
impl Host {
    pub(crate) fn emit(&self, records: &[u8]) -> i32 {
        // SAFETY: the host reads the records during the call.
        unsafe { host_emit(records.as_ptr(), records.len()) }
    }

    pub(crate) fn fail(&self, message: &str) {
        // SAFETY: as for `emit`.
        unsafe { host_fail(message.as_ptr(), message.len()) }
    }

    pub(crate) fn write_log(&self, level: u32, message: &str) {
        // SAFETY: as for `emit`.
        unsafe { host_log(level, message.as_ptr(), message.len()) }
    }

    pub(crate) fn read_resource(&self, name: &str) -> Option<Vec<u8>> {
        // SAFETY: as for `emit`; the host writes at most `len` bytes into the buffer.
        unsafe {
            let len = usize::try_from(host_resource_size(name.as_ptr(), name.len())).ok()?;
            let mut bytes = vec![0; len];
            (host_resource_read(name.as_ptr(), name.len(), bytes.as_mut_ptr(), len) == status::OK)
                .then_some(bytes)
        }
    }
}
