//! Container open and close, replacing rsmpeg's constructors.
//!
//! rsmpeg's input builder cannot install an interrupt callback before `avformat_open_input`, and
//! its output `Drop` unwraps `avio_close`, which panics after a failed write such as a full disk.

use std::{
    ffi::{CStr, CString, c_int, c_void},
    fs::File,
    io::Read,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    path::Path,
    ptr::{self, NonNull},
    sync::{Arc, Mutex, PoisonError},
};

use rsmpeg::{
    UnsafeDerefMut,
    avcodec::AVPacket,
    avformat::{AVFormatContextInput, AVFormatContextOutput, AVIOContextContainer},
    avutil::AVDictionary,
    error::RsmpegError,
    ffi,
};

use crate::BridgeError;

/// Returns whether the interrupted call should stop now.
type InterruptCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// `FFmpeg` holds a raw pointer to this, so the input keeps it alive as long as its context.
#[derive(Default)]
struct InputInterrupt {
    check: Mutex<Option<InterruptCheck>>,
}

impl InputInterrupt {
    fn requested(&self) -> bool {
        let check = self
            .check
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        check.is_some_and(|check| check())
    }
}

/// Nonzero stops the blocking call with `AVERROR_EXIT`.
unsafe extern "C" fn interrupt_requested(opaque: *mut c_void) -> c_int {
    // SAFETY: `opaque` is the owning `DemuxInput`'s `InputInterrupt`, alive until after the context
    // is closed; only shared access is taken.
    let interrupt = unsafe { &*opaque.cast::<InputInterrupt>() };
    c_int::from(interrupt.requested())
}

/// A container opened for reading with only the named demuxers, only the `file` protocol, and an
/// interrupt callback.
///
/// The context is read through [`Deref`] but cannot be moved out, since `FFmpeg` points at the
/// input's interrupt state:
///
/// ```compile_fail,E0596
/// # fn steal(mut demux: alpha2_ffmpeg_bridge::DemuxInput, other: rsmpeg::avformat::AVFormatContextInput) {
/// let stolen = std::mem::replace(&mut *demux, other);
/// drop(demux);
/// # }
/// ```
pub struct DemuxInput {
    // Declared first so it closes before the interrupt state is released.
    input: AVFormatContextInput,
    interrupt: Arc<InputInterrupt>,
}

