//! Video and audio encoding through `MediaCodec`, with byte-buffer input.
//!
//! The export loop hands over planar 4:2:0 or P010 planes; video input buffers take NV12
//! (`COLOR_FormatYUV420SemiPlanar`) or P010, so chroma is interleaved here. The encoders give
//! parameter sets as Annex B codec-config buffers, rebuilt into the `avcC` or `hvcC` record MP4
//! stores, and frames as Annex B, rewritten with NAL lengths.

use std::collections::HashMap;

use alpha2_host_core::encode::{
    AudioEncodeBackend, AudioEncodeConfig, AudioEncoders, EncodePoll, EncodedPacket, PlaneBuffer,
    VideoEncodeBackend, VideoEncodeConfig, VideoEncoders,
};
use alpha2_host_core::export_policy::{AvcProfile, avc_level};
use alpha2_host_core::parameter_sets::{
    avc_decoder_config, hevc_decoder_config, length_prefixed_units,
};
use alpha2_host_core::rate_control::AppliedRateControl;
use alpha2_host_core::video_stream::{BackendError, Submit};
use alpha2_media::{
    AudioCodec, BitDepth, BitrateMode, Chroma, ColorMatrix, ColorPrimaries, ColorRange,
    EncodeError, EncoderRefusal, OpenFailureKind, PixelLayout, PlaneLayout, TransferFunction,
    VideoCodec,
};
use bytes::Bytes;

use crate::codec::{
    Codec, FLAG_CODEC_CONFIG, FLAG_END_OF_STREAM, Format, Output, api_level, from_codec_us,
    to_codec_us,
};

/// `MediaCodec.BUFFER_FLAG_KEY_FRAME`.
const FLAG_KEY_FRAME: u32 = 1;

/// `COLOR_FormatYUV420SemiPlanar` (NV12) and `COLOR_FormatYUVP010`.
const COLOR_NV12: i32 = 21;
const COLOR_P010: i32 = 54;

/// `AVCProfileBaseline`, `AVCProfileMain`, `AVCProfileHigh`, `HEVCProfileMain` and
/// `HEVCProfileMain10`.
const AVC_PROFILE_BASELINE: i32 = 1;
const AVC_PROFILE_MAIN: i32 = 2;
const AVC_PROFILE_HIGH: i32 = 8;
const HEVC_PROFILE_MAIN: i32 = 1;
const HEVC_PROFILE_MAIN10: i32 = 2;

/// `MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR` and `BITRATE_MODE_CBR`.
const BITRATE_MODE_VBR: i32 = 1;
const BITRATE_MODE_CBR: i32 = 2;

/// `AudioFormat.ENCODING_PCM_16BIT`.
const PCM_16: i32 = 2;

/// `MediaCodecInfo.CodecProfileLevel.AACObjectLC`.
const AAC_LC: i32 = 2;

/// Samples of priming the platform AAC encoder puts before the input, by round-trip measurement
/// at 44.1 and 48 kHz; the encoder reports no delay itself.
const AAC_PRIMING: u32 = 2048;

const WAIT_US: i64 = 10_000;

fn adapter(error: BackendError) -> EncodeError {
    match error {
        BackendError::SessionLost(reason)
        | BackendError::Unsupported(reason)
        | BackendError::Corrupt(reason)
        | BackendError::HardwarePicture(reason)
        | BackendError::Failed(reason) => EncodeError::Adapter(reason),
    }
}

fn refusal(encoder: &str, hardware: bool, kind: OpenFailureKind, reason: String) -> EncoderRefusal {
    EncoderRefusal {
        encoder: encoder.to_owned(),
        hardware_accelerated: hardware,
        kind,
        reason,
    }
}

fn refused(what: &str, refusals: Vec<EncoderRefusal>) -> EncodeError {
    let message = if refusals.is_empty() {
        format!("no Android encoder for {what}")
    } else {
        refusals
            .iter()
            .map(|refusal| format!("{}: {}", refusal.encoder, refusal.reason))
            .collect::<Vec<_>>()
            .join("; ")
    };
    EncodeError::EncodersRefused { message, refusals }
}

