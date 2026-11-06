//! The demuxer against what `FFmpeg` reports for every MP4, MOV and M4A file of the corpus
//! (`tests/cases/container-mp4-expected.json`, written by
//! `tests/corpus/generate/container-mp4.py`).

use std::fmt::Write as _;
use std::path::Path;

use alpha2_media::{
    ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, DynamicRange, Orientation, Rotation,
    TransferFunction,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{cases_dir, corpus_dir, expected, packets};
use crate::{ContainerReader, Mp4Reader, Packet, TrackInfo, TrackKind};

/// Facts where the container and `FFmpeg`'s stream parameters differ on purpose: `FFmpeg`
/// reports what its decoder found, the container what the file declares.
const FACT_EXCEPTIONS: &[(&str, &str, &str)] = &[
    (
        "h264-bt709-crop-left.mp4",
        "width",
        "FFmpeg reports the SPS-cropped width; the sample entry stores the coded 128",
    ),
    (
        "h264-bt709-crop-bottom.mp4",
        "height",
        "FFmpeg reports the SPS-cropped height; the sample entry stores the coded 96",
    ),
];

/// Packets whose keyframe flag `FFmpeg` takes from its codec parser instead of the sample table:
/// file, track, packet number, `FFmpeg`'s flag, and why. The rest of each such track must match.
const KEYFRAME_EXCEPTIONS: &[(&str, u32, usize, bool, &str)] = &[(
    "recovery-h264-damaged.mp4",
    1,
    10,
    false,
    "the sample's NAL lengths are corrupt, so FFmpeg's H.264 parser finds no IDR slice in it; the \
     stss box still lists it as a sync sample",
)];

pub(crate) fn packet_line(packet: &Packet) -> String {
    let digest = Sha256::digest(&packet.data);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    format!(
        "{},{},{},{},{},{},{},{}",
        packet.pts,
        packet.dts,
        packet.duration,
        u8::from(packet.keyframe),
        packet.data.len(),
        hex,
        packet.discard.0,
        packet.discard.1
    )
}

pub(crate) fn expected_line(value: &Value) -> String {
    value
        .as_array()
        .expect("packet")
        .iter()
        .map(|field| match field {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
        TrackKind::Other => "data",
    }
}

/// `FFmpeg`'s colour names mapped the way alpha2's `FFmpeg` host maps codec parameters.
pub(crate) fn expected_color(color: &Value) -> Option<ColorMetadata> {
    if color.is_null() {
        return None;
    }
    let name = |key: &str| color[key].as_str().unwrap_or("unknown").to_string();
    let (primaries, transfer, space, range) = (
        name("primaries"),
        name("transfer"),
        name("space"),
        name("range"),
    );
    if [&primaries, &transfer, &space, &range]
        .iter()
        .all(|value| *value == "unknown")
    {
        return None;
    }
    let transfer = match transfer.as_str() {
        "smpte2084" => TransferFunction::Pq,
        "arib-std-b67" => TransferFunction::Hlg,
        "iec61966-2-1" => TransferFunction::Srgb,
        _ => TransferFunction::Bt709,
    };
    Some(ColorMetadata {
        primaries: match primaries.as_str() {
            "bt2020" => ColorPrimaries::Bt2020,
            "smpte170m" | "bt470bg" => ColorPrimaries::Bt601,
            _ => ColorPrimaries::Bt709,
        },
        transfer,
        matrix: match space.as_str() {
            "bt2020nc" => ColorMatrix::Bt2020Ncl,
            "smpte170m" | "bt470bg" => ColorMatrix::Bt601,
            "gbr" => ColorMatrix::Rgb,
            _ => ColorMatrix::Bt709,
        },
        range: if range == "pc" {
            ColorRange::Full
        } else {
            ColorRange::Limited
        },
        dynamic_range: if matches!(transfer, TransferFunction::Pq | TransferFunction::Hlg) {
            DynamicRange::Hdr
        } else {
            DynamicRange::Sdr
        },
    })
}

pub(crate) fn expected_orientation(value: &Value) -> Orientation {
    let turns = value["rotation"].as_i64().unwrap_or(0) / 90;
    Orientation {
        rotation: Rotation::from_quarter_turns(i32::try_from(turns).unwrap_or(0)),
        mirror: value["mirror"].as_bool().unwrap_or(false),
    }
}

fn excepted(file: &str, field: &str) -> bool {
    FACT_EXCEPTIONS
        .iter()
        .any(|(name, what, _)| *name == file && *what == field)
}

/// What a comparison covers.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Scope {
    /// A corpus file against `FFmpeg`'s reading of it: every fact.
    Source,
    /// A file this crate wrote against `FFmpeg`'s reading of its own stream copy. `FFmpeg`'s copy
    /// writes `colr`, `pasp` and tags from its decoder's view of the stream, so those are left
    /// out.
    Remux,
}

const REMUX_SKIPPED: &[&str] = &[
    "color",
    "sample aspect ratio",
    "language",
    "title",
    "speakers",
    "bits per sample",
];

#[allow(clippy::too_many_lines)]
pub(super) fn compare_facts(
    file: &str,
    track: &TrackInfo,
    expected: &Value,
    scope: Scope,
    failures: &mut Vec<String>,
) {
    let mut check = |field: &str, ours: String, theirs: String| {
        if scope == Scope::Remux && REMUX_SKIPPED.contains(&field) {
            return;
        }
        if ours != theirs && !excepted(file, field) {
            failures.push(format!(
                "{file} track {}: {field} is {ours}, FFmpeg says {theirs}",
                track.id.0
            ));
        }
    };
    let kind = expected["kind"].as_str().unwrap_or_default();
    check(
        "kind",
        kind_name(track.kind).into(),
        if kind == "video" || kind == "audio" {
            kind
        } else {
            "data"
        }
        .into(),
    );
    check(
        "timescale",
        track.timescale.to_string(),
        expected["timescale"].to_string(),
    );
    check(
        "duration",
        format!("{:?}", track.duration),
        format!("{:?}", expected["duration"].as_i64()),
    );
    check(
        "default",
        track.default.to_string(),
        expected["default"].to_string(),
    );
    let language = expected["language"].as_str().filter(|code| *code != "und");
    check(
        "language",
        format!("{:?}", track.language.as_deref()),
        format!("{language:?}"),
    );
    check(
        "title",
        format!("{:?}", track.title.as_deref()),
        format!("{:?}", expected["title"].as_str()),
    );
    if let Some(video) = &track.video {
        check(
            "width",
            video.width.to_string(),
            expected["width"].to_string(),
        );
        check(
            "height",
            video.height.to_string(),
            expected["height"].to_string(),
        );
        check(
            "orientation",
            format!("{:?}", video.orientation),
            format!("{:?}", expected_orientation(&expected["orientation"])),
        );
        check(
            "sample aspect ratio",
            format!(
                "{:?}",
                video
                    .sample_aspect_ratio
                    .map(|ratio| format!("{}:{}", ratio.num, ratio.den))
            ),
            format!("{:?}", expected["sar"].as_str().map(str::to_string)),
        );
        check(
            "color",
            format!("{:?}", video.color),
            format!("{:?}", expected_color(&expected["color"])),
        );
    }
    if let Some(audio) = &track.audio {
        check(
            "sample rate",
            audio.sample_rate.to_string(),
            expected["sample_rate"].to_string(),
        );
        check(
            "channels",
            audio.channels.to_string(),
            expected["channels"].to_string(),
        );
        let bits = expected["bits_per_sample"]
            .as_u64()
            .filter(|bits| *bits > 0);
        check(
            "bits per sample",
            format!("{:?}", audio.bits_per_sample.map(u64::from)),
            format!("{bits:?}"),
        );
        let speakers = match &expected["speakers"] {
            Value::Array(names) => Some(
                names
                    .iter()
                    .map(|name| name.as_str().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("+"),
            ),
            _ => None,
        };
        check(
            "speakers",
            format!("{:?}", audio.speakers.map(|layout| layout.describe())),
            format!("{speakers:?}"),
        );
    }
}

pub(super) fn compare_packets(
    file: &str,
    track: u32,
    ours: &[Packet],
    expected: &Value,
    failures: &mut Vec<String>,
) {
    let mut ours = ours.to_vec();
    for &(name, id, at, keyframe, _) in KEYFRAME_EXCEPTIONS {
        if name == file && id == track {
            match ours.get_mut(at) {
                Some(packet) if packet.keyframe != keyframe => packet.keyframe = keyframe,
                _ => failures.push(format!(
                    "{file} track {track}: packet {at} no longer differs from FFmpeg's keyframe flag"
                )),
            }
        }
    }
    let lines = ours.iter().map(packet_line).collect::<Vec<_>>();
    let mut digest = Sha256::new();
    for line in &lines {
        digest.update(line.as_bytes());
        digest.update(b"\n");
    }
    let hash = digest
        .finalize()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    let count = usize::try_from(expected["packet_count"].as_u64().unwrap_or(0)).unwrap_or(0);
    if hash == expected["packets_sha256"].as_str().unwrap_or_default() && lines.len() == count {
        return;
    }
    let shown = expected["first_packets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut report = format!(
        "{file} track {track}: {} packets, FFmpeg has {count}; packet hashes differ",
        lines.len()
    );
    for (at, theirs) in shown.iter().enumerate() {
        let theirs = expected_line(theirs);
        let mine = lines.get(at).cloned().unwrap_or_default();
        if mine != theirs {
            let _ = write!(
                report,
                "\n    packet {at}: ours   {mine}\n              FFmpeg {theirs}"
            );
        }
    }
    if std::env::var_os("ALPHA2_CONTAINER_DUMP").is_some() {
        for line in &lines {
            let _ = write!(report, "\n    {line}");
        }
    }
    failures.push(report);
}

fn compare_file(path: &Path, name: &str, expected: &Value, failures: &mut Vec<String>) {
    if let Some(error) = expected.get("error") {
        if Mp4Reader::open(path).is_ok() {
            failures.push(format!("{name}: opens, FFmpeg fails with {error}"));
        }
        return;
    }
    match Mp4Reader::open(path) {
        Ok(mut reader) => compare_reader(&mut reader, name, expected, Scope::Source, failures),
        Err(error) => failures.push(format!("{name}: {error}")),
    }
}

/// Compares every track of `reader` with `FFmpeg`'s report on the same file.
pub(super) fn compare_reader(
    reader: &mut dyn ContainerReader,
    name: &str,
    expected: &Value,
    scope: Scope,
    failures: &mut Vec<String>,
) {
    let tracks = reader.tracks().to_vec();
    let theirs = expected["tracks"].as_array().cloned().unwrap_or_default();
    for track in &theirs {
        if track["attached_pic"].as_bool() == Some(true) {
            // FFmpeg turns iTunes `covr` metadata into a picture stream; it is not a track.
            continue;
        }
        let id = track["id"].as_u64().unwrap_or(0);
        let Some(ours) = tracks.iter().find(|info| u64::from(info.id.0) == id) else {
            failures.push(format!("{name}: no track {id}"));
            continue;
        };
        compare_facts(name, ours, track, scope, failures);
        let read = packets(reader, ours.id);
        compare_packets(name, ours.id.0, &read, track, failures);
    }
    let listed = theirs
        .iter()
        .filter(|track| track["attached_pic"].as_bool() != Some(true))
        .count();
    if tracks.len() != listed {
        failures.push(format!(
            "{name}: {} tracks, FFmpeg has {listed}",
            tracks.len()
        ));
    }
}

#[test]
fn every_corpus_mp4_demuxes_as_ffmpeg_does() {
    let expected = expected();
    let mut failures = Vec::new();
    let files = expected["files"].as_object().expect("files");
    for (name, file) in files {
        compare_file(&corpus_dir().join(name), name, file, &mut failures);
    }
    for (name, file) in expected["fixtures"].as_object().expect("fixtures") {
        compare_file(&cases_dir().join(name), name, file, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(files.len() >= 70, "only {} files", files.len());
}