impl DemuxInput {
    /// Makes blocking I/O on this input stop with `AVERROR_EXIT` once `check` returns true.
    pub fn interrupt_when(&self, check: impl Fn() -> bool + Send + Sync + 'static) {
        *self
            .interrupt
            .check
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(check));
    }

    /// The `AVStream` index of the best decodable stream of `media_type`, never an attached
    /// picture such as cover art.
    ///
    /// `None` when no stream of the type can be decoded; the input's other streams still may.
    ///
    /// # Errors
    /// Any other `av_find_best_stream` failure.
    pub fn best_stream(&self, media_type: ffi::AVMediaType) -> Result<Option<usize>, RsmpegError> {
        let attached = |index: usize| {
            // bindgen's flag type differs by target, so widen both sides.
            self.streams().get(index).is_some_and(|stream| {
                i64::from(stream.disposition) & i64::from(ffi::AV_DISPOSITION_ATTACHED_PIC) != 0
            })
        };
        let find = |wanted: c_int| {
            let mut decoder: *const ffi::AVCodec = ptr::null();
            // SAFETY: the context is an open input this value owns; `av_find_best_stream` only
            // reads it, and writes the decoder it found into `decoder`, a local.
            let found = unsafe {
                ffi::av_find_best_stream(
                    self.input.as_ptr().cast_mut(),
                    media_type,
                    wanted,
                    -1,
                    &raw mut decoder,
                    0,
                )
            };
            match found {
                index if index >= 0 => Ok(usize::try_from(index).ok()),
                ffi::AVERROR_STREAM_NOT_FOUND | ffi::AVERROR_DECODER_NOT_FOUND => Ok(None),
                error => Err(RsmpegError::AVError(error)),
            }
        };
        match find(-1)? {
            Some(index) if attached(index) => {}
            found => return Ok(found),
        }
        for (index, stream) in self.streams().iter().enumerate() {
            if stream.codecpar().codec_type != media_type || attached(index) {
                continue;
            }
            if let Some(found) = find(c_int::try_from(index).unwrap_or(c_int::MAX))? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// The next packet in file order, or `None` at the end of the file.
    ///
    /// # Errors
    /// The `FFmpeg` read failure, or `AVERROR_EXIT` when interrupted.
    pub fn read_packet(&mut self) -> Result<Option<AVPacket>, RsmpegError> {
        self.input.read_packet()
    }

    /// Seeks to the key frame at `timestamp` (`stream_index` -1 for `AV_TIME_BASE` units).
    ///
    /// # Errors
    /// The `FFmpeg` seek failure, or `AVERROR_EXIT` when interrupted.
    pub fn seek(
        &mut self,
        stream_index: i32,
        timestamp: i64,
        flags: i32,
    ) -> Result<(), RsmpegError> {
        self.input.seek(stream_index, timestamp, flags)
    }

    /// Has the demuxer discard every stream but `index` (`mov` then skips their bytes entirely).
    pub fn discard_all_but(&mut self, index: usize) {
        self.keep_only(&[index]);
    }

    /// [`Self::discard_all_but`] for several streams.
    pub fn keep_only(&mut self, kept: &[usize]) {
        for (at, stream) in self.streams().iter().enumerate() {
            let discard = if kept.contains(&at) {
                ffi::AVDISCARD_DEFAULT
            } else {
                ffi::AVDISCARD_ALL
            };
            // SAFETY: the stream belongs to the exclusively borrowed input; `discard` is a plain
            // field the demuxer reads later.
            unsafe { (*stream.as_ptr().cast_mut()).discard = discard };
        }
    }
}

impl Deref for DemuxInput {
    type Target = AVFormatContextInput;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

/// Why a path cannot be passed to `FFmpeg` as a UTF-8 C string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The path is not valid Unicode.
    NotUtf8,
    /// The path holds a NUL byte, where a C string would end.
    Nul,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotUtf8 => "is not UTF-8",
            Self::Nul => "contains a NUL byte",
        })
    }
}

impl std::error::Error for PathError {}

/// `file:` plus the absolute path, so a name like `take:1.mov` is never read as a protocol.
///
/// # Errors
/// When the path is not UTF-8 or holds a NUL byte.
pub fn file_url(path: &Path) -> Result<CString, PathError> {
    // If `absolute` fails, the prefix alone still keeps the name a file.
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_str().ok_or(PathError::NotUtf8)?;
    CString::new(format!("file:{text}")).map_err(|_| PathError::Nul)
}

/// Why [`open_input`] opened nothing.
#[derive(Debug)]
pub enum OpenInputError {
    /// The path cannot be handed to `FFmpeg`.
    Path(PathError),
    /// The file is a container `formats` does not name (the claiming demuxer).
    Refused(String),
    /// `FFmpeg` could not open or read it.
    Ffmpeg(RsmpegError),
}

impl std::fmt::Display for OpenInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(error) => write!(formatter, "the path {error}"),
            Self::Refused(format) => write!(formatter, "the file is {format} input"),
            Self::Ffmpeg(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for OpenInputError {}

/// Opens `path` and reads its stream information using only the demuxers in `formats`
/// (comma-separated `FFmpeg` short names) and only the `file` protocol.
///
/// Still images pick their demuxer by signature. Otherwise `image2` claims any file named like an
/// image before the JPEG demuxer has seen a large JPEG whole, and `formats` leaves `image2` out.
///
/// # Errors
/// [`OpenInputError::Path`] for an unusable path, [`OpenInputError::Refused`] for a container
/// `formats` leaves out, else the `FFmpeg` failure.
pub fn open_input(path: &Path, formats: &CStr) -> Result<DemuxInput, OpenInputError> {
    let url = file_url(path).map_err(OpenInputError::Path)?;
    let url = url.as_c_str();
    crate::install_log_capture();
    let still = still_image_format(path)
        .filter(|name| allowed_name(name, formats))
        // SAFETY: `name` is a NUL-terminated demuxer name; the result is null or one of `FFmpeg`'s
        // static demuxers.
        .map_or(ptr::null(), |name| unsafe {
            ffi::av_find_input_format(name.as_ptr())
        });
    open_with(url, formats, still).map_err(|failure| match failure {
        OpenFailure::Open(opened) => refused_format(path, url, formats).map_or(
            OpenInputError::Ffmpeg(RsmpegError::OpenInputError(opened)),
            OpenInputError::Refused,
        ),
        OpenFailure::Other(error) => error,
    })
}

fn still_image_format(path: &Path) -> Option<&'static CStr> {
    let mut signature = [0_u8; 12];
    File::open(path).ok()?.read_exact(&mut signature).ok()?;
    if signature.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(c"jpeg_pipe")
    } else if signature.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(c"png_pipe")
    } else if signature.starts_with(b"RIFF") && signature.ends_with(b"WEBP") {
        Some(c"webp_pipe")
    } else {
        None
    }
}

