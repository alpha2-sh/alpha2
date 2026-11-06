//! Video encoding through `VideoToolbox`: H.264 and HEVC from the CPU planes the export renders.
//!
//! The encoder is chosen by ID from `VTCopyVideoEncoderList`, hardware encoders before software
//! ones, so a software encoder is never used while a hardware one opens. Frames are never
//! reordered, keyframes come at the configured interval, and Apple's per-run SEI is stripped from
//! H.264, so packets are in presentation order and repeatable.

// Every call into the media frameworks is FFI.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

use alpha2_host_core::encode::{
    EncodePoll, EncodedPacket, VideoEncodeBackend, VideoEncodeConfig, VideoEncoders,
};
use alpha2_host_core::export::PlaneBuffer;
use alpha2_host_core::rate_control::{AppliedRateControl, videotoolbox_quality};
use alpha2_host_core::video_stream::Submit;
use alpha2_media::{
    BitDepth, BitrateMode, Chroma, ColorMatrix, ColorPrimaries, ColorRange, EncodeError,
    EncoderRefusal, OpenFailureKind, PlaneLayout, TransferFunction, VideoCodec,
};
use bytes::Bytes;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMSampleAttachmentKey_NotSync,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferColorPrimaries_SMPTE_C, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_2100_HLG, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferTransferFunction_sRGB, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTCopyVideoEncoderList, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ConstantBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxFrameDelayCount, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_Quality,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTProfileLevel_HEVC_Main10_AutoLevel, kVTVideoEncoderList_CodecType,
    kVTVideoEncoderList_EncoderID, kVTVideoEncoderList_IsHardwareAccelerated,
    kVTVideoEncoderSpecification_EncoderID,
};

use crate::sys::{self, code, dictionary};

/// How long a receive that must wait blocks for a packet.
const WAIT: Duration = Duration::from_millis(10);

/// Frames an encoder may hold before it must emit one.
const MAX_FRAME_DELAY: i64 = 8;

/// Frames in flight before a submit waits for output, above the encoder's delay so a full
/// encoder always has a frame to emit.
const MAX_IN_FLIGHT: u64 = 2 * MAX_FRAME_DELAY.unsigned_abs();

/// One entry of the system's encoder list.
#[derive(Clone, Debug)]
pub(crate) struct Listed {
    id: String,
    pub(crate) hardware: bool,
}

/// A value of a list entry.
///
/// # Safety
/// `entry` must be a dictionary with `CFString` keys.
unsafe fn entry_value(entry: &CFDictionary, key: &CFString) -> Option<CFRetained<CFType>> {
    // SAFETY: the caller guarantees the key type; the value is retained before use.
    let value = unsafe { entry.value(ptr::from_ref(key).cast()) };
    let value = NonNull::new(value.cast_mut().cast::<CFType>())?;
    // SAFETY: a dictionary value is a live CF object.
    Some(unsafe { CFRetained::retain(value) })
}

