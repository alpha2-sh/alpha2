//! The D3D12VA backend: `FFmpeg`'s D3D12 Video encoders (`h264_d3d12va`,
//! `hevc_d3d12va`, `av1_d3d12va`) on the renderer's shared D3D12 device.
//!
//! The handoff is Vulkan's: the renderer packs NV12 into its own `R8Unorm`
//! and `Rg8Unorm` plane ring, and a raw D3D12 copy on the renderer's queue
//! moves each frame into a texture of `FFmpeg`'s encoder-input pool, so no
//! pixel crosses the CPU. wgpu can neither render nor copy into an NV12
//! texture, which is why the copy is raw. The copy signals the pool frame's
//! own fence and records the value on the frame; `FFmpeg`'s encode queue
//! waits for it on the GPU before it reads the texture.
//!
//! Two things differ from Vulkan, both because `FFmpeg` 8.1's D3D12VA
//! encoders write neither extradata nor a colour description: the session
//! runs its packets through an [`EncodedVideoFilter`], which writes the
//! export's colour into every sequence header and moves the in-band
//! parameter sets out into side data, and muxes into an output whose header
//! waits for the first packet ([`Muxer::open_deferred`]).
//!
//! What a device encodes is asked of the device itself
//! (`SharedD3D12Context::encode_support`), and a device that answers yes is
//! still held to one test encode, once per device and codec for the
//! process: a session can fail at `CreateVideoEncoder` on a device that
//! reports support, or write packets the software decoder cannot decode.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alpha2_media::{BitrateMode, ColorMetadata, EncodeError, VideoRateControl};
use alpha2_native_gpu::{D3D12EncodeCodec, D3D12EncodeExtent, SharedD3D12Context};
use alpha2_time::MediaTime;
use rsmpeg::avcodec::{AVCodec, AVCodecContext, AVPacket};
use rsmpeg::avutil::{AVFrame, AVHWDeviceContext, AVHWFramesContext, AVRational, av_rescale_q, ra};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use super::{
    EncoderInputHandoff, EncoderInputLayout, EncoderPoolStats, EncoderSurfaceDescriptor,
    HardwareEncodeBackend, HardwareEncodeCapability, HardwareEncodeOpenError,
    HardwareEncodeRequest, InputFrame,
};
use crate::NativeVideoCodec;
use crate::bitstream::EncodedVideoFilter;
use crate::convert::{encode_adapter_error, tick_base};
use crate::encode::{EncodeStageTimings, timed};
use crate::mux::Muxer;
use crate::rate_control::RateControlMechanism;

#[path = "d3d12va_rate_control.rs"]
mod rate_control_probe;
#[cfg(test)]
pub(crate) use rate_control_probe::encode_no_rate_control_frames;
pub(crate) use rate_control_probe::probe_d3d12va_rate_control;
pub use rate_control_probe::{
    D3D12RateControlOpen, D3D12RateControlRunner, open_d3d12va_rate_control,
    set_d3d12va_rate_control_runner,
};

/// One codec's D3D12VA encoder.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Encoder {
    codec: NativeVideoCodec,
    /// What D3D12 Video Encode is asked about.
    d3d12: D3D12EncodeCodec,
    /// The `FFmpeg` encoder.
    name: &'static str,
    c_name: &'static CStr,
    /// The profile it is opened at, as its `profile` option spells it: the
    /// one the device was asked about (`D3D12EncodeCodec::profile`).
    profile: &'static CStr,
}

impl Encoder {
    /// The encoder for `codec`, or why D3D12 Video Encode has none.
    fn for_codec(codec: NativeVideoCodec) -> Result<Self, String> {
        let (d3d12, name, c_name) = match codec {
            NativeVideoCodec::H264 => (D3D12EncodeCodec::H264, "h264_d3d12va", c"h264_d3d12va"),
            NativeVideoCodec::Hevc => (D3D12EncodeCodec::Hevc, "hevc_d3d12va", c"hevc_d3d12va"),
            NativeVideoCodec::Av1 => (D3D12EncodeCodec::Av1, "av1_d3d12va", c"av1_d3d12va"),
            other => {
                return Err(format!(
                    "D3D12 Video Encode encodes h264, hevc or av1, not {}",
                    other.as_str()
                ));
            }
        };
        // H.264 opens at Main, not High: FFmpeg 8.1.3's `h264_d3d12va` writes a High PPS with
        // `transform_8x8_mode_flag` but never enables the driver's adaptive 8x8 transform, so
        // slices lack the flag the PPS announces and no decoder reads past the first macroblock.
        // The slices are the same at Main (no 8x8 transform, no scaling lists, CABAC).
        let profile = c"main";
        Ok(Self {
            codec,
            d3d12,
            name,
            c_name,
            profile,
        })
    }

    fn find(self) -> Result<rsmpeg::avcodec::AVCodecRef<'static>, String> {
        AVCodec::find_encoder_by_name(self.c_name)
            .ok_or_else(|| format!("this FFmpeg runtime has no {} encoder", self.name))
    }
}

/// What one D3D12VA encode session turned out to require and produce.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct D3D12EncodeProbe {
    /// `FFmpeg` encoder that opened.
    pub encoder: String,
    /// The codec it encodes.
    pub codec: NativeVideoCodec,
    /// The rate-control mode it opened with, as the capability names it:
    /// `disabled` (a constant quantizer), `vbr` or `cbr`.
    pub rate_control: &'static str,
    /// The size of the texture a pool frame is.
    pub coded_size: (u32, u32),
    /// The profile the encoder configured the device with, as `FFmpeg` numbers
    /// it (`AVCodecContext::profile` after the open): 77 for H.264 Main, 1
    /// for HEVC Main, 0 for AV1 Main. The session asks for the one the
    /// device was asked about rather than leaving it to the encoder.
    pub opened_profile: i32,
    /// Extradata the encoder wrote at open; `FFmpeg` 8.1 writes none, and
    /// puts the parameter sets in-band instead.
    pub extradata_size: usize,
    /// The profile the stream says it is: `profile_idc` of the first H.264
    /// or HEVC sequence parameter set (77 is H.264 Main, 100 High; 1 is HEVC
    /// Main), or `seq_profile` of the first AV1 sequence header.
    pub profile: Option<u8>,
    /// The unit types of the first packet, in order: NAL unit types for
    /// H.264 and HEVC, OBU types for AV1.
    pub first_packet_units: Vec<u8>,
    /// Packets drained from one submitted frame plus a flush.
    pub packets: u32,
    /// Bytes in those packets.
    pub packet_bytes: u32,
    /// Why the software decoder could not decode those packets, as a
    /// reader of the export would: `None` when it decoded a clean picture
    /// for every packet.
    pub decode_failure: Option<String>,
}

