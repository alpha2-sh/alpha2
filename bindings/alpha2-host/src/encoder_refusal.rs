//! What an export reports when no video encoder opened: a busy device, an output smaller than
//! every encoder takes, or neither.
//!
//! The class decides what a client does next. A busy encoder (a consumer GPU's session limit,
//! every session held by other exports) frees up when they end, so it is `ENCODER_BUSY`, which a
//! client retries after [`ENCODER_BUSY_RETRY_AFTER_MS`]. An output below every encoder's minimum
//! fails the same way each time, so it is `INVALID_INPUT` with the sizes that would work.

use alpha2_native::{EncoderRefusal, NativeVideoCodec, OpenFailureKind};

use crate::protocol::code_tag;

/// How long a client waits before it asks a busy encoder again, as the failure's `retryAfterMs`:
/// about the time a short export holding a session takes to end.
pub const ENCODER_BUSY_RETRY_AFTER_MS: u64 = 2_000;

/// The GPU-resident path's encoder and the smallest output it takes, when the export was a
/// candidate for that path.
pub type GpuEncoderMinimum = (&'static str, (u32, u32));

/// The protocol error for an export of `codec` at `size` that no encoder opened for, from the
/// refusals of the candidates tried (`refusals`) and the GPU-resident path's busy refusal
/// (`gpu_busy`); `None` when neither a busy device nor the size explains it.
///
/// Any busy candidate makes it `ENCODER_BUSY`: nothing else opened, and that one can once the
/// exports holding it end. Otherwise, when every candidate refused the size, it is
/// `INVALID_INPUT` naming each encoder's minimum, the GPU path's included.
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