/// The platform's software encoder for a video codec.
const fn software_video_encoder(codec: VideoCodec) -> Option<&'static str> {
    match codec {
        VideoCodec::H264 => Some("c2.android.avc.encoder"),
        VideoCodec::Hevc => Some("c2.android.hevc.encoder"),
        VideoCodec::Vp9 => Some("c2.android.vp9.encoder"),
        VideoCodec::Av1 => Some("c2.android.av1.encoder"),
        _ => None,
    }
}

/// The color tags (`color-standard`, `color-transfer`, `color-range`) of `color`.
fn color_keys(format: &mut Format, color: alpha2_media::ColorMetadata) {
    let standard = match (color.primaries, color.matrix) {
        (ColorPrimaries::Bt2020, _) | (_, ColorMatrix::Bt2020Ncl) => 6,
        (ColorPrimaries::Bt601, _) | (_, ColorMatrix::Bt601) => 4,
        _ => 1,
    };
    let transfer = match color.transfer {
        TransferFunction::Pq => 6,
        TransferFunction::Hlg => 7,
        _ => 3,
    };
    let range = if color.range == ColorRange::Full {
        1
    } else {
        2
    };
    format.set_i32("color-standard", standard);
    format.set_i32("color-transfer", transfer);
    format.set_i32("color-range", range);
}

/// The track timescale for `rate`: its numerator, doubled up to at least 10000 so edits and
/// rates are fine-grained, as `FFmpeg`'s MOV muxer does.
fn timescale(rate: alpha2_time::FrameRate) -> u32 {
    let mut units = rate.numerator().max(1);
    while units < 10_000 {
        units *= 2;
    }
    units
}

/// How frames are laid into input buffers: the started encoder's row stride and slice height,
/// and the picture.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    stride: usize,
    slice_height: usize,
    width: usize,
    height: usize,
    layout: PixelLayout,
}

impl Geometry {
    /// Copies `planes` into `buffer` as NV12 or P010; returns the bytes written.
    fn fill(&self, buffer: &mut [u8], planes: &[PlaneBuffer]) -> Result<usize, BackendError> {
        let Self {
            stride,
            slice_height,
            width,
            height,
            layout,
        } = *self;
        let short = || BackendError::Failed("an input frame is smaller than its layout".into());
        let chroma_rows = height.div_ceil(2);
        let total = stride * slice_height + stride * chroma_rows;
        if buffer.len() < total || planes.len() < 2 {
            return Err(BackendError::Failed(format!(
                "a {total} byte frame does not fit a {} byte input buffer",
                buffer.len()
            )));
        }
        let sample = if layout.bit_depth == BitDepth::Ten {
            2
        } else {
            1
        };
        let luma = &planes[0];
        for row in 0..height {
            let from = luma
                .bytes
                .get(row * luma.stride..row * luma.stride + width * sample)
                .ok_or_else(short)?;
            buffer[row * stride..row * stride + width * sample].copy_from_slice(from);
        }
        let chroma_at = stride * slice_height;
        if layout.planes == PlaneLayout::P010 {
            let chroma = &planes[1];
            let row_bytes = width.div_ceil(2) * 4;
            for row in 0..chroma_rows {
                let from = chroma
                    .bytes
                    .get(row * chroma.stride..row * chroma.stride + row_bytes)
                    .ok_or_else(short)?;
                let at = chroma_at + row * stride;
                buffer[at..at + row_bytes].copy_from_slice(from);
            }
        } else {
            let (u, v) = (&planes[1], planes.get(2).ok_or_else(short)?);
            let columns = width.div_ceil(2);
            for row in 0..chroma_rows {
                let at = chroma_at + row * stride;
                let u_row = u
                    .bytes
                    .get(row * u.stride..row * u.stride + columns)
                    .ok_or_else(short)?;
                let v_row = v
                    .bytes
                    .get(row * v.stride..row * v.stride + columns)
                    .ok_or_else(short)?;
                for (pair, (&cb, &cr)) in buffer[at..at + columns * 2]
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(u_row.iter().zip(v_row))
                {
                    pair[0] = cb;
                    pair[1] = cr;
                }
            }
        }
        Ok(total)
    }
}

