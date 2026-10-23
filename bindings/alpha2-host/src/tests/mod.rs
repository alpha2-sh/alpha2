//! Host protocol, CLI, and export-pipeline tests.

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::audio::pad_audio_window_to_declared_end;
use crate::audio::{WindowedAudioSources, merge_sample_ranges};
use crate::decode::{
    BackwardRun, DecodeBudgetState, DecodeScheduler, ExportDecodeState, HardwareFallbackCounts,
    backward_run, unique_decoded_frame_bytes,
};
use crate::decode::{DecodeBudget, DecodeJob, DecodedSourceFrame};
use crate::effect_request::EffectRequest;
use crate::encode::EncodeWorker;
use crate::encode::{EncodeChannelDepth, EncodeCommand, PipelineFailure};
use crate::memory::InFlightBudget;
use crate::metrics::{ExportMetrics, add_stage_split_metrics, export_pipeline_limits};
use crate::protocol::{
    AudioCodecRequest, AudioQualityRequest, HostError, LoudnessRequest, RateRequest, Request,
    VideoCodecRequest, VideoQualityRequest, parse_request, request_id_from_line,
};
use crate::trace::{AudioBlockTrace, ExportTrace, VideoFrameTrace};
use alpha2_engine::{ExportFrameNeeds, ExportStepTimings};
use alpha2_eval::FrameNeed;
use alpha2_media::Decoder as _;
use alpha2_media::{
    AudioBlock, AudioBlockProvider, BitrateMode, CancelToken, ColorMetadata, DecodedVideoFrame,
    Orientation, PixelFormat, Plane, QualityLevel, Rect, SampleAspectRatio, SourceId,
    VideoRateControl,
};
use alpha2_model::{ClipId, MediaKind, Project};
use alpha2_native::{NativeAudioCodec, NativeVideoCodec};
use alpha2_render::{FrameSubmitTimings, InputSpec, RenderSize};
use alpha2_time::{MediaTime, TimeRange};
use serde_json::{Value, json};

mod audio;
mod conform;
mod copy;
#[cfg(all(target_os = "windows", feature = "hwencode"))]
mod d3d12va_encode;
mod decode_plan;
mod export_pipeline;
mod failures;
mod gpu_encode;
mod gpu_resident;
mod hardware_decode;
mod providers;
mod render_cache;
mod rendering;
mod requests;
mod stalls;

/// Serializes the tests that create a GPU device. Devices created and named
/// from many threads at once crash inside the Vulkan loader (seen with
/// loader 1.4.321 on NVIDIA, in `vkSetDebugUtilsObjectNameEXT`); the render
/// and engine suites serialize their GPU tests the same way.
fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static GPU_TEST_LOCK: Mutex<()> = Mutex::new(());
    alpha2_native::require_pinned_ffmpeg_for_tests();
    GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A file of the checked-in corpus. Like [`gpu_test_lock`] and
/// [`scratch_directory`], which most tests that reach `FFmpeg` start with,
/// it first stops a run on anything but the pinned SDK.
fn corpus(name: &str) -> PathBuf {
    alpha2_native::require_pinned_ffmpeg_for_tests();
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
}

/// The answers `serve_requests` wrote, one JSON value per line.
fn answers_of(output: Vec<u8>) -> Vec<Value> {
    String::from_utf8(output)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("an answer is JSON"))
        .collect()
}

fn no_gpu(_: VideoDecodePolicy) -> Result<Host, HostError> {
    Err(HostError::new(
        "GPU_INIT_FAILED",
        "could not acquire a GPU device: none in this test",
    ))
}

fn cursor_of(clip: &str) -> crate::decode::DecodeCursorKey {
    (
        SourceId::new("video-source").expect("source ID"),
        ClipId::new(clip).expect("clip ID"),
        None,
    )
}

fn ticks(times: &[MediaTime]) -> Vec<i64> {
    times.iter().map(|time| time.ticks()).collect()
}

/// Requests `ticks` of long-gop.mp4 through `clip`'s cursor, returning the
/// presented frame's time.
fn presented(state: &mut ExportDecodeState, clip: &str, ticks: i64) -> i64 {
    let (source, clip, _) = cursor_of(clip);
    state
        .decode_need(
            &FrameNeed {
                reader: None,
                clip_id: clip,
                source_id: source,
                media_time: MediaTime::from_ticks(ticks),
            },
            &CancelToken::default(),
        )
        .expect("decodes")
        .expect("a video frame")
        .frame
        .pts()
        .ticks()
}