/// The system's encoders for `codec_type`, hardware first.
pub(crate) fn listed(codec_type: u32) -> Vec<Listed> {
    let mut list: *const CFArray = ptr::null();
    // SAFETY: `list` is a valid out pointer; the array is returned +1.
    let status = unsafe { VTCopyVideoEncoderList(None, NonNull::from(&mut list)) };
    let Some(list) = NonNull::new(list.cast_mut()).filter(|_| status == 0) else {
        return Vec::new();
    };
    // SAFETY: a copied list is a +1 reference.
    let list = unsafe { CFRetained::from_raw(list) };
    let mut found = Vec::new();
    for index in 0..list.len() {
        // SAFETY: the list holds dictionaries; the index is in range.
        let entry = unsafe { list.value_at_index(isize::try_from(index).unwrap_or(0)) };
        // SAFETY: a list entry is a live dictionary while the list lives.
        let Some(entry) = (unsafe { entry.cast::<CFDictionary>().as_ref() }) else {
            continue;
        };
        // SAFETY: the list's dictionaries have `CFString` keys; the keys are framework constants.
        let (kind, id, hardware) = unsafe {
            (
                entry_value(entry, kVTVideoEncoderList_CodecType),
                entry_value(entry, kVTVideoEncoderList_EncoderID),
                entry_value(entry, kVTVideoEncoderList_IsHardwareAccelerated),
            )
        };
        let kind = kind
            .and_then(|kind| kind.downcast_ref::<CFNumber>().and_then(CFNumber::as_i64))
            .and_then(|kind| u32::try_from(kind).ok());
        let id = id.and_then(|id| id.downcast_ref::<CFString>().map(ToString::to_string));
        let hardware = hardware
            .and_then(|value| value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
            .unwrap_or(false);
        if let (Some(kind), Some(id)) = (kind, id)
            && kind == codec_type
        {
            found.push(Listed { id, hardware });
        }
    }
    found.sort_by_key(|listed| !listed.hardware);
    found
}

/// What the output callback hands the session.
#[derive(Default)]
struct Outputs {
    packets: VecDeque<EncodedPacket>,
    codec_private: Option<Bytes>,
    error: Option<String>,
    /// Frames the encoder has finished with, encoded, dropped or failed.
    completed: u64,
}

/// The output callback state and the condition a waiting receive sleeps on.
struct Shared {
    outputs: Mutex<Outputs>,
    ready: Condvar,
    timescale: i32,
    /// The configuration record's atom name: `avcC` or `hvcC`.
    record: &'static str,
    strip_sei: bool,
}

/// The configuration record in a sample's format description.
fn configuration_record(sample: &CMSampleBuffer, record: &str) -> Option<Bytes> {
    // SAFETY: the sample is live; the description and its extension are retained.
    let description = unsafe { sample.format_description() }?;
    // SAFETY: a framework constant.
    let key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
    // SAFETY: the description is live.
    let atoms = unsafe { description.extension(key) }?;
    let atoms = atoms.downcast_ref::<CFDictionary>()?;
    let name = CFString::from_str(record);
    // SAFETY: the atoms dictionary has `CFString` keys.
    let data = unsafe { entry_value(atoms, &name) }?;
    Some(Bytes::from(data.downcast_ref::<CFData>()?.to_vec()))
}

/// Whether a sample is a sync sample: it has no `NotSync` attachment set.
fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    // SAFETY: the sample is live; the array is retained.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    if attachments.is_empty() {
        return true;
    }
    // SAFETY: the array holds one dictionary per sample.
    let first = unsafe { attachments.value_at_index(0) };
    // SAFETY: an attachment dictionary is live while the array lives.
    let Some(first) = (unsafe { first.cast::<CFDictionary>().as_ref() }) else {
        return true;
    };
    // SAFETY: attachment dictionaries have `CFString` keys; the key is a framework constant.
    let not_sync = unsafe { entry_value(first, kCMSampleAttachmentKey_NotSync) };
    !not_sync
        .and_then(|value| value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
        .unwrap_or(false)
}

/// `time` in 1/`timescale` units.
fn units(time: CMTime, timescale: i32) -> Option<i64> {
    if !time.flags.contains(CMTimeFlags::Valid) || time.timescale <= 0 {
        return None;
    }
    if time.timescale == timescale {
        return Some(time.value);
    }
    let scaled = i128::from(time.value) * i128::from(timescale) / i128::from(time.timescale);
    i64::try_from(scaled).ok()
}

