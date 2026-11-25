//! Video encoding through `VideoToolbox`: H.264 and HEVC from rendered planes or lent pool buffers.
//!
//! The encoder is chosen by ID from `VTCopyVideoEncoderList`, hardware encoders before software
//! ones, so a software encoder is never used while a hardware one opens. Frames are never
//! reordered, keyframes come at the configured interval, and Apple's per-run SEI is stripped from
//! H.264, so packets are in presentation order and repeatable.
//!
//! A session the system invalidates (the app went to the background) is replaced in place: the
//! frames it had not finished are encoded again on a new session with the same encoder and
//! properties, the first as a keyframe, so the stream goes on with the same configuration.

// Every call into the media frameworks is FFI.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use alpha2_host_core::encode::{
    EncodePoll, EncodedPacket, Lend, LentBuffer, PoolLender, VideoEncodeBackend, VideoEncodeConfig,
    VideoEncoders,
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
    kCVImageBufferColorPrimaries_P3_D65, kCVImageBufferColorPrimaries_SMPTE_C,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_2100_HLG,
    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ, kCVImageBufferTransferFunction_sRGB,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
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

/// Session replacements in a row without a finished frame before the encoder fails.
const MAX_REPLACEMENTS: u32 = 3;

/// Frames an encoder may hold before it must emit one.
const MAX_FRAME_DELAY: i64 = 8;

/// Frames in flight before a submit waits for output, above the encoder's delay so a full
/// encoder always has a frame to emit. It holds when the encoder refused the delay bound too.
pub(crate) const MAX_IN_FLIGHT: u64 = 2 * MAX_FRAME_DELAY.unsigned_abs();

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
    /// Frames the encoder has finished with, encoded, dropped or failed, in submission order.
    completed: u64,
    /// The system invalidated the session; frames it finishes from then on are not counted.
    lost: bool,
    /// Whether this session's first packet was compared with the published record.
    checked: bool,
}

/// The output callback state and the condition a waiting receive sleeps on.
struct Shared {
    outputs: Mutex<Outputs>,
    ready: Condvar,
    timescale: i32,
    /// The configuration record's atom name: `avcC` or `hvcC`.
    record: &'static str,
    strip_sei: bool,
    /// Output callbacks that ran.
    #[cfg(test)]
    callbacks: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The time of the first frame each replacing session encoded, `None` for the next frame.
    #[cfg(test)]
    resumed: std::sync::Arc<Mutex<Vec<Option<i64>>>>,
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

/// Takes the first configuration record and fails a replaced session whose record differs.
fn check_record(outputs: &mut Outputs, sample: &CMSampleBuffer, record: &str) {
    if outputs.codec_private.is_none() {
        outputs.codec_private = configuration_record(sample, record);
        outputs.checked = outputs.codec_private.is_some();
    } else if !outputs.checked {
        outputs.checked = true;
        if configuration_record(sample, record) != outputs.codec_private {
            outputs.error.get_or_insert_with(|| {
                "the session that replaced a lost one has another configuration record".into()
            });
        }
    }
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
    if outputs.lost || status == sys::INVALID_SESSION {
        // The frame is encoded again on the session that replaces this one.
        outputs.lost = true;
    } else {
        if status != 0 {
            outputs
                .error
                .get_or_insert_with(|| format!("encoding failed ({})", sys::status(status)));
        } else if let Some(sample) = sample {
            check_record(&mut outputs, sample, shared.record);
            match packet(sample, shared) {
                Ok(packet) => outputs.packets.push_back(packet),
                Err(reason) => {
                    outputs.error.get_or_insert(reason);
                }
            }
        }
        outputs.completed += 1;
    }
    #[cfg(test)]
    shared
        .callbacks
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    drop(outputs);
    shared.ready.notify_all();
}

/// A compression session, settled and invalidated when dropped.
struct Session(CFRetained<VTCompressionSession>);

// SAFETY: a `VideoToolbox` session may be called from any thread; the encoder that owns it is
// used from one thread at a time.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // Every output callback returns before `complete_frames` does, so none reaches the
        // callback state after the encoder frees it.
        // SAFETY: the session is live; an invalid time completes every pending frame.
        let _ = unsafe { self.0.complete_frames(invalid_time()) };
        // SAFETY: the session is live; nothing uses it after this.
        unsafe { self.0.invalidate() };
    }
}

