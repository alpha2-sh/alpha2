//! The reference projects exported through the public API on a device or emulator, with the
//! repository's `tests/` copied to `ALPHA2_TESTS_DIR`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alpha2_container::TrackKind;
use serde_json::Value;

use crate::{Alpha2, EngineConfig, ExportProgress, ExportProgressEvent, ExportRequest, FontFile};

struct Quiet;

impl ExportProgress for Quiet {
    fn on_progress(&self, _event: ExportProgressEvent) {}
}

fn tests_dir() -> PathBuf {
    std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(|| PathBuf::from("tests"), PathBuf::from)
}

/// Exports `project` (a `tests/cases` file) with `assets` (`tests/corpus` files by source ID)
/// and returns the output and its report.
fn export(
    project: &str,
    assets: &[(&str, &str)],
    fonts: &[&str],
    output: &str,
) -> (PathBuf, Value) {
    let tests = tests_dir();
    let engine = Alpha2::new(EngineConfig::default());
    if !fonts.is_empty() {
        let files = fonts
            .iter()
            .map(|name| FontFile {
                bytes: std::fs::read(tests.join("fonts").join(name)).expect("font"),
                face_index: 0,
            })
            .collect();
        engine.set_fonts(files).expect("fonts");
    }
    let path = std::env::temp_dir().join(output);
    let request = ExportRequest {
        project_json: std::fs::read_to_string(tests.join("cases").join(project)).expect("project"),
        assets: assets
            .iter()
            .map(|(id, name)| {
                let file = tests.join("corpus").join(name);
                ((*id).to_owned(), file.to_string_lossy().into_owned())
            })
            .collect::<HashMap<_, _>>(),
        output_path: path.to_string_lossy().into_owned(),
        overwrite: true,
        container: None,
        video_codec: None,
        audio_codec: None,
        include_audio: true,
        start_ticks: None,
        end_ticks: None,
    };
    let handle = engine
        .export(request, Arc::new(Quiet))
        .expect("export starts");
    let outcome = pollster::block_on(handle.outcome());
    let outcome = outcome.unwrap_or_else(|error| panic!("{project}: {error:?}"));
    let report: Value = serde_json::from_str(&outcome.report_json).expect("report");
    eprintln!("{project}: {report}");
    (path, report)
}

/// The output has a video track with the reported frames, and audio when `audio`.
fn check(path: &std::path::Path, report: &Value, audio: bool) {
    let mut reader = alpha2_container::open(path).expect("the output reads back");
    let tracks = reader.tracks().to_vec();
    let video = tracks
        .iter()
        .find(|track| track.kind == TrackKind::Video)
        .expect("video");
    assert_eq!(
        tracks.iter().any(|track| track.kind == TrackKind::Audio),
        audio,
        "audio track"
    );
    let mut packets = 0;
    while reader.next_packet(video.id).expect("packet").is_some() {
        packets += 1;
    }
    assert_eq!(Some(packets), report["frames"].as_u64(), "frames");
}

/// A project file, its assets by source ID, its fonts, and whether its output has audio.
type Case = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
    bool,
);

#[test]
fn reference_projects_export_to_mp4() {
    let cases: [Case; 6] = [
        (
            "host-parity-project.json",
            &[("image-source", "cross_topology_parity.png")],
            &[],
            false,
        ),
        (
            "host-video-export-project.json",
            &[("video-source", "video-only.mp4")],
            &[],
            false,
        ),
        (
            "host-audio-export-project.json",
            &[
                ("image-source", "cross_topology_parity.png"),
                ("audio-source", "audio-only-aac-priming.m4a"),
            ],
            &[],
            true,
        ),
        (
            "host-parity-shapes-project.json",
            &[("still", "parity-image.png")],
            &[],
            false,
        ),
        (
            "host-parity-layout-project.json",
            &[("still", "parity-image.png")],
            &["Alpha2Fixture.ttf"],
            false,
        ),
        (
            "host-first-frame-small-layer-project.json",
            &[("still", "parity-image.png")],
            &[],
            false,
        ),
    ];
    for (project, assets, fonts, audio) in cases {
        let output = project.replace("-project.json", ".mp4");
        let (path, report) = export(project, assets, fonts, &output);
        check(&path, &report, audio);
        let _ = std::fs::remove_file(path);
    }
}
