//! The `OpenH264` video encoder behind the native sinks.
//!
//! Settings are the ones measured for alpha2's exports: High profile with
//! CABAC, medium complexity, frame skipping off (a dropped frame would break
//! a frame-exact export), a keyframe every two seconds, and a fixed slice
//! count with as many threads, load balancing off. With load balancing on,
//! `OpenH264` re-slices pictures by measured timing and multi-threaded output
//! differs on every run; with it off, two to eight threads give identical
//! bytes, so an export is the same on every machine. A quantizer request is
//! a true constant QP (`RC_OFF_MODE`, which takes it from the layer's
//! `iDLayerQp`); a bitrate runs `OpenH264`'s bitrate mode over the full QP
//! range.
//!
//! `OpenH264` has no B-frames, so each frame becomes one packet at once, with
//! its decode time equal to its presentation time, and nothing is pending
//! at the end. Its Annex B output is rewritten as the length-prefixed form
//! MP4, MOV and Matroska carry, and the sample description (`avcC`) is built
//! from the parameter sets the encoder writes before its first picture.

use std::collections::VecDeque;
use std::ffi::c_int;

use alpha2_media::{ColorMetadata, SampleAspectRatio};
use alpha2_time::FrameRate;
use rsmpeg::avcodec::{AVCodecParameters, AVPacket};
use rsmpeg::avutil::{AVFrame, AVRational};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use super::ffi as oh;
use super::library::OpenH264Library;

/// `RC_MODES::RC_BITRATE_MODE`.
const RC_BITRATE_MODE: c_int = 1;
/// `RC_MODES::RC_OFF_MODE`.
const RC_OFF_MODE: c_int = -1;
/// `ECOMPLEXITY_MODE::MEDIUM_COMPLEXITY`.
const MEDIUM_COMPLEXITY: c_int = 1;
/// `EParameterSetStrategy::CONSTANT_ID`.
const CONSTANT_ID: c_int = 0;
/// `EProfileIdc::PRO_HIGH`.
const PRO_HIGH: c_int = 100;
/// `SliceModeEnum::SM_SINGLE_SLICE`.
const SM_SINGLE_SLICE: c_int = 0;
/// `SliceModeEnum::SM_FIXEDSLCNUM_SLICE`.
const SM_FIXEDSLCNUM_SLICE: c_int = 1;
/// `EVideoFormatSPS::VF_UNDEF`.
const VF_UNDEF: u8 = 5;
/// `ESampleAspectRatio::ASP_EXT_SAR`.
const ASP_EXT_SAR: c_int = 255;
/// NAL unit types.
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// How the encoder is asked to spend bits.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum OpenH264Rate {
    /// A constant QP, 0-51.
    Quantizer(u32),
    /// A bitrate in bits per second; `constant` caps it at the target too.
    Bitrate { target: u64, constant: bool },
}

/// Everything an encoder is opened with.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct OpenH264Settings {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame_rate: FrameRate,
    pub(crate) color: ColorMetadata,
    pub(crate) sample_aspect_ratio: SampleAspectRatio,
    pub(crate) rate: OpenH264Rate,
    /// Frames between IDR pictures.
    pub(crate) keyframe_interval: u32,
}

/// Slices per picture: one per 270 rows, from one to eight, so four at 1080p
/// and seven at 1080x1920. It depends on the picture alone, never on the
/// machine, so the output does not either.
pub(crate) fn slice_count(height: u32) -> u32 {
    (height / 270).clamp(1, 8)
}

/// Encoder threads for `slices`: as many as slices. One thread and two or
/// more give different bytes, and two to eight give the same, so the count
/// is fixed by the picture rather than taken from the CPU budget.
pub(crate) fn thread_count(slices: u32) -> u16 {
    u16::try_from(slices).unwrap_or(1)
}