fn packet(sample: &CMSampleBuffer, shared: &Shared) -> Result<EncodedPacket, String> {
    // SAFETY: the sample is live.
    let block = unsafe { sample.data_buffer() }.ok_or("a sample without data")?;
    // SAFETY: the block is live.
    let length = unsafe { block.data_length() };
    let mut data = vec![0_u8; length];
    let destination = NonNull::new(data.as_mut_ptr().cast::<c_void>()).ok_or("no buffer")?;
    // SAFETY: `data` holds `length` bytes.
    let status = unsafe { block.copy_data_bytes(0, length, destination) };
    if status != 0 {
        return Err(format!("reading a packet failed ({})", sys::status(status)));
    }
    if shared.strip_sei
        && let Some(stripped) = alpha2_host_core::vt_sei::strip_bytes(&data)
    {
        data = stripped;
    }
    // SAFETY: the sample is live.
    let pts = units(
        unsafe { sample.presentation_time_stamp() },
        shared.timescale,
    )
    .ok_or("a packet without a presentation time")?;
    // SAFETY: the sample is live.
    let dts = units(unsafe { sample.decode_time_stamp() }, shared.timescale).unwrap_or(pts);
    Ok(EncodedPacket {
        pts,
        dts,
        keyframe: is_keyframe(sample),
        data: Bytes::from(data),
    })
}

/// The output callback: queues the packet, or records why there is none.
unsafe extern "C-unwind" fn encoded(
    refcon: *mut c_void,
    _source: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the refcon is the session's `Shared`, which outlives the session.
    let shared = unsafe { &*refcon.cast::<Shared>() };
    let mut outputs = shared
        .outputs
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // SAFETY: a non-null sample is live for the duration of the callback.
    let sample = unsafe { sample.as_ref() };
    if status != 0 {
        outputs
            .error
            .get_or_insert_with(|| format!("encoding failed ({})", sys::status(status)));
    } else if let Some(sample) = sample {
        if outputs.codec_private.is_none() {
            outputs.codec_private = configuration_record(sample, shared.record);
        }
        match packet(sample, shared) {
            Ok(packet) => outputs.packets.push_back(packet),
            Err(reason) => {
                outputs.error.get_or_insert(reason);
            }
        }
    }
    outputs.completed += 1;
    drop(outputs);
    shared.ready.notify_all();
}

/// A compression session, invalidated when dropped.
struct Session(CFRetained<VTCompressionSession>);

// SAFETY: a `VideoToolbox` session may be called from any thread; the encoder that owns it is
// used from one thread at a time.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is live; nothing uses it after this.
        unsafe { self.0.invalidate() };
    }
}

/// The session's pixel buffer pool.
struct Pool(CFRetained<CVPixelBufferPool>);

// SAFETY: pixel buffer pools are thread-safe.
unsafe impl Send for Pool {}

/// The plane layouts the encoder takes, and the pixel formats they fill.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Input {
    /// 8-bit planar 4:2:0, interleaved into NV12.
    Planar8,
    /// 10-bit planar 4:2:0 with low-aligned samples, shifted and interleaved into P010.
    Planar10,
    /// P010, copied as it is.
    P010,
}

/// The color tags a session writes, as `CoreVideo` names them.
fn color_tags(config: &VideoEncodeConfig) -> [&'static CFString; 3] {
    let color = config.color;
    // SAFETY: framework constants.
    unsafe {
        [
            match color.primaries {
                ColorPrimaries::Bt601 => kCVImageBufferColorPrimaries_SMPTE_C,
                ColorPrimaries::Bt709 => kCVImageBufferColorPrimaries_ITU_R_709_2,
                ColorPrimaries::Bt2020 => kCVImageBufferColorPrimaries_ITU_R_2020,
            },
            match color.transfer {
                TransferFunction::Bt709 => kCVImageBufferTransferFunction_ITU_R_709_2,
                TransferFunction::Srgb => kCVImageBufferTransferFunction_sRGB,
                TransferFunction::Pq => kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                TransferFunction::Hlg => kCVImageBufferTransferFunction_ITU_R_2100_HLG,
            },
            match color.matrix {
                ColorMatrix::Bt601 => kCVImageBufferYCbCrMatrix_ITU_R_601_4,
                ColorMatrix::Bt2020Ncl => kCVImageBufferYCbCrMatrix_ITU_R_2020,
                ColorMatrix::Bt709 | ColorMatrix::Rgb => kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            },
        ]
    }
}