impl D3D12EncodeProbe {
    /// Whether a decoder can start from the first packet: it holds the
    /// stream's parameter sets and a coded picture, and if not, what it
    /// holds.
    ///
    /// An encoder that opens and returns packets can still write ones no
    /// decoder takes. `FFmpeg` writes an IDR picture's parameter sets at the
    /// start of the encoder's output buffer and tells the driver to write the
    /// picture after them (`FrameStartOffset`). An Intel Iris Xe on driver
    /// 30.0.101.2079 does not honour that: `h264_d3d12va`'s first packet is
    /// the IDR slice alone (NAL unit types `[5]`), the picture written over
    /// the SPS and PPS, and `hevc_d3d12va`'s is the VPS, SPS and PPS with no
    /// slice after them (`[32, 33, 34]`).
    ///
    /// A first packet with every unit can still be coded against parameter
    /// sets that say otherwise, so the packets must also have decoded
    /// ([`Self::decode_failure`]): with a High PPS, `FFmpeg` 8.1.3's
    /// `h264_d3d12va` writes one no decoder reads past its first macroblock
    /// (see `Encoder::for_codec`).
    ///
    /// # Errors
    /// Names the units the first packet held and what it lacked, or why the
    /// software decoder refused the packets.
    pub fn first_packet_decodes(&self) -> Result<(), String> {
        let units = &self.first_packet_units;
        let has = |kinds: &[u8]| kinds.iter().all(|kind| units.contains(kind));
        let (parameter_sets, picture, unit) = match self.codec {
            NativeVideoCodec::H264 => (
                has(&[7, 8]),
                units.iter().any(|kind| (1..=5).contains(kind)),
                "NAL unit",
            ),
            NativeVideoCodec::Hevc => (
                has(&[32, 33, 34]),
                units.iter().any(|kind| *kind <= 21),
                "NAL unit",
            ),
            _ => (has(&[1]), has(&[6]) || has(&[3, 4]), "OBU"),
        };
        let lacks = match (parameter_sets, picture) {
            (true, true) => {
                return match &self.decode_failure {
                    None => Ok(()),
                    Some(failure) => Err(format!(
                        "{}'s packets do not decode: {failure}",
                        self.encoder
                    )),
                };
            }
            (false, true) => "its parameter sets",
            (true, false) => "a coded picture",
            (false, false) => "both its parameter sets and a coded picture",
        };
        Err(format!(
            "{}'s first packet lacks {lacks} ({unit} types {units:?}), so no decoder can start \
             from it",
            self.encoder
        ))
    }
}

/// Opens `codec`'s D3D12VA encoder on `shared`, as an export does, and
/// encodes one pool frame.
///
/// The frame is left as the pool allocated it: this answers whether the
/// session can be created and driven on this device, not what the picture
/// looks like. It opens with a rate-control mode the device lists, a
/// constant quantizer where it has one.
///
/// `FFmpeg` waits on the GPU without a bound here (its D3D12VA encoder's
/// waits are `INFINITE`), so a caller that must not hang runs this on a
/// thread it can abandon, as [`capability`] does.
///
/// # Errors
/// Returns an adapter failure when the encoder is absent, the device does
/// not encode `codec`, the pool cannot be created, the session cannot open,
/// or no packet is produced.
pub fn probe_d3d12va_encode(
    shared: &SharedD3D12Context,
    codec: NativeVideoCodec,
    width: u32,
    height: u32,
) -> Result<D3D12EncodeProbe, EncodeError> {
    let encoder = Encoder::for_codec(codec).map_err(EncodeError::Adapter)?;
    encoder.find().map_err(EncodeError::Adapter)?;
    let device = DeviceModes::of(shared, encoder).map_err(EncodeError::Adapter)?;
    let rate_control = probe_rate_control(&device).map_err(EncodeError::Adapter)?;
    let (mut frames, mut context, selection) = open_encoder(
        &shared.ffmpeg_device(),
        encoder,
        width,
        height,
        rate_control,
        (30, 1),
        ColorMetadata::EXPORT,
        &device,
    )?;
    let extradata_size = usize::try_from(context.extradata_size).unwrap_or(0);
    let opened_profile = context.profile;
    let mut frame = AVFrame::new();
    frames
        .get_buffer(&mut frame)
        .map_err(encode_adapter_error)?;
    let info = alpha2_ffmpeg_bridge::d3d12_frame_info(&frame).map_err(encode_adapter_error)?;
    frame.set_pts(0);
    context
        .send_frame(Some(&frame))
        .map_err(encode_adapter_error)?;
    context.send_frame(None).map_err(encode_adapter_error)?;
    let mut packets = Vec::new();
    loop {
        match context.receive_packet() {
            Ok(packet) => packets.push(packet),
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(error) => return Err(encode_adapter_error(error)),
        }
    }
    let Some(first) = packets.first() else {
        return Err(EncodeError::Adapter(format!(
            "{} opened but produced no packet",
            encoder.name
        )));
    };
    let (first_packet_units, profile) =
        stream_units(codec, &alpha2_ffmpeg_bridge::packet_bytes(first));
    let packet_bytes = packets.iter().fold(0_u32, |sum, packet| {
        sum.saturating_add(packet.size.unsigned_abs())
    });
    let decode_failure = software_decode_failure(codec, &packets);
    Ok(D3D12EncodeProbe {
        encoder: encoder.name.to_owned(),
        codec,
        rate_control: match selection.rate_control {
            crate::AppliedRateControl::Quantizer => "disabled",
            crate::AppliedRateControl::Bitrate if rate_control.mode == BitrateMode::Constant => {
                "cbr"
            }
            crate::AppliedRateControl::Bitrate => "vbr",
        },
        coded_size: (info.surface_width, info.surface_height),
        opened_profile,
        extradata_size,
        profile,
        first_packet_units,
        packets: u32::try_from(packets.len()).unwrap_or(u32::MAX),
        packet_bytes,
        decode_failure,
    })
}

