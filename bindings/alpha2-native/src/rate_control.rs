//! How a resolved rate control reaches one concrete encoder.
//!
//! Sinks receive numbers ([`VideoRateControl`]), never a quality level. Encoders differ only in
//! spelling (`h264_vulkan` takes `rc_mode`, NVENC `rc`, `MediaFoundation` `rate_control`), and
//! every spelling lives here so the software and GPU-resident sinks agree.

pub(crate) use alpha2_host_core::rate_control::{
    AppliedRateControl, RateControlMechanism, videotoolbox_quality,
};
use alpha2_media::{BitrateMode, VideoRateControl};
use rsmpeg::avutil::AVDictionary;
use std::ffi::CStr;

/// The private option that puts `encoder_name` into constant-bitrate mode, if its family has one.
///
/// Others rely on `minrate`/`maxrate`/`bufsize` alone, as libavcodec's own encoders and `libvpx`
/// do. `libsvtav1` uses `svtav1-params` instead ([`mechanism_request`]).
pub(crate) fn constant_mode_option(encoder_name: &str) -> Option<(&'static CStr, &'static CStr)> {
    if encoder_name.ends_with("_vulkan") {
        Some((c"rc_mode", c"cbr"))
    } else if encoder_name.ends_with("_nvenc") || encoder_name.ends_with("_amf") {
        Some((c"rc", c"cbr"))
    } else if encoder_name.ends_with("_vaapi") || encoder_name.ends_with("_d3d12va") {
        Some((c"rc_mode", c"CBR"))
    } else if encoder_name.ends_with("_mf") {
        Some((c"rate_control", c"cbr"))
    } else if encoder_name.ends_with("_videotoolbox") {
        Some((c"constant_bit_rate", c"1"))
    } else {
        None
    }
}

/// A hint for an open failure under a constant `rate`: `VideoToolbox` has CBR only from macOS 13,
/// D3D12 Video Encode only on some devices (others refuse with `HRESULT` 0x887A0020), and `FFmpeg`
/// reports either as a generic error.
pub(crate) fn constant_rate_hint(encoder_name: &str, rate: VideoRateControl) -> &'static str {
    if rate.mode != BitrateMode::Constant {
        ""
    } else if encoder_name.ends_with("_videotoolbox") {
        " (a constant bit rate was asked for, which VideoToolbox has only on macOS 13 and later)"
    } else if encoder_name.ends_with("_d3d12va") {
        " (a constant bit rate was asked for, which D3D12 Video Encode has only on some devices; \
         ask for a quantizer instead)"
    } else {
        ""
    }
}

/// Adds what `rate` needs beyond `AVCodecContext::bit_rate` to `options`.
///
/// A variable rate needs nothing more. A constant rate pins min and max against a
/// one-second buffer and sets the family's mode switch, so `reject_unused_encode_options`
/// catches an encoder that ignores it.
pub(crate) fn rate_control_options(
    encoder_name: &str,
    rate: VideoRateControl,
    options: AVDictionary,
) -> AVDictionary {
    match rate.mode {
        BitrateMode::Variable => options,
        BitrateMode::Constant => {
            match mechanism_request(
                encoder_name,
                RateControlMechanism::ConstantBitrate,
                rate.bitrate,
                0,
            ) {
                Some(request) => request.dictionary(options),
                None => options,
            }
        }
    }
}

/// The options that ask one encoder for one mechanism.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MechanismRequest {
    /// Private and generic `AVOption`s, in the order they are set.
    pub options: Vec<(&'static CStr, String)>,
    /// `AVCodecContext::bit_rate`; zero for a quantizer mode, so the
    /// encoder has no target to prefer.
    pub bit_rate: i64,
}

impl MechanismRequest {
    /// The options as `key=value` pairs, for a report.
    pub fn describe(&self) -> String {
        let mut parts = self
            .options
            .iter()
            .map(|(key, value)| format!("{}={value}", key.to_string_lossy()))
            .collect::<Vec<_>>();
        if self.bit_rate > 0 {
            parts.insert(0, format!("b={}", self.bit_rate));
        }
        parts.join(" ")
    }

    /// The options added to a dictionary to open a context with.
    pub fn dictionary(&self, mut options: AVDictionary) -> AVDictionary {
        for (key, value) in &self.options {
            options = crate::convert::set_encoder_option(options, key, value);
        }
        options
    }
}