/// Row `y` of `plane`, `length` bytes.
fn row(plane: &PlaneBuffer, y: usize, length: usize) -> Option<&[u8]> {
    let start = y * plane.stride;
    plane.bytes.get(start..start + length)
}

/// Sets the session's properties for `config`; returns the rate control it runs and whether the
/// encoder's frame delay is bounded.
fn configure(
    session: &Session,
    config: &VideoEncodeConfig,
    input: Input,
) -> Result<(AppliedRateControl, bool), String> {
    let set = |key: &CFString, value: &CFType| -> i32 {
        // SAFETY: the session is live and the value has the property's documented type.
        unsafe { VTSessionSetProperty(&session.0, key, Some(value)) }
    };
    let rate =
        f64::from(config.frame_rate.numerator()) / f64::from(config.frame_rate.denominator());
    let rate = CFNumber::new_f64(rate);
    let interval = sys::int(i64::from(config.keyframe_interval.max(1)));
    let [primaries, transfer, matrix] = color_tags(config);
    // SAFETY: framework constants.
    let required: [(&CFString, &CFType); 8] = unsafe {
        [
            (kVTCompressionPropertyKey_RealTime, sys::boolean(false)),
            (
                kVTCompressionPropertyKey_AllowFrameReordering,
                sys::boolean(false),
            ),
            (kVTCompressionPropertyKey_MaxKeyFrameInterval, &interval),
            (kVTCompressionPropertyKey_ExpectedFrameRate, &rate),
            (
                kVTCompressionPropertyKey_ProfileLevel,
                match (config.codec, input) {
                    (VideoCodec::H264, _) => kVTProfileLevel_H264_High_AutoLevel,
                    (_, Input::Planar8) => kVTProfileLevel_HEVC_Main_AutoLevel,
                    _ => kVTProfileLevel_HEVC_Main10_AutoLevel,
                },
            ),
            (kVTCompressionPropertyKey_ColorPrimaries, primaries),
            (kVTCompressionPropertyKey_TransferFunction, transfer),
            (kVTCompressionPropertyKey_YCbCrMatrix, matrix),
        ]
    };
    for (key, value) in required {
        let status = set(key, value);
        if status != 0 {
            return Err(format!("{key} was refused ({})", sys::status(status)));
        }
    }
    // SAFETY: a framework constant.
    let delay = unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount };
    let bounded = set(delay, &sys::int(MAX_FRAME_DELAY)) == 0;
    Ok((VideoToolboxEncoder::rate_control(config, &set)?, bounded))
}

/// A `VideoToolbox` encoder session.
pub struct VideoToolboxEncoder {
    // Declared before `shared`: the session must be invalidated before its callback state goes.
    session: Session,
    pool: Pool,
    shared: Box<Shared>,
    id: String,
    hardware: bool,
    timescale: u32,
    frame_duration: i64,
    input: Input,
    applied: AppliedRateControl,
    /// Whether the encoder's frame delay is bounded, so submits can wait for output.
    bounded: bool,
    submitted: u64,
    ended: bool,
}

impl std::fmt::Debug for VideoToolboxEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoToolboxEncoder")
            .field("encoder", &self.id)
            .finish_non_exhaustive()
    }
}

/// The `VideoToolbox` codec type and name of the codecs this backend encodes.
pub(crate) fn codec_type(codec: VideoCodec) -> Option<(u32, &'static str)> {
    match codec {
        VideoCodec::H264 => Some((code(*b"avc1"), "H.264")),
        VideoCodec::Hevc => Some((code(*b"hvc1"), "HEVC")),
        _ => None,
    }
}

