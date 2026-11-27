//! Container open and close, replacing rsmpeg's constructors.
//!
//! rsmpeg's input builder cannot install an interrupt callback or a custom input before
//! `avformat_open_input`, and its output `Drop` unwraps `avio_close`, which panics after a failed
//! write such as a full disk.

use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    path::Path,
    ptr::{self, NonNull},
    sync::{Arc, Mutex, PoisonError},
};

use rsmpeg::{
    UnsafeDerefMut,
    avcodec::AVPacket,
    avformat::{
        AVFormatContextInput, AVFormatContextOutput, AVIOContextContainer, AVIOContextCustom,
    },
    avutil::{AVDictionary, AVMem},
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
    // Declared first so it closes before the interrupt state and the custom input are released.
    input: AVFormatContextInput,
    _custom: Option<AVIOContextCustom>,
    read_error: ReadError,
    interrupt: Arc<InputInterrupt>,
}

/// The failure a custom input's reader returned last, which `FFmpeg` sees only as `AVERROR_EXIT`.
type ReadError = Arc<Mutex<Option<io::Error>>>;

/// A byte source `FFmpeg` reads instead of a file.
pub trait InputReader: Read + Seek + Send {}

impl<T: Read + Seek + Send> InputReader for T {}

/// An input read through readers its caller opens, such as a remote object read by range.
pub struct CustomInput<'a> {
    /// The input's name as `FFmpeg` sees its URL; probing reads its extension, as a file's.
    pub name: &'a str,
    /// The input's length in bytes, answered to `AVSEEK_SIZE`.
    pub length: u64,
    /// Opens a reader at the first byte; each sniff and the demuxer read through their own.
    pub open: &'a dyn Fn() -> io::Result<Box<dyn InputReader>>,
}

/// Where an input's bytes are.
#[derive(Clone, Copy)]
pub enum InputLocation<'a> {
    /// A local file, read through `FFmpeg`'s `file` protocol.
    File(&'a Path),
    /// Readers set as the context's `pb`; the input opens nothing else.
    Custom(&'a CustomInput<'a>),
}