/// `defaults` (`OpenH264`'s own) with `settings` applied.
///
/// # Errors
/// When a size, rate or colour tag does not fit what `OpenH264` takes.
pub(crate) fn encoder_params(
    defaults: &oh::EncParamExt,
    settings: &OpenH264Settings,
) -> Result<oh::EncParamExt, String> {
    let int = |value: u64, what: &str| {
        c_int::try_from(value).map_err(|_| format!("the {what} {value} is out of range"))
    };
    let mut params = *defaults;
    let frame_rate =
        f64::from(settings.frame_rate.numerator()) / f64::from(settings.frame_rate.denominator());
    #[allow(clippy::cast_possible_truncation)]
    let frame_rate = frame_rate as f32;
    params.pic_width = int(u64::from(settings.width), "width")?;
    params.pic_height = int(u64::from(settings.height), "height")?;
    params.max_frame_rate = frame_rate;
    params.temporal_layer_num = 1;
    params.spatial_layer_num = 1;
    params.complexity_mode = MEDIUM_COMPLEXITY;
    params.intra_period = settings.keyframe_interval;
    params.sps_pps_id_strategy = CONSTANT_ID;
    params.prefix_nal_adding_ctrl = false;
    params.entropy_coding_mode_flag = 1;
    params.enable_frame_skip = false;
    params.enable_long_term_reference = false;
    params.enable_denoise = false;
    params.enable_background_detection = true;
    params.enable_adaptive_quant = true;
    params.use_load_balancing = false;
    let slices = slice_count(settings.height);
    params.multiple_thread_idc = thread_count(slices);
    let (rc_mode, target, maximum, qp) = match settings.rate {
        OpenH264Rate::Quantizer(qp) => (RC_OFF_MODE, 0, 0, Some(qp)),
        OpenH264Rate::Bitrate { target, constant } => {
            let target = int(target, "bitrate")?;
            (
                RC_BITRATE_MODE,
                target,
                if constant { target } else { 0 },
                None,
            )
        }
    };
    params.rc_mode = rc_mode;
    params.target_bitrate = target;
    params.max_bitrate = maximum;
    // The bitrate mode clamps its QP to this range; OpenH264 would otherwise keep whatever a
    // wrapper set, and a cap of 31 overshot a 2 Mb/s target more than threefold.
    params.min_qp = 1;
    params.max_qp = 51;
    let layer = &mut params.spatial_layers[0];
    layer.video_width = params.pic_width;
    layer.video_height = params.pic_height;
    layer.frame_rate = frame_rate;
    layer.spatial_bitrate = target;
    layer.max_spatial_bitrate = maximum;
    layer.profile_idc = PRO_HIGH;
    if let Some(qp) = qp {
        layer.d_layer_qp = c_int::try_from(qp.min(51)).unwrap_or(51);
    }
    layer.slice_argument.slice_mode = if slices > 1 {
        SM_FIXEDSLCNUM_SLICE
    } else {
        SM_SINGLE_SLICE
    };
    layer.slice_argument.slice_num = slices;
    let (primaries, transfer, matrix, range) = crate::convert::color_tags(settings.color);
    let tag = |value: i64, what: &str| {
        u8::try_from(value)
            .map_err(|_| format!("OpenH264 cannot tag {what} {value}: H.264 VUI codes are 0-255"))
    };
    layer.video_signal_type_present = true;
    layer.video_format = VF_UNDEF;
    layer.full_range = range == ffi::AVCOL_RANGE_JPEG;
    layer.color_description_present = true;
    layer.color_primaries = tag(i64::from(primaries), "colour primaries")?;
    layer.transfer_characteristics = tag(i64::from(transfer), "transfer characteristics")?;
    layer.color_matrix = tag(i64::from(matrix), "matrix coefficients")?;
    let sar = settings.sample_aspect_ratio;
    if sar != SampleAspectRatio::default() {
        let (num, den) = reduce(sar.num, sar.den);
        layer.aspect_ratio_present = true;
        layer.aspect_ratio = ASP_EXT_SAR;
        layer.aspect_ratio_ext_width = u16::try_from(num)
            .map_err(|_| format!("OpenH264 cannot write the pixel aspect ratio {num}:{den}"))?;
        layer.aspect_ratio_ext_height = u16::try_from(den)
            .map_err(|_| format!("OpenH264 cannot write the pixel aspect ratio {num}:{den}"))?;
    }
    Ok(params)
}

fn reduce(num: u32, den: u32) -> (u32, u32) {
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    let divisor = a.max(1);
    (num / divisor, den / divisor)
}

/// The NAL units of an Annex B stream, without their start codes or any
/// trailing zero bytes.
pub(crate) fn annex_b_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 3 <= stream.len() {
        if stream[at..at + 3] == [0, 0, 1] {
            starts.push(at + 3);
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(stream.len(), |next| next - 3);
        let mut unit = &stream[start..end];
        // A four-byte start code's leading zero belongs to no unit; a NAL unit never ends in a zero
        // byte (its last byte holds the stop bit, or is a `cabac_zero_word`'s 0x03).
        while let [rest @ .., 0] = unit {
            unit = rest;
        }
        if !unit.is_empty() {
            units.push(unit);
        }
    }
    units
}