/// Why the software decoder an import would use cannot decode `packets`, a
/// test encode's whole output with its parameter sets in-band, or `None`
/// when it decodes a clean picture for each of them.
///
/// The units of a first packet say only that a decoder can start; whether
/// the slices agree with the parameter sets they are read against is
/// answered by decoding them.
fn software_decode_failure(codec: NativeVideoCodec, packets: &[AVPacket]) -> Option<String> {
    let decoder = match crate::decode::software_video_decoder(codec.codec_id()) {
        Ok(decoder) => decoder,
        Err(error) => {
            return Some(format!(
                "there is no software decoder to check them: {error}"
            ));
        }
    };
    let name = decoder.name().to_string_lossy().into_owned();
    let mut context = AVCodecContext::new(&decoder);
    if let Err(error) = context.open(None) {
        return Some(format!("the software {name} decoder did not open: {error}"));
    }
    let corrupt = i32::try_from(ffi::AV_FRAME_FLAG_CORRUPT).unwrap_or(0);
    let mut pictures = 0_usize;
    for (at, packet) in packets.iter().map(Some).chain([None]).enumerate() {
        if let Err(error) = context.send_packet(packet) {
            return Some(match packet {
                Some(_) => format!("the software {name} decoder refused packet {at}: {error}"),
                None => format!("the software {name} decoder did not flush: {error}"),
            });
        }
        loop {
            match context.receive_frame() {
                Ok(frame) if frame.flags & corrupt != 0 || frame.decode_error_flags != 0 => {
                    return Some(format!(
                        "the software {name} decoder decoded picture {pictures} of {} corrupt",
                        packets.len()
                    ));
                }
                Ok(_) => pictures += 1,
                Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
                Err(error) => return Some(format!("the software {name} decoder failed: {error}")),
            }
        }
    }
    (pictures != packets.len()).then(|| {
        format!(
            "the software {name} decoder decoded {pictures} pictures from {} packets",
            packets.len()
        )
    })
}

/// The rate control a probe opens with: a constant quantizer where the
/// device lists one, otherwise a variable or a constant rate, whichever it
/// lists.
fn probe_rate_control(device: &DeviceModes) -> Result<VideoRateControl, String> {
    let rate = |mode| VideoRateControl {
        bitrate: 4_000_000,
        mode,
        quantizer: None,
    };
    if device.lists("disabled") {
        Ok(VideoRateControl {
            quantizer: Some(26),
            ..rate(BitrateMode::Variable)
        })
    } else if device.lists("vbr") {
        Ok(rate(BitrateMode::Variable))
    } else if device.lists("cbr") {
        Ok(rate(BitrateMode::Constant))
    } else {
        Err(format!(
            "{}: {} lists no rate-control mode an export asks for (it lists {})",
            device.adapter,
            device.encoder.name,
            device.listed()
        ))
    }
}

/// The unit types of an encoded packet, in order, and the profile its first
/// sequence header names, for a report.
fn stream_units(codec: NativeVideoCodec, bytes: &[u8]) -> (Vec<u8>, Option<u8>) {
    let mut types = Vec::new();
    let mut profile = None;
    if codec == NativeVideoCodec::Av1 {
        // Low-overhead bitstream: each OBU has its size field, as FFmpeg's encoders write it.
        let mut at = 0;
        while at < bytes.len() {
            let header = bytes[at];
            let kind = (header >> 3) & 0x0f;
            types.push(kind);
            at += 1 + usize::from(header & 0x04 != 0);
            if header & 0x02 == 0 {
                break;
            }
            let (mut size, mut shift) = (0_usize, 0);
            while at < bytes.len() {
                let byte = bytes[at];
                at += 1;
                size |= usize::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 || shift >= 56 {
                    break;
                }
            }
            // An OBU_SEQUENCE_HEADER starts with its three-bit `seq_profile`.
            if kind == 1 && profile.is_none() {
                profile = bytes.get(at).map(|byte| byte >> 5);
            }
            at = at.saturating_add(size);
        }
        return (types, profile);
    }
    let mut at = 0;
    while at + 3 <= bytes.len() {
        if bytes[at..at + 3] != [0, 0, 1] {
            at += 1;
            continue;
        }
        at += 3;
        let Some(&header) = bytes.get(at) else {
            break;
        };
        if codec == NativeVideoCodec::Hevc {
            let kind = (header >> 1) & 0x3f;
            types.push(kind);
            // After the two-byte header of an SPS: the VPS id, sub-layers and nesting flag in one
            // byte, then `general_profile_space`, `general_tier_flag` and `general_profile_idc`.
            if kind == 33 && profile.is_none() {
                profile = bytes.get(at + 3).map(|byte| byte & 0x1f);
            }
        } else {
            let kind = header & 0x1f;
            types.push(kind);
            if kind == 7 && profile.is_none() {
                profile = bytes.get(at + 1).copied();
            }
        }
    }
    (types, profile)
}

/// The output size a test encode opens at: above every device's minimum
/// coded extent, and one frame of it is quick to encode.
pub const D3D12VA_TEST_ENCODE_SIZE: (u32, u32) = (256, 256);

/// The D3D12VA encoder this runtime registers for `codec`, which a test
/// encode would open, or why there is none.
///
/// # Errors
/// Says why D3D12 Video Encode has no encoder for `codec`, or that this
/// `FFmpeg` runtime does not register it.
pub fn d3d12va_encoder(codec: NativeVideoCodec) -> Result<&'static str, String> {
    let encoder = Encoder::for_codec(codec)?;
    encoder.find()?;
    Ok(encoder.name)
}

/// Runs the test encode that holds `shared`'s device to its answer for one
/// codec: [`probe_d3d12va_encode`] at [`D3D12VA_TEST_ENCODE_SIZE`], or what
/// stands in for it, and its answer or why it gave none.
///
/// The answer is held to the same check as the in-process one: a session
/// that opened is only offered when its first packet decodes.
pub type D3D12TestEncodeRunner =
    fn(&SharedD3D12Context, NativeVideoCodec) -> Result<D3D12EncodeProbe, String>;

/// What runs each test encode instead of a worker thread of this process.
static RUNNER: std::sync::OnceLock<D3D12TestEncodeRunner> = std::sync::OnceLock::new();

/// Makes `runner` what runs every test encode of this process, from now on,
/// in place of a worker thread of its own.
///
/// A driver that crashes inside its encode submission takes the process
/// with it, which no thread can contain. A caller that must survive that
/// runs the encode in another process and returns that process's answer,
/// or why it gave none: a crash, an exit, or a timeout. The runner bounds
/// its own wait; it is called with no timeout around it.
///
/// # Errors
/// Returns `runner` when one is installed already: the first one stays, so
/// no answer depends on which caller came last.
pub fn set_d3d12va_test_encode_runner(
    runner: D3D12TestEncodeRunner,
) -> Result<(), D3D12TestEncodeRunner> {
    RUNNER.set(runner)
}

/// Which adapter a test encode ran on: its LUID, which every context on the
/// adapter shares, or the device itself where DXGI names no adapter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ProbedAdapter {
    Luid(u64),
    Device(u64),
}

impl ProbedAdapter {
    fn of(shared: &SharedD3D12Context) -> Self {
        shared.adapter().map_or_else(
            || Self::Device(shared.identity().device),
            |adapter| Self::Luid(adapter.luid),
        )
    }
}

/// A test encode's answer, kept by the adapter and the codec.
type TestEncode = (
    (ProbedAdapter, D3D12EncodeCodec),
    Result<D3D12EncodeProbe, String>,
);