/// A `MediaCodec` video encoder session.
pub struct MediaCodecVideoEncoder {
    codec: Codec,
    hardware: bool,
    timescale: u32,
    kind: VideoCodec,
    geometry: Geometry,
    record: Option<Bytes>,
    times: HashMap<i64, i64>,
    frames_in: u64,
    eos_pending: bool,
    drained: bool,
}

impl std::fmt::Debug for MediaCodecVideoEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaCodecVideoEncoder")
            .field("codec", &self.codec.name())
            .finish_non_exhaustive()
    }
}

impl MediaCodecVideoEncoder {
    /// A session for `config`: the hardware encoder, then the platform's software one, unless
    /// `config.software` asks for software alone.
    ///
    /// # Errors
    /// [`EncodeError::EncodersRefused`] naming every encoder tried.
    pub fn open(config: &VideoEncodeConfig) -> Result<Self, EncodeError> {
        let mime = match config.codec {
            VideoCodec::H264 => "video/avc",
            VideoCodec::Hevc => "video/hevc",
            VideoCodec::Vp9 => "video/x-vnd.on2.vp9",
            VideoCodec::Av1 => "video/av01",
            other => {
                return Err(refused(other.as_str(), Vec::new()));
            }
        };
        let layout = config.layout;
        let ten_bit = layout.bit_depth == BitDepth::Ten;
        let suitable = layout.chroma == Chroma::Yuv420
            && !layout.alpha
            && match (layout.bit_depth, layout.planes) {
                (BitDepth::Eight, PlaneLayout::Planar) => true,
                (BitDepth::Ten, PlaneLayout::P010) => config.codec == VideoCodec::Hevc,
                _ => false,
            };
        if !suitable {
            return Err(refused(
                mime,
                vec![refusal(
                    mime,
                    false,
                    OpenFailureKind::Unsuitable,
                    format!(
                        "Android encoders take 8-bit 4:2:0, or 10-bit P010 for HEVC, not {layout:?}"
                    ),
                )],
            ));
        }
        if ten_bit && api_level() < 33 {
            return Err(refused(
                mime,
                vec![refusal(
                    mime,
                    false,
                    OpenFailureKind::Absent,
                    "10-bit input needs Android 13 for P010".into(),
                )],
            ));
        }
        let mut refusals = Vec::new();
        for (first, hardware) in video_candidates(config, mime, &mut refusals) {
            let name = first.name().to_owned();
            let mut fresh = Some(first);
            for profile in video_profiles(config) {
                // A refused configuration can leave the codec unusable: each try gets its own.
                let mut codec = match fresh.take().map_or_else(|| Codec::by_name(&name), Ok) {
                    Ok(codec) => codec,
                    Err(error) => {
                        refusals.push(refusal(
                            &name,
                            hardware,
                            OpenFailureKind::Unknown,
                            error.to_string(),
                        ));
                        break;
                    }
                };
                let format = video_format(mime, config, profile);
                if let Err(error) = codec.start(&format, true) {
                    refusals.push(refusal(
                        codec.name(),
                        hardware,
                        OpenFailureKind::Unsuitable,
                        error.to_string(),
                    ));
                    continue;
                }
                return Ok(Self::started(codec, hardware, config));
            }
        }
        Err(refused(mime, refusals))
    }