/// How `encoder_name` is asked for `mechanism`, or `None` where alpha2 knows no spelling.
///
/// `quantizer` is on the codec's own scale: 0-51 for H.264/HEVC, 0-255 for NVENC and Vulkan AV1,
/// 0-52 for every D3D12VA codec. Device support is left to the probe and the open-time fallback.
pub(crate) fn mechanism_request(
    encoder_name: &str,
    mechanism: RateControlMechanism,
    bitrate: u64,
    quantizer: u32,
) -> Option<MechanismRequest> {
    let target = i64::try_from(bitrate).unwrap_or(i64::MAX);
    let constant = |switch: Option<(&'static CStr, &'static str)>| {
        let mut options = vec![
            (c"maxrate", bitrate.to_string()),
            (c"minrate", bitrate.to_string()),
            (c"bufsize", bitrate.to_string()),
        ];
        if let Some((key, value)) = switch {
            options.push((key, value.to_owned()));
        }
        MechanismRequest {
            options,
            bit_rate: target,
        }
    };
    let switched = |key: &'static CStr, value: &str| MechanismRequest {
        options: vec![(key, value.to_owned())],
        bit_rate: target,
    };
    let fixed_quantizer = |options: Vec<(&'static CStr, String)>| MechanismRequest {
        options,
        bit_rate: 0,
    };
    let q = quantizer.to_string();
    let family = if encoder_name.ends_with("_vulkan") {
        "vulkan"
    } else if encoder_name.ends_with("_nvenc") {
        "nvenc"
    } else if encoder_name.ends_with("_vaapi") {
        "vaapi"
    } else if encoder_name.ends_with("_d3d12va") {
        "d3d12va"
    } else if encoder_name.ends_with("_amf") {
        "amf"
    } else if encoder_name.ends_with("_mf") {
        "mf"
    } else if encoder_name.ends_with("_videotoolbox") {
        "videotoolbox"
    } else if encoder_name.ends_with("_qsv") {
        "qsv"
    } else if matches!(encoder_name, "libx264" | "libx265") {
        "x26x"
    } else if matches!(
        encoder_name,
        "libvpx" | "libvpx-vp9" | "libaom-av1" | "libsvtav1"
    ) {
        "crf"
    } else {
        "libavcodec"
    };
    Some(match (family, mechanism) {
        // SVT-AV1 has CBR only in its low-delay structure and refuses a maximum bitrate
        // outside its quantizer mode, so the switch is spelled in its own parameters
        // against a one-second buffer.
        ("crf", RateControlMechanism::ConstantBitrate) if encoder_name == "libsvtav1" => {
            MechanismRequest {
                options: vec![
                    (c"bufsize", bitrate.to_string()),
                    (c"svtav1-params", "rc=2:pred-struct=1".to_owned()),
                ],
                bit_rate: target,
            }
        }
        (_, RateControlMechanism::ConstantBitrate) => constant(
            constant_mode_option(encoder_name)
                .map(|(key, value)| (key, value.to_str().unwrap_or_default())),
        ),
        ("vulkan", RateControlMechanism::VariableBitrate) => switched(c"rc_mode", "vbr"),
        ("nvenc", RateControlMechanism::VariableBitrate) => switched(c"rc", "vbr"),
        ("vaapi" | "d3d12va", RateControlMechanism::VariableBitrate) => switched(c"rc_mode", "VBR"),
        ("amf", RateControlMechanism::VariableBitrate) => switched(c"rc", "vbr_peak"),
        ("mf", RateControlMechanism::VariableBitrate) => switched(c"rate_control", "u_vbr"),
        (_, RateControlMechanism::VariableBitrate) => MechanismRequest {
            options: Vec::new(),
            bit_rate: target,
        },
        ("vulkan", RateControlMechanism::ConstantQuantizer) => {
            fixed_quantizer(vec![(c"rc_mode", "cqp".to_owned()), (c"qp", q)])
        }
        ("nvenc", RateControlMechanism::ConstantQuantizer) => {
            fixed_quantizer(vec![(c"rc", "constqp".to_owned()), (c"qp", q)])
        }
        // D3D12VA derives I and B quantizers from `qp` via `i_qfactor`/`b_qfactor` and offsets,
        // left at the encoder defaults (1 and 0 for HEVC/AV1; 0.8 and 1.25 + 1.25 for H.264).
        ("vaapi" | "d3d12va", RateControlMechanism::ConstantQuantizer) => {
            fixed_quantizer(vec![(c"rc_mode", "CQP".to_owned()), (c"qp", q)])
        }
        ("amf", RateControlMechanism::ConstantQuantizer) => fixed_quantizer(vec![
            (c"rc", "cqp".to_owned()),
            (c"qp_i", q.clone()),
            (c"qp_p", q),
        ]),
        // x264 and x265 take a true constant QP as `qp`; their `crf` is a different scale.
        ("x26x", RateControlMechanism::ConstantQuantizer) => fixed_quantizer(vec![(c"qp", q)]),
        ("crf", RateControlMechanism::ConstantQuantizer) => fixed_quantizer(vec![(c"crf", q)]),
        // `VideoToolbox` has no QP: `flags=+qscale` makes `FFmpeg` pass
        // `global_quality / (FF_QP2LAMBDA * 100)` as `kVTCompressionPropertyKey_Quality`, so
        // the quantizer is mapped onto that 0-1 scale ([`videotoolbox_quality`]). `FFmpeg`
        // refuses `qscale` off Apple silicon, where the bitrate fallback runs. The map is
        // H.264's; HEVC keeps the bitrate.
        ("videotoolbox", RateControlMechanism::ConstantQuantizer)
            if encoder_name == "h264_videotoolbox" =>
        {
            fixed_quantizer(videotoolbox_quality_options(quantizer))
        }
        // These families accept `flags=+qscale global_quality=N` but none reads N as a QP, so the
        // quantizer falls back to bitrate visibly. `MediaFoundation` passes it unscaled as
        // `AVEncVideoEncodeQP`; libavcodec's `mpeg4` reads per-frame `quality`, which no sink sets.
        _ => return None,
    })
}