/// One test encode per adapter and codec, for the process: another context on
/// the same adapter gets the same answer.
static PROBED: Mutex<Vec<TestEncode>> = Mutex::new(Vec::new());

/// The adapters whose test encode outlived the GPU wait timeout: the D3D12VA
/// path is unavailable on them for the rest of the process, whatever the
/// codec, because the thread that ran it may never return.
static TIMED_OUT: std::sync::LazyLock<Mutex<HashMap<ProbedAdapter, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// What `E_OUTOFMEMORY` reads as in a test encode's answer: the device was busy
/// with other work, which a later ask may not find, so the answer is not kept.
const BUSY_HRESULT: &str = "0x8007000E";

fn timed_out_on(adapter: ProbedAdapter) -> Option<String> {
    TIMED_OUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&adapter)
        .cloned()
}

/// Keeps `answer` for `key` unless it says the device was busy.
fn keep_answer(
    probed: &mut Vec<TestEncode>,
    key: (ProbedAdapter, D3D12EncodeCodec),
    answer: &Result<D3D12EncodeProbe, String>,
) {
    if !matches!(answer, Err(reason) if reason.contains(BUSY_HRESULT)) {
        probed.push((key, answer.clone()));
    }
}

/// What `codec`'s D3D12VA encoder accepts on this device, from the device's
/// own answers, once a session has encoded a frame on it.
pub(super) fn capability(
    shared: &SharedD3D12Context,
    codec: NativeVideoCodec,
) -> Result<HardwareEncodeCapability, String> {
    let encoder = Encoder::for_codec(codec)?;
    // Only an encoder this runtime registers can be offered: `FFmpeg` 8.0 has `hevc_d3d12va` alone.
    encoder.find()?;
    let device = DeviceModes::of(shared, encoder)?;
    let capability = capability_with(encoder, &device.extent);
    test_encode(shared, encoder)?;
    Ok(capability)
}

/// The first of H.264, HEVC and AV1 this device encodes, the order a report
/// leads with: H.264 is the default export codec.
pub(super) fn preferred_capability(
    shared: &SharedD3D12Context,
) -> Result<HardwareEncodeCapability, String> {
    let mut reasons = Vec::new();
    for codec in [
        NativeVideoCodec::H264,
        NativeVideoCodec::Hevc,
        NativeVideoCodec::Av1,
    ] {
        match capability(shared, codec) {
            Ok(capability) => return Ok(capability),
            Err(reason) => reasons.push(reason),
        }
    }
    Err(reasons.join("; "))
}

fn capability_with(encoder: Encoder, extent: &D3D12EncodeExtent) -> HardwareEncodeCapability {
    HardwareEncodeCapability {
        backend: HardwareEncodeBackend::D3D12Va,
        encoder: encoder.name,
        codec: encoder.codec,
        gpu_input_only: true,
        input_layout: EncoderInputLayout::Nv12,
        handoff: EncoderInputHandoff::GpuCopy,
        shares_renderer_device: true,
        minimum: extent.minimum,
        maximum: extent.maximum,
        granularity: extent.granularity,
        max_p_picture_l0_references: Some(extent.max_p_picture_l0_references),
        max_b_picture_l0_references: Some(extent.max_b_picture_l0_references),
        max_l1_references: Some(extent.max_l1_references),
        rate_control_modes: extent.rate_control_modes.clone(),
        plane_ring_slots: alpha2_native_gpu::D3D12EncodeCopier::slots(),
        // The copier keeps each copy's frame in its slot until the slot is reused.
        handoff_retained_surfaces: alpha2_native_gpu::D3D12EncodeCopier::slots(),
        codec_retained_surfaces: CODEC_RETAINED_SURFACES,
    }
}

/// Surfaces a D3D12VA encoder holds after `avcodec_send_frame` has returned,
/// beyond what the copier already keeps. `FFmpeg` frees an input picture
/// only once it is encoded and no longer a reference, with up to
/// `async_depth` (2) in flight, but those are recent frames, still in the
/// copier's slots. Measured within the copier's retention on an Intel Iris Xe
/// (8086:9a49, drivers 30.0.101.2079 and 32.0.101.7092) with `FFmpeg` 8.1.3: for `h264_d3d12va`
/// and `hevc_d3d12va`, at 256x192, 1080p and 4K, at channel depths 0, 1, 2,
/// 4 and 8, and over 72 frames (past an IDR), the pool never allocated more
/// than the host's lease peak plus the copier's four slots (exactly four
/// more at depth 0, one more at depth 4 and 8). One adapter is one data
/// point: `the_pool_allocates_within_the_host_leases_plus_the_retained_surfaces`
/// checks this on whatever adapter it runs on.
const CODEC_RETAINED_SURFACES: usize = 0;

/// Runs the one test encode of `encoder` on this device, or answers as the
/// earlier one did.
///
/// `FFmpeg`'s D3D12VA waits are `INFINITE`, so the encode runs on a thread
/// of its own, bounded by the GPU wait timeout. A thread that outlives it is
/// left behind, not joined, and the path is unavailable for the rest of the
/// process: whatever hung it may hang the next session too. A driver that
/// crashes inside its encode submission takes the process with it, which no
/// thread can contain: an Intel Iris Xe on driver 30.0.101.2079 hangs or
/// crashes there about once in a few thousand frames with `FFmpeg` 8.1.3, on
/// `FFmpeg`'s own device as on the renderer's, so this encodes one frame and
/// no more, and a caller that must survive it installs a runner
/// ([`set_d3d12va_test_encode_runner`]) that encodes in another process.
fn test_encode(shared: &SharedD3D12Context, encoder: Encoder) -> Result<(), String> {
    let adapter_key = ProbedAdapter::of(shared);
    if let Some(reason) = timed_out_on(adapter_key) {
        return Err(reason);
    }
    let key = (adapter_key, encoder.d3d12);
    // Held across the encode, so one device and codec is only ever tested once.
    let mut probed = PROBED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, answer)) = probed.iter().find(|(probed, _)| *probed == key) {
        return accepted(answer.clone(), &shared.gpu().capabilities().adapter_name);
    }
    let adapter = shared.gpu().capabilities().adapter_name.clone();
    if let Some(runner) = RUNNER.get() {
        let answer = runner(shared, encoder.codec).map_err(|error| {
            format!(
                "{} did not open and encode on {adapter}: {error}",
                encoder.name
            )
        });
        keep_answer(&mut probed, key, &answer);
        return accepted(answer, &adapter);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = shared.clone();
    let codec = encoder.codec;
    std::thread::Builder::new()
        .name(format!("alpha2 {} probe", encoder.name))
        .spawn(move || {
            let _ = sender.send(
                probe_d3d12va_encode(
                    &worker,
                    codec,
                    D3D12VA_TEST_ENCODE_SIZE.0,
                    D3D12VA_TEST_ENCODE_SIZE.1,
                )
                .map_err(|error| error.to_string()),
            );
        })
        .map_err(|error| format!("{}: the test encode did not start: {error}", encoder.name))?;
    let timeout = alpha2_native_gpu::d3d12_wait_timeout();
    let answer = match receiver.recv_timeout(timeout) {
        Ok(answer) => answer.map_err(|error| {
            format!(
                "{} did not open and encode on {adapter}: {error}",
                encoder.name
            )
        }),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let reason = format!(
                "{}'s test encode on {adapter} did not finish within {timeout:?}; the D3D12VA path \
                 is off on this adapter for the rest of this process",
                encoder.name
            );
            TIMED_OUT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(adapter_key, reason.clone());
            return Err(reason);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(format!(
            "{}'s test encode on {adapter} ended without an answer",
            encoder.name
        )),
    };
    keep_answer(&mut probed, key, &answer);
    accepted(answer, &adapter)
}

