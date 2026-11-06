//! Video decoding through `VideoToolbox` into CPU planes.
//!
//! Sessions decode synchronously and copy each picture out of its pixel buffer inside the output
//! callback, so the decoder's buffer pool never waits on a frame the host still holds. A copy is
//! the whole coded picture, which the stream crops by the SPS.

// Every call into the media frameworks is FFI.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, Once, PoisonError};

use alpha2_container::{Packet, TrackInfo, VideoCodec, VideoTrack};
use alpha2_host_core::parameter_sets::{ParameterSetCodec, sequence_facts};
use alpha2_host_core::video_stream::{
    BackendError, BackendFrame, BackendImage, BackendKind, Poll, Submit, VideoBackends,
    VideoDecodeBackend,
};
use alpha2_media::{ColorRange, Plane, Rect, YuvFormat};
use bytes::Bytes;
use objc2_core_foundation::{CFBoolean, CFData, CFDictionary, CFRetained, CFString};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionCreate, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets, kCMBlockBufferAssureMemoryNowFlag,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetExtendedPixels, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSessionCopyProperty,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};

use crate::config::{avc_parameter_sets, avc_sps_format, es_descriptor, hevc_parameter_sets};
use crate::sys::{self, code, dictionary, fourcc};

/// The sample format a stream decodes to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Picture {
    /// Bits per sample.
    depth: u8,
    /// `chroma_format_idc`: 1 for 4:2:0, 2 for 4:2:2, 3 for 4:4:4.
    chroma: u8,
    full_range: bool,
}

impl Picture {
    /// The bi-planar pixel format that holds these samples as decoded, and its layout.
    fn output(self) -> Result<(u32, YuvFormat), BackendError> {
        let (shift_x, shift_y, names) = match self.chroma {
            0 | 1 => (1, 1, [b"420v", b"420f", b"x420", b"xf20"]),
            2 => (1, 0, [b"422v", b"422f", b"x422", b"xf22"]),
            3 => (0, 0, [b"444v", b"444f", b"x444", b"xf44"]),
            other => {
                return Err(BackendError::Unsupported(format!("chroma format {other}")));
            }
        };
        let (deep, depth) = match self.depth {
            0..=8 => (0, 8),
            9 | 10 => (2, 10),
            depth => {
                return Err(BackendError::Unsupported(format!(
                    "{depth}-bit video: VideoToolbox outputs up to 10 bits here"
                )));
            }
        };
        let format = YuvFormat {
            chroma_shift_x: shift_x,
            chroma_shift_y: shift_y,
            bit_depth: depth,
            interleaved_chroma: true,
            msb_aligned: depth > 8,
            alpha: false,
        };
        Ok((code(*names[deep + usize::from(self.full_range)]), format))
    }
}

/// A codec's name for messages.
fn codec_name(codec: VideoCodec) -> String {
    match codec {
        VideoCodec::H264 => "H.264".into(),
        VideoCodec::Hevc => "HEVC".into(),
        VideoCodec::Vp8 => "VP8".into(),
        VideoCodec::Vp9 => "VP9".into(),
        VideoCodec::Av1 => "AV1".into(),
        VideoCodec::Mpeg4Part2 => "MPEG-4 Part 2".into(),
        VideoCodec::ProRes(fourcc) => format!("ProRes ({fourcc})"),
        VideoCodec::Other(fourcc) => format!("'{fourcc}'"),
    }
}

/// The range the bitstream's own VUI signals, which is what the decoder reports its samples in.
fn vui_full_range(codec: ParameterSetCodec, record: &[u8]) -> bool {
    sequence_facts(codec, record)
        .and_then(|facts| facts.vui.color)
        .is_some_and(|color| color.range == ColorRange::Full)
}

