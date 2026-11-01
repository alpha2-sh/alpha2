//! Whether media a host found is the media a project recorded.

use alpha2_model::{MediaFingerprint, StreamTopology, VideoStreamShape};
use alpha2_time::{FrameRate, TICKS_PER_SECOND};
use serde::Serialize;

/// What comparing a recorded fingerprint with a found one shows.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FingerprintMatch {
    /// Both content hashes are present and equal.
    Identical,
    /// No shared hash, and every field both sides recorded agrees. Sameness is not proven.
    Consistent,
    /// A field both sides recorded differs.
    Mismatch {
        /// The parts that differ.
        fields: Vec<FingerprintField>,
    },
    /// The two record no field in common, so nothing was compared.
    Unverified,
}

impl FingerprintMatch {
    /// Whether the found media may stand in for the recorded media.
    #[must_use]
    pub const fn is_acceptable(&self) -> bool {
        !matches!(self, Self::Mismatch { .. })
    }
}

/// A fingerprint field.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FingerprintField {
    /// The bytes' hash.
    Content,
    /// The byte length.
    ByteLength,
    /// The video stream's size or rate.
    Video,
    /// The audio stream's channels or rate.
    Audio,
    /// Which streams there are.
    Streams,
    /// The duration.
    Duration,
    /// The pictures' color.
    Color,
}

/// Compares `found` with `recorded`.
///
/// When both sides have a content hash, the hashes alone decide. Otherwise every field both
/// recorded is compared with demuxer tolerances: frame rates within 0.05 % and durations within
/// [`duration_tolerance`]. The modification time is never compared.
#[must_use]
pub fn compare(recorded: &MediaFingerprint, found: &MediaFingerprint) -> FingerprintMatch {
    if let (Some(recorded), Some(found)) = (&recorded.content_sha256, &found.content_sha256) {
        return if recorded == found {
            FingerprintMatch::Identical
        } else {
            FingerprintMatch::Mismatch {
                fields: vec![FingerprintField::Content],
            }
        };
    }
    let mut compared = false;
    let mut fields = Vec::new();
    let mut check = |field, same: Option<bool>| {
        if let Some(same) = same {
            compared = true;
            if !same {
                fields.push(field);
            }
        }
    };
    check(
        FingerprintField::ByteLength,
        both(recorded.byte_length, found.byte_length).map(|(a, b)| a == b),
    );
    if let (Some(a), Some(b)) = (&recorded.streams, &found.streams) {
        compare_streams(a, b, &mut check);
    }
    let (recorded_video, recorded_audio) = stream_rates(recorded.streams.as_ref());
    let (found_video, found_audio) = stream_rates(found.streams.as_ref());
    let tolerance = duration_tolerance(
        &[recorded_video, found_video],
        &[recorded_audio, found_audio],
    );
    check(
        FingerprintField::Duration,
        both(recorded.duration_ticks, found.duration_ticks)
            .map(|(a, b)| a.abs_diff(b) <= tolerance),
    );
    check(
        FingerprintField::Color,
        both(recorded.color, found.color).map(|(a, b)| a == b),
    );
    if !fields.is_empty() {
        FingerprintMatch::Mismatch { fields }
    } else if compared {
        FingerprintMatch::Consistent
    } else {
        FingerprintMatch::Unverified
    }
}

fn compare_streams(
    recorded: &StreamTopology,
    found: &StreamTopology,
    check: &mut impl FnMut(FingerprintField, Option<bool>),
) {
    check(
        FingerprintField::Streams,
        Some(
            recorded.video.is_some() == found.video.is_some()
                && recorded.audio.is_some() == found.audio.is_some(),
        ),
    );
    if let (Some(a), Some(b)) = (&recorded.video, &found.video) {
        check(FingerprintField::Video, Some(same_video(a, b)));
    }
    if let (Some(a), Some(b)) = (&recorded.audio, &found.audio) {
        check(FingerprintField::Audio, Some(a == b));
    }
}

fn same_video(a: &VideoStreamShape, b: &VideoStreamShape) -> bool {
    a.width == b.width
        && a.height == b.height
        && both(a.frame_rate, b.frame_rate).is_none_or(|(a, b)| rates_agree(a, b))
}

/// Whether two measured rates ([`crate::FrameRateEvidence`]) agree within 0.05 %.
///
/// The tolerance covers proxy timestamps re-quantized to another time base. It is half the
/// 0.0999 % gap between an NTSC rate and its whole neighbour, so 29.97 and 30 never agree.
#[must_use]
pub fn rates_agree(a: FrameRate, b: FrameRate) -> bool {
    let a = rate(a);
    let b = rate(b);
    (a - b).abs() <= a.max(b) * 5e-4
}

/// How far two demuxers' durations of the same media may differ, in ticks.
///
/// One video frame, else one 1024-sample audio frame (encoder priming), else a millisecond. The
/// longest frame among the given rates is used, so the result is symmetric.
#[must_use]
pub fn duration_tolerance(video_rates: &[Option<FrameRate>], audio_rates: &[Option<u32>]) -> u64 {
    let ticks = u64::try_from(TICKS_PER_SECOND).unwrap_or(u64::MAX);
    let video = video_rates
        .iter()
        .flatten()
        .map(|rate| (ticks * u64::from(rate.denominator())).div_ceil(u64::from(rate.numerator())))
        .max();
    let audio = audio_rates
        .iter()
        .flatten()
        .map(|&rate| (ticks * 1_024).div_ceil(u64::from(rate.max(1))))
        .max();
    video.or(audio).unwrap_or(ticks / 1_000)
}

fn stream_rates(streams: Option<&StreamTopology>) -> (Option<FrameRate>, Option<u32>) {
    (
        streams.and_then(|streams| streams.video.as_ref()?.frame_rate),
        streams.and_then(|streams| Some(streams.audio.as_ref()?.sample_rate)),
    )
}

fn rate(rate: FrameRate) -> f64 {
    f64::from(rate.numerator()) / f64::from(rate.denominator())
}

const fn both<T: Copy>(a: Option<T>, b: Option<T>) -> Option<(T, T)> {
    match (a, b) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}