/// Whether a test encode's answer lets the path be offered: it opened, encoded,
/// and wrote a first packet a decoder can start from.
fn accepted(answer: Result<D3D12EncodeProbe, String>, adapter: &str) -> Result<(), String> {
    let probe = answer?;
    let decodes = probe
        .first_packet_decodes()
        .map_err(|reason| format!("the D3D12VA test encode on {adapter} is unusable: {reason}"));
    #[cfg(test)]
    if decodes.is_err() && tests::accepts_undecodable_packets() {
        return Ok(());
    }
    decodes
}

/// What one device's D3D12 Video Encode does for one encoder: the modes it
/// encodes with, and those it refused.
struct DeviceModes {
    adapter: String,
    encoder: Encoder,
    extent: D3D12EncodeExtent,
}

impl DeviceModes {
    fn of(shared: &SharedD3D12Context, encoder: Encoder) -> Result<Self, String> {
        Ok(Self {
            adapter: shared.gpu().capabilities().adapter_name.clone(),
            encoder,
            extent: shared.encode_support(encoder.d3d12)?,
        })
    }

    fn lists(&self, mode: &str) -> bool {
        #[cfg(test)]
        if tests::refuses_rate_control_mode(mode) {
            return false;
        }
        self.extent.rate_control_modes.contains(&mode)
    }

    fn listed(&self) -> String {
        self.extent.rate_control_modes.join(", ")
    }

    /// Why `mode` cannot be asked for, when it cannot: named, with what the
    /// device answered, so that an `auto` export falls back saying so.
    fn refusal(&self, mode: &'static str) -> Option<String> {
        if self.lists(mode) {
            return None;
        }
        let answer = self
            .extent
            .refused_rate_control_modes
            .iter()
            .find(|(refused, _)| *refused == mode)
            .map_or_else(
                || "does not list it".to_owned(),
                |(_, status)| {
                    if *status == 0 {
                        "refuses it".to_owned()
                    } else {
                        format!("refuses it with HRESULT {status:#010X}")
                    }
                },
            );
        Some(format!(
            "{}: {} has no {mode} rate control here (D3D12 Video Encode {answer}; it encodes with {}){}",
            self.adapter,
            self.encoder.name,
            self.listed(),
            if self.lists("disabled") {
                "; ask for a quantizer instead"
            } else {
                ""
            }
        ))
    }
}

/// Opens `request.codec`'s D3D12VA encoder on the shared device, its input
/// half, and the output.
pub(super) fn open(
    shared: &SharedD3D12Context,
    request: &HardwareEncodeRequest<'_>,
    generation: u64,
) -> Result<(D3D12EncodeSession, D3D12EncodeInput), HardwareEncodeOpenError> {
    let refused = |error: EncodeError| HardwareEncodeOpenError::Refused(error.to_string());
    let (width, height) = (request.width, request.height);
    let capability = capability(shared, request.codec).map_err(HardwareEncodeOpenError::Refused)?;
    capability
        .accepts(width, height)
        .map_err(HardwareEncodeOpenError::Refused)?;
    let encoder = Encoder::for_codec(request.codec).map_err(HardwareEncodeOpenError::Refused)?;
    let device = DeviceModes::of(shared, encoder).map_err(HardwareEncodeOpenError::Refused)?;
    let frame_rate = super::session_frame_rate(request.frame_rate).map_err(refused)?;
    let (frames, context, selection) = open_encoder(
        &shared.ffmpeg_device(),
        encoder,
        width,
        height,
        request.rate_control,
        frame_rate,
        request.color,
        &device,
    )
    .map_err(refused)?;
    let time_base = ra(frame_rate.1, frame_rate.0);
    let bitstream = EncodedVideoFilter::new(&context.extract_codecpar(), request.color, time_base)
        .map_err(refused)?;
    let copier = alpha2_native_gpu::D3D12EncodeCopier::new(shared)
        .map_err(|error| HardwareEncodeOpenError::Refused(format!("{error:?}")))?;
    let gpu = shared.gpu();
    // One pair per copier slot: the coordinator can run ahead of the encoder, so a single pair
    // would let the next render overwrite planes a copy is still reading.
    let planes = (0..alpha2_native_gpu::D3D12EncodeCopier::slots())
        .map(|_| Nv12PlanePair {
            luma: plane_texture(
                gpu.device(),
                "alpha2 NV12 luma",
                width,
                height,
                wgpu::TextureFormat::R8Unorm,
            ),
            chroma: plane_texture(
                gpu.device(),
                "alpha2 NV12 chroma",
                width / 2,
                height / 2,
                wgpu::TextureFormat::Rg8Unorm,
            ),
        })
        .collect();
    let pool = Arc::new(PoolView::default());
    let input = D3D12EncodeInput {
        copier,
        planes,
        frames,
        size: (width, height),
        time_base,
        color: request.color,
        pool: pool.clone(),
        stats: EncoderPoolStats::default(),
        textures: HashSet::new(),
        #[cfg(debug_assertions)]
        thread: std::thread::current().id(),
    };
    // From here on a failure is the export's rather than a refusal: the partial output exists. Its
    // header waits for the parameter sets the first packet brings.
    let muxer = Muxer::open_deferred(
        request.output,
        request.container,
        &context,
        time_base,
        request.audio,
    )
    .map_err(HardwareEncodeOpenError::Failed)?;
    Ok((
        D3D12EncodeSession {
            context,
            time_base,
            bitstream,
            muxer,
            stages: EncodeStageTimings::default(),
            selection,
            generation,
            shared: shared.clone(),
            size: (width, height),
            sent: 0,
            pool,
        },
        input,
    ))
}