enum OpenFailure {
    Open(c_int),
    Other(OpenInputError),
}

/// Opens `url` with `format`, or lets `FFmpeg` pick when it is null.
fn open_with(
    url: &CStr,
    formats: &CStr,
    format: *const ffi::AVInputFormat,
) -> Result<DemuxInput, OpenFailure> {
    let interrupt = Arc::new(InputInterrupt::default());
    // SAFETY: allocation has no preconditions; a null result is handled below.
    let mut context = unsafe { ffi::avformat_alloc_context() };
    if context.is_null() {
        return Err(OpenFailure::Other(OpenInputError::Ffmpeg(
            RsmpegError::AVError(ffi::AVERROR(ffi::ENOMEM)),
        )));
    }
    // SAFETY: the context is fresh and unshared. The opaque pointer outlives it: the returned input
    // keeps the `Arc`, and on failure `avformat_open_input` frees the context first.
    unsafe {
        (*context).interrupt_callback = ffi::AVIOInterruptCB {
            callback: Some(interrupt_requested),
            opaque: Arc::as_ptr(&interrupt).cast_mut().cast(),
        };
    }
    let options =
        AVDictionary::new(c"format_whitelist", formats, 0).set(c"protocol_whitelist", c"file", 0);
    let mut options = options.into_raw().as_ptr();
    // SAFETY: `context` is fresh, `url` NUL-terminated, `format` null or a static demuxer, and
    // `options` a dictionary the call may consume; it frees and nulls `context` on failure.
    let opened = unsafe {
        ffi::avformat_open_input(&raw mut context, url.as_ptr(), format, &raw mut options)
    };
    // SAFETY: `options` is null or the dictionary `avformat_open_input` handed back, which this
    // owns.
    unsafe { ffi::av_dict_free(&raw mut options) };
    if opened < 0 {
        return Err(OpenFailure::Open(opened));
    }
    let Some(context) = NonNull::new(context) else {
        return Err(OpenFailure::Open(opened));
    };
    // SAFETY: `avformat_open_input` succeeded, so `context` is an open input context this now owns;
    // its `Drop` closes it with `avformat_close_input`.
    let mut input = unsafe { AVFormatContextInput::from_raw(context) };
    // SAFETY: the context is open and exclusively owned; null options read no per-stream
    // dictionaries.
    let found = unsafe { ffi::avformat_find_stream_info(input.as_mut_ptr(), ptr::null_mut()) };
    if found < 0 {
        return Err(OpenFailure::Other(OpenInputError::Ffmpeg(
            RsmpegError::FindStreamInfoError(found),
        )));
    }
    Ok(DemuxInput { input, interrupt })
}