fn picture(video: &VideoTrack) -> Result<Picture, BackendError> {
    let record = &video.codec_private[..];
    let container_full = video
        .color
        .is_some_and(|color| color.range == ColorRange::Full);
    let unreadable = || {
        BackendError::Unsupported(format!(
            "an unreadable {} configuration",
            codec_name(video.codec)
        ))
    };
    Ok(match video.codec {
        VideoCodec::H264 => {
            let (sets, _) = avc_parameter_sets(record).ok_or_else(unreadable)?;
            let sps = sets
                .iter()
                .find(|set| set.first().is_some_and(|byte| byte & 0x1f == 7))
                .ok_or_else(unreadable)?;
            let (depth, chroma) = avc_sps_format(sps).ok_or_else(unreadable)?;
            if depth > 8 || chroma != 1 {
                return Err(BackendError::Unsupported(format!(
                    "VideoToolbox decodes 8-bit 4:2:0 H.264, not {depth}-bit with chroma format \
                     {chroma}"
                )));
            }
            Picture {
                depth,
                chroma,
                full_range: vui_full_range(ParameterSetCodec::H264, record),
            }
        }
        VideoCodec::Hevc => Picture {
            depth: (record.get(17).ok_or_else(unreadable)? & 7) + 8,
            chroma: record.get(16).ok_or_else(unreadable)? & 3,
            full_range: vui_full_range(ParameterSetCodec::Hevc, record),
        },
        VideoCodec::Av1 => {
            let flags = *record.get(2).ok_or_else(unreadable)?;
            let chroma = match (flags >> 3 & 1, flags >> 2 & 1) {
                (1, 1) => 1,
                (1, 0) => 2,
                _ => 3,
            };
            Picture {
                depth: match (flags >> 6 & 1, flags >> 5 & 1) {
                    (0, _) => 8,
                    (_, 0) => 10,
                    _ => 12,
                },
                chroma: if flags >> 4 & 1 == 1 { 1 } else { chroma },
                full_range: container_full,
            }
        }
        VideoCodec::Vp9 => {
            let byte = *record.get(6).ok_or_else(unreadable)?;
            Picture {
                depth: byte >> 4,
                chroma: match byte >> 1 & 7 {
                    0 | 1 => 1,
                    2 => 2,
                    _ => 3,
                },
                full_range: byte & 1 == 1,
            }
        }
        VideoCodec::ProRes(fourcc) => Picture {
            depth: 10,
            chroma: if fourcc.0.starts_with(b"ap4") { 3 } else { 2 },
            full_range: false,
        },
        VideoCodec::Mpeg4Part2 => Picture {
            depth: 8,
            chroma: 1,
            full_range: container_full,
        },
        VideoCodec::Vp8 | VideoCodec::Other(_) => {
            return Err(BackendError::Unsupported(format!(
                "VideoToolbox has no {} decoder",
                codec_name(video.codec)
            )));
        }
    })
}

