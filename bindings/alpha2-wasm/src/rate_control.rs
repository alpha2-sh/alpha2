//! The engine's level-to-rate mapping, published to JavaScript.
//!
//! A browser sink must encode from the same numbers a native sink does, and
//! the only way two sinks can be held to that is for both to ask the same
//! function. This is the browser's way in to it: the app calls it once at the
//! top of an export and hands the resolved numbers to whichever sink it built.

use alpha2_media::{
    AudioCodec, QualityLevel, VideoCodec, resolve_audio_bitrate, resolve_video_rate_control,
};
use alpha2_time::FrameRate;

/// Resolves a level into the rate control every sink encodes with, as JSON:
/// `{ "bitrate": <bps>, "mode": "variable" | "constant", "quantizer": <n> | null }`.
///
/// `codec` takes the engine's spellings (`h264`, `hevc`, `vp9`, `av1`,
/// `mpeg4-part2`), and `level` one of the five level names.
///
/// # Errors
/// Returns a message for an unknown codec or level, a zero dimension, or an
/// invalid frame rate.
pub fn resolve_rate_control_json(
    codec: &str,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    level: &str,
) -> Result<String, String> {
    let invalid = |error: &dyn std::fmt::Display| format!("invalid rate-control request: {error}");
    let codec: VideoCodec = codec.parse().map_err(|error| invalid(&error))?;
    let level: QualityLevel = level.parse().map_err(|error| invalid(&error))?;
    let frame_rate = FrameRate::new(frame_rate_num, frame_rate_den)
        .map_err(|error| invalid(&format!("frame rate {error:?}")))?;
    let resolved = resolve_video_rate_control(codec, width, height, frame_rate, level)
        .map_err(|error| invalid(&error))?;
    serde_json::to_string(&resolved).map_err(|error| error.to_string())
}

/// Resolves a level into a stereo audio bitrate for `aac` or `opus`.
///
/// # Errors
/// Returns a message for an unknown codec or level.
pub fn resolve_audio_bitrate_for(codec: &str, level: &str) -> Result<u32, String> {
    let invalid = |error: &dyn std::fmt::Display| format!("invalid rate-control request: {error}");
    let codec: AudioCodec = codec.parse().map_err(|error| invalid(&error))?;
    let level: QualityLevel = level.parse().map_err(|error| invalid(&error))?;
    Ok(resolve_audio_bitrate(codec, level))
}

#[cfg(test)]
mod tests {
    use super::{resolve_audio_bitrate_for, resolve_rate_control_json};

    #[test]
    fn resolves_audio_through_the_engine_table() {
        assert_eq!(resolve_audio_bitrate_for("aac", "high"), Ok(192_000));
        assert_eq!(resolve_audio_bitrate_for("opus", "very-low"), Ok(48_000));
        assert_eq!(
            resolve_audio_bitrate_for("mp3", "high"),
            Err(
                "invalid rate-control request: unknown audio codec \"mp3\"; expected aac or opus"
                    .to_owned()
            )
        );
        assert!(resolve_audio_bitrate_for("aac", "ultra").is_err());
    }

    #[test]
    fn resolves_the_same_numbers_as_the_engine() {
        let json = resolve_rate_control_json("h264", 1920, 1080, 30, 1, "high").expect("resolves");
        assert_eq!(
            json,
            r#"{"bitrate":10000000,"mode":"variable","quantizer":27}"#
        );
    }

    #[test]
    fn rejects_what_the_engine_does_not_name() {
        for (codec, level) in [("avc", "high"), ("h264", "ultra"), ("h264", "lossless")] {
            let error = resolve_rate_control_json(codec, 1920, 1080, 30, 1, level)
                .expect_err("must be rejected");
            assert!(error.starts_with("invalid rate-control request"), "{error}");
        }
        assert!(resolve_rate_control_json("h264", 0, 1080, 30, 1, "high").is_err());
        assert!(resolve_rate_control_json("h264", 1920, 1080, 30, 0, "high").is_err());
    }
}