/// One renderer-owned NV12 plane target for the encode handoff.
///
/// `COPY_SRC` is what lets the raw copy into the encoder frame read it; the
/// copy puts it back in `RENDER_TARGET`, the state wgpu tracks it in.
fn plane_texture(
    device: &wgpu::Device,
    label: &'static str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// One slot of the NV12 plane ring the renderer packs into.
struct Nv12PlanePair {
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
}

/// Opens `encoder` on `device` with an NV12 encoder-input pool.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn open_encoder(
    device: &AVHWDeviceContext,
    encoder: Encoder,
    width: u32,
    height: u32,
    rate_control: VideoRateControl,
    frame_rate: (i32, i32),
    color: ColorMetadata,
    modes: &DeviceModes,
) -> Result<
    (
        AVHWFramesContext,
        AVCodecContext,
        crate::NativeVideoEncoderSelection,
    ),
    EncodeError,
> {
    let name = encoder.name;
    let codec = encoder.find().map_err(EncodeError::Adapter)?;
    let coded_width =
        i32::try_from(width).map_err(|_| EncodeError::Adapter("width is out of range".into()))?;
    let coded_height =
        i32::try_from(height).map_err(|_| EncodeError::Adapter("height is out of range".into()))?;
    let rate = u32::try_from(frame_rate.0)
        .ok()
        .zip(u32::try_from(frame_rate.1).ok())
        .and_then(|(num, den)| alpha2_time::FrameRate::new(num, den).ok())
        .ok_or_else(|| EncodeError::Adapter("frame rate is out of range".into()))?;
    // An unsupported bitrate is refused before anything opens; FFmpeg would refuse it only at open,
    // with a generic error.
    let bitrate_mode = if rate_control.mode == BitrateMode::Constant {
        "cbr"
    } else {
        "vbr"
    };
    let bitrate_refused = modes.refusal(bitrate_mode);
    let frames = d3d12va_encode_frames(device, coded_width, coded_height)?;
    let open = |quantized: Option<&crate::rate_control::MechanismRequest>| {
        let mut context = AVCodecContext::new(&codec);
        context.set_width(coded_width);
        context.set_height(coded_height);
        context.set_pix_fmt(ffi::AV_PIX_FMT_D3D12);
        // `ff_encode_preinit` copies the pool's `sw_format` into `sw_pix_fmt`, and rejects the open
        // if a caller set a different one.
        context.set_time_base(ra(frame_rate.1, frame_rate.0));
        context.set_framerate(ra(frame_rate.0, frame_rate.1));
        // Every mode is set explicitly: with `rc_mode=auto` FFmpeg takes the device's first
        // supported mode, which can be a constant QP that ignores the bitrate.
        let bitrate = crate::rate_control::mechanism_request(
            name,
            if rate_control.mode == BitrateMode::Constant {
                RateControlMechanism::ConstantBitrate
            } else {
                RateControlMechanism::VariableBitrate
            },
            rate_control.bitrate,
            0,
        )
        .ok_or_else(|| EncodeError::Adapter(format!("{name} has no known bitrate option")))?;
        let request = quantized.unwrap_or(&bitrate);
        context.set_bit_rate(request.bit_rate);
        context.set_gop_size(crate::capabilities::keyframe_interval(rate));
        // No B pictures, as on Vulkan where two cost 0.34-0.63 dB PSNR-Y at a fixed rate; FFmpeg's
        // D3D12VA default is `bf=2`. The D3D12 cost is not measured yet.
        context.set_max_b_frames(0);
        context.set_hw_frames_ctx(frames.clone());
        // FFmpeg 8.1's D3D12VA encoders ignore this flag and send parameter sets in-band, which the
        // bitstream filters move into the MP4 header; set anyway for an FFmpeg that honours it.
        context.set_flags(
            context.flags | i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).unwrap_or(0),
        );
        // The profile the device was asked about, not the encoder's default. The colour options tag
        // the context (read by `colr`); the encoder writes no VUI, so the bitstream filters add it.
        let options = crate::convert::color_options(color).set(c"profile", encoder.profile, 0);
        let unused =
            alpha2_ffmpeg_bridge::open_codec_context(&mut context, request.dictionary(options))
                .map_err(|error| {
                    EncodeError::Adapter(format!(
                        "{name} session could not open: {error}{}",
                        match quantized {
                            Some(_) => "",
                            None => crate::rate_control::constant_rate_hint(name, rate_control),
                        }
                    ))
                })?;
        crate::convert::reject_unused_encode_options(unused)?;
        Ok::<_, EncodeError>(context)
    };
    let open_bitrate = || match &bitrate_refused {
        Some(reason) => Err(EncodeError::Adapter(reason.clone())),
        None => open(None),
    };
    // A refused quantizer falls back to the rate control's own bitrate on the same encoder, and the
    // selection says which one runs. A device without a constant QP refuses it before it opens.
    let (context, applied, quantizer_refused) =
        match crate::rate_control::quantizer_request(name, rate_control) {
            Some(Ok(quantized)) => match modes.refusal("disabled") {
                Some(reason) => (
                    open_bitrate()?,
                    crate::AppliedRateControl::Bitrate,
                    Some(format!("{} refused: {reason}", quantized.describe())),
                ),
                None => match open(Some(&quantized)) {
                    Ok(context) => (context, crate::AppliedRateControl::Quantizer, None),
                    Err(error) => (
                        open_bitrate()?,
                        crate::AppliedRateControl::Bitrate,
                        Some(format!("{} refused: {error}", quantized.describe())),
                    ),
                },
            },
            Some(Err(reason)) => (
                open_bitrate()?,
                crate::AppliedRateControl::Bitrate,
                Some(reason),
            ),
            None => (open_bitrate()?, crate::AppliedRateControl::Bitrate, None),
        };
    if let Some(reason) = &quantizer_refused {
        alpha2_os::diagnostic!("alpha2 native export: {name} runs the bitrate fallback: {reason}");
    }
    // No candidate list: this encoder is chosen from the device, and anything that could refuse it
    // already has.
    let selection = crate::NativeVideoEncoderSelection {
        encoder_name: name.to_owned(),
        class: crate::NativeEncoderClass::Hardware,
        hardware_accelerated: true,
        rejected: Vec::new(),
        rate_control: applied,
        quantizer_refused,
        software_threads: None,
        pixels: alpha2_media::PixelLayout::YUV420P,
    };
    Ok((frames, context, selection))
}

/// The encode worker's half of a D3D12VA session: the codec, the bitstream
/// filters, and the muxer.
pub(super) struct D3D12EncodeSession {
    context: AVCodecContext,
    time_base: AVRational,
    bitstream: EncodedVideoFilter,
    muxer: Muxer,
    stages: EncodeStageTimings,
    selection: crate::NativeVideoEncoderSelection,
    generation: u64,
    /// The device, asked whether it was removed when a call fails.
    shared: SharedD3D12Context,
    size: (u32, u32),
    /// Frames sent to the encoder so far.
    sent: u64,
    /// The input half's pool, as a failure message reports it.
    pool: Arc<PoolView>,
}

