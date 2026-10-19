//! Opening containers: what `FFmpeg` may open when it reads one, and how a
//! written one is closed.
//!
//! rsmpeg's own constructors give neither. Its input builder allocates the
//! format context itself, so an interrupt callback cannot be installed before
//! `avformat_open_input` reads the file; and its output keeps the file's
//! `AVIOContext` in a value whose `Drop` unwraps `avio_close`, which returns
//! the error a failed write left behind, so dropping an output after a full
//! disk panics.

use std::{
    ffi::{CStr, c_int, c_void},
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

/// What an interrupt callback asks: whether the call it interrupts should
/// stop now.
type InterruptCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// The state an input's `AVIOInterruptCB` reads. `FFmpeg` holds a raw pointer
/// to it, so it lives in an `Arc` the input keeps for as long as its context.
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

/// `AVIOInterruptCB::callback`: nonzero stops the blocking call in progress
/// with `AVERROR_EXIT`.
unsafe extern "C" fn interrupt_requested(opaque: *mut c_void) -> c_int {
    // SAFETY: `opaque` is the `InputInterrupt` of the `DemuxInput` that owns this format context
    // and the I/O it opened; the input keeps it alive until after the context is closed, and only
    // shared access is taken.
    let interrupt = unsafe { &*opaque.cast::<InputInterrupt>() };
    c_int::from(interrupt.requested())
}

/// A container opened for reading under an input policy: only the demuxers
/// it names, only the `file` protocol, and an interrupt callback the reader
/// binds to its own cancellation.
///
/// The format context is read through [`Deref`], and moved only by the
/// methods here. `FFmpeg` holds a pointer to this input's interrupt state,
/// which lives exactly as long as the input, so the context can never be
/// taken out of it and outlive that state:
///
/// ```compile_fail,E0596
/// # fn steal(mut demux: alpha2_ffmpeg_bridge::DemuxInput, other: rsmpeg::avformat::AVFormatContextInput) {
/// let stolen = std::mem::replace(&mut *demux, other);
/// drop(demux);
/// # }
/// ```
pub struct DemuxInput {
    // Declared first, so it is closed before the interrupt state it points to is released.
    input: AVFormatContextInput,
    interrupt: Arc<InputInterrupt>,
}

impl DemuxInput {
    /// Makes every `FFmpeg` call on this input that waits on I/O, from now
    /// until the next call to this, stop with `AVERROR_EXIT` once `check`
    /// returns true.
    pub fn interrupt_when(&self, check: impl Fn() -> bool + Send + Sync + 'static) {
        *self
            .interrupt
            .check
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(check));
    }

    /// The stream of `media_type` a reader shows, by its `AVStream` index:
    /// the one `av_find_best_stream` picks among those with a decoder, never
    /// an attached picture (a song's cover art is metadata, not its video).
    /// When `FFmpeg` picks an attached picture, the first other stream of the
    /// type that has a decoder is chosen, in stream order.
    ///
    /// `None` when the input has no such stream, or none that this runtime
    /// can decode: a stream without a decoder leaves the type unselected
    /// rather than failing the input, whose other streams may decode.
    ///
    /// # Errors
    /// Any other `av_find_best_stream` failure.
    pub fn best_stream(&self, media_type: ffi::AVMediaType) -> Result<Option<usize>, RsmpegError> {
        let attached = |index: usize| {
            // `bindgen` types the flag by the target's C ABI; widening both sides compiles
            // everywhere.
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

    /// The next packet in file order (`av_read_frame`), or `None` at the
    /// end of the file.
    ///
    /// # Errors
    /// The `FFmpeg` read failure, or `AVERROR_EXIT` when the interrupt
    /// check stopped it.
    pub fn read_packet(&mut self) -> Result<Option<AVPacket>, RsmpegError> {
        self.input.read_packet()
    }

    /// Seeks to the key frame at `timestamp` of `stream_index`
    /// (`av_seek_frame`; -1 for `AV_TIME_BASE` units).
    ///
    /// # Errors
    /// The `FFmpeg` seek failure, or `AVERROR_EXIT` when the interrupt check
    /// stopped it.
    pub fn seek(
        &mut self,
        stream_index: i32,
        timestamp: i64,
        flags: i32,
    ) -> Result<(), RsmpegError> {
        self.input.seek(stream_index, timestamp, flags)
    }

    /// Has the demuxer skip every stream but `index`: a reader of one stream
    /// then neither reads nor copies the others' packets (`AVDISCARD_ALL`),
    /// which demuxers such as `mov` do not even fetch from the file.
    pub fn discard_all_but(&mut self, index: usize) {
        self.keep_only(&[index]);
    }

    /// [`Self::discard_all_but`] for a reader of the streams `kept`.
    pub fn keep_only(&mut self, kept: &[usize]) {
        for (at, stream) in self.streams().iter().enumerate() {
            let discard = if kept.contains(&at) {
                ffi::AVDISCARD_DEFAULT
            } else {
                ffi::AVDISCARD_ALL
            };
            // SAFETY: the stream belongs to the open input this value owns exclusively (`&mut
            // self`); `discard` is a plain field the demuxer reads on its next read or seek.
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

/// Why [`open_input`] opened nothing.
#[derive(Debug)]
pub enum OpenInputError {
    /// The file is a container `formats` does not name: the name of the
    /// demuxer that claims it.
    Refused(String),
    /// `FFmpeg` could not open or read it.
    Ffmpeg(RsmpegError),
}

impl std::fmt::Display for OpenInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(format) => write!(formatter, "the file is {format} input"),
            Self::Ffmpeg(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for OpenInputError {}

/// Opens `url` and reads its stream information with only the demuxers
/// named in `formats` (comma-separated `FFmpeg` short names) and only the
/// `file` protocol, for the input and anything it opens in turn.
///
/// A still image opens with its own demuxer, chosen by its signature, when
/// `formats` names it. `FFmpeg` otherwise picks from the file's name as well
/// as its first bytes, and stops reading bytes once a demuxer is fairly sure:
/// the `image2` demuxer, for numbered image files, claims any file named like
/// an image on its name alone, above the JPEG demuxer, which is sure of a
/// JPEG only once it has seen the whole picture. A JPEG larger than the first
/// probe was `image2` input, which `formats` leaves out.
///
/// # Errors
/// [`OpenInputError::Refused`] for a container `formats` leaves out, else the
/// `FFmpeg` failure.
pub fn open_input(url: &CStr, formats: &CStr) -> Result<DemuxInput, OpenInputError> {
    let still = still_image_format(url)
        .filter(|name| allowed_name(name, formats))
        // SAFETY: `name` is a NUL-terminated demuxer name; the result is null or one of `FFmpeg`'s
        // static demuxers.
        .map_or(ptr::null(), |name| unsafe {
            ffi::av_find_input_format(name.as_ptr())
        });
    open_with(url, formats, still).map_err(|failure| match failure {
        OpenFailure::Open(opened) => refused_format(url, formats).map_or(
            OpenInputError::Ffmpeg(RsmpegError::OpenInputError(opened)),
            OpenInputError::Refused,
        ),
        OpenFailure::Other(error) => error,
    })
}

/// The demuxer of the still image format whose signature `url` starts with.
fn still_image_format(url: &CStr) -> Option<&'static CStr> {
    let mut signature = [0_u8; 12];
    File::open(Path::new(url.to_str().ok()?))
        .ok()?
        .read_exact(&mut signature)
        .ok()?;
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

/// Opens `url` with `format`, or the demuxer `FFmpeg` picks when it is null.
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
    // SAFETY: the context was just allocated and nothing else refers to it. The opaque pointer
    // stays valid for the context's life: on success the returned input keeps the `Arc`, and on
    // failure `avformat_open_input` frees the context before this returns.
    unsafe {
        (*context).interrupt_callback = ffi::AVIOInterruptCB {
            callback: Some(interrupt_requested),
            opaque: Arc::as_ptr(&interrupt).cast_mut().cast(),
        };
    }
    let options =
        AVDictionary::new(c"format_whitelist", formats, 0).set(c"protocol_whitelist", c"file", 0);
    let mut options = options.into_raw().as_ptr();
    // SAFETY: `context` is a fresh context, `url` a NUL-terminated string, `format` null or one of
    // `FFmpeg`'s static demuxers, and `options` a dictionary this call takes and leaves the unused
    // entries of; it frees and nulls `context` on failure.
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

/// The demuxer `FFmpeg` would pick for `url`, when `formats` does not name
/// it: what a failed open is told so its error says why, rather than
/// `EINVAL`. It probes the bytes the way an open does, without opening the
/// container.
fn refused_format(url: &CStr, formats: &CStr) -> Option<String> {
    const PROBE_BYTES: u64 = 1 << 20;
    let path = Path::new(url.to_str().ok()?);
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

/// Whether `formats` names the demuxer `name`, under any of its
/// comma-separated names.
fn allowed_name(name: &CStr, formats: &CStr) -> bool {
    let allowed = formats.to_string_lossy();
    name.to_string_lossy()
        .split(',')
        .any(|part| allowed.split(',').any(|format| format == part))
}

/// A container opened for writing whose file is closed without panicking.
///
/// [`Self::close`] reports the error closing the file returns: a write that
/// failed earlier, or one the file system only reports at `close(2)`.
/// Dropping it closes the file and discards that error, which is what an
/// abandoned output wants.
pub struct OutputFile(AVFormatContextOutput);

impl OutputFile {
    /// Creates the output, guessing its muxer from `filename`, and opens the
    /// file.
    ///
    /// # Errors
    /// When no muxer fits or the file cannot be opened.
    pub fn create(filename: &CStr) -> Result<Self, RsmpegError> {
        AVFormatContextOutput::create(filename).map(Self)
    }

    /// Closes the file, after the trailer.
    ///
    /// # Errors
    /// The error the file's I/O reports on closing.
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

/// Closes `output`'s file, if it has one open, and detaches it, so the
/// format context is freed without it.
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
            // rsmpeg's `Drop` for it would close it again and unwrap the result.
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
