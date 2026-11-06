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
    export_configured(
        EngineConfig::default(),
        project,
        (assets, frames),
        fonts,
        output,
    )
}

/// Files by source ID, and image sequence frames by source ID.
type Assets<'a> = (&'a [(&'a str, &'a str)], &'a [(&'a str, &'a [&'a str])]);

/// [`export_with_frames`] by a session set up with `config`.
fn export_configured(
    config: EngineConfig,
    project: &str,
    assets: Assets<'_>,
    fonts: &[&str],
    output: &str,
) -> (PathBuf, Value) {
    let (path, report) = try_export(config, project, assets, fonts, output);
    (
        path,
        report.unwrap_or_else(|error| panic!("{project}: {error:?}")),
    )
}

/// [`export_configured`], answering the export's failure instead of panicking on it.
fn try_export(
    config: EngineConfig,
    project: &str,
    (assets, frames): Assets<'_>,
    fonts: &[&str],
    output: &str,
) -> (PathBuf, Result<Value, crate::Alpha2Error>) {
    let tests = tests_dir();
    let engine = Alpha2::new(config);
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
    let report = pollster::block_on(handle.outcome()).map(|outcome| {
        let report: Value = serde_json::from_str(&outcome.report_json).expect("report");
        eprintln!("{project}: {report}");
        report
    });
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
}

#[test]
fn a_project_with_audio_exports_to_mkv_and_webm() {
    let assets = [
        ("image-source", "cross_topology_parity.png"),
        ("audio-source", "audio-only-aac-priming.m4a"),
    ];
    let (path, report) = export(
        "host-audio-export-project.json",
        &assets,
        &[],
        "audio-export.mkv",
    );
    check(&path, &report, true);
    assert_eq!(report["container"], "mkv");
    let _ = std::fs::remove_file(path);
    // `WebM` takes VP9 or AV1, which not every device encodes.
    let capabilities: Value =
        serde_json::from_str(&Alpha2::new(EngineConfig::default()).capabilities()).expect("JSON");
    let encoders = &capabilities["media"]["videoEncoders"];
    let webm = ["vp9", "av1"].iter().any(|codec| {
        encoders
            .as_array()
            .is_some_and(|all| all.iter().any(|e| e == codec))
    });
    if !webm {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::WebmEncode,
            "the device encodes {encoders}"
        );
        return;
    }
    let (path, report) = export(
        "host-audio-export-project.json",
        &assets,
        &[],
        "audio-export.webm",
    );
    check(&path, &report, true);
    assert_eq!(report["container"], "webm");
    let _ = std::fs::remove_file(path);
}

/// Counts the frames an export reports.
struct Counting(std::sync::atomic::AtomicU64);

impl ExportProgress for Counting {
    fn on_progress(&self, event: ExportProgressEvent) {
        self.0
            .store(event.frames_done, std::sync::atomic::Ordering::Release);
    }
}