impl DemuxInput {
    /// The failure a custom input's reader returned, taken once: `FFmpeg` reports such a failure
    /// as `AVERROR_EXIT`.
    pub fn take_read_error(&self) -> Option<io::Error> {
        self.read_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

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
    /// The `FFmpeg` read failure, or `AVERROR_EXIT` when interrupted or when a custom input's
    /// reader failed.
    pub fn read_packet(&mut self) -> Result<Option<AVPacket>, RsmpegError> {
        let packet = self.input.read_packet();
        self.unless_read_failed(packet)
    }

    /// A custom input's failed read can still leave `FFmpeg` a packet, cut short and flagged
    /// corrupt; the call fails instead, and the reader's failure stays for [`Self::take_read_error`].
    fn unless_read_failed<T>(&self, result: Result<T, RsmpegError>) -> Result<T, RsmpegError> {
        let failed = self
            .read_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        if failed {
            return Err(RsmpegError::AVError(ffi::AVERROR_EXIT));
        }
        result
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
        let sought = self.input.seek(stream_index, timestamp, flags);
        self.unless_read_failed(sought)
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
    /// A custom input's reader failed.
    Read(io::Error),
    /// `FFmpeg` could not open or read it.
    Ffmpeg(RsmpegError),
}

impl std::fmt::Display for OpenInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(error) => write!(formatter, "the path {error}"),
            Self::Refused(format) => write!(formatter, "the file is {format} input"),
            Self::Read(error) => write!(formatter, "{error}"),
            Self::Ffmpeg(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for OpenInputError {}

/// Opens `location` and reads its stream information using only the demuxers in `formats`
/// (comma-separated `FFmpeg` short names) and only the `file` protocol. With `decoders`
/// (comma-separated `FFmpeg` decoder names), probing opens only those decoders.
///
/// A custom input is read only through its own readers: the context's `pb` is set before the open
/// and every nested open (`io_open`) is refused, so its bytes can never make `FFmpeg` open a file.
///
/// Still images pick their demuxer by signature. Otherwise `image2` claims any file named like an
/// image before the JPEG demuxer has seen a large JPEG whole, and `formats` leaves `image2` out.
///
/// # Errors
/// [`OpenInputError::Path`] for an unusable path, [`OpenInputError::Refused`] for a container
/// `formats` leaves out, [`OpenInputError::Read`] when a custom input's reader failed, else the
/// `FFmpeg` failure.
pub fn open_input(
    location: InputLocation<'_>,
    formats: &CStr,
    decoders: Option<&CStr>,
) -> Result<DemuxInput, OpenInputError> {
    crate::install_log_capture();
    let (url, mut reader) = match &location {
        InputLocation::File(path) => (file_url(path).map_err(OpenInputError::Path)?, None),
        InputLocation::Custom(custom) => (
            CString::new(custom.name).map_err(|_| OpenInputError::Path(PathError::Nul))?,
            Some((custom.open)().map_err(OpenInputError::Read)?),
        ),
    };
    let url = url.as_c_str();
    let signature = match (&location, reader.as_mut()) {
        (InputLocation::File(path), _) => read_signature(File::open(path).ok()),
        (InputLocation::Custom(_), Some(reader)) => {
            let signature = read_signature(Some(&mut *reader));
            reader
                .seek(SeekFrom::Start(0))
                .map_err(OpenInputError::Read)?;
            signature
        }
        (InputLocation::Custom(_), None) => None,
    };
    let still = signature
        .and_then(still_image_format)
        .filter(|name| allowed_name(name, formats))
        // SAFETY: `name` is a NUL-terminated demuxer name; the result is null or one of `FFmpeg`'s
        // static demuxers.
        .map_or(ptr::null(), |name| unsafe {
            ffi::av_find_input_format(name.as_ptr())
        });
    let custom = match (&location, reader) {
        (InputLocation::Custom(custom), Some(reader)) => Some((reader, custom.length)),
        _ => None,
    };
    open_with(url, formats, decoders, still, custom).map_err(|failure| match failure {
        OpenFailure::Open(opened) => {
            let head = match &location {
                InputLocation::File(path) => read_head(File::open(path).ok()),
                InputLocation::Custom(custom) => read_head((custom.open)().ok()),
            };
            head.and_then(|head| refused_format(head, url, formats))
                .map_or(
                    OpenInputError::Ffmpeg(RsmpegError::OpenInputError(opened)),
                    OpenInputError::Refused,
                )
        }
        OpenFailure::Read(error) => OpenInputError::Read(error),
        OpenFailure::Other(error) => error,
    })
}

fn read_signature(reader: Option<impl Read>) -> Option<[u8; 12]> {
    let mut signature = [0_u8; 12];
    reader?.read_exact(&mut signature).ok()?;
    Some(signature)
}

fn still_image_format(signature: [u8; 12]) -> Option<&'static CStr> {
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
    Read(io::Error),
    Other(OpenInputError),
}

/// `FFmpeg`'s read callback over a custom input's reader. A failure is kept for the caller and
/// stops the call with `AVERROR_EXIT`, so `FFmpeg` never takes it for the end of the input.
fn custom_read(reader: &mut dyn InputReader, buffer: &mut [u8], error: &ReadError) -> c_int {
    loop {
        match reader.read(buffer) {
            Ok(0) => return ffi::AVERROR_EOF,
            Ok(read) => return c_int::try_from(read).unwrap_or(c_int::MAX),
            Err(failure) if failure.kind() == io::ErrorKind::Interrupted => {}
            Err(failure) => {
                *error.lock().unwrap_or_else(PoisonError::into_inner) = Some(failure);
                return ffi::AVERROR_EXIT;
            }
        }
    }
}

/// `FFmpeg`'s seek callback: `AVSEEK_SIZE` answers `length` without moving.
fn custom_seek(reader: &mut dyn InputReader, length: u64, offset: i64, whence: c_int) -> i64 {
    let size = c_int::try_from(ffi::AVSEEK_SIZE).unwrap_or(c_int::MAX);
    if whence & size != 0 {
        return i64::try_from(length).unwrap_or(i64::MAX);
    }
    let force = c_int::try_from(ffi::AVSEEK_FORCE).unwrap_or(0);
    let target = match u32::try_from(whence & !force) {
        Ok(ffi::SEEK_SET) => u64::try_from(offset).ok().map(SeekFrom::Start),
        Ok(ffi::SEEK_CUR) => Some(SeekFrom::Current(offset)),
        Ok(ffi::SEEK_END) => Some(SeekFrom::End(offset)),
        _ => None,
    };
    let Some(target) = target else {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    };
    reader.seek(target).map_or_else(
        |_| i64::from(ffi::AVERROR(ffi::EINVAL)),
        |position| i64::try_from(position).unwrap_or(i64::MAX),
    )
}

/// Refuses every nested open, so a custom input can never make `FFmpeg` open another resource.
unsafe extern "C" fn refuse_nested_open(
    _context: *mut ffi::AVFormatContext,
    _io: *mut *mut ffi::AVIOContext,
    _url: *const c_char,
    _flags: c_int,
    _options: *mut *mut ffi::AVDictionary,
) -> c_int {
    ffi::AVERROR(ffi::EPERM)
}

/// The I/O buffer size of a custom input.
const CUSTOM_BUFFER_BYTES: usize = 64 * 1024;

fn custom_context(
    reader: Box<dyn InputReader>,
    length: u64,
    error: &ReadError,
) -> AVIOContextCustom {
    let reader = Arc::new(Mutex::new(reader));
    let (read_error, seek_reader) = (Arc::clone(error), Arc::clone(&reader));
    AVIOContextCustom::alloc_context(
        AVMem::new(CUSTOM_BUFFER_BYTES),
        false,
        Vec::new(),
        Some(Box::new(move |_, buffer| {
            let mut reader = reader.lock().unwrap_or_else(PoisonError::into_inner);
            custom_read(&mut **reader, buffer, &read_error)
        })),
        None,
        Some(Box::new(move |_, offset, whence| {
            let mut reader = seek_reader.lock().unwrap_or_else(PoisonError::into_inner);
            custom_seek(&mut **reader, length, offset, whence)
        })),
    )
}

/// Opens `url` with `format`, or lets `FFmpeg` pick when it is null.
fn open_with(
    url: &CStr,
    formats: &CStr,
    decoders: Option<&CStr>,
    format: *const ffi::AVInputFormat,
    custom: Option<(Box<dyn InputReader>, u64)>,
) -> Result<DemuxInput, OpenFailure> {
    let interrupt = Arc::new(InputInterrupt::default());
    let read_error = ReadError::default();
    let mut custom = custom.map(|(reader, length)| custom_context(reader, length, &read_error));
    let read_failure = |failure: OpenFailure| {
        read_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map_or(failure, OpenFailure::Read)
    };
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
    if let Some(custom) = custom.as_mut() {
        // SAFETY: the context is fresh and unshared. `pb` outlives it: the returned input keeps the
        // custom context and drops it after the format context, and `avformat_open_input` frees
        // the format context on failure without closing a `pb` it did not open.
        unsafe {
            (*context).pb = custom.as_mut_ptr();
            (*context).io_open = Some(refuse_nested_open);
        }
    }
    let mut options =
        AVDictionary::new(c"format_whitelist", formats, 0).set(c"protocol_whitelist", c"file", 0);
    // The format context hands its list to every decoder `avformat_find_stream_info` opens.
    if let Some(decoders) = decoders {
        options = options.set(c"codec_whitelist", decoders, 0);
    }
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
        return Err(read_failure(OpenFailure::Open(opened)));
    }
    let Some(context) = NonNull::new(context) else {
        return Err(OpenFailure::Open(opened));
    };
    // SAFETY: `avformat_open_input` succeeded, so `context` is an open input context this now owns;
    // its `Drop` closes it with `avformat_close_input`.
    let mut input = unsafe { AVFormatContextInput::from_raw(context) };
    // Probing decodes a picture to learn its size, so the decoders it opens refuse one larger
    // than alpha2 takes before allocating it.
    let max_pixels = i64::try_from(alpha2_media::MAX_FRAME_PIXELS).unwrap_or(i64::MAX);
    let mut options = (0..input.nb_streams)
        .map(|_| {
            AVDictionary::new_int(c"max_pixels", max_pixels, 0)
                .into_raw()
                .as_ptr()
        })
        .collect::<Vec<_>>();
    let pointer = if options.is_empty() {
        ptr::null_mut()
    } else {
        options.as_mut_ptr()
    };
    // SAFETY: the context is open and exclusively owned; `pointer` is null or holds one dictionary
    // per stream the context had before the call, which is how many the call reads.
    let found = unsafe { ffi::avformat_find_stream_info(input.as_mut_ptr(), pointer) };
    for dictionary in &mut options {
        // SAFETY: each entry is null or a dictionary this owns, which the call may have replaced.
        unsafe { ffi::av_dict_free(dictionary) };
    }
    if found < 0 {
        return Err(read_failure(OpenFailure::Other(OpenInputError::Ffmpeg(
            RsmpegError::FindStreamInfoError(found),
        ))));
    }
    if let Some(error) = read_error
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    {
        return Err(OpenFailure::Read(error));
    }
    Ok(DemuxInput {
        input,
        _custom: custom,
        read_error,
        interrupt,
    })
}

/// Up to the first 1 MiB of an input, for [`refused_format`].
fn read_head(reader: Option<impl Read>) -> Option<Vec<u8>> {
    const PROBE_BYTES: u64 = 1 << 20;
    let mut bytes = Vec::new();
    reader?.take(PROBE_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The demuxer `FFmpeg` would pick for an input starting with `bytes` when `formats` does not name
/// it, so a failed open can say why instead of `EINVAL`.
fn refused_format(mut bytes: Vec<u8>, url: &CStr, formats: &CStr) -> Option<String> {
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