    /// The session of `codec`, started for `config`, laid out as its input format says.
    fn started(codec: Codec, hardware: bool, config: &VideoEncodeConfig) -> Self {
        let input = codec.input_format();
        let positive = |key: &str| {
            input
                .i32(key)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0)
        };
        let width = config.width as usize;
        let height = config.height as usize;
        let sample = if config.layout.bit_depth == BitDepth::Ten {
            2
        } else {
            1
        };
        Self {
            codec,
            hardware,
            timescale: timescale(config.frame_rate),
            kind: config.codec,
            geometry: Geometry {
                stride: positive("stride")
                    .unwrap_or(width * sample)
                    .max(width * sample),
                slice_height: positive("slice-height").unwrap_or(height).max(height),
                width,
                height,
                layout: config.layout,
            },
            // VP9 has no configuration record; Matroska and MP4 describe it from the frames.
            record: (config.codec == VideoCodec::Vp9).then(Bytes::new),
            times: HashMap::new(),
            frames_in: 0,
            eos_pending: false,
            drained: false,
        }
    }

    fn try_queue_eos(&mut self) -> Result<(), BackendError> {
        if let Some(index) = self.codec.dequeue_input(0)? {
            self.codec.queue_input(index, &[], 0, FLAG_END_OF_STREAM)?;
            self.eos_pending = false;
        }
        Ok(())
    }

    /// Keeps the configuration record the Annex B parameter sets in `bytes` make.
    fn configure(&mut self, bytes: &[u8]) {
        let record = match self.kind {
            VideoCodec::H264 => avc_decoder_config(bytes),
            VideoCodec::Hevc => hevc_decoder_config(bytes),
            // The encoder's configuration is the `av1C` record, or the sequence header it holds.
            VideoCodec::Av1 => Some(if bytes.first() == Some(&0x81) {
                bytes.to_vec()
            } else {
                av1_config_record(bytes)
            }),
            _ => None,
        };
        if let Some(record) = record {
            self.record = Some(Bytes::from(record));
        }
    }
}

/// The `av1C` record (AV1-ISOBMFF 2.3) of an encoder's sequence header OBU, for the 8-bit 4:2:0
/// profile 0 streams these encoders take: version 1, the level left at 31 (unconstrained), then
/// the sequence header as the configuration OBUs.
fn av1_config_record(sequence_header: &[u8]) -> Vec<u8> {
    // Marker and version; profile 0 and level 31; main tier, 8 bits, colour, chroma subsampled
    // in both directions at an unknown position; no initial presentation delay.
    let mut record = vec![0x81, 31, 0b0000_1100, 0];
    record.extend_from_slice(sequence_header);
    record
}

/// The encoders to try for `config`, hardware first; refusals of missing ones go to `refusals`.
fn video_candidates(
    config: &VideoEncodeConfig,
    mime: &str,
    refusals: &mut Vec<EncoderRefusal>,
) -> Vec<(Codec, bool)> {
    let mut candidates = Vec::new();
    if !config.software {
        match Codec::encoder_for(mime) {
            Ok(codec) if !codec.is_software() => candidates.push((codec, true)),
            Ok(_) => {}
            Err(error) => refusals.push(refusal(
                mime,
                true,
                OpenFailureKind::Absent,
                error.to_string(),
            )),
        }
    }
    // The platform's Codec2 software encoder, else its older OMX one (Android 10 images).
    let software = software_video_encoder(config.codec)
        .and_then(|name| Codec::by_name(name).ok())
        .or_else(|| Codec::encoder_for(mime).ok().filter(Codec::is_software));
    match software {
        Some(codec) => candidates.push((codec, false)),
        None => refusals.push(refusal(
            mime,
            false,
            OpenFailureKind::Absent,
            "no software encoder".into(),
        )),
    }
    candidates
}