/// The format description `VideoToolbox` opens a decoder from.
///
/// H.264 and HEVC descriptions are built from the record's parameter sets: a description made
/// from an `hev1` sample entry does not open a session, while one from the same `hvcC`'s
/// parameter sets does.
fn description(video: &VideoTrack) -> Result<CFRetained<CMFormatDescription>, BackendError> {
    let record = &video.codec_private[..];
    let unreadable = || {
        BackendError::Unsupported(format!(
            "an unreadable {} configuration",
            codec_name(video.codec)
        ))
    };
    let mut out: *const CMFormatDescription = ptr::null();
    let status = match video.codec {
        VideoCodec::H264 | VideoCodec::Hevc => {
            let (sets, length_size) = if video.codec == VideoCodec::H264 {
                avc_parameter_sets(record)
            } else {
                hevc_parameter_sets(record)
            }
            .filter(|(sets, _)| !sets.is_empty())
            .ok_or_else(unreadable)?;
            let mut pointers = sets
                .iter()
                .map(|set| NonNull::from(*set).cast::<u8>())
                .collect::<Vec<_>>();
            let mut sizes = sets.iter().map(|set| set.len()).collect::<Vec<_>>();
            let length = i32::try_from(length_size).unwrap_or(4);
            let pointers = NonNull::new(pointers.as_mut_ptr()).ok_or_else(unreadable)?;
            let sizes = NonNull::new(sizes.as_mut_ptr()).ok_or_else(unreadable)?;
            if video.codec == VideoCodec::H264 {
                // SAFETY: `pointers` and `sizes` describe `sets.len()` slices that outlive the call.
                unsafe {
                    CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        None,
                        sets.len(),
                        pointers,
                        sizes,
                        length,
                        NonNull::from(&mut out),
                    )
                }
            } else {
                // SAFETY: as above.
                unsafe {
                    CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        None,
                        sets.len(),
                        pointers,
                        sizes,
                        length,
                        None,
                        NonNull::from(&mut out),
                    )
                }
            }
        }
        codec => {
            let (codec_type, atom) = match codec {
                VideoCodec::Vp9 => (code(*b"vp09"), Some(("vpcC", record.to_vec()))),
                VideoCodec::Av1 => (code(*b"av01"), Some(("av1C", record.to_vec()))),
                VideoCodec::Mpeg4Part2 => {
                    // An `esds` box body: version and flags, then the descriptor.
                    let mut esds = vec![0; 4];
                    esds.extend_from_slice(&es_descriptor(0x20, 4, record));
                    (code(*b"mp4v"), Some(("esds", esds)))
                }
                VideoCodec::ProRes(fourcc) => (u32::from_be_bytes(fourcc.0), None),
                _ => return Err(unreadable()),
            };
            let extensions = atom.map(|(name, body)| {
                let atoms = dictionary(&[(&CFString::from_str(name), &CFData::from_bytes(&body))]);
                // SAFETY: the key is a framework constant.
                let key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
                dictionary(&[(key, &atoms)])
            });
            let (width, height) = (
                i32::try_from(video.width).unwrap_or(i32::MAX),
                i32::try_from(video.height).unwrap_or(i32::MAX),
            );
            // SAFETY: `extensions` is a property-list dictionary and `out` a valid out pointer.
            unsafe {
                CMVideoFormatDescriptionCreate(
                    None,
                    codec_type,
                    width,
                    height,
                    extensions.as_deref().map(CFDictionary::as_opaque),
                    NonNull::from(&mut out),
                )
            }
        }
    };
    if status != 0 {
        return Err(BackendError::Unsupported(format!(
            "CoreMedia refused the {} configuration ({})",
            codec_name(video.codec),
            sys::status(status)
        )));
    }
    let out = NonNull::new(out.cast_mut()).ok_or_else(unreadable)?;
    // SAFETY: a successful create returns a +1 reference.
    Ok(unsafe { CFRetained::from_raw(out) })
}

/// Registers the system's supplemental VP9 and AV1 decoders, which sessions otherwise do not
/// find. The function exists from iOS 26.2 and macOS 11, so it is looked up at run time.
fn register_supplemental_decoders() {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let Some(function) = sys::symbol(c"VTRegisterSupplementalVideoDecoderIfAvailable") else {
            return;
        };
        // SAFETY: the symbol is `void VTRegisterSupplementalVideoDecoderIfAvailable(CMVideoCodecType)`.
        let register: unsafe extern "C" fn(u32) = unsafe { std::mem::transmute(function) };
        for codec in [*b"vp09", *b"av01"] {
            // SAFETY: any codec type is accepted; unknown ones are ignored.
            unsafe { register(code(codec)) };
        }
    });
}

/// Whether an MPEG-4 Part 2 configuration's visual object sequence header declares Advanced
/// Simple Profile (`profile_and_level_indication` 0xF0 to 0xF7), which `VideoToolbox` may not read.
fn mpeg4_advanced_simple(video: &VideoTrack) -> bool {
    video.codec == VideoCodec::Mpeg4Part2
        && video
            .codec_private
            .windows(5)
            .find(|window| window[..4] == [0, 0, 1, 0xb0])
            .is_some_and(|window| matches!(window[4], 0xf0..=0xf7))
}