#[test]
fn an_export_holds_in_the_background_and_finishes_after_a_memory_warning() {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let tests = tests_dir();
    let engine = Alpha2::new(EngineConfig::default());
    let output = std::env::temp_dir().join("background-round-trip.mp4");
    let progress = Arc::new(Counting(std::sync::atomic::AtomicU64::new(0)));
    let request = ExportRequest {
        project_json: std::fs::read_to_string(
            tests.join("cases/host-first-frame-small-layer-project.json"),
        )
        .expect("project"),
        assets: HashMap::from([(
            "still".to_owned(),
            tests
                .join("corpus/parity-image.png")
                .to_string_lossy()
                .into_owned(),
        )]),
        output_path: output.to_string_lossy().into_owned(),
        overwrite: true,
        ..ExportRequest::default()
    };
    let handle = engine
        .export(request, Arc::clone(&progress) as Arc<dyn ExportProgress>)
        .expect("export starts");
    let until = Instant::now() + Duration::from_secs(30);
    while progress.0.load(Ordering::Acquire) < 3 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    // The app leaves the foreground, where the system warns it of memory pressure: the export
    // finishes the frame it is drawing, if any, and holds at the next boundary, then continues
    // on a halved frame budget.
    engine.pause();
    let paused_at = progress.0.load(Ordering::Acquire);
    engine.memory_warning();
    let until = Instant::now() + Duration::from_secs(30);
    while !handle.is_held() && !handle.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(handle.is_held(), "the export did not hold");
    let held = progress.0.load(Ordering::Acquire);
    assert!(
        held <= paused_at + 1,
        "paused at frame {paused_at}, then reached {held}"
    );
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        progress.0.load(Ordering::Acquire),
        held,
        "frames while held"
    );
    assert!(!handle.is_finished());
    engine.resume();
    let outcome = pollster::block_on(handle.outcome()).expect("export");
    let report: Value = serde_json::from_str(&outcome.report_json).expect("report");
    eprintln!("held at frame {held}; {report}");
    check(&output, &report, false);
    let _ = std::fs::remove_file(output);
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

    /// Attaches `reader`'s window to `player` and waits for its first frame; `false`, recorded as
    /// a skip, when the GPU refuses the window (the emulator's GL-only images refuse an image
    /// reader's window; their export path is tested all the same). Any other failure fails.
    fn attached(player: &crate::Player, reader: &WindowReader, width: u32, height: u32) -> bool {
        use alpha2_test_support::{Capability, skip};

        player
            .attach_window(reader.window(), width, height)
            .expect("attach");
        let shown = wait_for(Some(reader), 10, || {
            player.frames_presented() >= 1 || player.last_error().is_some()
        });
        match player.last_error() {
            Some(error) if error.starts_with(crate::player::PRESENT_REFUSED) => {
                skip!(Capability::WindowPresent, "{error}");
                false
            }
            Some(error) => panic!("{error}"),
            None => {
                assert!(shown, "no first frame in 10 s");
                true
            }
        }
    }

    #[test]
    fn preview_presents_frames_into_a_window_and_survives_a_rebuild() {
        use crate::{Player, PlayerRequest};

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
        if !attached(&player, &reader, 64, 48) {
            return;
        }
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
        if !attached(&player, &reader, 64, 48) {
            return;
        }
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

    #[test]
    fn preview_of_a_video_presents_its_frames() {
        use crate::{Player, PlayerRequest};

        let tests = super::tests_dir();
        let player = Player::new(
            Alpha2::new(EngineConfig::default()),
            PlayerRequest {
                project_json: super::gpu_frames::project(320, 180, 15),
                assets: HashMap::from([(
                    "v".to_owned(),
                    tests
                        .join("corpus/h264-padded.mp4")
                        .to_string_lossy()
                        .into_owned(),
                )]),
                ..PlayerRequest::default()
            },
        )
        .expect("player");
        let reader = WindowReader::new(320, 180);
        if !attached(&player, &reader, 320, 180) {
            return;
        }
        // Playing draws each of the clip's 15 frames as it comes due, or counts it as dropped
        // when it came due before the previous one was drawn (a device too slow for the rate).
        let (presented, dropped) = (player.frames_presented(), player.frames_dropped());
        player.play();
        assert!(wait_for(Some(&reader), 5, || player.is_playing()));
        assert!(
            wait_for(Some(&reader), 10, || !player.is_playing()),
            "{:?}",
            player.last_error()
        );
        let drawn = player.frames_presented() - presented;
        let skipped = player.frames_dropped() - dropped;
        eprintln!("played 15 frames: {drawn} drawn, {skipped} dropped");
        assert_eq!(drawn + skipped, 15, "{drawn} drawn and {skipped} dropped");
        // The first frame and the last are always drawn.
        assert!(drawn >= 2, "only {drawn} frames drawn");
        let before = player.frames_presented();
        player.seek(alpha2_time::TICKS_PER_SECOND / 5);
        assert!(wait_for(Some(&reader), 5, || player.position_ticks()
            == alpha2_time::TICKS_PER_SECOND / 5));
        assert!(
            wait_for(Some(&reader), 5, || player.frames_presented() > before),
            "the seek presented no frame"
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

#[cfg(target_os = "android")]
#[test]
fn the_memory_frame_path_stays_selectable() {
    let assets: &[(&str, &str)] = &[("v", "h264-padded.mp4")];
    let project = std::env::temp_dir().join("gpu-frames-project.json");
    std::fs::write(&project, gpu_frames::project(320, 180, 15)).expect("project file");
    // An absolute path replaces the cases directory the helper joins it to.
    let project_path = project.to_string_lossy().into_owned();
    let imports = gpu_frames::renders_surfaces("h264-padded.mp4");
    for (mode, surfaces) in [
        (crate::VideoFrameMode::Cpu, false),
        (crate::VideoFrameMode::Auto, imports),
    ] {
        let config = EngineConfig {
            video_frames: mode,
            ..EngineConfig::default()
        };
        let (path, report) =
            export_configured(config, &project_path, (assets, &[]), &[], "gpu-frames.mp4");
        check(&path, &report, false);
        let imported = report["surfaceFrames"].as_u64().expect("surfaceFrames");
        assert_eq!(
            imported > 0,
            surfaces,
            "{mode:?}: {imported} surface frames"
        );
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_file(project);
}

#[cfg(target_os = "android")]
mod gpu_frames {
    //! Frames decoded into GPU buffers and imported, against the same frames decoded into
    //! memory.

    use std::sync::Arc;

    use alpha2_android_media::{AndroidSurface, AndroidVideo, VideoOutput};
    use alpha2_engine::{EngineProject, Exporter, QualityProfile, TextTextures};
    use alpha2_gpu::GpuContext;
    use alpha2_host_core::encode::PlaneBuffer;
    use alpha2_host_core::export::{ExportRenderer, StreamSources, VideoSink, export_video};
    use alpha2_host_core::video_stream::{
        BackendImage, DecodePreference, StreamOptions, VideoBackends, VideoStream,
    };
    use alpha2_media::{CancelToken, EncodeError, PixelLayout, SourceId};
    use alpha2_native_gpu::{AhbImporter, reopen_for_ahb_import};
    use alpha2_render::Renderer;
    use alpha2_test_support::{Capability, skip};
    use alpha2_time::MediaTime;

    use super::tests_dir;

    /// A project of `width` x `height` at 30 fps showing source `v` from its start for `frames`
    /// frames.
    pub(super) fn project(width: u32, height: u32, frames: i64) -> String {
        let end = frames * alpha2_time::TICKS_PER_SECOND / 30;
        serde_json::json!({
            "schema_version": 1,
            "id": "gpu-frames",
            "name": "GPU frames",
            "settings": {
                "width": width,
                "height": height,
                "frame_rate": { "num": 30, "den": 1 },
                "sample_rate": 48000,
            },
            "sources": [{ "id": "v", "locator": "asset://v", "kind": "video" }],
            "timeline": { "tracks": [{
                "id": "video",
                "name": "Video",
                "kind": "video",
                "clips": [{
                    "id": "clip",
                    "source_id": "v",
                    "timeline_range": { "start": 0, "end": end },
                    "source_range": { "start": 0, "end": end },
                }],
            }]},
        })
        .to_string()
    }

    /// Whether this device decodes `name` into surfaces it imports; skipped when not.
    pub(super) fn renders_surfaces(name: &str) -> bool {
        importer().is_some_and(|(_, importer)| {
            hardware_stream(
                name,
                VideoOutput::Surfaces {
                    ten_bit: importer.supports_ten_bit(),
                },
            )
            .is_some()
        })
    }

    fn importer() -> Option<(GpuContext, AhbImporter)> {
        let gpu = match pollster::block_on(GpuContext::request_product()) {
            Ok(gpu) => gpu,
            Err(error) => {
                skip!(Capability::AhbImport, "no GPU device: {error}");
                return None;
            }
        };
        match reopen_for_ahb_import(gpu) {
            Ok(opened) => Some(opened),
            Err((_, reason)) => {
                skip!(Capability::AhbImport, "{reason}");
                None
            }
        }
    }

    /// A stream of `name`; the reference in memory may fall back to software, which decodes the
    /// same pictures, and the surface stream stays on hardware, which alone renders surfaces.
    fn hardware_stream(name: &str, output: VideoOutput) -> Option<VideoStream> {
        let preference = match output {
            VideoOutput::ByteBuffers => DecodePreference::Auto,
            VideoOutput::Surfaces { .. } => DecodePreference::Hardware,
        };
        let opened = VideoStream::open_file(
            &tests_dir().join("corpus").join(name),
            Arc::new(AndroidVideo { output }),
            StreamOptions {
                preference,
                ..StreamOptions::default()
            },
        );
        match opened {
            Ok(stream) => Some(stream),
            Err(error) => {
                skip!(
                    Capability::HardwareVideoDecode,
                    "{name} has no hardware decoder: {error}"
                );
                None
            }
        }
    }

    /// The texture's texels, rows packed, and bytes per texel.
    fn read(gpu: &GpuContext, texture: &wgpu::Texture) -> (Vec<u8>, usize) {
        let size = texture.size();
        let texel = texture
            .format()
            .block_copy_size(None)
            .expect("plain format") as usize;
        let row = size.width as usize * texel;
        let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize);
        let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("plane readback"),
            size: (padded * size.height as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(u32::try_from(padded).expect("row size")),
                    rows_per_image: Some(size.height),
                },
            },
            size,
        );
        gpu.submit([encoder.finish()]);
        gpu.map_async(&buffer.slice(..), wgpu::MapMode::Read, |result| {
            result.expect("mapped");
        });
        gpu.device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .expect("readback finishes");
        let mapped = buffer.slice(..).get_mapped_range().expect("mapped range");
        let packed = mapped
            .chunks(padded)
            .flat_map(|line| &line[..row])
            .copied()
            .collect();
        drop(mapped);
        buffer.unmap();
        (packed, texel)
    }

    /// Sample `index` of a row of `bytes`, `width` bytes per sample: 8-bit as is, 16-bit as its
    /// 10-bit code.
    fn sample(row: &[u8], index: usize, width: usize) -> i32 {
        if width == 1 {
            i32::from(row[index])
        } else {
            i32::from(u16::from_le_bytes([row[index * 2], row[index * 2 + 1]]) >> 6)
        }
    }

    /// The largest code difference between `cpu` (`stride` bytes per row) and `gpu` (packed,
    /// `gpu_width` samples per row) over `area`, `(x, y, columns, rows)` in samples from each
    /// side's own origin.
    struct Planes<'a> {
        cpu: &'a [u8],
        stride: usize,
        cpu_origin: (usize, usize),
        gpu: &'a [u8],
        gpu_width: usize,
        gpu_origin: (usize, usize),
        /// Samples per texel on the GPU side, and which of them this compares.
        gpu_step: (usize, usize),
        cpu_step: (usize, usize),
        bytes: usize,
    }

    impl Planes<'_> {
        fn max_difference(&self, columns: usize, rows: usize) -> (i32, usize) {
            let mut most = 0;
            let mut unpadded = 0;
            for y in 0..rows {
                let cpu_row = &self.cpu[(self.cpu_origin.1 + y) * self.stride..];
                let gpu_row = &self.gpu
                    [(self.gpu_origin.1 + y) * self.gpu_width * self.bytes * self.gpu_step.0..];
                for x in 0..columns {
                    let cpu = sample(
                        cpu_row,
                        (self.cpu_origin.0 + x) * self.cpu_step.0 + self.cpu_step.1,
                        self.bytes,
                    );
                    let at = (self.gpu_origin.0 + x) * self.gpu_step.0 + self.gpu_step.1;
                    let gpu = sample(gpu_row, at, self.bytes);
                    if self.bytes == 2
                        && u16::from_le_bytes([gpu_row[at * 2], gpu_row[at * 2 + 1]]) & 63 != 0
                    {
                        unpadded += 1;
                    }
                    most = most.max((cpu - gpu).abs());
                }
            }
            (most, unpadded)
        }
    }

    /// Decodes `name` into memory and into GPU buffers, imports each GPU buffer, and returns the
    /// frame count and the largest luma and chroma code differences over the visible area.
    #[allow(clippy::too_many_lines)]
    fn compare(name: &str) -> Option<(usize, i32, i32)> {
        let (gpu, decoder_buffers) = importer()?;
        let mut memory = hardware_stream(name, VideoOutput::ByteBuffers)?;
        let mut surfaces = hardware_stream(
            name,
            VideoOutput::Surfaces {
                ten_bit: decoder_buffers.supports_ten_bit(),
            },
        )?;
        let cancel = CancelToken::default();
        let (mut frames, mut luma, mut chroma) = (0, 0, 0);
        loop {
            let (cpu, surface) = match (
                memory.next(&cancel).expect("memory frame"),
                surfaces.next(&cancel).expect("surface frame"),
            ) {
                (Some(cpu), Some(surface)) => (cpu, surface),
                (None, None) => break,
                (cpu, surface) => panic!(
                    "{name}: frame {frames} exists in one path only ({} {})",
                    cpu.is_some(),
                    surface.is_some()
                ),
            };
            assert_eq!(cpu.facts.pts, surface.facts.pts, "{name}: frame {frames}");
            let (shown, gpu_shown) = (cpu.facts.visible_rect, surface.facts.visible_rect);
            assert_eq!(
                (shown.width, shown.height),
                (gpu_shown.width, gpu_shown.height),
                "{name}: visible sizes"
            );
            let BackendImage::Cpu { format, planes } = &cpu.image else {
                panic!("{name}: the byte-buffer session produced a surface");
            };
            let BackendImage::Surface(lease) = &surface.image else {
                panic!("{name}: the hardware session did not render into a surface");
            };
            let android = lease
                .as_any()
                .downcast_ref::<AndroidSurface>()
                .expect("an Android surface");
            let scope = gpu.device().push_error_scope(wgpu::ErrorFilter::Validation);
            // SAFETY: the importer keeps the lease, so the buffer lives and is not rewritten
            // until the conversion has read it.
            let imported = unsafe {
                decoder_buffers.import(
                    android.hardware_buffer(),
                    android.ten_bit(),
                    Box::new(Arc::clone(lease)),
                )
            }
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            let [y_texture, uv_texture] = imported.lease.textures();
            let (y, bytes) = read(&gpu, y_texture);
            let (uv, _) = read(&gpu, uv_texture);
            assert!(
                pollster::block_on(scope.pop()).is_none(),
                "{name}: GPU validation error"
            );
            if frames == 0 {
                eprintln!(
                    "{name}: memory {:?} {}x{} visible {shown:?}; surface {:?} visible {gpu_shown:?}",
                    format, planes[0].width, planes[0].height, imported.coded_size
                );
            }
            let gpu_width = imported.coded_size[0] as usize;
            let (columns, rows) = (shown.width as usize, shown.height as usize);
            let origin = |rect: alpha2_media::Rect, shift: u32| {
                ((rect.x >> shift) as usize, (rect.y >> shift) as usize)
            };
            let (difference, unpadded) = Planes {
                cpu: &planes[0].bytes,
                stride: planes[0].stride as usize,
                cpu_origin: origin(shown, 0),
                gpu: &y,
                gpu_width,
                gpu_origin: origin(gpu_shown, 0),
                gpu_step: (1, 0),
                cpu_step: (1, 0),
                bytes,
            }
            .max_difference(columns, rows);
            luma = luma.max(difference);
            assert_eq!(unpadded, 0, "{name}: 10-bit luma not stored as code << 6");
            let chroma_size = (columns.div_ceil(2), rows.div_ceil(2));
            let chroma_width = gpu_width.div_ceil(2);
            for component in 0..2 {
                let (cpu_plane, cpu_step) = if format.interleaved_chroma {
                    (&planes[1], (2, component))
                } else {
                    (&planes[1 + component], (1, 0))
                };
                let (difference, unpadded) = Planes {
                    cpu: &cpu_plane.bytes,
                    stride: cpu_plane.stride as usize,
                    cpu_origin: origin(shown, 1),
                    gpu: &uv,
                    gpu_width: chroma_width,
                    gpu_origin: origin(gpu_shown, 1),
                    gpu_step: (2, component),
                    cpu_step,
                    bytes,
                }
                .max_difference(chroma_size.0, chroma_size.1);
                chroma = chroma.max(difference);
                assert_eq!(unpadded, 0, "{name}: 10-bit chroma not stored as code << 6");
            }
            frames += 1;
        }
        eprintln!("{name}: {frames} frames, largest code difference luma {luma} chroma {chroma}");
        Some((frames, luma, chroma))
    }

    #[test]
    fn imported_decoder_buffers_equal_the_decoded_planes() {
        for name in [
            "h264-padded.mp4",
            "h264-bt709-crop-left.mp4",
            "hevc-padded.mp4",
            "hevc-main10-padded.mp4",
            "h264-1080p.mp4",
        ] {
            // A fixture the device does not decode in hardware is skipped (recorded); the rest
            // are still compared.
            let Some((frames, luma, chroma)) = compare(name) else {
                continue;
            };
            assert!(frames > 0, "{name}: no frames");
            assert!(
                luma <= 1 && chroma <= 1,
                "{name}: luma {luma} chroma {chroma}"
            );
        }
    }

    /// Every rendered plane of a project showing the fixture, decoded by `decoders` and, with
    /// `import`, imported from GPU buffers.
    fn render(
        renderer: &Renderer,
        name: &str,
        (width, height): (u32, u32),
        decoders: Arc<dyn VideoBackends>,
        import: Option<Arc<dyn alpha2_host_core::export::SurfaceImport>>,
    ) -> Vec<Vec<u8>> {
        struct Collect(Vec<Vec<u8>>);
        impl VideoSink for Collect {
            fn write_video(
                &mut self,
                _index: u64,
                _time: MediaTime,
                _layout: PixelLayout,
                planes: &[PlaneBuffer],
            ) -> Result<(), EncodeError> {
                self.0.push(
                    planes
                        .iter()
                        .flat_map(|plane| plane.bytes.clone())
                        .collect(),
                );
                Ok(())
            }
        }
        let snapshot = EngineProject::new(
            alpha2_model::Project::from_json(&project(width, height, 12)).expect("project"),
        )
        .expect("snapshot");
        let mut exporter = Exporter::for_project(
            &snapshot,
            QualityProfile::export(),
            alpha2_media::VideoCodec::H264,
        )
        .expect("exporter");
        let path = tests_dir().join("corpus").join(name);
        let mut sources = StreamSources::new(move |_: &SourceId| {
            crate::export::open_source(
                &path,
                Arc::clone(&decoders),
                StreamOptions {
                    preference: DecodePreference::Hardware,
                    ..StreamOptions::default()
                },
            )
        });
        if let Some(import) = import {
            sources = sources.with_surface_import(import);
        }
        let mut sink = Collect(Vec::new());
        export_video(
            &snapshot,
            &mut exporter,
            ExportRenderer::Gpu(renderer),
            &mut TextTextures::new(),
            &mut sources,
            &mut sink,
            (
                &alpha2_host_core::export::FrameBudget::new(1 << 30),
                &CancelToken::default(),
            ),
            &mut |_, _| {},
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        sink.0
    }

    #[test]
    fn rendered_frames_match_between_the_memory_and_gpu_paths() {
        let gpu = match pollster::block_on(GpuContext::request_product()) {
            Ok(gpu) => gpu,
            Err(error) => {
                skip!(Capability::AhbImport, "no GPU device: {error}");
                return;
            }
        };
        let (gpu, Some(frames)) = crate::zero_copy::open(gpu, true) else {
            skip!(
                Capability::AhbImport,
                "the GPU device does not import decoder buffers"
            );
            return;
        };
        let mut renderer = Renderer::new(gpu);
        renderer.register_builtins().expect("built-in effects");
        for name in ["h264-padded.mp4", "hevc-main10-padded.mp4"] {
            if !renders_surfaces(name) {
                continue;
            }
            let memory = render(
                &renderer,
                name,
                (320, 180),
                Arc::new(AndroidVideo::default()),
                None,
            );
            let surfaces = render(
                &renderer,
                name,
                (320, 180),
                Arc::clone(&frames.decoders),
                Some(Arc::clone(&frames.import)),
            );
            assert_eq!(memory.len(), surfaces.len(), "{name}: frame count");
            let most = memory
                .iter()
                .zip(&surfaces)
                .flat_map(|(a, b)| a.iter().zip(b))
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            let differing = memory
                .iter()
                .zip(&surfaces)
                .flat_map(|(a, b)| a.iter().zip(b))
                .filter(|(a, b)| a != b)
                .count();
            eprintln!(
                "{name}: {} rendered frames, largest difference {most}, {differing} samples differ",
                memory.len()
            );
            assert!(most <= 1, "{name}: largest difference {most}");
        }
    }
}