/// The `profile` and `level` values to try for `config`, most capable first: the H.264 ladder
/// down to Constrained Baseline, or HEVC's profile; then neither, for encoders that refuse every
/// profile they are told (some older software encoders take a profile only with a level).
fn video_profiles(config: &VideoEncodeConfig) -> Vec<Option<(i32, Option<i32>)>> {
    let mut profiles = match config.codec {
        VideoCodec::H264 => {
            let level = avc_level_constant(avc_level(
                config.width,
                config.height,
                config.frame_rate,
                config.rate.bitrate,
            ));
            AvcProfile::LADDER
                .iter()
                .map(|profile| {
                    let profile = match profile {
                        AvcProfile::High => AVC_PROFILE_HIGH,
                        AvcProfile::Main => AVC_PROFILE_MAIN,
                        AvcProfile::ConstrainedBaseline => AVC_PROFILE_BASELINE,
                    };
                    Some((profile, level))
                })
                .collect()
        }
        VideoCodec::Hevc if config.layout.bit_depth == BitDepth::Ten => {
            vec![Some((HEVC_PROFILE_MAIN10, None))]
        }
        VideoCodec::Hevc => vec![Some((HEVC_PROFILE_MAIN, None))],
        // VP9 and AV1 run at the encoder's own profile for the input.
        _ => Vec::new(),
    };
    profiles.push(None);
    profiles
}

/// `MediaCodecInfo.CodecProfileLevel`'s `AVCLevel*` constant of `level_idc`.
fn avc_level_constant(level: u8) -> Option<i32> {
    const LEVELS: [u8; 20] = [
        10, 9, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52, 60, 61, 62,
    ];
    LEVELS
        .iter()
        .position(|known| *known == level)
        .map(|bit| 1 << bit)
}

fn video_format(
    mime: &str,
    config: &VideoEncodeConfig,
    profile: Option<(i32, Option<i32>)>,
) -> Format {
    let mut format = Format::new();
    format.set_str("mime", mime);
    format.set_i32("width", i32::try_from(config.width).unwrap_or(i32::MAX));
    format.set_i32("height", i32::try_from(config.height).unwrap_or(i32::MAX));
    let ten_bit = config.layout.bit_depth == BitDepth::Ten;
    format.set_i32(
        "color-format",
        if ten_bit { COLOR_P010 } else { COLOR_NV12 },
    );
    format.set_i32(
        "bitrate",
        i32::try_from(config.rate.bitrate).unwrap_or(i32::MAX),
    );
    format.set_i32(
        "bitrate-mode",
        match config.rate.mode {
            BitrateMode::Variable => BITRATE_MODE_VBR,
            BitrateMode::Constant => BITRATE_MODE_CBR,
        },
    );
    let rate = config.frame_rate;
    let fps = f64::from(rate.numerator()) / f64::from(rate.denominator().max(1));
    #[allow(clippy::cast_possible_truncation)]
    {
        format.set_f32("frame-rate", fps as f32);
        format.set_f32(
            "i-frame-interval",
            (f64::from(config.keyframe_interval.max(1)) / fps) as f32,
        );
    }
    format.set_i32("max-bframes", 0);
    if let Some((profile, level)) = profile {
        format.set_i32("profile", profile);
        if let Some(level) = level {
            format.set_i32("level", level);
        }
    }
    color_keys(&mut format, config.color);
    format
}