/// Writes mono 32-bit float PCM as a WAV file.
fn write_float_wav(path: &Path, sample_rate: u32, samples: &[f32]) {
    let data_bytes = u32::try_from(samples.len() * 4).expect("WAV fits");
    let mut bytes = Vec::with_capacity(44 + samples.len() * 4);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&3_u16.to_le_bytes()); // IEEE float
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&32_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write WAV");
}

/// An image clip and a `tone` audio clip, both `[0, end_ticks)`.
fn image_and_tone_project(end_ticks: i64, sample_rate: u32) -> Value {
    let clip = |id: &str, source: &str| {
        json!({
            "id": id, "source_id": source,
            "timeline_range": {"start": 0, "end": end_ticks},
            "source_range": {"start": 0, "end": end_ticks}
        })
    };
    json!({
        "schema_version": 1,
        "id": "loudness",
        "name": "Loudness",
        "settings": {
            "width": 64, "height": 48,
            "frame_rate": {"num": 30, "den": 1},
            "sample_rate": sample_rate
        },
        "sources": [
            {"id": "image", "locator": "fixture://image", "kind": "image"},
            {"id": "tone", "locator": "fixture://tone", "kind": "audio"}
        ],
        "timeline": {"tracks": [
            {"id": "video", "name": "Video", "kind": "video",
             "clips": [clip("image-clip", "image")]},
            {"id": "audio", "name": "Audio", "kind": "audio",
             "clips": [clip("tone-clip", "tone")]}
        ]}
    })
}

/// A Vulkan device fit for `FFmpeg` video sessions, or `None` (with a note)
/// on a machine without one.
#[cfg(all(target_os = "linux", feature = "hwdecode"))]
fn vulkan_video_context() -> Option<alpha2_native_gpu::SharedVulkanContext> {
    let context = alpha2_native_gpu::SharedVulkanContext::create()
        .inspect_err(|error| eprintln!("skipped: no Vulkan video decode device ({error:?})"))
        .ok()?;
    // A debug build loads the system's validation layer wherever it has one, and one older than 1.4
    // crashes inside FFmpeg's video session setup.
    if let Some(hazard) = context.video_session_hazard() {
        eprintln!("skipped: {hazard}; use 1.4+");
        return None;
    }
    Some(context)
}

/// A host whose renderer's device feeds a hardware encoder, with what that
/// encoder takes, whichever backend this platform has; or `None` (with a
/// note) on a machine without one. `ALPHA2_REQUIRE_VIDEOTOOLBOX` makes a Mac
/// without one a failure, where the Mac GPU workflow runs.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn gpu_output_host(
    policy: VideoDecodePolicy,
) -> Option<(Host, alpha2_native::HardwareEncodeCapability)> {
    // The host itself is made everywhere, lavapipe included; only the GPU-resident output may be
    // missing.
    let host = create_host(policy).expect("host");
    // These tests export H.264, which a Windows device may not encode while it encodes HEVC or
    // AV1; elsewhere the one encoder there is encodes H.264.
    #[cfg(target_os = "windows")]
    let made = host
        .gpu_output_candidate(NativeVideoCodec::H264, None)
        .map(|(_, capability)| capability)
        .map_err(|refusal| refusal.reason);
    #[cfg(not(target_os = "windows"))]
    let made = host.gpu_output_capability();
    let made = made.map(|capability| (host, capability));
    match made {
        Ok(made) => Some(made),
        Err(reason) => {
            assert!(
                !cfg!(target_os = "macos")
                    || std::env::var_os("ALPHA2_REQUIRE_VIDEOTOOLBOX").is_none(),
                "the GPU-resident path is required: {reason}"
            );
            eprintln!("skipped: no GPU-resident output here ({reason})");
            None
        }
    }
}

/// Whether `host` decodes `codec` into surfaces its renderer imports: the
/// Vulkan device lists what its video sessions take, and `VideoToolbox`
/// decodes H.264 on every Mac, as D3D12VA does on every adapter it decodes on.
#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
#[allow(clippy::unnecessary_wraps)]
fn imports_decoded(host: &Host, codec: &str) -> Option<bool> {
    #[cfg(target_os = "linux")]
    return Some(
        host.hardware_decode_ready()
            && host
                .shared_vulkan
                .as_ref()
                .is_some_and(|context| context.decode_codecs().contains(&codec)),
    );
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    return (codec == "h264").then(|| host.hardware_decode_ready());
}