/// `FFmpeg`'s `FF_QP2LAMBDA`, which `qscale` encoders divide `global_quality` by.
const FF_QP2LAMBDA: u32 = 118;

/// Options that make `FFmpeg` pass [`videotoolbox_quality`]: `global_quality` is the hundredths
/// times `FF_QP2LAMBDA`, which `FFmpeg` divides back exactly.
fn videotoolbox_quality_options(quantizer: u32) -> Vec<(&'static CStr, String)> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hundredths = (videotoolbox_quality(quantizer) * 100.0).round() as u32;
    vec![
        (c"flags", "+qscale".to_owned()),
        (c"global_quality", (hundredths * FF_QP2LAMBDA).to_string()),
    ]
}

/// What `encoder_name` runs when it opened with its quantizer request.
pub(crate) fn applied_of_quantizer_request(encoder_name: &str) -> AppliedRateControl {
    if encoder_name.ends_with("_videotoolbox") {
        AppliedRateControl::Quality
    } else {
        AppliedRateControl::Quantizer
    }
}

/// How `OpenH264` runs `rate`, and which form that is.
///
/// `OpenH264` is driven through its own API; with rate control off it encodes every picture at the
/// layer QP. Without a quantizer it runs the bitrate, also capped when the rate is constant.
pub(crate) fn openh264_rate(
    rate: VideoRateControl,
) -> (crate::openh264::OpenH264Rate, AppliedRateControl) {
    match rate.quantizer {
        Some(quantizer) => (
            crate::openh264::OpenH264Rate::Quantizer(quantizer),
            AppliedRateControl::Quantizer,
        ),
        None => (
            crate::openh264::OpenH264Rate::Bitrate {
                target: rate.bitrate,
                constant: rate.mode == BitrateMode::Constant,
            },
            AppliedRateControl::Bitrate,
        ),
    }
}

/// The constant-quantizer request for `encoder_name` when `rate` carries a quantizer; `Err` says
/// why it cannot be asked for.
pub(crate) fn quantizer_request(
    encoder_name: &str,
    rate: VideoRateControl,
) -> Option<Result<MechanismRequest, String>> {
    let quantizer = rate.quantizer?;
    Some(
        mechanism_request(
            encoder_name,
            RateControlMechanism::ConstantQuantizer,
            rate.bitrate,
            quantizer,
        )
        .ok_or_else(|| format!("{encoder_name} has no constant-quantizer option alpha2 knows")),
    )
}
