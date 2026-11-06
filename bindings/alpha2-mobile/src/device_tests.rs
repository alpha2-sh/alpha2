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
    export_with_frames(project, assets, &[], fonts, output)
}

/// [`export`] with image sequence frames (`tests/corpus` files) by source ID.
fn export_with_frames(
    project: &str,
    assets: &[(&str, &str)],
    frames: &[(&str, &[&str])],
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
        frames: frames
            .iter()
            .map(|(id, names)| {
                let files = names
                    .iter()
                    .map(|name| {
                        tests
                            .join("corpus")
                            .join(name)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                ((*id).to_owned(), files)
            })
            .collect(),
        resources: HashMap::new(),
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

#[test]
fn extension_sources_export_to_mp4() {
    let (path, report) = export_with_frames(
        "host-parity-extensions-project.json",
        &[
            ("gif", "extensions/animation.gif"),
            ("webp", "extensions/animation.webp"),
            ("apng", "extensions/animation.png"),
            ("logo", "extensions/shapes.svg"),
            ("label", "extensions/label.svg"),
            ("lottie", "extensions/moving.json"),
            ("dot", "extensions/pair.lottie"),
            ("exr", "extensions/window.exr"),
        ],
        &[
            (
                "seq",
                &[
                    "extensions/sequence_0007.png",
                    "extensions/sequence_0008.png",
                    "extensions/sequence_0009.png",
                    "extensions/sequence_0010.png",
                ][..],
            ),
            (
                "exrseq",
                &[
                    "extensions/sequence_0001.exr",
                    "extensions/sequence_0002.exr",
                ][..],
            ),
        ],
        &["Alpha2Fixture.ttf"],
        "host-parity-extensions.mp4",
    );
    check(&path, &report, false);
    let _ = std::fs::remove_file(path);
}

#[test]
fn latex_and_pdf_sources_export_to_mp4() {
    // The small parity project with a formula and a PDF page drawn over its picture.
    let tests = tests_dir();
    let mut project: Value = serde_json::from_str(
        &std::fs::read_to_string(tests.join("cases/host-parity-project.json")).expect("project"),
    )
    .expect("JSON");
    project["sources"].as_array_mut().expect("sources").extend([
        serde_json::json!({
            "id": "formula",
            "kind": "image",
            "provider": { "format": "latex", "latex": "x^2 + \\frac{a}{b}", "font_size": 24 },
        }),
        serde_json::json!({
            "id": "page",
            "locator": "asset://page",
            "kind": "image",
            "provider": { "format": "pdf", "page": 1 },
        }),
    ]);
    let tracks = project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks");
    for (id, source) in [("formula", "formula"), ("page", "page")] {
        tracks.push(serde_json::json!({
            "id": id,
            "name": id,
            "kind": "video",
            "clips": [{
                "id": format!("{id}-clip"),
                "source_id": source,
                "timeline_range": { "start": 0, "end": 5000 },
                "source_range": { "start": 0, "end": 5000 },
            }],
        }));
    }
    let path = std::env::temp_dir().join("latex-pdf-project.json");
    std::fs::write(&path, project.to_string()).expect("project file");
    let engine = Alpha2::new(EngineConfig::default());
    let output = std::env::temp_dir().join("latex-pdf.mp4");
    let request = ExportRequest {
        project_json: project.to_string(),
        assets: HashMap::from([
            (
                "image-source".to_owned(),
                tests
                    .join("corpus/cross_topology_parity.png")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "page".to_owned(),
                tests
                    .join("corpus/extensions/pages.pdf")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]),
        output_path: output.to_string_lossy().into_owned(),
        overwrite: true,
        ..ExportRequest::default()
    };
    let handle = engine
        .export(request, Arc::new(Quiet))
        .expect("export starts");
    let outcome = pollster::block_on(handle.outcome()).expect("export");
    let report: Value = serde_json::from_str(&outcome.report_json).expect("report");
    eprintln!("latex and pdf: {report}");
    check(&output, &report, false);
    let _ = std::fs::remove_file(output);
    let _ = std::fs::remove_file(path);
}

/// Previews into an image reader's window, which only Android has.
#[cfg(target_os = "android")]
mod reader_window {
    use super::*;

    /// An image reader whose window stands in for an app's surface, so a test can read back what was
    /// presented.
    struct WindowReader(std::ptr::NonNull<ndk_sys::AImageReader>);

    impl WindowReader {
        fn new(width: i32, height: i32) -> Self {
            /// `AIMAGE_FORMAT_RGBA_8888`, and GPU color output plus frequent CPU reads.
            const RGBA_8888: i32 = 1;
            const USAGE: u64 = 0x200 | 0x3;
            let mut reader = std::ptr::null_mut();
            // SAFETY: the out-pointer is valid; the reader is deleted on drop.
            let status = unsafe {
                ndk_sys::AImageReader_newWithUsage(
                    width,
                    height,
                    RGBA_8888,
                    USAGE,
                    4,
                    &raw mut reader,
                )
            };
            assert_eq!(status.0, 0, "AImageReader_newWithUsage");
            Self(std::ptr::NonNull::new(reader).expect("reader"))
        }

        /// The window, with a reference of its own for the player to take over.
        fn window(&self) -> u64 {
            let mut window = std::ptr::null_mut();
            // SAFETY: the reader is live; the window it returns belongs to it.
            let status =
                unsafe { ndk_sys::AImageReader_getWindow(self.0.as_ptr(), &raw mut window) };
            assert_eq!(status.0, 0, "AImageReader_getWindow");
            // SAFETY: the window is live; the player releases this reference.
            unsafe { ndk_sys::ANativeWindow_acquire(window) };
            window as u64
        }

        /// The mean of the red, green and blue samples of the newest presented image.
        fn latest_mean(&self) -> Option<[f64; 3]> {
            let mut image = std::ptr::null_mut();
            // SAFETY: the reader is live and the image is deleted below.
            let status = unsafe {
                ndk_sys::AImageReader_acquireLatestImage(self.0.as_ptr(), &raw mut image)
            };
            if status.0 != 0 || image.is_null() {
                return None;
            }
            let (mut data, mut length, mut stride) = (std::ptr::null_mut(), 0, 0);
            // SAFETY: the image is live; the plane stays mapped until the image is deleted.
            let mean = unsafe {
                let read = ndk_sys::AImage_getPlaneData(image, 0, &raw mut data, &raw mut length);
                ndk_sys::AImage_getPlaneRowStride(image, 0, &raw mut stride);
                (read.0 == 0 && !data.is_null()).then(|| {
                    let bytes =
                        std::slice::from_raw_parts(data, usize::try_from(length).unwrap_or(0));
                    let mut sums = [0.0; 3];
                    let pixels = bytes.len() / 4;
                    for pixel in bytes.as_chunks::<4>().0 {
                        for (sum, value) in sums.iter_mut().zip(pixel) {
                            *sum += f64::from(*value);
                        }
                    }
                    let _ = stride;
                    #[allow(clippy::cast_precision_loss)]
                    sums.map(|sum| sum / pixels.max(1) as f64)
                })
            };
            // SAFETY: the image came from this reader and is not used after.
            unsafe { ndk_sys::AImage_delete(image) };
            mean
        }
    }

    impl Drop for WindowReader {
        fn drop(&mut self) {
            // SAFETY: the reader is owned and not used after.
            unsafe { ndk_sys::AImageReader_delete(self.0.as_ptr()) };
        }
    }

    /// Waits up to `seconds` for `done`, consuming what `reader` was shown as a compositor would, so
    /// its window always has a buffer free (none to keep the newest image for a check).
    fn wait_for(reader: Option<&WindowReader>, seconds: u64, done: impl Fn() -> bool) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while std::time::Instant::now() < until {
            if done() {
                return true;
            }
            if let Some(reader) = reader {
                let _ = reader.latest_mean();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        done()
    }

    /// Whether the GPU here presents to an image reader's window. The emulator's GL-only images
    /// (Android 10) refuse that window; their export path is tested all the same.
    fn presents_to_reader_windows() -> bool {
        let capabilities: Value =
            serde_json::from_str(&Alpha2::new(EngineConfig::default()).capabilities())
                .expect("JSON");
        let backend = capabilities["gpu"]["backend"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if backend.eq_ignore_ascii_case("gl") {
            eprintln!("skipped: the {backend} device does not present to an image reader's window");
            return false;
        }
        true
    }

    #[test]
    fn preview_presents_frames_into_a_window_and_survives_a_rebuild() {
        use crate::{Player, PlayerRequest};

        if !presents_to_reader_windows() {
            return;
        }
        let tests = tests_dir();
        let engine = Alpha2::new(EngineConfig::default());
        let player = Player::new(
            engine,
            PlayerRequest {
                project_json: std::fs::read_to_string(
                    tests.join("cases/host-video-export-project.json"),
                )
                .expect("project"),
                assets: HashMap::from([(
                    "video-source".to_owned(),
                    tests
                        .join("corpus/h264-bt709-copy.mp4")
                        .to_string_lossy()
                        .into_owned(),
                )]),
                ..PlayerRequest::default()
            },
        )
        .expect("player");
        let reader = WindowReader::new(64, 48);
        player
            .attach_window(reader.window(), 64, 48)
            .expect("attach");
        assert!(
            wait_for(None, 10, || player.frames_presented() >= 1),
            "{:?}",
            player.last_error()
        );
        let shown = reader.latest_mean();
        eprintln!("first frame mean {shown:?}");
        assert!(
            shown.is_some_and(|mean| mean.iter().any(|value| *value > 1.0)),
            "{shown:?}"
        );

        // One second of playing at 30 fps presents about 30 frames and stops at the end.
        let before = player.frames_presented();
        player.play();
        assert!(wait_for(Some(&reader), 5, || player.is_playing()));
        assert!(
            wait_for(Some(&reader), 10, || !player.is_playing()),
            "{:?}",
            player.last_error()
        );
        let frames_played = player.frames_presented() - before;
        eprintln!(
            "played {frames_played} frames, position {}",
            player.position_ticks()
        );
        assert!((20..=40).contains(&frames_played), "{frames_played} frames");

        // A seek shows its frame; a new window (the app's surface rebuilt) shows it again.
        player.seek(0);
        assert!(
            wait_for(Some(&reader), 10, || player.position_ticks() == 0),
            "{:?}",
            player.last_error()
        );
        player.detach_window();
        let rebuilt = WindowReader::new(64, 48);
        let before = player.frames_presented();
        player
            .attach_window(rebuilt.window(), 64, 48)
            .expect("reattach");
        assert!(
            wait_for(None, 10, || player.frames_presented() > before),
            "{:?}",
            player.last_error()
        );
        assert!(rebuilt.latest_mean().is_some());
        assert_eq!(player.last_error(), None);
        player.end();
    }

    #[test]
    fn preview_of_a_project_with_audio_keeps_time_without_an_output() {
        use crate::{Player, PlayerRequest};

        // A test process has no app context, so audio output cannot open; the picture keeps time by
        // the wall clock instead.
        if !presents_to_reader_windows() {
            return;
        }
        let tests = tests_dir();
        let player = Player::new(
            Alpha2::new(EngineConfig::default()),
            PlayerRequest {
                project_json: std::fs::read_to_string(
                    tests.join("cases/host-audio-export-project.json"),
                )
                .expect("project"),
                assets: HashMap::from([
                    (
                        "image-source".to_owned(),
                        tests
                            .join("corpus/cross_topology_parity.png")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    (
                        "audio-source".to_owned(),
                        tests
                            .join("corpus/audio-only-aac-priming.m4a")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                ]),
                ..PlayerRequest::default()
            },
        )
        .expect("player");
        let reader = WindowReader::new(64, 48);
        player
            .attach_window(reader.window(), 64, 48)
            .expect("attach");
        player.play();
        assert!(wait_for(Some(&reader), 5, || player.is_playing()));
        assert!(
            wait_for(Some(&reader), 10, || !player.is_playing()),
            "{:?}",
            player.last_error()
        );
        eprintln!("presented {}", player.frames_presented());
        assert!(
            player.frames_presented() >= 6,
            "{}",
            player.frames_presented()
        );
        assert_eq!(player.last_error(), None);
        player.end();
    }
}

/// Previews into a Metal layer, as an iOS view's.
#[cfg(target_os = "ios")]
mod metal_layer {
    use super::*;

    /// Waits up to `seconds` for `done`.
    fn wait_for(seconds: u64, done: impl Fn() -> bool) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while std::time::Instant::now() < until {
            if done() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        done()
    }

    #[test]
    fn preview_plays_into_a_metal_layer_and_survives_a_new_one() {
        use crate::{Player, PlayerRequest};

        let tests = tests_dir();
        let player = Player::new(
            Alpha2::new(EngineConfig::default()),
            PlayerRequest {
                project_json: std::fs::read_to_string(
                    tests.join("cases/host-video-export-project.json"),
                )
                .expect("project"),
                assets: HashMap::from([(
                    "video-source".to_owned(),
                    tests
                        .join("corpus/h264-bt709-copy.mp4")
                        .to_string_lossy()
                        .into_owned(),
                )]),
                ..PlayerRequest::default()
            },
        )
        .expect("player");
        let layer = objc2_quartz_core::CAMetalLayer::new();
        player
            .attach_window(objc2::rc::Retained::as_ptr(&layer) as u64, 64, 48)
            .expect("attach");
        assert!(
            wait_for(10, || player.frames_presented() >= 1),
            "{:?}",
            player.last_error()
        );
        let before = player.frames_presented();
        player.play();
        assert!(wait_for(5, || player.is_playing()));
        assert!(
            wait_for(10, || !player.is_playing()),
            "{:?}",
            player.last_error()
        );
        let frames_played = player.frames_presented() - before;
        eprintln!("played {frames_played} frames into a Metal layer");
        assert!((20..=40).contains(&frames_played), "{frames_played} frames");
        player.detach_window();
        let rebuilt = objc2_quartz_core::CAMetalLayer::new();
        let before = player.frames_presented();
        player
            .attach_window(objc2::rc::Retained::as_ptr(&rebuilt) as u64, 64, 48)
            .expect("reattach");
        assert!(wait_for(10, || player.frames_presented() > before));
        assert_eq!(player.last_error(), None);
        player.end();
    }
}