/// Every visible luma plane of a video file, in presentation order.
fn decoded_luma(path: &Path) -> Vec<Vec<u8>> {
    decoded_planes(path)
        .into_iter()
        .map(|mut planes| planes.swap_remove(0))
        .collect()
}

/// Every frame of a video file, in presentation order, as the visible
/// samples of each of its planes.
fn decoded_planes(path: &Path) -> Vec<Vec<Vec<u8>>> {
    let mut video = alpha2_native::NativeDecoder::open(path).expect("open video");
    let cancel = CancelToken::default();
    let mut frames = Vec::new();
    loop {
        match video.decode_next(&cancel).expect("decode video") {
            alpha2_media::DecodeEvent::Video(frame) => {
                frames.push(
                    frame
                        .planes
                        .iter()
                        .map(|plane| {
                            plane
                                .bytes
                                .chunks(plane.stride as usize)
                                .take(plane.height as usize)
                                .flat_map(|row| row[..plane.width as usize].iter().copied())
                                .collect()
                        })
                        .collect(),
                );
            }
            alpha2_media::DecodeEvent::EndOfStream => return frames,
            _ => {}
        }
    }
}

/// A 1 s project showing `video` (the 12 fps parity clip) with `recorded` as
/// its fingerprint.
fn conform_project(recorded: &Value) -> Value {
    json!({
        "schema_version": 1, "id": "conform", "name": "Conform",
        "settings": {"width": 96, "height": 64, "frame_rate": {"num": 12, "den": 1}, "sample_rate": 48_000},
        "sources": [
            {"id": "video", "locator": "asset://video", "kind": "video", "fingerprint": recorded},
            {"id": "unused", "locator": "asset://unused", "kind": "video"}
        ],
        "timeline": {"tracks": [
            {"id": "v1", "name": "V1", "kind": "video", "clips": [{
                "id": "clip", "source_id": "video",
                "timeline_range": {"start": 0, "end": 120_000},
                "source_range": {"start": 0, "end": 120_000}
            }]}
        ]}
    })
}

fn sha256_hex(path: &Path) -> String {
    use sha2::Digest as _;
    crate::assets::hex(&sha2::Sha256::digest(std::fs::read(path).expect("read")))
}

fn resolve(
    project: &Value,
    originals: &[(&str, PathBuf)],
    proxies: &[(&str, PathBuf)],
    purpose: alpha2_assets::MediaPurpose,
) -> crate::assets::ResolvedMedia {
    let project = Project::from_json(&project.to_string()).expect("project");
    let originals = originals
        .iter()
        .map(|(id, path)| ((*id).to_owned(), path.clone()))
        .collect::<HashMap<_, _>>();
    let mut proxy_map = HashMap::<String, Vec<crate::assets::ProxyRequest>>::new();
    for (id, path) in proxies {
        proxy_map
            .entry((*id).to_owned())
            .or_default()
            .push(crate::assets::ProxyRequest::at(path.clone()));
    }
    let no_frames = HashMap::new();
    let media = crate::assets::RequestMedia {
        originals: &originals,
        proxies: &proxy_map,
        frames: &no_frames,
    };
    crate::assets::resolve_media(
        &project,
        &media,
        &media,
        purpose,
        &crate::assets::HardwareVideoDecode::None,
    )
}

fn still_project(background: [f32; 3], cache: &str) -> Value {
    let mut project = image_and_tone_project(alpha2_time::TICKS_PER_SECOND, 48_000);
    project["sources"] = json!([{"id": "image", "locator": "fixture://image", "kind": "image"}]);
    let tracks = project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks");
    tracks.truncate(1);
    tracks[0]["clips"][0]["cache"] = json!(cache);
    project["settings"]["background"] =
        json!({"red": background[0], "green": background[1], "blue": background[2]});
    project
}