/// A refusal of `encoder` for `reason`.
fn refusal(encoder: &str, hardware: bool, kind: OpenFailureKind, reason: String) -> EncoderRefusal {
    EncoderRefusal {
        encoder: encoder.into(),
        hardware_accelerated: hardware,
        kind,
        reason,
    }
}

fn refused(refusals: Vec<EncoderRefusal>) -> EncodeError {
    let message = refusals
        .iter()
        .map(|refusal| format!("{}: {}", refusal.encoder, refusal.reason))
        .collect::<Vec<_>>()
        .join("; ");
    EncodeError::EncodersRefused { message, refusals }
}

impl VideoToolboxEncoder {
    /// A session for `config` on the first of the system's encoders that opens, hardware before
    /// software, or on a software one only when `config.software` asks for it.
    ///
    /// # Errors
    /// [`EncodeError::EncodersRefused`] naming every encoder tried, or why none was.
    pub fn open(config: &VideoEncodeConfig) -> Result<Self, EncodeError> {
        let Some((codec_type, name)) = codec_type(config.codec) else {
            return Err(refused(vec![refusal(
                "videotoolbox",
                !config.software,
                OpenFailureKind::Absent,
                format!(
                    "VideoToolbox encodes H.264 and HEVC here, not {:?}",
                    config.codec
                ),
            )]));
        };
        let layout = config.layout;
        let input = match (layout.chroma, layout.bit_depth, layout.planes, layout.alpha) {
            (Chroma::Yuv420, BitDepth::Eight, PlaneLayout::Planar, false) => Input::Planar8,
            (Chroma::Yuv420, BitDepth::Ten, PlaneLayout::Planar, false) => Input::Planar10,
            (Chroma::Yuv420, BitDepth::Ten, PlaneLayout::P010, false) => Input::P010,
            _ => {
                return Err(refused(vec![refusal(
                    "videotoolbox",
                    !config.software,
                    OpenFailureKind::Unsuitable,
                    format!("VideoToolbox takes 4:2:0 frames without alpha here, not {layout:?}"),
                )]));
            }
        };
        if input != Input::Planar8 && config.codec != VideoCodec::Hevc {
            return Err(refused(vec![refusal(
                "videotoolbox",
                !config.software,
                OpenFailureKind::Unsuitable,
                "VideoToolbox encodes 10 bits only as HEVC".into(),
            )]));
        }
        let candidates = listed(codec_type)
            .into_iter()
            .filter(|listed| !(config.software && listed.hardware))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            let kind = if config.software { "software " } else { "" };
            return Err(refused(vec![refusal(
                &format!("videotoolbox {name}"),
                !config.software,
                OpenFailureKind::Absent,
                format!("this system lists no {kind}{name} encoder"),
            )]));
        }
        let mut refusals = Vec::new();
        for candidate in candidates {
            match Self::open_with(config, codec_type, input, &candidate) {
                Ok(session) => return Ok(session),
                Err(reason) => refusals.push(refusal(
                    &candidate.id,
                    candidate.hardware,
                    OpenFailureKind::Unsuitable,
                    reason,
                )),
            }
        }
        Err(refused(refusals))
    }

    fn open_with(
        config: &VideoEncodeConfig,
        codec_type: u32,
        input: Input,
        encoder: &Listed,
    ) -> Result<Self, String> {
        let (time_base_num, time_base_den) =
            alpha2_host_core::encode_policy::encoder_time_base(config.frame_rate, None)
                .map_err(|error| error.to_string())?;
        let full = config.color.range == ColorRange::Full;
        let pixel_format = match (input, full) {
            (Input::Planar8, false) => code(*b"420v"),
            (Input::Planar8, true) => code(*b"420f"),
            (_, false) => code(*b"x420"),
            (_, true) => code(*b"xf20"),
        };
        let id = CFString::from_str(&encoder.id);
        // SAFETY: a framework constant.
        let specification = dictionary(&[(unsafe { kVTVideoEncoderSpecification_EncoderID }, &id)]);
        let empty = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: framework constants.
        let attributes = unsafe {
            dictionary(&[
                (
                    kCVPixelBufferPixelFormatTypeKey,
                    &sys::int(i64::from(pixel_format)),
                ),
                (kCVPixelBufferWidthKey, &sys::int(i64::from(config.width))),
                (kCVPixelBufferHeightKey, &sys::int(i64::from(config.height))),
                (kCVPixelBufferIOSurfacePropertiesKey, &empty),
            ])
        };
        let shared = Box::new(Shared {
            outputs: Mutex::new(Outputs::default()),
            ready: Condvar::new(),
            timescale: time_base_den,
            record: if config.codec == VideoCodec::H264 {
                "avcC"
            } else {
                "hvcC"
            },
            strip_sei: config.codec == VideoCodec::H264,
        });
        let mut session: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: the dictionaries hold the documented value types, the refcon stays valid while
        // the session lives, and `session` is a valid out pointer.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                i32::try_from(config.width).unwrap_or(i32::MAX),
                i32::try_from(config.height).unwrap_or(i32::MAX),
                codec_type,
                Some(specification.as_opaque()),
                Some(attributes.as_opaque()),
                None,
                Some(encoded),
                ptr::from_ref::<Shared>(&shared).cast_mut().cast(),
                NonNull::from(&mut session),
            )
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or_else(|| format!("the session did not open ({})", sys::status(status)))?;
        // SAFETY: a successful create returns a +1 reference.
        let session = Session(unsafe { CFRetained::from_raw(session) });
        let (applied, bounded) = configure(&session, config, input)?;
        // SAFETY: the session is live.
        let status = unsafe { session.0.prepare_to_encode_frames() };
        if status != 0 {
            return Err(format!(
                "preparing to encode failed ({})",
                sys::status(status)
            ));
        }
        // SAFETY: the session is live.
        let pool = unsafe { session.0.pixel_buffer_pool() }.ok_or("the session has no pool")?;
        Ok(Self {
            session,
            pool: Pool(pool),
            shared,
            id: encoder.id.clone(),
            hardware: encoder.hardware,
            timescale: u32::try_from(time_base_den).unwrap_or(1),
            frame_duration: i64::from(time_base_num),
            input,
            applied,
            bounded,
            submitted: 0,
            ended: false,
        })
    }

    /// Sets the rate control `config` resolved: a quantizer as `VideoToolbox`'s quality, else the
    /// bitrate, constant where the encoder holds one and capped per second where it does not.
    fn rate_control(
        config: &VideoEncodeConfig,
        set: &dyn Fn(&CFString, &CFType) -> i32,
    ) -> Result<AppliedRateControl, String> {
        let rate = config.rate;
        // SAFETY: framework constants.
        let (quality, constant, average, limits) = unsafe {
            (
                kVTCompressionPropertyKey_Quality,
                kVTCompressionPropertyKey_ConstantBitRate,
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_DataRateLimits,
            )
        };
        if let Some(quantizer) = rate.quantizer
            && set(quality, &CFNumber::new_f64(videotoolbox_quality(quantizer))) == 0
        {
            return Ok(AppliedRateControl::Quality);
        }
        let bitrate = i64::try_from(rate.bitrate).unwrap_or(i64::MAX);
        if rate.mode == BitrateMode::Constant && set(constant, &sys::int(bitrate)) == 0 {
            return Ok(AppliedRateControl::Bitrate);
        }
        let status = set(average, &sys::int(bitrate));
        if status != 0 {
            return Err(format!("the bitrate was refused ({})", sys::status(status)));
        }
        if rate.mode == BitrateMode::Constant {
            // A one-second window holding the bitrate, in bytes.
            let bytes = sys::int(bitrate / 8);
            let second = CFNumber::new_f64(1.0);
            let items: [&CFType; 2] = [&bytes, &second];
            let window = CFArray::<CFType>::from_objects(&items);
            let status = set(limits, &window);
            if status != 0 {
                return Err(format!(
                    "the rate limit was refused ({})",
                    sys::status(status)
                ));
            }
        }
        Ok(AppliedRateControl::Bitrate)
    }

    fn outputs(&self) -> std::sync::MutexGuard<'_, Outputs> {
        self.shared
            .outputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn failed(&self, reason: &str) -> EncodeError {
        EncodeError::Adapter(format!("{}: {reason}", self.id))
    }

    /// Copies `planes` into a pool buffer as the session's pixel format.
    fn fill(&self, planes: &[PlaneBuffer]) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let mut buffer: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: the pool is live and `buffer` is a valid out pointer.
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool.0, NonNull::from(&mut buffer))
        };
        let buffer = NonNull::new(buffer)
            .filter(|_| status == 0)
            .ok_or_else(|| self.failed(&format!("no pool buffer ({status})")))?;
        // SAFETY: a created buffer is a +1 reference.
        let buffer = unsafe { CFRetained::from_raw(buffer) };
        let expected = if self.input == Input::P010 { 2 } else { 3 };
        if planes.len() != expected {
            return Err(self.failed(&format!("{} planes, not {expected}", planes.len())));
        }
        // SAFETY: the buffer is live; it is unlocked below.
        let locked =
            unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        if locked != 0 {
            return Err(self.failed(&format!("the pool buffer could not be written ({locked})")));
        }
        let filled = self.copy_into(&buffer, planes);
        // SAFETY: the buffer was locked above.
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        filled.map(|()| buffer)
    }

    fn copy_into(&self, buffer: &CVPixelBuffer, planes: &[PlaneBuffer]) -> Result<(), EncodeError> {
        let target = |index: usize| -> Result<(&mut [u8], usize, usize, usize), EncodeError> {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
            let width = CVPixelBufferGetWidthOfPlane(buffer, index);
            let height = CVPixelBufferGetHeightOfPlane(buffer, index);
            if base.is_null() {
                return Err(self.failed(&format!("pool buffer plane {index} has no memory")));
            }
            // SAFETY: a locked plane holds `height` rows of `stride` bytes.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), stride * height) };
            Ok((bytes, stride, width, height))
        };
        let row = |plane, y, length| {
            row(plane, y, length).ok_or_else(|| self.failed("a plane is shorter than its rows"))
        };
        let sample = if self.input == Input::Planar8 { 1 } else { 2 };
        // Luma, then interleaved chroma.
        let (luma, stride, width, height) = target(0)?;
        for y in 0..height.min(planes[0].height as usize) {
            let source = row(&planes[0], y, width * sample)?;
            let destination = &mut luma[y * stride..y * stride + width * sample];
            if self.input == Input::Planar10 {
                let words = destination.as_chunks_mut::<2>().0.iter_mut();
                for (out, word) in words.zip(source.as_chunks::<2>().0) {
                    *out = (u16::from_le_bytes(*word) << 6).to_le_bytes();
                }
            } else {
                destination.copy_from_slice(source);
            }
        }
        let (chroma, stride, width, height) = target(1)?;
        for y in 0..height {
            let destination = &mut chroma[y * stride..y * stride + width * 2 * sample];
            match self.input {
                Input::P010 => destination.copy_from_slice(row(&planes[1], y, width * 4)?),
                Input::Planar8 => {
                    let (u, v) = (row(&planes[1], y, width)?, row(&planes[2], y, width)?);
                    let pairs = destination.as_chunks_mut::<2>().0.iter_mut();
                    for (pair, (u, v)) in pairs.zip(u.iter().zip(v)) {
                        *pair = [*u, *v];
                    }
                }
                Input::Planar10 => {
                    let (u, v) = (
                        row(&planes[1], y, width * 2)?,
                        row(&planes[2], y, width * 2)?,
                    );
                    let samples = u.as_chunks::<2>().0.iter().zip(v.as_chunks::<2>().0);
                    for (pair, (u, v)) in destination.as_chunks_mut::<4>().0.iter_mut().zip(samples)
                    {
                        let u = (u16::from_le_bytes(*u) << 6).to_le_bytes();
                        let v = (u16::from_le_bytes(*v) << 6).to_le_bytes();
                        *pair = [u[0], u[1], v[0], v[1]];
                    }
                }
            }
        }
        Ok(())
    }

    fn take_error(&self) -> Result<(), EncodeError> {
        let error = self.outputs().error.take();
        error.map_or(Ok(()), |reason| Err(self.failed(&reason)))
    }
}