/// What the output callback hands the session.
struct Outputs {
    pixel_format: u32,
    format: YuvFormat,
    /// The SPS's coded size, for H.264 and HEVC.
    coded: Option<(usize, usize)>,
    timescale: i32,
    frames: VecDeque<BackendFrame>,
    /// The first failure the callback saw since it was last taken.
    error: Option<i32>,
    copy_error: Option<String>,
}

/// `units` of 1/`from` in 1/`to`, rounded down.
fn rescale(units: i64, from: i32, to: i32) -> i64 {
    if from == to || from == 0 {
        return units;
    }
    let scaled = i128::from(units) * i128::from(to);
    i64::try_from(scaled.div_euclid(i128::from(from))).unwrap_or(units)
}

/// A decoded picture and the part of it the buffer reports as the picture.
struct Copied {
    planes: Vec<Plane>,
    coded: (u32, u32),
    visible: Rect,
}

/// The size to read from a buffer of `size` with `extended` pixels to its right and below: the
/// SPS's coded size when the buffer is a smaller part of it and the extended pixels hold the rest.
///
/// For a stream whose SPS crops, macOS reports the cropped size, leaves the buffer at the coded
/// picture's origin even for a left or top crop, and keeps the rest as extended pixels; iOS
/// reports the whole coded picture. Reading the coded picture either way lets the stream crop by
/// the SPS.
fn readable(
    size: (usize, usize),
    extended: (usize, usize),
    coded: Option<(usize, usize)>,
) -> (usize, usize) {
    match coded {
        Some((width, height))
            if size != (width, height)
                && (size.0..=size.0 + extended.0).contains(&width)
                && (size.1..=size.1 + extended.1).contains(&height) =>
        {
            (width, height)
        }
        _ => size,
    }
}

/// Copies a picture out of `buffer`, rows of each plane `stride` bytes apart, the last row without
/// its padding.
fn copy_picture(buffer: &CVPixelBuffer, outputs: &Outputs) -> Result<Copied, String> {
    let actual = CVPixelBufferGetPixelFormatType(buffer);
    if actual != outputs.pixel_format {
        return Err(format!(
            "the decoder gave {} pixels, not the {} asked for",
            fourcc(actual),
            fourcc(outputs.pixel_format)
        ));
    }
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    // SAFETY: `buffer` is a live pixel buffer and the out pointers are valid.
    unsafe {
        CVPixelBufferGetExtendedPixels(
            buffer,
            &raw mut left,
            &raw mut right,
            &raw mut top,
            &raw mut bottom,
        );
    }
    let size = (
        CVPixelBufferGetWidth(buffer),
        CVPixelBufferGetHeight(buffer),
    );
    let (width, height) = readable(size, (right, bottom), outputs.coded);
    let format = outputs.format;
    let (shift_x, shift_y) = (format.chroma_shift_x, format.chroma_shift_y);
    // SAFETY: `buffer` is a live pixel buffer; the lock is released below.
    let locked = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
    if locked != 0 {
        return Err(format!("the picture could not be read ({locked})"));
    }
    let sample = usize::from(format.bit_depth.div_ceil(8));
    let planes = (0..CVPixelBufferGetPlaneCount(buffer))
        .map(|index| {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
            let (columns, rows, components) = if index == 0 {
                (width, height, 1)
            } else {
                (
                    width.div_ceil(1 << shift_x),
                    height.div_ceil(1 << shift_y),
                    2,
                )
            };
            let row = columns * components * sample;
            if base.is_null() || rows == 0 || row > stride {
                return Err(format!("plane {index} of the picture is unreadable"));
            }
            let length = stride * (rows - 1) + row;
            // SAFETY: a locked plane holds its rows `stride` bytes apart from its base address,
            // followed by the rows of its extended pixels.
            let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), length) };
            Ok(Plane {
                bytes: Bytes::copy_from_slice(bytes),
                stride: u32::try_from(stride).unwrap_or(u32::MAX),
                width: u32::try_from(columns).unwrap_or(u32::MAX),
                height: u32::try_from(rows).unwrap_or(u32::MAX),
            })
        })
        .collect::<Result<Vec<_>, _>>();
    // SAFETY: the buffer was locked read-only above.
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };
    let dimension = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
    Ok(Copied {
        planes: planes?,
        coded: (dimension(width), dimension(height)),
        visible: Rect {
            x: 0,
            y: 0,
            width: dimension(size.0),
            height: dimension(size.1),
        },
    })
}

