//! Owned `AMediaCodec` and `AMediaFormat` handles.
//!
//! The NDK objects are not thread-safe; a session owns its codec and uses it from one thread at a
//! time, which the host-core backend traits guarantee (`&mut self` methods on a `Send` value).

// The crate's only FFI boundary.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::{NonNull, null_mut};

use alpha2_host_core::video_stream::BackendError;
use ndk_sys as sys;

/// `AMEDIACODEC_ERROR_INSUFFICIENT_RESOURCE` and `AMEDIACODEC_ERROR_RECLAIMED`: the system took
/// the codec's resources for another app.
const ERROR_INSUFFICIENT_RESOURCE: i32 = 1100;
const ERROR_RECLAIMED: i32 = 1101;
/// `AMEDIA_ERROR_INVALID_OBJECT` and `AMEDIA_ERROR_INVALID_OPERATION`: the codec died under us.
const ERROR_INVALID_OBJECT: i32 = -10002;
const ERROR_INVALID_OPERATION: i32 = -10006;

/// `AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM`.
pub(crate) const FLAG_END_OF_STREAM: u32 = 4;
/// `AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG`.
pub(crate) const FLAG_CODEC_CONFIG: u32 = 2;

/// Added to every timestamp handed to a codec: sessions misorder or clamp negative times, and
/// audio decoders time outputs from their first input, so container times before zero (priming,
/// edit lists) must arrive positive.
const TIME_OFFSET_US: i128 = 1 << 40;

/// The codec timestamp of `pts` in units of `timescale` per second.
pub(crate) fn to_codec_us(pts: i64, timescale: i64) -> i64 {
    let scaled = i128::from(pts) * 1_000_000 / i128::from(timescale.max(1)) + TIME_OFFSET_US;
    i64::try_from(scaled.max(0)).unwrap_or(i64::MAX)
}

/// The time in units of `timescale` per second of codec timestamp `us`.
pub(crate) fn from_codec_us(us: i64, timescale: i64) -> i64 {
    let scaled = (i128::from(us) - TIME_OFFSET_US) * i128::from(timescale) / 1_000_000;
    i64::try_from(scaled).unwrap_or(0)
}

fn status(call: &str, code: i32) -> Result<(), BackendError> {
    match code {
        0 => Ok(()),
        ERROR_RECLAIMED
        | ERROR_INSUFFICIENT_RESOURCE
        | ERROR_INVALID_OBJECT
        | ERROR_INVALID_OPERATION => Err(BackendError::SessionLost(format!(
            "MediaCodec {call} failed with {code}"
        ))),
        _ => Err(BackendError::Failed(format!(
            "MediaCodec {call} failed with {code}"
        ))),
    }
}

