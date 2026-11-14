//! The `alpha2-plugin/1` ABI, covering what a plugin exports, what a host gives it, and the binary
//! records a plugin emits values in. Native plugins exchange the `#[repr(C)]` tables below; wasm
//! plugins use the export and import names in [`wasm`]. Both send emits as [`records`].

#![no_std]

extern crate alloc;

pub mod records;

use core::ffi::c_void;

/// The ABI a manifest's `abi` names.
pub const ABI: &str = "alpha2-plugin/1";

/// The major version in [`ABI`], which the native tables and `alpha2_abi` carry.
pub const ABI_MAJOR: u32 = 1;

/// A plugin call returns one of these.
pub mod status {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// The extractor failed; the message it passed to `fail` says why.
    pub const FAILED: i32 = 1;
    /// A panic was caught at the boundary.
    pub const TRAPPED: i32 = 2;
}

/// `format` of a video call.
pub mod video_format {
    /// Four bytes per pixel, straight alpha, display-encoded sRGB.
    pub const RGBA8: u32 = 0;
    /// One byte per pixel: BT.709 luma of the display-encoded picture.
    pub const LUMA8: u32 = 1;
}

/// `direction` of a pass call.
pub mod direction {
    /// Times increase through the pass.
    pub const FORWARD: u32 = 0;
    /// Times decrease through the pass.
    pub const BACKWARD: u32 = 1;
}

/// `level` of a log call.
pub mod log_level {
    /// Detail for debugging a plugin.
    pub const DEBUG: u32 = 0;
    /// Something the user may want to know.
    pub const INFO: u32 = 1;
    /// Something went wrong without failing the run.
    pub const WARN: u32 = 2;
}

/// The symbol a native plugin exports, NUL-terminated: `extern "C" fn() -> *const PluginV1`.
pub const NATIVE_ENTRY: &[u8] = b"alpha2_plugin_v1\0";

/// What a host gives a native plugin instance. Every function takes `context` first.
///
/// Pointers passed to these functions are borrowed for the call alone. `/1` grows only by fields
/// appended here; a plugin reads `abi` and `size` first and uses a later field only when `size`
/// covers it.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct HostV1 {
    /// [`ABI_MAJOR`].
    pub abi: u32,
    /// Bytes of the table as the host that filled it knows it.
    pub size: u32,
    /// The host's own state for this instance.
    pub context: *mut c_void,
    /// Takes a batch of [`records`]; returns [`status::OK`], or [`status::FAILED`] once the
    /// session has refused an emit and the plugin should stop.
    pub emit: unsafe extern "C" fn(context: *mut c_void, records: *const u8, len: usize) -> i32,
    /// The resource `name` (UTF-8) and its length, or null when the manifest declares none by that
    /// name. The bytes stay valid until the instance is destroyed.
    pub resource: unsafe extern "C" fn(
        context: *mut c_void,
        name: *const u8,
        name_len: usize,
        len: *mut usize,
    ) -> *const u8,
    /// Records why the call about to return [`status::FAILED`] failed (UTF-8).
    pub fail: unsafe extern "C" fn(context: *mut c_void, message: *const u8, len: usize),
    /// Writes a diagnostic (UTF-8) at a [`log_level`].
    pub log: unsafe extern "C" fn(context: *mut c_void, level: u32, message: *const u8, len: usize),
}