/// The NAL unit type of `unit`.
fn unit_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |header| header & 0x1f)
}

/// An `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15) for one SPS and one
/// or more PPS, with four-byte NAL lengths.
///
/// # Errors
/// When `sps` is too short to name its profile and level, or a set is longer
/// than the record's 16-bit lengths hold.
pub(crate) fn avc_decoder_configuration(sps: &[u8], pps: &[&[u8]]) -> Result<Vec<u8>, String> {
    let [_, profile, compatibility, level, ..] = *sps else {
        return Err("the sequence parameter set is too short".into());
    };
    let length = |unit: &[u8]| {
        u16::try_from(unit.len())
            .map(u16::to_be_bytes)
            .map_err(|_| "a parameter set is longer than avcC holds".to_owned())
    };
    let mut record = vec![1, profile, compatibility, level, 0xfc | 3, 0xe0 | 1];
    record.extend_from_slice(&length(sps)?);
    record.extend_from_slice(sps);
    record.push(u8::try_from(pps.len()).map_err(|_| "too many picture parameter sets")?);
    for set in pps {
        record.extend_from_slice(&length(set)?);
        record.extend_from_slice(set);
    }
    // The High profiles carry their chroma format and bit depths after the sets: 4:2:0, 8-bit.
    if !matches!(profile, 66 | 77 | 88) {
        record.extend_from_slice(&[0xfc | 1, 0xf8, 0xf8, 0]);
    }
    Ok(record)
}

/// `units` as MP4 and Matroska carry them: each behind its length in four
/// big-endian bytes.
pub(crate) fn length_prefixed(units: &[&[u8]]) -> Result<Vec<u8>, String> {
    let mut packet = Vec::with_capacity(units.iter().map(|unit| unit.len() + 4).sum());
    for unit in units {
        let length =
            u32::try_from(unit.len()).map_err(|_| "a NAL unit is larger than 4 GiB".to_owned())?;
        packet.extend_from_slice(&length.to_be_bytes());
        packet.extend_from_slice(unit);
    }
    Ok(packet)
}

/// One open `OpenH264` encoder and its stream description.
pub(crate) struct OpenH264Encoder {
    encoder: oh::Encoder,
    width: c_int,
    height: c_int,
    time_base: AVRational,
    /// The parameter sets in the sample description, each NAL unit as it is.
    parameter_sets: Vec<Vec<u8>>,
    parameters: AVCodecParameters,
    packets: VecDeque<AVPacket>,
    flushed: bool,
    threads: u16,
}

impl OpenH264Encoder {
    /// Opens an encoder on `library` with `settings`, in `time_base` (the
    /// frame length).
    ///
    /// # Errors
    /// When `OpenH264` refuses the parameters or writes no usable parameter
    /// sets.
    pub(crate) fn open(
        library: &OpenH264Library,
        settings: &OpenH264Settings,
        time_base: AVRational,
    ) -> Result<Self, String> {
        let mut encoder = oh::Encoder::create(std::sync::Arc::clone(&library.api))?;
        let params = encoder_params(&encoder.default_params()?, settings)?;
        encoder.initialize(&params)?;
        let sets = encoder.parameter_sets()?;
        let units = annex_b_units(&sets);
        let sps = units
            .iter()
            .find(|unit| unit_type(unit) == NAL_SPS)
            .ok_or("OpenH264 wrote no sequence parameter set")?;
        let pps = units
            .iter()
            .filter(|unit| unit_type(unit) == NAL_PPS)
            .copied()
            .collect::<Vec<_>>();
        if pps.is_empty() {
            return Err("OpenH264 wrote no picture parameter set".into());
        }
        let extradata = avc_decoder_configuration(sps, &pps)?;
        let (primaries, transfer, matrix, range) = crate::convert::color_tags(settings.color);
        let sar = settings.sample_aspect_ratio;
        let parameters = alpha2_ffmpeg_bridge::video_codec_parameters(
            &alpha2_ffmpeg_bridge::VideoStreamDescription {
                codec_id: ffi::AV_CODEC_ID_H264,
                width: params.pic_width,
                height: params.pic_height,
                pixel_format: ffi::AV_PIX_FMT_YUV420P,
                profile: c_int::from(sps[1]),
                level: c_int::from(sps[3]),
                color_primaries: primaries,
                color_trc: transfer,
                color_space: matrix,
                color_range: range,
                sample_aspect_ratio: if sar == SampleAspectRatio::default() {
                    rsmpeg::avutil::ra(0, 1)
                } else {
                    rsmpeg::avutil::ra(
                        c_int::try_from(sar.num).unwrap_or(1),
                        c_int::try_from(sar.den).unwrap_or(1),
                    )
                },
                frame_rate: rsmpeg::avutil::ra(
                    c_int::try_from(settings.frame_rate.numerator()).unwrap_or(c_int::MAX),
                    c_int::try_from(settings.frame_rate.denominator()).unwrap_or(1),
                ),
                bit_rate: match settings.rate {
                    OpenH264Rate::Quantizer(_) => 0,
                    OpenH264Rate::Bitrate { target, .. } => i64::try_from(target).unwrap_or(0),
                },
                extradata: &extradata,
            },
        )
        .map_err(|error| format!("describing the OpenH264 stream: {error}"))?;
        let parameter_sets = units
            .iter()
            .filter(|unit| matches!(unit_type(unit), NAL_SPS | NAL_PPS))
            .map(|unit| unit.to_vec())
            .collect();
        Ok(Self {
            encoder,
            width: params.pic_width,
            height: params.pic_height,
            time_base,
            parameter_sets,
            parameters,
            packets: VecDeque::new(),
            flushed: false,
            threads: params.multiple_thread_idc,
        })
    }