impl VideoEncodeBackend for MediaCodecVideoEncoder {
    fn name(&self) -> &str {
        self.codec.name()
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
        let Some(index) = self.codec.dequeue_input(0).map_err(adapter)? else {
            return Ok(Submit::Busy);
        };
        if keyframe && self.frames_in > 0 {
            let mut request = Format::new();
            request.set_i32("request-sync", 0);
            self.codec.set_parameters(&request).map_err(adapter)?;
        }
        let pts_us = to_codec_us(pts, i64::from(self.timescale));
        let geometry = self.geometry;
        self.codec
            .write_input(index, pts_us, 0, |buffer| geometry.fill(buffer, planes))
            .map_err(adapter)?;
        self.times.insert(pts_us, pts);
        self.frames_in += 1;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError> {
        if self.drained {
            return Ok(EncodePoll::Drained);
        }
        if self.eos_pending {
            self.try_queue_eos().map_err(adapter)?;
        }
        loop {
            match self
                .codec
                .dequeue_output(if wait { WAIT_US } else { 0 })
                .map_err(adapter)?
            {
                Output::TryAgain => return Ok(EncodePoll::Pending),
                Output::FormatChanged => {
                    // Some encoders publish the parameter sets only in the output format.
                    if self.record.is_none() {
                        let format = self.codec.output_format();
                        let mut sets = format.buffer("csd-0").unwrap_or_default();
                        sets.extend(format.buffer("csd-1").unwrap_or_default());
                        self.configure(&sets);
                    }
                }
                Output::Buffer {
                    index,
                    offset,
                    size,
                    pts_us,
                    flags,
                } => {
                    let bytes = self
                        .codec
                        .output_bytes(index, offset, size)
                        .map(<[u8]>::to_vec);
                    self.codec.release_output(index).map_err(adapter)?;
                    let end = flags & FLAG_END_OF_STREAM != 0;
                    self.drained = end;
                    match bytes {
                        Some(bytes) if flags & FLAG_CODEC_CONFIG != 0 => self.configure(&bytes),
                        Some(bytes) if !bytes.is_empty() => {
                            let pts = self.times.remove(&pts_us).unwrap_or_else(|| {
                                from_codec_us(pts_us, i64::from(self.timescale))
                            });
                            return Ok(EncodePoll::Packet(EncodedPacket {
                                pts,
                                dts: pts,
                                keyframe: flags & FLAG_KEY_FRAME != 0,
                                data: match self.kind {
                                    VideoCodec::H264 | VideoCodec::Hevc => {
                                        Bytes::from(length_prefixed_units(&bytes))
                                    }
                                    _ => Bytes::from(bytes),
                                },
                            }));
                        }
                        _ => {}
                    }
                    if end {
                        return Ok(EncodePoll::Drained);
                    }
                }
            }
        }
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        self.eos_pending = true;
        self.try_queue_eos().map_err(adapter)
    }

    fn codec_private(&self) -> Option<Bytes> {
        self.record.clone()
    }

    fn applied_rate_control(&self) -> AppliedRateControl {
        // No constant quality: the platform does not say whether an encoder applies it.
        AppliedRateControl::Bitrate
    }
}

/// The `OpusHead` and pre-skip in the Opus encoder's codec-config blob: chunks of an 8-byte
/// marker, a little-endian 64-bit length and the payload (`AOPUSHDR`, `AOPUSDLY`, `AOPUSPRL`).
fn opus_head(blob: &[u8]) -> Option<(Bytes, u32)> {
    let mut rest = blob;
    while rest.len() >= 16 {
        let (marker, tail) = rest.split_at(8);
        let length = usize::try_from(u64::from_le_bytes(tail[..8].try_into().ok()?)).ok()?;
        let payload = tail.get(8..8 + length)?;
        if marker == b"AOPUSHDR" {
            let skip = u16::from_le_bytes([*payload.get(10)?, *payload.get(11)?]);
            return Some((Bytes::copy_from_slice(payload), u32::from(skip)));
        }
        rest = &tail[8 + length..];
    }
    // A bare `OpusHead`.
    (blob.starts_with(b"OpusHead") && blob.len() >= 19).then(|| {
        (
            Bytes::copy_from_slice(blob),
            u32::from(u16::from_le_bytes([blob[10], blob[11]])),
        )
    })
}

/// A `MediaCodec` audio encoder session with 16-bit interleaved input.
pub struct MediaCodecAudioEncoder {
    codec: Codec,
    kind: AudioCodec,
    rate: u32,
    channels: usize,
    frame_size: usize,
    record: Option<Bytes>,
    priming: u32,
    packets: i64,
    /// Silent samples still to queue before the end of input.
    padding: usize,
    eos_pending: bool,
    drained: bool,
}

impl std::fmt::Debug for MediaCodecAudioEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaCodecAudioEncoder")
            .field("codec", &self.codec.name())
            .finish_non_exhaustive()
    }
}

