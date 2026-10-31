//! How a resolved rate control reaches one concrete encoder.
//!
//! The sinks receive numbers ([`VideoRateControl`]) and never a quality
//! level. What differs between encoders is only the spelling: `h264_vulkan`
//! takes `rc_mode`, NVENC takes `rc`, `MediaFoundation` takes `rate_control`.
//! Everything that spells it lives here, so the software sink and the
//! GPU-resident sink hold a mode the same way.

use alpha2_media::{BitrateMode, VideoRateControl};
use rsmpeg::avutil::AVDictionary;
use std::ffi::CStr;

/// The private option that puts `encoder_name` into constant-bitrate mode,
/// when its family has one.
///
/// Encoders without one are held to a constant rate by the generic
/// `minrate`/`maxrate`/`bufsize` alone, which is how libavcodec's own
/// encoders (`mpeg4` among them) implement it, and which `libvpx` takes as
/// its own CBR switch. `libsvtav1` sets it through `svtav1-params` instead
/// ([`mechanism_request`]).
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

/// What to add to an open failure of `encoder_name` under `rate`, when the
/// constant mode is the likely reason: `VideoToolbox` has a constant bit rate
/// only from macOS 13, D3D12 Video Encode only on some devices (an Intel
/// integrated GPU refuses it with `HRESULT` 0x887A0020 and takes a constant
/// QP), and `FFmpeg` reports either absence as a generic error.
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
/// A variable rate is the encoder's own bitrate-driven default and needs
/// nothing more. A constant rate pins the rate from both sides against a
/// one-second buffer and, where the encoder family has a mode switch, sets it:
/// `reject_unused_encode_options` then names any encoder that ignored it,
/// rather than letting a constant request quietly run variable.
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

/// A way of controlling an encoder's rate, independent of how a family
/// spells it.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum RateControlMechanism {
    /// Constant bitrate.
    ConstantBitrate,
    /// Variable (average) bitrate.
    VariableBitrate,
    /// Constant quantizer: `FFmpeg`'s `cqp`/`constqp`, the family of `crf`.
    ConstantQuantizer,
}

impl RateControlMechanism {
    /// Every mechanism, in the order the probe reports them.
    pub const ALL: [Self; 3] = [
        Self::ConstantBitrate,
        Self::VariableBitrate,
        Self::ConstantQuantizer,
    ];

    /// Stable identifier for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConstantBitrate => "cbr",
            Self::VariableBitrate => "vbr",
            Self::ConstantQuantizer => "cqp",
        }
    }
}

/// The options that ask one encoder for one mechanism.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MechanismRequest {
    /// Private and generic `AVOption`s, in the order they are set.
    pub options: Vec<(&'static CStr, String)>,
    /// `AVCodecContext::bit_rate`; zero for a quantizer mode, which must not
    /// carry a target it would then be free to prefer.
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

/// How `encoder_name` is asked for `mechanism`, or `None` where alpha2 knows no
/// spelling for it on that family.
///
/// `quantizer` is on the codec's own scale: 0-51 for H.264/HEVC, 0-255 for
/// NVENC and Vulkan AV1, 0-52 for every D3D12VA codec, AV1 included. Whether
/// the device accepts the mode is not decided here: that is what the probe
/// and the open-time fallback find out.
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
        // SVT-AV1 has a constant rate only in its low-delay prediction structure, and refuses a
        // maximum bitrate outside its quantizer mode, which `FFmpeg` would pass on from `maxrate`.
        // So the switch is spelled in its own parameters, against the same one-second buffer.
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
        // D3D12VA derives its I and B quantizers from `qp` through `i_qfactor`/`b_qfactor` and
        // their offsets, which stay at the encoder's defaults: 1 and 0 for HEVC and AV1, and
        // libavcodec's 0.8 and 1.25 + 1.25 for H.264.
        ("vaapi" | "d3d12va", RateControlMechanism::ConstantQuantizer) => {
            fixed_quantizer(vec![(c"rc_mode", "CQP".to_owned()), (c"qp", q)])
        }
        ("amf", RateControlMechanism::ConstantQuantizer) => fixed_quantizer(vec![
            (c"rc", "cqp".to_owned()),
            (c"qp_i", q.clone()),
            (c"qp_p", q),
        ]),
        // x264 and x265 take a true constant QP as `qp`; their `crf` is another scale (libx264 at
        // `crf` 23-41 ran 0.8-2.1 dB below `qp` at the same number on the 1080p showcase).
        ("x26x", RateControlMechanism::ConstantQuantizer) => fixed_quantizer(vec![(c"qp", q)]),
        ("crf", RateControlMechanism::ConstantQuantizer) => fixed_quantizer(vec![(c"crf", q)]),
        // VideoToolbox has no QP; `flags=+qscale` makes FFmpeg pass `global_quality /
        // (FF_QP2LAMBDA * 100)` as `kVTCompressionPropertyKey_Quality`, so the quantizer is mapped
        // onto that 0-1 scale ([`videotoolbox_quality`]). FFmpeg refuses `qscale` off Apple
        // silicon, where the open-time fallback runs the bitrate. The map is H.264's; HEVC keeps
        // the bitrate.
        ("videotoolbox", RateControlMechanism::ConstantQuantizer)
            if encoder_name == "h264_videotoolbox" =>
        {
            fixed_quantizer(videotoolbox_quality_options(quantizer))
        }
        // These families open with `flags=+qscale global_quality=N` but none reads N as a QP, so a
        // quantizer visibly falls back to bitrate. MediaFoundation passes it unscaled as
        // `AVEncVideoEncodeQP` (VideoToolbox on Apple silicon, before its map, wrote `very-high`
        // smaller than `very-low` the same way); libavcodec's `mpeg4` reads each frame's
        // `quality`, which no sink sets.
        _ => return None,
    })
}