/// The output callback: copies the picture, or records why there is none.
unsafe extern "C-unwind" fn decoded(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the refcon is the session's `Outputs`, which outlives the session.
    let outputs = unsafe { &*refcon.cast::<Mutex<Outputs>>() };
    let mut outputs = outputs.lock().unwrap_or_else(PoisonError::into_inner);
    if status != 0 {
        outputs.error.get_or_insert(status);
        return;
    }
    // SAFETY: a non-null image is a live pixel buffer for the duration of the callback.
    let Some(buffer) = (unsafe { image.as_ref() }) else {
        return;
    };
    match copy_picture(buffer, &outputs) {
        Ok(copied) => {
            let pts = rescale(pts.value, pts.timescale, outputs.timescale);
            let format = outputs.format;
            outputs.frames.push_back(BackendFrame {
                pts,
                image: BackendImage::Cpu {
                    format,
                    planes: copied.planes,
                },
                coded: copied.coded,
                visible: Some(copied.visible),
                sample_aspect_ratio: None,
                color: None,
                chroma_location: None,
            });
        }
        Err(reason) => {
            outputs.copy_error.get_or_insert(reason);
        }
    }
}

/// A decompression session, invalidated when dropped.
struct Session(CFRetained<VTDecompressionSession>);

// SAFETY: a `VideoToolbox` session may be called from any thread; the backend that owns it is
// used from one thread at a time.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is live; nothing uses it after this.
        unsafe { self.0.invalidate() };
    }
}

/// A format description shared with the sample buffers made from it.
struct Description(CFRetained<CMFormatDescription>);

// SAFETY: format descriptions are immutable after creation.
unsafe impl Send for Description {}

/// A `VideoToolbox` video decoder session.
pub struct VideoToolboxVideo {
    // Declared before `outputs`: the session must be invalidated before its callback state goes.
    session: Session,
    description: Description,
    outputs: Box<Mutex<Outputs>>,
    kind: BackendKind,
    codec: VideoCodec,
    timescale: i32,
    ended: bool,
}