impl MediaCodecAudioEncoder {
    /// A session for `config` on the platform's own AAC or Opus encoder.
    ///
    /// # Errors
    /// [`EncodeError::EncodersRefused`] when the encoder is missing or refuses the format.
    pub fn open(config: &AudioEncodeConfig) -> Result<Self, EncodeError> {
        let (name, mime, frame_size) = match config.codec {
            AudioCodec::Aac => ("c2.android.aac.encoder", "audio/mp4a-latm", 1024),
            AudioCodec::Opus => ("c2.android.opus.encoder", "audio/opus", 960),
        };
        // AAC keeps the project's rate where it is a standard one; Opus runs at 48 kHz.
        let rate = match (config.codec, config.project_rate) {
            (
                AudioCodec::Aac,
                rate @ (8_000 | 11_025 | 12_000 | 16_000 | 22_050 | 24_000 | 32_000 | 44_100
                | 48_000 | 64_000 | 88_200 | 96_000),
            ) => rate,
            _ => 48_000,
        };
        let channels = config.speakers.speakers().len();
        // The Codec2 encoder, else the older OMX one of Android 10 images (the same FDK AAC
        // encoder, with the same delay).
        let mut codec = Codec::by_name(name)
            .ok()
            .or_else(|| Codec::encoder_for(mime).ok().filter(Codec::is_software))
            .ok_or_else(|| {
                refused(
                    mime,
                    vec![refusal(
                        name,
                        false,
                        OpenFailureKind::Absent,
                        format!("no platform {mime} encoder"),
                    )],
                )
            })?;
        let mut format = Format::new();
        format.set_str("mime", mime);
        format.set_i32("sample-rate", i32::try_from(rate).unwrap_or(48_000));
        format.set_i32("channel-count", i32::try_from(channels).unwrap_or(2));
        format.set_i32("bitrate", i32::try_from(config.bitrate).unwrap_or(128_000));
        format.set_i32("pcm-encoding", PCM_16);
        format.set_i32(
            "max-input-size",
            i32::try_from(frame_size * channels * 2).unwrap_or(i32::MAX),
        );
        if config.codec == AudioCodec::Aac {
            format.set_i32("aac-profile", AAC_LC);
        }
        codec.start(&format, true).map_err(|error| {
            refused(
                mime,
                vec![refusal(
                    name,
                    false,
                    OpenFailureKind::Unsuitable,
                    error.to_string(),
                )],
            )
        })?;
        Ok(Self {
            codec,
            kind: config.codec,
            rate,
            channels,
            frame_size,
            record: None,
            priming: if config.codec == AudioCodec::Aac {
                AAC_PRIMING
            } else {
                0
            },
            packets: 0,
            padding: 0,
            eos_pending: false,
            drained: false,
        })
    }

    /// Queues the padding, then the end of input, as input buffers free up.
    fn try_queue_eos(&mut self) -> Result<(), BackendError> {
        while self.padding > 0 {
            let Some(index) = self.codec.dequeue_input(0)? else {
                return Ok(());
            };
            let frames = self.padding.min(self.frame_size);
            let bytes = frames * self.channels * 2;
            self.codec.write_input(index, 0, 0, |buffer| {
                let target = buffer.get_mut(..bytes).ok_or_else(|| {
                    BackendError::Failed("the padding does not fit an input buffer".into())
                })?;
                target.fill(0);
                Ok(bytes)
            })?;
            self.padding -= frames;
        }
        if let Some(index) = self.codec.dequeue_input(0)? {
            self.codec.queue_input(index, &[], 0, FLAG_END_OF_STREAM)?;
            self.eos_pending = false;
        }
        Ok(())
    }
}