/// What the input half's pool has done, for the encode half's failure messages: the two halves
/// run on different threads.
#[derive(Default)]
struct PoolView {
    acquired: AtomicU64,
    allocated: AtomicU64,
    /// The copier slot of the last frame copied.
    slot: AtomicU64,
}

/// The coordinator's half of a D3D12VA session.
///
/// The codec moves to an encode worker; the copy stays with the thread that
/// rendered the planes, because the wgpu plane textures and the queue order
/// that puts the copy after their render belong to it. This is what stays
/// behind: the copier and its plane ring, and a reference to the
/// encoder-input pool, which is reference counted, so a frame taken here
/// stays valid for as long as the encoder needs it even though the two
/// halves are on different threads.
///
/// Fields drop in declaration order, and the copier comes first: dropping it
/// waits on every in-flight copy, retaining the textures, encoder frames, and
/// device together if that wait times out, since wgpu cannot track these raw
/// copies. Dropping the planes first would free them under a running copy.
pub(super) struct D3D12EncodeInput {
    copier: alpha2_native_gpu::D3D12EncodeCopier,
    planes: Vec<Nv12PlanePair>,
    frames: AVHWFramesContext,
    size: (u32, u32),
    time_base: AVRational,
    color: ColorMetadata,
    pool: Arc<PoolView>,
    stats: EncoderPoolStats,
    /// The distinct textures the pool has handed out, which is how many it
    /// allocated: it recycles a texture only once its last reference drops.
    textures: HashSet<u64>,
    /// The thread that renders the planes: the raw copy must reach the queue
    /// after the render that packed them, which only that thread orders.
    #[cfg(debug_assertions)]
    thread: std::thread::ThreadId,
}

/// One frame taken from a D3D12VA encoder-input pool, and whether a copy
/// into it has been submitted.
pub(super) struct D3D12EncoderFrame {
    frame: AVFrame,
    /// Until a copy is fenced on it, the texture holds whatever the pool left,
    /// and its fence value says nothing about the planes.
    packed: bool,
}

impl D3D12EncoderFrame {
    pub(super) fn set_pts(&mut self, pts: i64) {
        self.frame.set_pts(pts);
    }
}

impl D3D12EncodeInput {
    pub(super) fn descriptor(&self, generation: u64) -> EncoderSurfaceDescriptor {
        EncoderSurfaceDescriptor {
            backend: HardwareEncodeBackend::D3D12Va,
            pool_generation: generation,
            layout: EncoderInputLayout::Nv12,
            visible: self.size,
            color: self.color,
        }
    }

    /// Takes one frame from the encoder-input pool.
    ///
    /// `FFmpeg`'s pool recycles the texture behind a frame once the last
    /// reference to it is dropped: the host's, the encoder's, and the
    /// copier's, which keeps each frame it copied into until its slot comes
    /// round again.
    pub(super) fn acquire_frame(&mut self) -> Result<D3D12EncoderFrame, EncodeError> {
        let mut frame = AVFrame::new();
        self.frames
            .get_buffer(&mut frame)
            .map_err(encode_adapter_error)?;
        let texture = alpha2_ffmpeg_bridge::d3d12_frame_info(&frame)
            .map_err(encode_adapter_error)?
            .resource;
        self.textures.insert(texture);
        self.stats.acquired = self.stats.acquired.saturating_add(1);
        self.stats.allocated = u64::try_from(self.textures.len()).unwrap_or(u64::MAX);
        self.pool
            .acquired
            .store(self.stats.acquired, Ordering::Relaxed);
        self.pool
            .allocated
            .store(self.stats.allocated, Ordering::Relaxed);
        Ok(D3D12EncoderFrame {
            frame,
            packed: false,
        })
    }

    /// Waits until the plane pair the next copy reads is free. The render that
    /// packs the planes is submitted before [`Self::submit`]'s copy, on the
    /// same queue, which is all that orders the two.
    pub(super) fn begin_frame(&mut self) -> Result<InputFrame, EncodeError> {
        let slot = self.copier.await_slot().map_err(encode_adapter_error)?;
        if slot >= self.planes.len() {
            return Err(EncodeError::Adapter(format!(
                "encode copier reported slot {slot}"
            )));
        }
        Ok(InputFrame { slot })
    }

    pub(super) fn planes(&self, frame: &InputFrame) -> (&wgpu::Texture, &wgpu::Texture) {
        let pair = &self.planes[frame.slot];
        (&pair.luma, &pair.chroma)
    }

    /// Copies `frame`'s planes into `backing`. The copy signals the frame's
    /// own fence and records the value on the frame, so the encoder waits for
    /// it on the GPU and this call does not have to.
    // Taking the token is the contract: a begun frame is submitted once.
    #[allow(clippy::needless_pass_by_value)]
    pub(super) fn submit(
        &mut self,
        frame: InputFrame,
        backing: &mut D3D12EncoderFrame,
    ) -> Result<(), EncodeError> {
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            std::thread::current().id(),
            self.thread,
            "the D3D12 encode copy must be submitted by the thread that rendered its planes"
        );
        let InputFrame { slot } = frame;
        self.pool
            .slot
            .store(u64::try_from(slot).unwrap_or(u64::MAX), Ordering::Relaxed);
        let pair = &self.planes[slot];
        // `sync_ctx.fence_value` is written only once the copy is queued and fenced, so the encoder
        // waits for exactly the value this copy signals.
        let mut access = alpha2_ffmpeg_bridge::D3D12EncodeFrameAccess::begin(&mut backing.frame)
            .map_err(encode_adapter_error)?;
        self.copier
            .submit(&mut access, &pair.luma, &pair.chroma, self.size)
            .map_err(encode_adapter_error)?;
        backing.packed = true;
        self.stats.copy_operations = self.stats.copy_operations.saturating_add(2);
        self.stats.copy_bytes = self
            .stats
            .copy_bytes
            .saturating_add(EncoderInputLayout::Nv12.frame_bytes(self.size.0, self.size.1));
        Ok(())
    }

    /// Presentation timestamp for the frame at `time`, in encoder ticks.
    pub(super) fn timestamp(&self, time: MediaTime) -> i64 {
        av_rescale_q(time.ticks(), tick_base(), self.time_base)
    }

    pub(super) const fn stats(&self) -> EncoderPoolStats {
        self.stats
    }

    /// Waits on every copy's fence before the plane textures it reads drop.
    pub(super) fn finish(self) -> Result<(), EncodeError> {
        let Self { copier, planes, .. } = self;
        copier.finish().map_err(encode_adapter_error)?;
        drop(planes);
        Ok(())
    }
}

