//! A busy encoder frees up when other exports end, so it is `ENCODER_BUSY` and retried. An output
//! below every encoder's minimum fails the same way each time, so it is `INVALID_INPUT`.

use alpha2_native::{EncoderRefusal, NativeVideoCodec, OpenFailureKind};

use crate::protocol::code_tag;

/// The `videoEncoderFallbacks` entries for the encoders tried before the one that opened.
pub fn fallbacks_json(rejected: &[alpha2_native::NativeVideoEncoderFallback]) -> serde_json::Value {
    serde_json::Value::Array(
        rejected
            .iter()
            .map(|fallback| {
                serde_json::json!({
                    "encoder": fallback.encoder_name,
                    "hardwareAccelerated": fallback.hardware_accelerated,
                    "kind": fallback.kind.as_str(),
                    "reason": fallback.reason.as_str(),
                    "detail": fallback.detail,
                })
            })
            .collect(),
    )
}

/// About the time a short export holding a session takes to end.
pub const ENCODER_BUSY_RETRY_AFTER_MS: u64 = 2_000;

pub type GpuEncoderMinimum = (&'static str, (u32, u32));

/// Returns `None` when neither a busy device nor the size explains the refusals.
pub fn encoders_refused_error(
    codec: NativeVideoCodec,
    size: (u32, u32),
    refusals: &[EncoderRefusal],
    gpu_busy: Option<&EncoderRefusal>,
    gpu_minimum: Option<GpuEncoderMinimum>,
) -> Option<String> {
    if let Some(busy) = gpu_busy
        .into_iter()
        .chain(refusals)
        .find(|refusal| refusal.kind == OpenFailureKind::Busy)
    {
        return Some(code_tag(
            "ENCODER_BUSY",
            format!(
                "every hardware {} encoder here is busy ({}: {}); retry when another export \
                 finishes or run fewer exports at once",
                codec.as_str(),
                busy.encoder,
                busy.reason
            ),
        ));
    }
    let below = |minimum: (u32, u32)| size.0 < minimum.0 || size.1 < minimum.1;
    let minimums = refusals
        .iter()
        .map(|refusal| {
            (
                refusal.encoder.clone(),
                alpha2_native::encoder_minimum_dimensions(&refusal.encoder),
            )
        })
        .collect::<Vec<_>>();
    let every_one_too_small = !refusals.is_empty()
        && refusals
            .iter()
            .zip(&minimums)
            .all(|(refusal, (_, minimum))| {
                refusal.kind == OpenFailureKind::Unsuitable && below(*minimum)
            });
    if !every_one_too_small {
        return None;
    }
    let mut named = Vec::<(String, (u32, u32))>::new();
    for (encoder, minimum) in minimums.into_iter().chain(
        gpu_minimum
            .filter(|(_, minimum)| below(*minimum))
            .map(|(encoder, minimum)| (encoder.to_owned(), minimum)),
    ) {
        if !named.iter().any(|(listed, _)| *listed == encoder) {
            named.push((encoder, minimum));
        }
    }
    let list = named
        .iter()
        .map(|(encoder, (width, height))| format!("{encoder} {width}x{height}"))
        .collect::<Vec<_>>()
        .join(", ");
    let remedy = if codec == NativeVideoCodec::H264 {
        format!(
            "export at least that size, or {}",
            alpha2_native::h264_encoder_remedy()
        )
    } else {
        "export at least that size or another codec".to_owned()
    };
    Some(code_tag(
        "INVALID_INPUT",
        format!(
            "{}x{} is below the smallest size every {} encoder here accepts ({list}); {remedy}",
            size.0,
            size.1,
            codec.as_str()
        ),
    ))
}