fn c_string(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

/// An owned `AMediaFormat`.
pub(crate) struct Format(NonNull<sys::AMediaFormat>);

// SAFETY: a format is plain data the NDK copies on use; it is only touched through `&self` or
// `&mut self` of its owner.
unsafe impl Send for Format {}

impl Format {
    pub(crate) fn new() -> Self {
        // SAFETY: no arguments; the NDK returns a new format or aborts on allocation failure.
        let raw = unsafe { sys::AMediaFormat_new() };
        Self(NonNull::new(raw).expect("AMediaFormat_new returned null"))
    }

    pub(crate) fn as_ptr(&self) -> *mut sys::AMediaFormat {
        self.0.as_ptr()
    }

    pub(crate) fn set_str(&mut self, key: &str, value: &str) {
        let (key, value) = (c_string(key), c_string(value));
        // SAFETY: both strings are NUL-terminated and outlive the call, which copies them.
        unsafe { sys::AMediaFormat_setString(self.as_ptr(), key.as_ptr(), value.as_ptr()) };
    }

    pub(crate) fn set_i32(&mut self, key: &str, value: i32) {
        let key = c_string(key);
        // SAFETY: the key is NUL-terminated and outlives the call.
        unsafe { sys::AMediaFormat_setInt32(self.as_ptr(), key.as_ptr(), value) };
    }

    pub(crate) fn set_f32(&mut self, key: &str, value: f32) {
        let key = c_string(key);
        // SAFETY: the key is NUL-terminated and outlives the call.
        unsafe { sys::AMediaFormat_setFloat(self.as_ptr(), key.as_ptr(), value) };
    }

    pub(crate) fn set_buffer(&mut self, key: &str, bytes: &[u8]) {
        let key = c_string(key);
        // SAFETY: the key and bytes outlive the call, which copies the bytes.
        unsafe {
            sys::AMediaFormat_setBuffer(
                self.as_ptr(),
                key.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
            );
        }
    }

    pub(crate) fn i32(&self, key: &str) -> Option<i32> {
        let key = c_string(key);
        let mut value = 0;
        // SAFETY: the key is NUL-terminated; `value` outlives the call.
        unsafe { sys::AMediaFormat_getInt32(self.as_ptr(), key.as_ptr(), &raw mut value) }
            .then_some(value)
    }

    pub(crate) fn buffer(&self, key: &str) -> Option<Vec<u8>> {
        let key = c_string(key);
        let mut data: *mut c_void = null_mut();
        let mut size = 0_usize;
        // SAFETY: the out-pointers outlive the call; the returned bytes belong to the format and
        // are copied before it changes.
        let found = unsafe {
            sys::AMediaFormat_getBuffer(self.as_ptr(), key.as_ptr(), &raw mut data, &raw mut size)
        };
        if !found || data.is_null() {
            return None;
        }
        // SAFETY: the NDK reported `size` readable bytes at `data`.
        Some(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec())
    }

    /// The `crop` rectangle as `(left, top, right, bottom)`, inclusive.
    pub(crate) fn rect(&self, key: &str) -> Option<(i32, i32, i32, i32)> {
        let key = c_string(key);
        let (mut left, mut top, mut right, mut bottom) = (0, 0, 0, 0);
        // SAFETY: the out-pointers outlive the call.
        unsafe {
            sys::AMediaFormat_getRect(
                self.as_ptr(),
                key.as_ptr(),
                &raw mut left,
                &raw mut top,
                &raw mut right,
                &raw mut bottom,
            )
        }
        .then_some((left, top, right, bottom))
    }

    /// The format as the NDK prints it, for diagnostics.
    pub(crate) fn describe(&self) -> String {
        // SAFETY: the returned string belongs to the format and is copied at once.
        let text = unsafe { sys::AMediaFormat_toString(self.as_ptr()) };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: the NDK returns a NUL-terminated string.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        // SAFETY: the format is owned and not used after this.
        unsafe { sys::AMediaFormat_delete(self.as_ptr()) };
    }
}

/// What a dequeue of output found.
pub(crate) enum Output {
    /// A filled buffer.
    Buffer {
        index: usize,
        offset: usize,
        size: usize,
        pts_us: i64,
        flags: u32,
    },
    /// The output format changed; read it before the next buffer.
    FormatChanged,
    /// Nothing within the timeout.
    TryAgain,
}

/// An owned, started `AMediaCodec`.
pub(crate) struct Codec {
    raw: NonNull<sys::AMediaCodec>,
    name: String,
    started: bool,
}

// SAFETY: the codec is used from one thread at a time through its owner's `&mut self`.
unsafe impl Send for Codec {}

impl Codec {
    /// The codec named `name`.
    pub(crate) fn by_name(name: &str) -> Result<Self, BackendError> {
        let text = c_string(name);
        // SAFETY: the name is NUL-terminated and outlives the call.
        let raw = unsafe { sys::AMediaCodec_createCodecByName(text.as_ptr()) };
        Self::from_raw(raw, name)
    }

    /// The platform's preferred decoder for `mime`, hardware first.
    pub(crate) fn decoder_for(mime: &str) -> Result<Self, BackendError> {
        let text = c_string(mime);
        // SAFETY: the type is NUL-terminated and outlives the call.
        let raw = unsafe { sys::AMediaCodec_createDecoderByType(text.as_ptr()) };
        Self::from_raw(raw, mime)
    }

    /// The platform's preferred encoder for `mime`, hardware first.
    pub(crate) fn encoder_for(mime: &str) -> Result<Self, BackendError> {
        let text = c_string(mime);
        // SAFETY: the type is NUL-terminated and outlives the call.
        let raw = unsafe { sys::AMediaCodec_createEncoderByType(text.as_ptr()) };
        Self::from_raw(raw, mime)
    }

    fn from_raw(raw: *mut sys::AMediaCodec, asked: &str) -> Result<Self, BackendError> {
        let raw = NonNull::new(raw)
            .ok_or_else(|| BackendError::Unsupported(format!("no MediaCodec {asked}")))?;
        let mut name: *mut c_char = null_mut();
        // SAFETY: `raw` is a live codec; the name is released with the matching call.
        let named = unsafe { sys::AMediaCodec_getName(raw.as_ptr(), &raw mut name) }.0 == 0;
        let text = if named && !name.is_null() {
            // SAFETY: the NDK returned a NUL-terminated string it owns until released.
            let text = unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: `name` came from `AMediaCodec_getName` on this codec.
            unsafe { sys::AMediaCodec_releaseName(raw.as_ptr(), name) };
            text
        } else {
            asked.to_owned()
        };
        Ok(Self {
            raw,
            name: text,
            started: false,
        })
    }

    /// The component's name (`c2.android.avc.decoder`).
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Whether the component is one of the platform's software codecs, as their names say. The
    /// API 36 codec store would declare it, but on some Android 16 builds a process that has
    /// read the store aborts when it exits (a double free in its static codec table).
    pub(crate) fn is_software(&self) -> bool {
        self.name.starts_with("c2.android.") || self.name.starts_with("OMX.google.")
    }

    /// Configures for byte-buffer output and starts.
    pub(crate) fn start(&mut self, format: &Format, encoder: bool) -> Result<(), BackendError> {
        self.configure_and_start(format, null_mut(), encoder)
    }

    /// Configures a decoder to render into `window` and starts. The window must outlive the
    /// codec.
    pub(crate) fn start_on(
        &mut self,
        format: &Format,
        window: *mut sys::ANativeWindow,
    ) -> Result<(), BackendError> {
        self.configure_and_start(format, window, false)
    }

    /// Stops the codec and starts it again with `format`, into `window` when it is not null.
    /// The window must outlive the codec.
    pub(crate) fn restart(
        &mut self,
        format: &Format,
        window: *mut sys::ANativeWindow,
    ) -> Result<(), BackendError> {
        if self.started {
            // SAFETY: the codec is started; stopping returns it to the uninitialized state.
            let stopped = unsafe { sys::AMediaCodec_stop(self.raw.as_ptr()) };
            self.started = false;
            status("stop", stopped.0)?;
        }
        self.configure_and_start(format, window, false)
    }

    fn configure_and_start(
        &mut self,
        format: &Format,
        window: *mut sys::ANativeWindow,
        encoder: bool,
    ) -> Result<(), BackendError> {
        // SAFETY: the codec and format are live; the window is null or outlives the codec per
        // the callers; no crypto is passed.
        let configured = unsafe {
            sys::AMediaCodec_configure(
                self.raw.as_ptr(),
                format.as_ptr(),
                window,
                null_mut(),
                u32::from(encoder),
            )
        };
        if configured.0 != 0 {
            return Err(BackendError::Unsupported(format!(
                "{} refused the format ({}): {}",
                self.name,
                configured.0,
                format.describe()
            )));
        }
        // SAFETY: the codec is configured.
        let started = unsafe { sys::AMediaCodec_start(self.raw.as_ptr()) };
        status("start", started.0)?;
        self.started = true;
        Ok(())
    }

    /// An input buffer's index, or `None` within `timeout_us`.
    pub(crate) fn dequeue_input(&mut self, timeout_us: i64) -> Result<Option<usize>, BackendError> {
        // SAFETY: the codec is started.
        let index = unsafe { sys::AMediaCodec_dequeueInputBuffer(self.raw.as_ptr(), timeout_us) };
        match index {
            index if index >= 0 => Ok(usize::try_from(index).ok()),
            -1 => Ok(None),
            error => status(
                "dequeueInputBuffer",
                i32::try_from(error).unwrap_or(i32::MIN),
            )
            .map(|()| None),
        }
    }

    /// Copies `bytes` into input buffer `index` and queues it.
    pub(crate) fn queue_input(
        &mut self,
        index: usize,
        bytes: &[u8],
        pts_us: i64,
        flags: u32,
    ) -> Result<(), BackendError> {
        let mut capacity = 0_usize;
        // SAFETY: `index` came from `dequeue_input` on this started codec.
        let buffer =
            unsafe { sys::AMediaCodec_getInputBuffer(self.raw.as_ptr(), index, &raw mut capacity) };
        if buffer.is_null() || bytes.len() > capacity {
            return Err(BackendError::Failed(format!(
                "{}: a {} byte packet does not fit its {capacity} byte input buffer",
                self.name,
                bytes.len()
            )));
        }
        // SAFETY: the buffer holds `capacity` >= `bytes.len()` writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
        let time = u64::try_from(pts_us.max(0)).unwrap_or(0);
        // SAFETY: the buffer is filled; ownership returns to the codec.
        let queued = unsafe {
            sys::AMediaCodec_queueInputBuffer(self.raw.as_ptr(), index, 0, bytes.len(), time, flags)
        };
        status("queueInputBuffer", queued.0)
    }

    /// Lets `fill` write into input buffer `index`, then queues the bytes it reports written.
    pub(crate) fn write_input(
        &mut self,
        index: usize,
        pts_us: i64,
        flags: u32,
        fill: impl FnOnce(&mut [u8]) -> Result<usize, BackendError>,
    ) -> Result<(), BackendError> {
        let mut capacity = 0_usize;
        // SAFETY: `index` came from `dequeue_input` on this started codec.
        let buffer =
            unsafe { sys::AMediaCodec_getInputBuffer(self.raw.as_ptr(), index, &raw mut capacity) };
        if buffer.is_null() {
            return Err(BackendError::Failed(format!(
                "{}: input buffer {index} is missing",
                self.name
            )));
        }
        // SAFETY: the codec lent `capacity` writable bytes until the buffer is queued.
        let written = fill(unsafe { std::slice::from_raw_parts_mut(buffer, capacity) })?;
        let time = u64::try_from(pts_us.max(0)).unwrap_or(0);
        // SAFETY: `written` <= `capacity` bytes are filled; ownership returns to the codec.
        let queued = unsafe {
            sys::AMediaCodec_queueInputBuffer(
                self.raw.as_ptr(),
                index,
                0,
                written.min(capacity),
                time,
                flags,
            )
        };
        status("queueInputBuffer", queued.0)
    }

    /// The format input buffers are laid out in.
    pub(crate) fn input_format(&self) -> Format {
        // SAFETY: the codec is started; the returned format is owned by the caller.
        let raw = unsafe { sys::AMediaCodec_getInputFormat(self.raw.as_ptr()) };
        Format(NonNull::new(raw).unwrap_or_else(|| Format::new().0))
    }

    /// Changes parameters of the running codec (`request-sync`, `video-bitrate`).
    pub(crate) fn set_parameters(&mut self, format: &Format) -> Result<(), BackendError> {
        // SAFETY: the codec is started and the format outlives the call.
        status(
            "setParameters",
            unsafe { sys::AMediaCodec_setParameters(self.raw.as_ptr(), format.as_ptr()) }.0,
        )
    }

    /// The next output event within `timeout_us`.
    pub(crate) fn dequeue_output(&mut self, timeout_us: i64) -> Result<Output, BackendError> {
        // SAFETY: an all-zero buffer info is valid; the call fills it.
        let mut info: sys::AMediaCodecBufferInfo = unsafe { std::mem::zeroed() };
        // SAFETY: the codec is started and `info` outlives the call.
        let index = unsafe {
            sys::AMediaCodec_dequeueOutputBuffer(self.raw.as_ptr(), &raw mut info, timeout_us)
        };
        match index {
            index if index >= 0 => Ok(Output::Buffer {
                index: usize::try_from(index).unwrap_or(0),
                offset: usize::try_from(info.offset).unwrap_or(0),
                size: usize::try_from(info.size).unwrap_or(0),
                pts_us: info.presentationTimeUs,
                flags: info.flags,
            }),
            -1 | -3 => Ok(Output::TryAgain),
            -2 => Ok(Output::FormatChanged),
            error => status(
                "dequeueOutputBuffer",
                i32::try_from(error).unwrap_or(i32::MIN),
            )
            .map(|()| Output::TryAgain),
        }
    }

    /// The bytes of output buffer `index` at `offset`, `size` long.
    pub(crate) fn output_bytes(&self, index: usize, offset: usize, size: usize) -> Option<&[u8]> {
        let mut capacity = 0_usize;
        // SAFETY: `index` came from `dequeue_output` and is not yet released.
        let buffer = unsafe {
            sys::AMediaCodec_getOutputBuffer(self.raw.as_ptr(), index, &raw mut capacity)
        };
        if buffer.is_null() || offset.checked_add(size)? > capacity {
            return None;
        }
        // SAFETY: the buffer holds `capacity` bytes until released, which needs `&mut self`.
        Some(unsafe { std::slice::from_raw_parts(buffer.add(offset), size) })
    }

    /// Returns output buffer `index` to the codec.
    pub(crate) fn release_output(&mut self, index: usize) -> Result<(), BackendError> {
        // SAFETY: `index` came from `dequeue_output` and is released once.
        status(
            "releaseOutputBuffer",
            unsafe { sys::AMediaCodec_releaseOutputBuffer(self.raw.as_ptr(), index, false) }.0,
        )
    }

    /// Renders output buffer `index` into the codec's window, stamped `timestamp_ns`, and returns
    /// it to the codec.
    pub(crate) fn render_output(
        &mut self,
        index: usize,
        timestamp_ns: i64,
    ) -> Result<(), BackendError> {
        // SAFETY: `index` came from `dequeue_output` and is released once.
        status(
            "releaseOutputBufferAtTime",
            unsafe {
                sys::AMediaCodec_releaseOutputBufferAtTime(self.raw.as_ptr(), index, timestamp_ns)
            }
            .0,
        )
    }

    /// The current output format.
    pub(crate) fn output_format(&self) -> Format {
        // SAFETY: the codec is started; the returned format is owned by the caller.
        let raw = unsafe { sys::AMediaCodec_getOutputFormat(self.raw.as_ptr()) };
        Format(NonNull::new(raw).unwrap_or_else(|| Format::new().0))
    }

    /// Drops every queued buffer.
    pub(crate) fn flush(&mut self) -> Result<(), BackendError> {
        // SAFETY: the codec is started.
        status(
            "flush",
            unsafe { sys::AMediaCodec_flush(self.raw.as_ptr()) }.0,
        )
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        // SAFETY: the codec is owned and not used after this.
        unsafe {
            if self.started {
                sys::AMediaCodec_stop(self.raw.as_ptr());
            }
            sys::AMediaCodec_delete(self.raw.as_ptr());
        }
    }
}

/// The Android API level of this device.
pub(crate) fn api_level() -> u32 {
    let mut value = [0 as c_char; libc::PROP_VALUE_MAX as usize];
    // SAFETY: the name is NUL-terminated and `value` holds `PROP_VALUE_MAX` bytes.
    let length = unsafe {
        libc::__system_property_get(c"ro.build.version.sdk".as_ptr(), value.as_mut_ptr())
    };
    if length <= 0 {
        return 0;
    }
    // SAFETY: the call wrote a NUL-terminated string.
    unsafe { CStr::from_ptr(value.as_ptr()) }
        .to_str()
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}