impl std::fmt::Debug for VideoToolboxVideo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoToolboxVideo")
            .field("codec", &self.codec)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl VideoToolboxVideo {
    /// A session of `kind` for `track`; its kind is the one `VideoToolbox` reports it chose.
    ///
    /// # Errors
    /// [`BackendError::Unsupported`] when no decoder of that kind opens for the track.
    pub fn open(track: &TrackInfo, kind: BackendKind) -> Result<Self, BackendError> {
        let video = track
            .video
            .as_ref()
            .ok_or_else(|| BackendError::Unsupported("not a video track".into()))?;
        let (pixel_format, format) = picture(video)?.output()?;
        if matches!(video.codec, VideoCodec::Vp9 | VideoCodec::Av1) {
            register_supplemental_decoders();
        }
        let description = description(video)?;
        let timescale = i32::try_from(track.timescale.max(1)).unwrap_or(i32::MAX);
        let parameter_sets = match video.codec {
            VideoCodec::H264 => Some(ParameterSetCodec::H264),
            VideoCodec::Hevc => Some(ParameterSetCodec::Hevc),
            _ => None,
        };
        let coded = parameter_sets
            .and_then(|codec| sequence_facts(codec, &video.codec_private))
            .map(|facts| {
                (
                    facts.crop.coded_width as usize,
                    facts.crop.coded_height as usize,
                )
            });
        let outputs = Box::new(Mutex::new(Outputs {
            pixel_format,
            format,
            coded,
            timescale,
            frames: VecDeque::new(),
            error: None,
            copy_error: None,
        }));
        // SAFETY: framework constants.
        let specification = unsafe {
            match kind {
                BackendKind::Hardware => dictionary(&[(
                    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                    sys::boolean(true),
                )]),
                BackendKind::Software => dictionary(&[(
                    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
                    sys::boolean(false),
                )]),
            }
        };
        // SAFETY: a framework constant.
        let attributes = dictionary(&[(
            unsafe { kCVPixelBufferPixelFormatTypeKey },
            &sys::int(i64::from(pixel_format)),
        )]);
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decoded),
            decompressionOutputRefCon: ptr::from_ref::<Mutex<Outputs>>(&outputs).cast_mut().cast(),
        };
        let mut session: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: the dictionaries hold the documented value types, the callback's refcon stays
        // valid while the session lives, and `session` is a valid out pointer.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &description,
                Some(specification.as_opaque()),
                Some(attributes.as_opaque()),
                &raw const callback,
                NonNull::from(&mut session),
            )
        };
        let kind_name = match kind {
            BackendKind::Hardware => "hardware",
            BackendKind::Software => "software",
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or_else(|| {
                let profile = if mpeg4_advanced_simple(video) {
                    ", Advanced Simple Profile"
                } else {
                    ""
                };
                BackendError::Unsupported(format!(
                    "VideoToolbox here has no {kind_name} {} decoder for this stream ({}{profile})",
                    codec_name(video.codec),
                    sys::status(status)
                ))
            })?;
        // SAFETY: a successful create returns a +1 reference.
        let session = Session(unsafe { CFRetained::from_raw(session) });
        let kind = if on_hardware(&session.0) {
            BackendKind::Hardware
        } else {
            BackendKind::Software
        };
        Ok(Self {
            session,
            description: Description(description),
            outputs,
            kind,
            codec: video.codec,
            timescale,
            ended: false,
        })
    }

    fn outputs(&self) -> std::sync::MutexGuard<'_, Outputs> {
        self.outputs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn error(&self, status: i32, during: &str) -> BackendError {
        let reason = format!(
            "VideoToolbox {} {during} failed ({})",
            codec_name(self.codec),
            sys::status(status)
        );
        match status {
            sys::INVALID_SESSION | sys::DECODER_NOT_AVAILABLE_NOW => {
                BackendError::SessionLost(reason)
            }
            sys::BAD_DATA | sys::REFERENCE_MISSING => BackendError::Corrupt(reason),
            sys::NO_DECODER | sys::UNSUPPORTED_DATA_FORMAT => BackendError::Unsupported(reason),
            _ if self.kind == BackendKind::Hardware => BackendError::HardwarePicture(reason),
            _ => BackendError::Failed(reason),
        }
    }

    /// The failure the callback recorded since the last call, if any.
    fn take_callback_error(&self, during: &str) -> Result<(), BackendError> {
        let (status, copy) = {
            let mut outputs = self.outputs();
            (outputs.error.take(), outputs.copy_error.take())
        };
        if let Some(status) = status {
            return Err(self.error(status, during));
        }
        copy.map_or(Ok(()), |reason| Err(BackendError::Failed(reason)))
    }

    fn sample(&self, packet: &Packet) -> Result<CFRetained<CMSampleBuffer>, BackendError> {
        let failed = |what: &str, status: i32| {
            BackendError::Failed(format!("{what} failed ({})", sys::status(status)))
        };
        let length = packet.data.len();
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        // SAFETY: a null memory block makes CoreMedia allocate `length` bytes now.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                length,
                None,
                ptr::null(),
                0,
                length,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            )
        };
        let block = NonNull::new(block)
            .filter(|_| status == 0)
            .ok_or_else(|| failed("allocating a packet", status))?;
        // SAFETY: a successful create returns a +1 reference.
        let block = unsafe { CFRetained::from_raw(block) };
        let source = NonNull::from(&packet.data[..]).cast::<c_void>();
        // SAFETY: the block holds `length` bytes and `source` points at `length` bytes.
        let status = unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, length) };
        if status != 0 {
            return Err(failed("copying a packet", status));
        }
        let time = |value: i64| CMTime {
            value,
            timescale: self.timescale,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let timing = CMSampleTimingInfo {
            duration: if packet.duration > 0 {
                time(packet.duration)
            } else {
                CMTime {
                    value: 0,
                    timescale: 0,
                    flags: CMTimeFlags::empty(),
                    epoch: 0,
                }
            },
            presentationTimeStamp: time(packet.pts),
            decodeTimeStamp: time(packet.dts),
        };
        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        // SAFETY: one sample of `length` bytes with one timing entry; `sample` is a valid out
        // pointer.
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self.description.0),
                1,
                1,
                &raw const timing,
                1,
                &raw const length,
                NonNull::from(&mut sample),
            )
        };
        let sample = NonNull::new(sample)
            .filter(|_| status == 0)
            .ok_or_else(|| failed("wrapping a packet", status))?;
        // SAFETY: a successful create returns a +1 reference.
        Ok(unsafe { CFRetained::from_raw(sample) })
    }

    /// Emits the pictures the decoder holds back and waits for every callback.
    fn complete(&self, during: &str) -> Result<(), BackendError> {
        // SAFETY: the session is live.
        let status = unsafe { self.session.0.finish_delayed_frames() };
        if status != 0 {
            return Err(self.error(status, during));
        }
        // SAFETY: the session is live.
        let status = unsafe { self.session.0.wait_for_asynchronous_frames() };
        if status != 0 {
            return Err(self.error(status, during));
        }
        Ok(())
    }
}