impl D3D12EncodeSession {
    pub(super) const fn selection(&self) -> &crate::NativeVideoEncoderSelection {
        &self.selection
    }

    pub(super) fn stage_timings(&self) -> EncodeStageTimings {
        self.muxer.with_audio_timings(self.stages)
    }

    /// Submits `frame` and muxes whatever packets it produced.
    pub(super) fn encode(
        &mut self,
        generation: u64,
        frame: &D3D12EncoderFrame,
    ) -> Result<(), EncodeError> {
        if generation != self.generation {
            return Err(EncodeError::Adapter(format!(
                "encoder surface from pool {generation} reached session {}",
                self.generation
            )));
        }
        // A surface nothing was copied into would encode whatever the pool left in it.
        if !frame.packed {
            return Err(EncodeError::Adapter(
                "an encoder surface reached the encoder unpacked".into(),
            ));
        }
        let mark = alpha2_ffmpeg_bridge::log_mark();
        let index = self.sent;
        let sent = timed(&mut self.stages.video_send_frame, || {
            #[cfg(test)]
            if tests::send_fails_at(index) {
                return Err(RsmpegError::AVError(-22));
            }
            self.context.send_frame(Some(&frame.frame))
        });
        self.sent += 1;
        if let Err(error) = sent {
            return Err(self.call_failed("send_frame", index, &error, Some(frame), mark));
        }
        self.drain()
    }

    /// The error for `call` failing with `error` on frame `index`: a lost device when the device
    /// says it was removed, else a frame the encoder rejected on a device still usable, with what
    /// the session and `FFmpeg` said.
    fn call_failed(
        &self,
        call: &str,
        index: u64,
        error: &RsmpegError,
        frame: Option<&D3D12EncoderFrame>,
        mark: alpha2_ffmpeg_bridge::LogMark,
    ) -> EncodeError {
        let encoder = self.selection.encoder_name.clone();
        let adapter = self.shared.adapter_label();
        if let Some(reason) = self.shared.device_removed_reason() {
            return EncodeError::Gpu {
                fault: alpha2_media::GpuFault::DeviceLost,
                message: format!(
                    "{encoder} {call} on {adapter} failed ({error}): the device was removed ({})",
                    alpha2_native_gpu::hresult_text(reason)
                ),
            };
        }
        let fence = frame.map_or_else(
            || "none".to_owned(),
            |frame| {
                alpha2_ffmpeg_bridge::d3d12_frame_fence_value(&frame.frame).map_or_else(
                    |error| format!("unreadable ({error})"),
                    |value| value.to_string(),
                )
            },
        );
        let lines = alpha2_ffmpeg_bridge::log_since(mark, &[encoder.as_str(), "d3d12va"]);
        EncodeError::EncoderRejected {
            message: format!(
                "{encoder} rejected frame {index} ({}x{}) on {adapter} at {call}: {error}; pool \
                 allocated {}, acquired {}, copier slot {}, frame fence {fence}{}",
                self.size.0,
                self.size.1,
                self.pool.allocated.load(Ordering::Relaxed),
                self.pool.acquired.load(Ordering::Relaxed),
                self.pool.slot.load(Ordering::Relaxed),
                alpha2_ffmpeg_bridge::log_detail(&lines),
            ),
            encoder,
            frame: index,
        }
    }

    pub(super) fn write_audio(
        &mut self,
        block: &alpha2_media::AudioBlock,
    ) -> Result<(), EncodeError> {
        self.muxer.write_audio(block, &mut self.stages)
    }

    /// Flushes the audio, the encoder and the bitstream filters, then closes
    /// the output. Flushing and closing are one step because only a closed
    /// output renames the partial file into place.
    pub(super) fn finish(&mut self, cancel: &alpha2_media::CancelToken) -> Result<(), EncodeError> {
        // Every audio packet must exist before the muxer decides the final interleave.
        self.muxer.finish_audio()?;
        crate::encode::beat_encode_call();
        self.context
            .send_frame(None)
            .map_err(encode_adapter_error)?;
        self.drain()?;
        let tail = self.bitstream.flush()?;
        self.write_video(tail)?;
        self.muxer.write_ready_audio(true, &mut self.stages)?;
        self.muxer.close(cancel)
    }

    pub(super) fn publish_report(&self) -> Option<crate::PublishReport> {
        self.muxer.publish_report()
    }

    pub(super) fn abort(&mut self) {
        self.muxer.abort();
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        loop {
            let mark = alpha2_ffmpeg_bridge::log_mark();
            let received = timed(&mut self.stages.video_receive_packet, || {
                self.context.receive_packet()
            });
            match received {
                Ok(packet) => {
                    let filtered = self.bitstream.filter(packet)?;
                    self.write_video(filtered)?;
                }
                Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => {
                    return self.muxer.write_ready_audio(false, &mut self.stages);
                }
                Err(error) => {
                    let last = self.sent.saturating_sub(1);
                    return Err(self.call_failed("receive_packet", last, &error, None, mark));
                }
            }
        }
    }

    /// Muxes packets out of the bitstream filters, which keep the encoder's
    /// time base.
    fn write_video(&mut self, packets: Vec<AVPacket>) -> Result<(), EncodeError> {
        for mut packet in packets {
            self.muxer.write_video(&mut packet, self.time_base)?;
        }
        Ok(())
    }
}

/// Builds the encoder-input pool: one NV12 texture per frame, of exactly the
/// output size, allocated on demand.
///
/// `avcodec_get_hw_frames_parameters` is a decode-side API and answers
/// `ENOENT` for an encoder, so the pool is described here. Each frame is a
/// texture of its own, with its own fence, never a slice of a texture array:
/// the copy writes the frame's planes as subresources 0 and 1 of its
/// texture, and signals the frame's fence for the encoder to wait on.
fn d3d12va_encode_frames(
    device: &AVHWDeviceContext,
    width: i32,
    height: i32,
) -> Result<AVHWFramesContext, EncodeError> {
    let mut frames = device.hwframe_ctx_alloc();
    {
        let data = frames.data();
        data.format = ffi::AV_PIX_FMT_D3D12;
        data.sw_format = ffi::AV_PIX_FMT_NV12;
        data.width = width;
        data.height = height;
        data.initial_pool_size = 0;
    }
    // `D3D12_RESOURCE_FLAG_NONE`: wgpu cannot render into NV12, so the texture is only a copy
    // destination and the encoder's input.
    alpha2_ffmpeg_bridge::d3d12_frames_configure(
        &mut frames,
        alpha2_ffmpeg_bridge::DXGI_FORMAT_NV12,
        0,
    )
    .map_err(encode_adapter_error)?;
    frames.init().map_err(encode_adapter_error)?;
    Ok(frames)
}

#[cfg(test)]
#[path = "d3d12va_tests.rs"]
mod tests;