/// The time that asks `complete_frames` for every pending frame.
const fn invalid_time() -> CMTime {
    CMTime {
        value: 0,
        timescale: 0,
        flags: CMTimeFlags::empty(),
        epoch: 0,
    }
}

/// The session's pixel buffer pool.
struct Pool(CFRetained<CVPixelBufferPool>);

// SAFETY: pixel buffer pools are thread-safe.
unsafe impl Send for Pool {}

/// A submitted, unfinished frame, kept to encode it again if the session is lost.
struct Kept {
    pts: i64,
    buffer: CFRetained<CVPixelBuffer>,
    keyframe: bool,
}

// SAFETY: pixel buffers may be used from any thread; only the encoder's thread touches these.
unsafe impl Send for Kept {}

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
                ColorPrimaries::DisplayP3 => kCVImageBufferColorPrimaries_P3_D65,
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

/// Sets the session's properties for `config`; returns the rate control it runs.
fn configure(
    session: &Session,
    config: &VideoEncodeConfig,
    input: Input,
) -> Result<AppliedRateControl, String> {
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
    // An encoder refusing the bound must emit by `MAX_IN_FLIGHT` or fail the sink's stall bound.
    // SAFETY: a framework constant.
    let delay = unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount };
    #[cfg(test)]
    let refuse = hooks::REFUSE_DELAY_BOUND.get();
    #[cfg(not(test))]
    let refuse = false;
    if !refuse {
        let _ = set(delay, &sys::int(MAX_FRAME_DELAY));
    }
    VideoToolboxEncoder::rate_control(config, &set)
}

fn lends(input: Input, config: &VideoEncodeConfig) -> bool {
    input == Input::Planar8 && config.color.range != ColorRange::Full
}

struct Lent {
    buffer: Option<CFRetained<CVPixelBuffer>>,
    count: Arc<AtomicU64>,
}

// SAFETY: pixel buffers may be used from any thread; the lent buffer is only handed back to the
// encoder that lent it.
unsafe impl Send for Lent {}

impl Drop for Lent {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A session of encoder `id` for `config` whose output callback writes to `shared`, with its
/// pool and the rate control it runs.
fn create(
    config: &VideoEncodeConfig,
    codec_type: u32,
    input: Input,
    id: &str,
    shared: &Shared,
) -> Result<(Session, Pool, AppliedRateControl), String> {
    let full = config.color.range == ColorRange::Full;
    let pixel_format = match (input, full) {
        (Input::Planar8, false) => code(*b"420v"),
        (Input::Planar8, true) => code(*b"420f"),
        (_, false) => code(*b"x420"),
        (_, true) => code(*b"xf20"),
    };
    let id = CFString::from_str(id);
    // SAFETY: a framework constant.
    let specification = dictionary(&[(unsafe { kVTVideoEncoderSpecification_EncoderID }, &id)]);
    let empty = CFDictionary::<CFString, CFType>::empty();
    let (format, width, height) = (
        sys::int(i64::from(pixel_format)),
        sys::int(i64::from(config.width)),
        sys::int(i64::from(config.height)),
    );
    // SAFETY: framework constants.
    let mut attributes: Vec<(&CFString, &CFType)> = unsafe {
        vec![
            (kCVPixelBufferPixelFormatTypeKey, &format),
            (kCVPixelBufferWidthKey, &width),
            (kCVPixelBufferHeightKey, &height),
            (kCVPixelBufferIOSurfacePropertiesKey, &empty),
        ]
    };
    if lends(input, config) {
        // SAFETY: a framework constant.
        attributes.push((
            unsafe { kCVPixelBufferMetalCompatibilityKey },
            sys::boolean(true),
        ));
    }
    let attributes = dictionary(&attributes);
    let mut session: *mut VTCompressionSession = ptr::null_mut();
    // SAFETY: the dictionaries hold the documented value types, `session` is a valid out pointer,
    // and the refcon outlives the session, which the encoder drops before its callback state.
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
            ptr::from_ref::<Shared>(shared).cast_mut().cast(),
            NonNull::from(&mut session),
        )
    };
    let session = NonNull::new(session)
        .filter(|_| status == 0)
        .ok_or_else(|| format!("the session did not open ({})", sys::status(status)))?;
    // SAFETY: a successful create returns a +1 reference.
    let session = Session(unsafe { CFRetained::from_raw(session) });
    let applied = configure(&session, config, input)?;
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
    Ok((session, Pool(pool), applied))
}