/// Whether `VideoToolbox` chose a hardware decoder for `session`.
fn on_hardware(session: &VTDecompressionSession) -> bool {
    let mut value: *const CFBoolean = ptr::null();
    // SAFETY: the property is a `CFBoolean`, returned +1 into `value`.
    let status = unsafe {
        VTSessionCopyProperty(
            session,
            kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            None,
            (&raw mut value).cast(),
        )
    };
    let Some(value) = NonNull::new(value.cast_mut()).filter(|_| status == 0) else {
        return false;
    };
    // SAFETY: a copied property is a +1 reference.
    let value = unsafe { CFRetained::from_raw(value) };
    value.as_bool()
}

impl VideoDecodeBackend for VideoToolboxVideo {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn submit(&mut self, packet: &Packet) -> Result<Submit, BackendError> {
        if packet.data.is_empty() {
            return Ok(Submit::Accepted);
        }
        let sample = self.sample(packet)?;
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: the sample buffer is live; synchronous decoding calls back before returning.
        let status = unsafe {
            self.session.0.decode_frame(
                &sample,
                VTDecodeFrameFlags::empty(),
                ptr::null_mut(),
                &raw mut info,
            )
        };
        if status != 0 {
            self.outputs().error = None;
            return Err(self.error(status, "decode"));
        }
        self.take_callback_error("decode")?;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, _wait: bool) -> Result<Poll, BackendError> {
        self.take_callback_error("decode")?;
        Ok(match self.outputs().frames.pop_front() {
            Some(frame) => Poll::Frame(frame),
            None if self.ended => Poll::Drained,
            None => Poll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), BackendError> {
        self.complete("drain")?;
        self.ended = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        let completed = self.complete("flush");
        let mut outputs = self.outputs();
        outputs.frames.clear();
        outputs.error = None;
        outputs.copy_error = None;
        drop(outputs);
        self.ended = false;
        completed
    }
}

/// Opens `VideoToolbox` video sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppleVideo;

impl VideoBackends for AppleVideo {
    fn open(
        &self,
        track: &TrackInfo,
        kind: BackendKind,
    ) -> Result<Box<dyn VideoDecodeBackend>, BackendError> {
        Ok(Box::new(VideoToolboxVideo::open(track, kind)?))
    }
}
