//! The demuxer against what `FFmpeg` reports for every Matroska and `WebM` file of the corpus
//! (`tests/cases/container-matroska-expected.json`, written by
//! `tests/corpus/generate/container-matroska.py`).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::expected;
use crate::mp4::tests::corpus::{
    expected_color, expected_line, expected_orientation, kind_name, packet_line,
};
use crate::mp4::tests::{cases_dir, corpus_dir, packets};
use crate::{AudioCodec, ContainerReader, MatroskaReader, Packet, TrackInfo};

/// Facts the reader reports differently from `FFmpeg` on purpose: file, field, and why.
const FACT_EXCEPTIONS: &[(&str, &str, &str)] = &[
    (
        "audio-pts-ahead.mkv",
        "duration",
        "the segment declares no duration and FFmpeg estimates one from the bit rate",
    ),
    (
        "audio-pts-behind.mkv",
        "duration",
        "the segment declares no duration and FFmpeg estimates one from the bit rate",
    ),
    (
        "container-matroska-hdr-pq.mkv",
        "duration",
        "FFmpeg's H.264 decoder sees too few frames to settle its reorder depth, so the stream \
         has no start time and takes the segment's duration",
    ),
];

/// What a comparison covers.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Scope {
    /// A corpus file against `FFmpeg`'s reading of it: every fact.
    Source,
    /// A file this crate wrote against `FFmpeg`'s reading of its own stream copy, which takes
    /// tags and colour from its decoders' view of the stream.
    Remux,
}

fn excepted(file: &str, field: &str) -> bool {
    FACT_EXCEPTIONS
        .iter()
        .any(|(name, what, _)| *name == file && *what == field)
}

#[allow(clippy::too_many_lines)]
fn compare_facts(
    file: &str,
    track: &TrackInfo,
    expected: &Value,
    scope: Scope,
    failures: &mut Vec<String>,
) {
    let mut check = |field: &str, ours: String, theirs: String| {
        if scope == Scope::Remux && field == "duration" {
            return;
        }
        if ours != theirs && !excepted(file, field) {
            failures.push(format!(
                "{file} track {}: {field} is {ours}, FFmpeg says {theirs}",
                track.id.0
            ));
        }
    };
    check("id", track.id.0.to_string(), expected["id"].to_string());
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
        "time base",
        format!("1/{}", track.timescale),
        expected["time_base"].as_str().unwrap_or_default().into(),
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
    check(
        "language",
        format!("{:?}", track.language.as_deref()),
        format!("{:?}", expected["language"].as_str()),
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
        check(
            "bits per sample",
            format!("{:?}", audio.bits_per_sample.map(u64::from)),
            format!("{:?}", expected["bits_per_sample"].as_u64()),
        );
        check(
            "priming",
            audio.priming.to_string(),
            expected["initial_padding"].to_string(),
        );
        let theirs = expected["speakers"].as_array().map(|names| {
            names
                .iter()
                .map(|name| name.as_str().unwrap_or_default().to_string())
                .collect::<Vec<_>>()
        });
        let ours = audio.speakers.map(|layout| {
            layout
                .speakers()
                .iter()
                .map(|speaker| speaker.name().to_string())
                .collect::<Vec<_>>()
        });
        // Vorbis and Opus decoders hand out FFmpeg's channel order; the headers declare their own.
        let unordered = matches!(audio.codec, AudioCodec::Vorbis | AudioCodec::Opus);
        let spell = |names: Option<Vec<String>>| {
            names.map(|mut names| {
                if unordered {
                    names = names
                        .into_iter()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                }
                names.join("+")
            })
        };
        check(
            "speakers",
            format!("{:?}", spell(ours)),
            format!("{:?}", spell(theirs)),
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

/// Compares every track of `reader` with `FFmpeg`'s report on the same file, in container order.
pub(super) fn compare_reader(
    reader: &mut dyn ContainerReader,
    name: &str,
    expected: &Value,
    scope: Scope,
    failures: &mut Vec<String>,
) {
    let tracks = reader.tracks().to_vec();
    let theirs = expected["tracks"].as_array().cloned().unwrap_or_default();
    if tracks.len() != theirs.len() {
        failures.push(format!(
            "{name}: {} tracks, FFmpeg has {}",
            tracks.len(),
            theirs.len()
        ));
    }
    for (ours, track) in tracks.iter().zip(&theirs) {
        compare_facts(name, ours, track, scope, failures);
        let read = packets(reader, ours.id);
        compare_packets(name, ours.id.0, &read, track, failures);
    }
}

fn compare_file(path: &Path, name: &str, expected: &Value, failures: &mut Vec<String>) {
    match MatroskaReader::open(path) {
        Ok(mut reader) => compare_reader(&mut reader, name, expected, Scope::Source, failures),
        Err(error) => failures.push(format!("{name}: {error}")),
    }
}

#[test]
fn every_corpus_matroska_file_demuxes_as_ffmpeg_does() {
    let expected = expected();
    let mut failures = Vec::new();
    let files = expected["files"].as_object().expect("files");
    for (name, file) in files {
        compare_file(&corpus_dir().join(name), name, file, &mut failures);
    }
    let fixtures = expected["fixtures"].as_object().expect("fixtures");
    for (name, file) in fixtures {
        compare_file(&cases_dir().join(name), name, file, &mut failures);
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(files.len() >= 28, "only {} files", files.len());
    assert!(fixtures.len() >= 16, "only {} fixtures", fixtures.len());
}

#[test]
fn every_corpus_matroska_file_has_expectations() {
    let expected = expected();
    let files = expected["files"].as_object().expect("files");
    for entry in std::fs::read_dir(corpus_dir()).expect("corpus") {
        let name = entry
            .expect("entry")
            .file_name()
            .into_string()
            .expect("name");
        if Path::new(&name)
            .extension()
            .is_some_and(|extension| extension == "mkv" || extension == "webm")
        {
            assert!(
                files.contains_key(&name),
                "{name} has no FFmpeg expectations"
            );
        }
    }
}