    /// The time base packets are stamped in.
    pub(crate) const fn time_base(&self) -> AVRational {
        self.time_base
    }

    /// The stream's codec parameters, `avcC` included.
    pub(crate) fn codec_parameters(&self) -> AVCodecParameters {
        self.parameters.clone()
    }

    /// Encoder threads.
    pub(crate) const fn threads(&self) -> u16 {
        self.threads
    }

    /// Encodes `frame`, stamped in [`Self::time_base`], or with `None` ends
    /// the stream, which leaves nothing to drain: `OpenH264` holds no frame
    /// back.
    ///
    /// # Errors
    /// When `OpenH264` fails or skips the frame, or changes its parameter sets.
    pub(crate) fn send(&mut self, frame: Option<&AVFrame>) -> Result<(), String> {
        let Some(frame) = frame else {
            self.flushed = true;
            return Ok(());
        };
        if self.flushed {
            return Err("the encoder was already flushed".into());
        }
        let encoded = self.encoder.encode(frame, self.width, self.height)?;
        if encoded.frame_type == oh::FRAME_TYPE_SKIP || encoded.annex_b.is_empty() {
            // Frame skipping is off, so this never happens; a skipped frame would leave the export
            // a frame short.
            return Err("OpenH264 skipped a frame".into());
        }
        let units = annex_b_units(&encoded.annex_b);
        let mut picture = Vec::with_capacity(units.len());
        for unit in units {
            match unit_type(unit) {
                NAL_SPS | NAL_PPS => {
                    // With `CONSTANT_ID` every IDR picture repeats the header's sets; they live in
                    // the sample description, and a picture coded against others would be decoded
                    // against the header's.
                    if !self.parameter_sets.iter().any(|set| set == unit) {
                        return Err(
                            "OpenH264 changed its parameter sets mid-stream, which one stream \
                             header cannot carry"
                                .into(),
                        );
                    }
                }
                NAL_AUD => {}
                _ => picture.push(unit),
            }
        }
        let mut packet = alpha2_ffmpeg_bridge::packet_from_bytes(&length_prefixed(&picture)?)
            .map_err(|error| format!("allocating a packet: {error}"))?;
        packet.set_pts(frame.pts);
        packet.set_dts(frame.pts);
        if encoded.frame_type == oh::FRAME_TYPE_IDR {
            packet.set_flags(packet.flags | i32::try_from(ffi::AV_PKT_FLAG_KEY).unwrap_or(1));
        }
        self.packets.push_back(packet);
        Ok(())
    }

    /// The next packet, as `avcodec_receive_packet` answers: a packet, or
    /// `EncoderDrainError` until the stream is flushed and
    /// `EncoderFlushedError` after.
    pub(crate) fn receive(&mut self) -> Result<AVPacket, RsmpegError> {
        match self.packets.pop_front() {
            Some(packet) => Ok(packet),
            None if self.flushed => Err(RsmpegError::EncoderFlushedError),
            None => Err(RsmpegError::EncoderDrainError),
        }
    }
}