/// `FFmpeg`'s `FF_QP2LAMBDA`, the factor `qscale` encoders divide
/// `global_quality` by.
const FF_QP2LAMBDA: u32 = 118;

/// `VideoToolbox` quality (`kVTCompressionPropertyKey_Quality`, 0-1, higher
/// better) per H.264 quantizer, at the quantizers the quality levels resolve
/// to (`alpha2_media::level_quantizer`): the quality at which `h264_videotoolbox`
/// on an Apple-silicon GPU holds `OpenH264`'s PSNR-Y at the same level within
/// 0.5 dB.
///
/// Measured on the perf project P1 (the complex parity project at 1080p30,
/// first 4 s), each output against an `OpenH264` QP 0 encode of the same
/// frames: `OpenH264` at the five levels gave 40.6, 44.3, 47.3, 49.7 and
/// 52.0 dB; `VideoToolbox` at 0.25, 0.40, 0.50, 0.57 and 0.65 gave 40.6,
/// 44.3, 47.5, 50.0 and 51.7 dB, at 0.85x, 0.80x, 0.74x, 0.74x and 0.66x of
/// `OpenH264`'s bitrate, the same on the readback and GPU-resident paths. Grainy content (P1 with film grain) needs about 0.05
/// to 0.10 more for the same match, so the map is the parity project's, as
/// the quantizers' own definition is one content's.
const VIDEOTOOLBOX_QUALITY: [(u32, f64); 5] =
    [(23, 0.65), (27, 0.57), (31, 0.50), (35, 0.40), (41, 0.25)];

/// The `VideoToolbox` quality for an H.264 `quantizer`: [`VIDEOTOOLBOX_QUALITY`]
/// between its points, extended along its end segments, rounded to hundredths
/// and held to 0.01-1.
pub(crate) fn videotoolbox_quality(quantizer: u32) -> f64 {
    let table = &VIDEOTOOLBOX_QUALITY;
    let segment = table
        .windows(2)
        .find(|pair| quantizer <= pair[1].0)
        .unwrap_or(&table[table.len() - 2..]);
    let ((q0, v0), (q1, v1)) = (segment[0], segment[1]);
    let position = (f64::from(quantizer) - f64::from(q0)) / (f64::from(q1) - f64::from(q0));
    let quality = v0 + (v1 - v0) * position;
    (quality * 100.0).round().clamp(1.0, 100.0) / 100.0
}

/// The options that make `FFmpeg` pass [`videotoolbox_quality`] for
/// `quantizer`: `global_quality` is the hundredths times `FF_QP2LAMBDA`,
/// which `FFmpeg` divides back exactly.
fn videotoolbox_quality_options(quantizer: u32) -> Vec<(&'static CStr, String)> {
    // Hundredths between 1 and 100.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hundredths = (videotoolbox_quality(quantizer) * 100.0).round() as u32;
    vec![
        (c"flags", "+qscale".to_owned()),
        (c"global_quality", (hundredths * FF_QP2LAMBDA).to_string()),
    ]
}

/// Which form of a resolved rate control an encoder actually ran.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum AppliedRateControl {
    /// The constant quantizer.
    Quantizer,
    /// The quantizer mapped onto the encoder's own quality scale
    /// (`VideoToolbox`'s 0-1 `Quality`), which has no QP.
    Quality,
    /// The bitrate: either none was asked for, or the encoder refused it.
    Bitrate,
}

impl AppliedRateControl {
    /// Stable identifier for the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quantizer => "quantizer",
            Self::Quality => "quality",
            Self::Bitrate => "bitrate",
        }
    }

    /// What `encoder_name` runs when it opened with its quantizer request.
    pub(crate) fn of_quantizer_request(encoder_name: &str) -> Self {
        if encoder_name.ends_with("_videotoolbox") {
            Self::Quality
        } else {
            Self::Quantizer
        }
    }
}

/// How `OpenH264` runs `rate`, and which form of it that is.
///
/// `OpenH264` is driven through its own API rather than `AVOption`s, so it has
/// no spelling above, and it needs none to honour a quantizer: its
/// rate-control-off mode encodes every picture at the layer QP, the H.264
/// scale the quality levels resolve to. Without a quantizer it runs the
/// bitrate, capped at the target too when the rate is constant.
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

/// The constant-quantizer request for `encoder_name`, when `rate` carries a
/// quantizer; `Err` names why it cannot be asked for at all.
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