impl AudioEncodeBackend for MediaCodecAudioEncoder {
    fn name(&self) -> &str {
        self.codec.name()
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn frame_size(&self) -> usize {
        self.frame_size
    }

    fn submit(&mut self, pts: i64, planes: &[Vec<f32>]) -> Result<Submit, EncodeError> {
        let Some(index) = self.codec.dequeue_input(0).map_err(adapter)? else {
            return Ok(Submit::Busy);
        };
        let frames = planes.first().map_or(0, Vec::len);
        let channels = self.channels;
        let pts_us = to_codec_us(pts, i64::from(self.rate));
        self.codec
            .write_input(index, pts_us, 0, |buffer| {
                let bytes = frames * channels * 2;
                let target = buffer.get_mut(..bytes).ok_or_else(|| {
                    BackendError::Failed("an audio frame does not fit its input buffer".into())
                })?;
                for (frame, out) in target.chunks_exact_mut(channels * 2).enumerate() {
                    for (channel, sample) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                        let value = planes
                            .get(channel)
                            .and_then(|plane| plane.get(frame))
                            .copied()
                            .unwrap_or(0.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let code = (value.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
                        sample.copy_from_slice(&code.to_le_bytes());
                    }
                }
                Ok(bytes)
            })
            .map_err(adapter)?;
        Ok(Submit::Accepted)
    }

    fn receive(&mut self, wait: bool) -> Result<EncodePoll, EncodeError> {
        if self.drained {
            return Ok(EncodePoll::Drained);
        }
        if self.eos_pending {
            self.try_queue_eos().map_err(adapter)?;
        }
        loop {
            match self
                .codec
                .dequeue_output(if wait { WAIT_US } else { 0 })
                .map_err(adapter)?
            {
                Output::TryAgain => return Ok(EncodePoll::Pending),
                Output::FormatChanged => {
                    if self.record.is_none() && self.kind == AudioCodec::Aac {
                        self.record = self.codec.output_format().buffer("csd-0").map(Bytes::from);
                    }
                }
                Output::Buffer {
                    index,
                    offset,
                    size,
                    flags,
                    ..
                } => {
                    let bytes = self
                        .codec
                        .output_bytes(index, offset, size)
                        .map(<[u8]>::to_vec);
                    self.codec.release_output(index).map_err(adapter)?;
                    let end = flags & FLAG_END_OF_STREAM != 0;
                    self.drained = end;
                    match bytes {
                        Some(bytes) if flags & FLAG_CODEC_CONFIG != 0 => match self.kind {
                            AudioCodec::Aac => self.record = Some(Bytes::from(bytes)),
                            AudioCodec::Opus => {
                                if let Some((head, skip)) = opus_head(&bytes) {
                                    self.record = Some(head);
                                    self.priming = skip;
                                }
                            }
                        },
                        Some(bytes) if !bytes.is_empty() => {
                            // Packets follow each other with no gaps, one frame each.
                            let pts = self.packets * i64::try_from(self.frame_size).unwrap_or(0);
                            self.packets += 1;
                            return Ok(EncodePoll::Packet(EncodedPacket {
                                pts,
                                dts: pts,
                                keyframe: true,
                                data: Bytes::from(bytes),
                            }));
                        }
                        _ => {}
                    }
                    if end {
                        return Ok(EncodePoll::Drained);
                    }
                }
            }
        }
    }

    fn finish_input(&mut self) -> Result<(), EncodeError> {
        // Some encoders end their output where their input ends less their look-ahead (the
        // priming): silence that long carries the last samples out, and the sink drops the
        // padding packets after them.
        self.padding = usize::try_from(self.priming).unwrap_or(0);
        self.eos_pending = true;
        self.try_queue_eos().map_err(adapter)
    }

    fn codec_private(&self) -> Option<Bytes> {
        self.record.clone()
    }

    fn priming(&self) -> u32 {
        self.priming
    }
}

/// Opens `MediaCodec` video encoders.
#[derive(Clone, Copy, Debug, Default)]
pub struct AndroidVideoEncoders;

impl VideoEncoders for AndroidVideoEncoders {
    fn open(&self, config: &VideoEncodeConfig) -> Result<Box<dyn VideoEncodeBackend>, EncodeError> {
        Ok(Box::new(MediaCodecVideoEncoder::open(config)?))
    }
}

/// Opens `MediaCodec` audio encoders.
#[derive(Clone, Copy, Debug, Default)]
pub struct AndroidAudioEncoders;

impl AudioEncoders for AndroidAudioEncoders {
    fn open(&self, config: &AudioEncodeConfig) -> Result<Box<dyn AudioEncodeBackend>, EncodeError> {
        Ok(Box::new(MediaCodecAudioEncoder::open(config)?))
    }
}