/// The static table a native plugin's [`NATIVE_ENTRY`] returns, which outlives every instance.
///
/// Calls other than `describe` and `create` take the instance `create` returned. Pointers passed to
/// the plugin are borrowed for the call alone. Every entry is required. `/1` grows only by fields
/// appended here; a host reads `abi` and `size` first and refuses a table shorter than its own.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PluginV1 {
    /// [`ABI_MAJOR`].
    pub abi: u32,
    /// Bytes of the table as the plugin that filled it knows it.
    pub size: u32,
    /// The descriptor as UTF-8 JSON and its length; static, valid while the library is loaded.
    pub describe: unsafe extern "C" fn(len: *mut usize) -> *const u8,
    /// A new instance that calls `host`'s functions; null when it cannot be made. `host` stays
    /// valid until `destroy`.
    pub create: unsafe extern "C" fn(host: *const HostV1) -> *mut c_void,
    /// Frees an instance; no call takes it afterwards.
    pub destroy: unsafe extern "C" fn(instance: *mut c_void),
    /// Starts the run with its context, UTF-8 JSON.
    pub begin: unsafe extern "C" fn(instance: *mut c_void, context: *const u8, len: usize) -> i32,
    /// Starts pass `pass` in a [`direction`].
    pub pass: unsafe extern "C" fn(instance: *mut c_void, pass: u32, direction: u32) -> i32,
    /// One frame: `len` bytes of rows top first, tightly packed in a [`video_format`].
    pub video: unsafe extern "C" fn(
        instance: *mut c_void,
        time: i64,
        width: u32,
        height: u32,
        format: u32,
        data: *const u8,
        len: usize,
    ) -> i32,
    /// One block of `count` samples, planar, channel after channel.
    pub audio: unsafe extern "C" fn(
        instance: *mut c_void,
        start: i64,
        start_sample: i64,
        frames: u32,
        rate: u32,
        channels: u32,
        samples: *const f32,
        count: usize,
    ) -> i32,
    /// Ends the run; whole-range work emits here.
    pub end: unsafe extern "C" fn(instance: *mut c_void) -> i32,
}

/// Names a wasm plugin exports and imports. Pointers and lengths are `i32` offsets into the
/// module's exported `memory`.
pub mod wasm {
    /// The module the host functions are imported from.
    pub const IMPORT_MODULE: &str = "alpha2";
    /// `() -> i32`: [`super::ABI_MAJOR`].
    pub const ABI: &str = "alpha2_abi";
    /// `(size: i32, align: i32) -> i32`: memory the host writes call arguments into.
    pub const ALLOC: &str = "alpha2_alloc";
    /// `(ptr: i32, size: i32, align: i32)`: frees what `alpha2_alloc` returned.
    pub const FREE: &str = "alpha2_free";
    /// `() -> i64`: the descriptor JSON's offset in the low 32 bits and its length in the high 32.
    pub const DESCRIBE: &str = "alpha2_describe";
    /// `(ptr: i32, len: i32) -> i32`.
    pub const BEGIN: &str = "alpha2_begin";
    /// `(pass: i32, direction: i32) -> i32`.
    pub const PASS: &str = "alpha2_pass";
    /// `(time: i64, width: i32, height: i32, format: i32, ptr: i32, len: i32) -> i32`.
    pub const VIDEO: &str = "alpha2_video";
    /// `(start: i64, start_sample: i64, frames: i32, rate: i32, channels: i32, ptr: i32,
    /// count: i32) -> i32`; `count` samples of 4 bytes each.
    pub const AUDIO: &str = "alpha2_audio";
    /// `() -> i32`.
    pub const END: &str = "alpha2_end";
    /// The exported memory.
    pub const MEMORY: &str = "memory";
    /// Import `(ptr: i32, len: i32) -> i32`, as [`super::HostV1::emit`].
    pub const EMIT: &str = "emit";
    /// Import `(name: i32, name_len: i32) -> i64`: the resource's length, or -1.
    pub const RESOURCE_SIZE: &str = "resource_size";
    /// Import `(name: i32, name_len: i32, dst: i32, dst_len: i32) -> i32`: copies the resource;
    /// 0 when it was copied whole.
    pub const RESOURCE_READ: &str = "resource_read";
    /// Import `(ptr: i32, len: i32)`, as [`super::HostV1::fail`].
    pub const FAIL: &str = "fail";
    /// Import `(level: i32, ptr: i32, len: i32)`, as [`super::HostV1::log`].
    pub const LOG: &str = "log";
}

#[cfg(test)]
mod tests;