/// An empty scratch directory `alpha2-<name>-<pid>` for this test. A run
/// that panics keeps its directory, and a later process can get the same
/// id, so whatever is left there goes first.
fn scratch_directory(name: &str) -> ScratchDirectory {
    alpha2_native::require_pinned_ffmpeg_for_tests();
    let directory = env::temp_dir().join(format!("alpha2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    ScratchDirectory(directory)
}

/// A test's own scratch directory, removed with everything in it when the test
/// ends, whether it passes or panics. Removed as a test's last step, it would
/// be left behind whenever an assertion failed first, one more `alpha2-*`
/// directory in the temp directory per such run. Only the directory
/// [`scratch_directory`] made is removed.
struct ScratchDirectory(PathBuf);

impl std::ops::Deref for ScratchDirectory {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ScratchDirectory {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Where an output's decoded video and audio start and end, in ticks.
fn stream_spans(path: &Path) -> ((i64, i64), (i64, i64)) {
    let cancel = CancelToken::default();
    let mut video = (i64::MAX, i64::MIN);
    let mut decoder = alpha2_native::NativeDecoder::open(path).expect("open video");
    loop {
        match decoder.decode_next(&cancel).expect("decode video") {
            alpha2_media::DecodeEvent::Video(frame) => {
                let start = frame.pts.ticks();
                let end = start + frame.duration.map_or(0, alpha2_time::MediaTime::ticks);
                video = (video.0.min(start), video.1.max(end));
            }
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    let mut audio = (i64::MAX, i64::MIN);
    let mut decoder = alpha2_native::NativeAudioDecoder::open(path, 48_000).expect("open audio");
    loop {
        match decoder.decode_next(&cancel).expect("decode audio") {
            alpha2_media::DecodeEvent::Audio(block) => {
                let tick = |sample: i64| sample * alpha2_time::TICKS_PER_SECOND / 48_000;
                let frames = i64::try_from(block.frames()).expect("frames");
                audio = (
                    audio.0.min(tick(block.start_sample)),
                    audio.1.max(tick(block.start_sample + frames)),
                );
            }
            alpha2_media::DecodeEvent::EndOfStream => break,
            _ => {}
        }
    }
    (video, audio)
}

/// A decode pool of `concurrency` shards over the video export project, its
/// `video-source` read from `asset`, configured by `configure`.
fn decode_worker_over(
    asset: PathBuf,
    concurrency: usize,
    configure: impl FnOnce(&mut crate::decode::DecodeWorkerConfig),
) -> crate::decode::DecodeWorker {
    let project: Project = serde_json::from_str(include_str!(
        "../../../../tests/cases/host-video-export-project.json"
    ))
    .expect("video export project");
    let assets = HashMap::from([("video-source".to_owned(), asset)]);
    let mut config = crate::decode::DecodeWorkerConfig {
        providers: crate::providers::ProviderMedia::none(),
        decoding: std::sync::Arc::default(),
        depth: 2,
        byte_budget: crate::memory::ByteBudget::exact(256 << 20),
        concurrency,
        decoder_threads: 1,
        max_sessions: concurrency,
        reverse_cache_bytes: 0,
        reverse_hardware_surfaces: 0,
        stall_timeout: Duration::from_secs(20),
        #[cfg(all(target_os = "linux", feature = "hwdecode"))]
        shared_vulkan: None,
        #[cfg(all(target_os = "macos", feature = "hwdecode"))]
        shared_metal: None,
        #[cfg(all(target_os = "windows", feature = "hwdecode"))]
        shared_d3d12: None,
        #[cfg(all(
            any(target_os = "linux", target_os = "macos", target_os = "windows"),
            feature = "hwdecode"
        ))]
        hardware_required: false,
        stall: None,
    };
    configure(&mut config);
    crate::decode::DecodeWorker::spawn(
        &project,
        &assets,
        &config,
        CancelToken::default(),
        PipelineFailure::default(),
    )
    .expect("decode worker")
}

/// The protocol code and failure class a stall error is reported with.
fn stall_report(error: String) -> (&'static str, crate::failure::FailureClass) {
    let error = HostError::classify(error);
    let class = crate::failure::classify_failure(
        error.code,
        &error.message,
        error.evidence,
        crate::failure::ExportPhase::Decode,
        &alpha2_gpu::GpuDiagnosticSnapshot::default(),
    );
    (error.code, class)
}

/// Runs `body`, aborting the test process if it has not returned within
/// `limit`: the defect the encode stall tests cover is a wait that never
/// ends, and a hung test would hang the whole run with it.
fn within<T>(limit: Duration, what: &str, body: impl FnOnce() -> T) -> T {
    let (done, finished) = mpsc::channel::<()>();
    let what = what.to_owned();
    let watchdog = std::thread::spawn(move || {
        if finished.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            eprintln!("{what} did not return within {limit:?}");
            std::process::abort();
        }
    });
    let value = body();
    drop(done);
    watchdog.join().expect("watchdog");
    value
}