/// A `VideoToolbox` encoder session.
pub struct VideoToolboxEncoder {
    // Declared before `shared`: the session must be invalidated before its callback state goes.
    session: Session,
    pool: Pool,
    shared: Box<Shared>,
    config: VideoEncodeConfig,
    codec_type: u32,
    id: String,
    hardware: bool,
    timescale: u32,
    frame_duration: i64,
    input: Input,
    applied: AppliedRateControl,
    /// Frames submitted and not finished, oldest first.
    kept: VecDeque<Kept>,
    lent: Arc<AtomicU64>,
    generation: u64,
    /// Frames finished and let go from `kept`.
    released: u64,
    /// Sessions replaced since a frame last finished.
    replacements: u32,
    /// Whether the next frame is a keyframe because a replaced session had no frame to re-encode.
    key_next: bool,
    ended: bool,
    /// Answers the encode call of this frame (counted from 0) as a lost session, and whether the
    /// replacing session then publishes another configuration record.
    #[cfg(test)]
    lose_at: Option<(u64, bool)>,
    #[cfg(test)]
    lose_after_lend: Option<u64>,
    #[cfg(test)]
    lends: u64,
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
            #[cfg(test)]
            callbacks: std::sync::Arc::default(),
            #[cfg(test)]
            resumed: std::sync::Arc::default(),
        });
        let (session, pool, applied) = create(config, codec_type, input, &encoder.id, &shared)?;
        Ok(Self {
            session,
            pool,
            shared,
            config: *config,
            codec_type,
            id: encoder.id.clone(),
            hardware: encoder.hardware,
            timescale: u32::try_from(time_base_den).unwrap_or(1),
            frame_duration: i64::from(time_base_num),
            input,
            applied,
            kept: VecDeque::new(),
            lent: Arc::default(),
            generation: 0,
            released: 0,
            replacements: 0,
            key_next: false,
            ended: false,
            #[cfg(test)]
            lose_at: None,
            #[cfg(test)]
            lose_after_lend: None,
            #[cfg(test)]
            lends: 0,
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

    /// A buffer of the session's pool, or the pool's status.
    fn pool_buffer(&self) -> Result<CFRetained<CVPixelBuffer>, i32> {
        let mut buffer: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: the pool is live and `buffer` is a valid out pointer.
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool.0, NonNull::from(&mut buffer))
        };
        let buffer = NonNull::new(buffer).filter(|_| status == 0).ok_or(status)?;
        // SAFETY: a created buffer is a +1 reference.
        Ok(unsafe { CFRetained::from_raw(buffer) })
    }

    fn take_pool_buffer(&mut self) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        match self.pool_buffer() {
            Err(sys::INVALID_SESSION) => {
                self.replace()?;
                self.pool_buffer()
            }
            buffer => buffer,
        }
        .map_err(|status| self.failed(&format!("no pool buffer ({status})")))
    }

    /// Copies `planes` into a pool buffer as the session's pixel format.
    fn fill(&mut self, planes: &[PlaneBuffer]) -> Result<CFRetained<CVPixelBuffer>, EncodeError> {
        let buffer = self.take_pool_buffer()?;
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

    /// Frames submitted whose callback has not finished them, and buffers lent.
    pub(crate) fn in_flight(&mut self) -> u64 {
        self.release();
        self.kept.len() as u64 + self.lent.load(Ordering::Acquire)
    }

    /// Lets go of the kept frames the encoder finished.
    fn release(&mut self) {
        let completed = self.outputs().completed;
        while self.released < completed && self.kept.pop_front().is_some() {
            self.released += 1;
            self.replacements = 0;
        }
    }

    /// Hands `frame` to the session; returns the session's status.
    fn encode(&self, frame: &Kept, keyframe: bool) -> i32 {
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
        unsafe {
            self.session.0.encode_frame(
                &frame.buffer,
                time(frame.pts),
                time(self.frame_duration),
                options.as_deref().map(CFDictionary::as_opaque),
                ptr::null_mut(),
                &raw mut flags,
            )
        }
    }

    /// Opens a new session with the same encoder and properties in place of a lost one, and
    /// encodes the frames the lost one did not finish again, the first as a keyframe.
    fn replace(&mut self) -> Result<(), EncodeError> {
        loop {
            self.replacements += 1;
            if self.replacements > MAX_REPLACEMENTS {
                return Err(self.failed(&format!(
                    "the system invalidated {MAX_REPLACEMENTS} sessions in a row"
                )));
            }
            let (session, pool, _) = create(
                &self.config,
                self.codec_type,
                self.input,
                &self.id,
                &self.shared,
            )
            .map_err(|reason| self.failed(&format!("a lost session was not replaced: {reason}")))?;
            // Dropping the lost session returns once its last callback has.
            drop(std::mem::replace(&mut self.session, session));
            self.pool = pool;
            self.generation += 1;
            self.release();
            {
                let mut outputs = self.outputs();
                outputs.lost = false;
                outputs.checked = false;
            }
            self.key_next = self.kept.is_empty();
            #[cfg(test)]
            self.shared
                .resumed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self.kept.front().map(|frame| frame.pts));
            let mut lost = false;
            for (index, frame) in self.kept.iter().enumerate() {
                match self.encode(frame, frame.keyframe || index == 0) {
                    0 => {}
                    sys::INVALID_SESSION => {
                        lost = true;
                        break;
                    }
                    status => {
                        return Err(
                            self.failed(&format!("a frame was refused ({})", sys::status(status)))
                        );
                    }
                }
            }
            if !lost {
                return Ok(());
            }
        }
    }

    /// Replaces the session when a callback reported it lost.
    fn recover(&mut self) -> Result<(), EncodeError> {
        if self.outputs().lost {
            self.replace()?;
        }
        Ok(())
    }

    /// Waits until the session finished every frame, replacing it when it is lost.
    fn complete(&mut self) -> Result<(), EncodeError> {
        loop {
            self.recover()?;
            // SAFETY: the session is live; completing waits for every pending frame.
            let status = unsafe { self.session.0.complete_frames(invalid_time()) };
            if status == sys::INVALID_SESSION || self.outputs().lost {
                self.replace()?;
                continue;
            }
            if status != 0 {
                return Err(self.failed(&format!(
                    "completing frames failed ({})",
                    sys::status(status)
                )));
            }
            self.release();
            return self.take_error();
        }
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
        self.recover()?;
        self.take_error()?;
        if self.in_flight() >= MAX_IN_FLIGHT {
            return Ok(Submit::Busy);
        }
        let buffer = self.fill(planes)?;
        self.encode_buffer(pts, buffer, keyframe)?;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError> {
        self.recover()?;
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

    fn settle(&mut self) -> Result<(), EncodeError> {
        self.complete()
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        self.complete()?;
        self.ended = true;
        Ok(())
    }

    fn codec_private(&self) -> Option<Bytes> {
        self.outputs().codec_private.clone()
    }

    fn applied_rate_control(&self) -> AppliedRateControl {
        self.applied
    }

    fn lender(&mut self) -> Option<&mut dyn PoolLender> {
        if lends(self.input, &self.config) {
            Some(self)
        } else {
            None
        }
    }
}

impl PoolLender for VideoToolboxEncoder {
    fn lend(&mut self) -> Result<Lend, EncodeError> {
        self.recover()?;
        self.take_error()?;
        if self.in_flight() >= MAX_IN_FLIGHT {
            return Ok(Lend::Busy);
        }
        let buffer = self.take_pool_buffer()?;
        self.lent.fetch_add(1, Ordering::AcqRel);
        let surface = CFRetained::as_ptr(&buffer).as_ptr().cast();
        let lent = Lent {
            buffer: Some(buffer),
            count: Arc::clone(&self.lent),
        };
        #[cfg(test)]
        {
            if self.lose_after_lend == Some(self.lends) {
                self.outputs().lost = true;
            }
            self.lends += 1;
        }
        Ok(Lend::Buffer(LentBuffer::new(
            surface,
            self.generation,
            usize::try_from(MAX_IN_FLIGHT).unwrap_or(usize::MAX),
            Box::new(lent),
        )))
    }

    fn encode_lent(
        &mut self,
        pts: i64,
        buffer: LentBuffer,
        keyframe: bool,
    ) -> Result<(), EncodeError> {
        let mut lent = buffer
            .into_owner()
            .downcast::<Lent>()
            .ok()
            .filter(|lent| Arc::ptr_eq(&lent.count, &self.lent))
            .ok_or_else(|| self.failed("a buffer this encoder did not lend came back"))?;
        let buffer = lent
            .buffer
            .take()
            .ok_or_else(|| self.failed("a lent buffer came back empty"))?;
        drop(lent);
        self.recover()?;
        self.take_error()?;
        self.encode_buffer(pts, buffer, keyframe)
    }
}

impl VideoToolboxEncoder {
    fn encode_buffer(
        &mut self,
        pts: i64,
        buffer: CFRetained<CVPixelBuffer>,
        keyframe: bool,
    ) -> Result<(), EncodeError> {
        let keyframe = keyframe || std::mem::take(&mut self.key_next);
        let frame = Kept {
            pts,
            buffer,
            keyframe,
        };
        #[cfg(test)]
        let lose = {
            let index = self.released + self.kept.len() as u64;
            self.lose_at.take_if(|(at, _)| *at == index)
        };
        #[cfg(not(test))]
        let lose: Option<(u64, bool)> = None;
        let status = if let Some((_, other_record)) = lose {
            // As the system does: frames in flight are lost with the session.
            let mut outputs = self.outputs();
            outputs.lost = true;
            if other_record && let Some(record) = outputs.codec_private.as_mut() {
                *record = [&record[..], &[0]].concat().into();
            }
            sys::INVALID_SESSION
        } else {
            self.encode(&frame, keyframe)
        };
        match status {
            0 => self.kept.push_back(frame),
            sys::INVALID_SESSION => {
                self.kept.push_back(frame);
                self.replace()?;
            }
            status => {
                return Err(self.failed(&format!("a frame was refused ({})", sys::status(status))));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl VideoToolboxEncoder {
    /// Answers the encode call of frame `index` (counted from 0) as a lost session, once; with
    /// `other_record` the replacing session's record differs from the published one.
    pub(crate) fn lose_session_at(&mut self, index: u64, other_record: bool) {
        self.lose_at = Some((index, other_record));
    }

    /// Where each replacing session started, which outlives the encoder.
    pub(crate) fn resumed(&self) -> std::sync::Arc<Mutex<Vec<Option<i64>>>> {
        std::sync::Arc::clone(&self.shared.resumed)
    }

    pub(crate) fn lose_session_after_lend(&mut self, index: u64) {
        self.lose_after_lend = Some(index);
    }

    /// The count of output callbacks that ran, which outlives the encoder.
    pub(crate) fn callbacks(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::clone(&self.shared.callbacks)
    }
}

/// Test hooks, read on the thread that opens the session.
#[cfg(test)]
pub(crate) mod hooks {
    use std::cell::Cell;

    thread_local! {
        /// Opens sessions without the frame delay bound, as an encoder that refuses it.
        pub(crate) static REFUSE_DELAY_BOUND: Cell<bool> = const { Cell::new(false) };
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