/// The demuxer `FFmpeg` would pick for `path` when `formats` does not name it, so a failed open
/// can say why instead of `EINVAL`.
fn refused_format(path: &Path, url: &CStr, formats: &CStr) -> Option<String> {
    const PROBE_BYTES: u64 = 1 << 20;
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(PROBE_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let size = c_int::try_from(bytes.len()).ok()?;
    bytes.resize(
        bytes.len() + usize::try_from(ffi::AVPROBE_PADDING_SIZE).ok()?,
        0,
    );
    let probe = ffi::AVProbeData {
        filename: url.as_ptr(),
        buf: bytes.as_mut_ptr(),
        buf_size: size,
        mime_type: ptr::null(),
    };
    let mut score = 0;
    // SAFETY: `probe` points at `size` readable bytes followed by the zeroed padding `FFmpeg`
    // requires, and at a NUL-terminated filename; both outlive the call.
    let format = unsafe { ffi::av_probe_input_format3(&raw const probe, 1, &raw mut score) };
    if format.is_null() {
        return None;
    }
    // SAFETY: a non-null result is one of `FFmpeg`'s static demuxer descriptions, whose name is a
    // static NUL-terminated string.
    let name = unsafe { CStr::from_ptr((*format).name) };
    (!allowed_name(name, formats)).then(|| name.to_string_lossy().into_owned())
}

fn allowed_name(name: &CStr, formats: &CStr) -> bool {
    let allowed = formats.to_string_lossy();
    name.to_string_lossy()
        .split(',')
        .any(|part| allowed.split(',').any(|format| format == part))
}

/// A container opened for writing whose file is closed without panicking.
///
/// [`Self::close`] reports any earlier write error or one reported at `close(2)`; dropping discards
/// it.
pub struct OutputFile(AVFormatContextOutput);

/// Why [`OutputFile::create`] created nothing.
#[derive(Debug)]
pub enum CreateOutputError {
    /// The path cannot be handed to `FFmpeg`.
    Path(PathError),
    /// There is no such muxer, or the file cannot be opened.
    Ffmpeg(RsmpegError),
}

impl std::fmt::Display for CreateOutputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(error) => write!(formatter, "the path {error}"),
            Self::Ffmpeg(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CreateOutputError {}

impl OutputFile {
    /// Creates the output with the `FFmpeg` muxer `format` (never guessed from the file name) and
    /// opens `path` through the `file` protocol only.
    ///
    /// # Errors
    /// When the path is unusable, there is no such muxer, or the file cannot be opened.
    pub fn create(path: &Path, format: &CStr) -> Result<Self, CreateOutputError> {
        let url = file_url(path).map_err(CreateOutputError::Path)?;
        let mut options = Some(AVDictionary::new(c"protocol_whitelist", c"file", 0));
        AVFormatContextOutput::builder()
            .format_name(format)
            .filename(&url)
            .options(&mut options)
            .build()
            .map(Self)
            .map_err(CreateOutputError::Ffmpeg)
    }

    /// Closes the file, after the trailer.
    ///
    /// # Errors
    /// The I/O error reported on closing.
    pub fn close(mut self) -> Result<(), BridgeError> {
        close_io(&mut self.0)
    }
}

impl Deref for OutputFile {
    type Target = AVFormatContextOutput;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OutputFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        let _ = close_io(&mut self.0);
    }
}

/// Closes and detaches `output`'s file, if open.
fn close_io(output: &mut AVFormatContextOutput) -> Result<(), BridgeError> {
    let Some(io) = output.io_context.take() else {
        return Ok(());
    };
    // SAFETY: the context is live and exclusively borrowed. `pb` is the I/O just taken, which is
    // closed below; `avformat_free_context` and rsmpeg's `Drop` skip a null `pb`.
    unsafe {
        output.deref_mut().pb = ptr::null_mut();
    }
    match io {
        AVIOContextContainer::Url(url) => {
            // rsmpeg's `Drop` would close it again and unwrap the result.
            let mut url = ManuallyDrop::new(url);
            let mut raw = url.as_mut_ptr();
            // SAFETY: `raw` is the I/O context `avio_open2` opened for this output, closed nowhere
            // else: its owner is never dropped.
            let closed = unsafe { ffi::avio_closep(&raw mut raw) };
            if closed < 0 {
                return Err(BridgeError::Ffmpeg(RsmpegError::AVError(closed)));
            }
            Ok(())
        }
        AVIOContextContainer::Custom(custom) => {
            drop(custom);
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "container_tests.rs"]
mod tests;