impl VideoEncodeBackend for VideoToolboxEncoder {
    fn name(&self) -> &str {
        &self.id
    }

    fn hardware(&self) -> bool {
        self.hardware
    }

    fn timescale(&self) -> u32 {
        self.timescale
    }

    fn submit(
        &mut self,
        pts: i64,
        planes: &[PlaneBuffer],
        keyframe: bool,
    ) -> Result<Submit, EncodeError> {
        self.take_error()?;
        if self.bounded && self.submitted - self.outputs().completed >= MAX_IN_FLIGHT {
            return Ok(Submit::Busy);
        }
        let buffer = self.fill(planes)?;
        let time = |value: i64| CMTime {
            value,
            timescale: self.shared.timescale,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        // SAFETY: a framework constant.
        let force = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let options = keyframe.then(|| dictionary(&[(force, sys::boolean(true))]));
        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: the buffer is live and the options dictionary holds a boolean.
        let status = unsafe {
            self.session.0.encode_frame(
                &buffer,
                time(pts),
                time(self.frame_duration),
                options.as_deref().map(CFDictionary::as_opaque),
                ptr::null_mut(),
                &raw mut flags,
            )
        };
        if status != 0 {
            return Err(self.failed(&format!("a frame was refused ({})", sys::status(status))));
        }
        self.submitted += 1;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError> {
        self.take_error()?;
        let mut outputs = self.outputs();
        if outputs.packets.is_empty() && wait && !self.ended {
            outputs = self
                .shared
                .ready
                .wait_timeout(outputs, WAIT)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(match outputs.packets.pop_front() {
            Some(packet) => EncodePoll::Packet(packet),
            None if self.ended => EncodePoll::Drained,
            None => EncodePoll::Pending,
        })
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags::empty(),
            epoch: 0,
        };
        // SAFETY: the session is live; completing waits for every pending frame.
        let status = unsafe { self.session.0.complete_frames(invalid) };
        if status != 0 {
            return Err(self.failed(&format!(
                "completing frames failed ({})",
                sys::status(status)
            )));
        }
        self.ended = true;
        self.take_error()
    }

    fn codec_private(&self) -> Option<Bytes> {
        self.outputs().codec_private.clone()
    }

    fn applied_rate_control(&self) -> AppliedRateControl {
        self.applied
    }
}

/// Opens `VideoToolbox` encoder sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppleVideoEncoders;

impl AppleVideoEncoders {
    /// The codecs this system encodes through this backend: those of H.264 and HEVC for which
    /// `VTCopyVideoEncoderList` lists an encoder, hardware or software.
    #[must_use]
    pub fn available_codecs(&self) -> Vec<VideoCodec> {
        VideoCodec::ALL
            .into_iter()
            .filter(|codec| {
                codec_type(*codec).is_some_and(|(codec_type, _)| !listed(codec_type).is_empty())
            })
            .collect()
    }
}

impl VideoEncoders for AppleVideoEncoders {
    fn open(&self, config: &VideoEncodeConfig) -> Result<Box<dyn VideoEncodeBackend>, EncodeError> {
        Ok(Box::new(VideoToolboxEncoder::open(config)?))
    }
}
