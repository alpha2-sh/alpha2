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
    AudioQualityRequest, HostError, LoudnessRequest, RateRequest, Request, VideoQualityRequest,
    parse_request,
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

mod alpha_sources;
mod audio;
mod conform;
mod conformance;
mod copy;
mod cpu_renderer;
#[cfg(all(target_os = "windows", feature = "hwencode"))]
mod d3d12va_encode;
mod decode_plan;
mod decoder_sessions;
mod device_hung;
mod edit;
mod encoder_refusal;
mod export_pipeline;
mod extract;
mod failures;
mod gpu_encode;
mod gpu_resident;
mod hardware_decode;
mod openh264;
mod outputs;
mod paths;
mod plugins;
mod providers;
mod render_cache;
mod rendering;
mod request_isolation;
mod requests;
mod stalls;
mod stills;

/// Devices created and named from many threads at once crash inside the Vulkan loader
/// (`vkSetDebugUtilsObjectNameEXT`).
fn gpu_test_lock() -> alpha2_gpu::validation_log::TestTurn<std::sync::MutexGuard<'static, ()>> {
    static GPU_TEST_LOCK: Mutex<()> = Mutex::new(());
    alpha2_native::require_pinned_ffmpeg_for_tests();
    alpha2_gpu::validation_log::TestTurn::new(
        GPU_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn create_gpu_test_host(policy: VideoDecodePolicy) -> Result<Host, HostError> {
    crate::create_host_with(policy, crate::config::RendererPolicy::Gpu)
}

/// `ALPHA2_GPU_ADAPTER`, else `auto`. On a hybrid Windows laptop the default (`performance`)
/// passes over the integrated GPU, the only one whose D3D12 device decodes and encodes.
fn media_adapter() -> crate::adapter_choice::AdapterPolicy {
    crate::adapter_choice::AdapterPolicy::set_in_env()
        .unwrap_or(crate::adapter_choice::AdapterPolicy::Auto)
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn create_media_host(policy: VideoDecodePolicy) -> Result<Host, HostError> {
    let renderer = crate::config::RendererPolicy::from_env().map_err(HostError::invalid_input)?;
    crate::create_host_on(policy, renderer, &media_adapter())
}

fn create_gpu_media_host(policy: VideoDecodePolicy) -> Result<Host, HostError> {
    crate::create_host_on(policy, crate::config::RendererPolicy::Gpu, &media_adapter())
}

/// Asserts that `budget` is the default, possibly fitted to a small machine or the device memory.
fn assert_unrequested(budget: &Value) {
    assert!(
        ["default", "machine", "gpu"].contains(&budget["source"].as_str().unwrap_or_default()),
        "{budget}"
    );
}

/// Stops the run on anything but the pinned `FFmpeg` SDK.
fn corpus(name: &str) -> PathBuf {
    alpha2_native::require_pinned_ffmpeg_for_tests();
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(name)
}

/// `input` is JSON lines, each sent as one frame. Cancels are answered first, as they would be when
/// the input is written at once.
fn serve_input(
    serving: &mut Serving,
    input: &[u8],
    unusable: &crate::Unusable<'_>,
) -> (Result<crate::Served, HostError>, Vec<u8>) {
    let captured = crate::serve_io::Captured::default();
    let out = captured.responder();
    let frames = crate::serve_io::frames_of_lines(input);
    let requests = crate::serve_io::requests_of(&frames, &out, &serving.lifecycle);
    let served = serve_requests(serving, &requests, &out, unusable);
    (served, captured.bytes())
}

/// The answers' headers, without `state` messages.
fn answers_of(output: &[u8]) -> Vec<Value> {
    crate::serve_io::messages_of(output)
        .into_iter()
        .map(|(header, _)| header)
        .filter(|message| message["type"] != "state")
        .collect()
}

/// The headers of what a child wrote to stdout after the frames' magic; libtest writes before it.
fn child_messages(stdout: &[u8]) -> Vec<Value> {
    let mut stdout = stdout;
    alpha2_host_core::host_frames::skip_to_magic(&mut stdout, usize::MAX)
        .expect("a slice reads")
        .unwrap_or_else(|| panic!("no magic in {}", String::from_utf8_lossy(stdout)));
    crate::serve_io::messages_of(stdout)
        .into_iter()
        .map(|(header, _)| header)
        .collect()
}

/// `serve`'s flags from `ALPHA2_TEST_SERVE_ARGS`, for a child that runs `serve`.
fn test_serve_limits() -> crate::lifecycle::ServeOptions {
    let args = std::env::var("ALPHA2_TEST_SERVE_ARGS").unwrap_or_default();
    crate::lifecycle::ServeOptions::from_args(args.split_whitespace().map(str::to_owned))
        .expect("ALPHA2_TEST_SERVE_ARGS")
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

#[cfg(all(target_os = "linux", feature = "hwdecode"))]
fn vulkan_video_context() -> Option<alpha2_native_gpu::SharedVulkanContext> {
    let context = alpha2_test_support::available!(
        alpha2_test_support::Capability::VulkanVideo,
        alpha2_native_gpu::SharedVulkanContext::create()
            .map_err(|error| format!("no Vulkan video decode device ({error:?})"))
    )?;
    // A debug build loads the system validation layer, and one older than 1.4 crashes inside
    // FFmpeg's video session setup.
    if let Some(hazard) = context.video_session_hazard() {
        alpha2_test_support::skip!(
            alpha2_test_support::Capability::VulkanVideo,
            "{hazard}; run a release build, or point VK_LAYER_PATH at the 1.4 layer \
             scripts/fetch-vulkan-validation-layer.sh fetches"
        );
        return None;
    }
    Some(context)
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn h264_gpu_output(host: &Host) -> Result<alpha2_native::HardwareEncodeCapability, String> {
    // A Windows device may encode HEVC or AV1 but not H.264.
    #[cfg(target_os = "windows")]
    return host
        .gpu_output_candidate(NativeVideoCodec::H264, None)
        .map(|(_, capability)| capability)
        .map_err(|refusal| refusal.reason);
    #[cfg(not(target_os = "windows"))]
    host.gpu_output_capability()
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos", target_os = "windows"),
    feature = "hwencode"
))]
fn gpu_output_host(
    policy: VideoDecodePolicy,
) -> Option<(Host, alpha2_native::HardwareEncodeCapability)> {
    let host = create_media_host(policy).expect("host");
    match h264_gpu_output(&host) {
        Ok(capability) => Some((host, capability)),
        Err(reason) => {
            alpha2_test_support::skip!(
                alpha2_test_support::Capability::GpuResident,
                "no GPU-resident H.264 output here ({reason})"
            );
            None
        }
    }
}

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

fn decoded_luma(path: &Path) -> Vec<Vec<u8>> {
    decoded_planes(path)
        .into_iter()
        .map(|mut planes| planes.swap_remove(0))
        .collect()
}

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

/// Every hardware encoder stays above this on the transcode test's frames; a chroma plane swapped,
/// shifted by a row or offset by a column falls far below it.
pub(crate) const PLANE_PSNR_FLOOR_DB: f64 = 38.0;

pub(crate) fn plane_psnr(exported: &[u8], original: &[u8]) -> f64 {
    let squared = exported
        .iter()
        .zip(original)
        .map(|(a, b)| {
            let difference = f64::from(*a) - f64::from(*b);
            difference * difference
        })
        .sum::<f64>();
    if squared == 0.0 {
        return f64::INFINITY;
    }
    #[allow(clippy::cast_precision_loss)]
    let samples = exported.len() as f64;
    10.0 * (255.0 * 255.0 * samples / squared).log10()
}

fn plane_sad(exported: &[u8], original: &[u8], width: usize, row_offset: usize) -> u64 {
    exported
        .chunks(width)
        .zip(original.chunks(width).skip(row_offset))
        .flat_map(|(a, b)| a.iter().zip(b))
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum()
}

/// Each chroma plane must also be closer to its own source plane than to the other one or to
/// itself one row down, which catches swaps and shifts that pass the PSNR floor.
pub(crate) fn plane_check(
    exported: &[Vec<Vec<u8>>],
    original: &[Vec<Vec<u8>>],
    width: u32,
    height: u32,
) -> Result<(), String> {
    if exported.len() != original.len() {
        return Err(format!(
            "{} frames exported for {} source frames",
            exported.len(),
            original.len()
        ));
    }
    let chroma_width = width.div_ceil(2) as usize;
    for (index, (frame, source)) in exported.iter().zip(original).enumerate() {
        if frame.len() != 3 || source.len() != 3 {
            return Err(format!("frame {index}: not three planes"));
        }
        for plane in 0..3 {
            if frame[plane].len() != source[plane].len() {
                return Err(format!("frame {index} plane {plane}: another size"));
            }
            let psnr = plane_psnr(&frame[plane], &source[plane]);
            if psnr < PLANE_PSNR_FLOOR_DB {
                return Err(format!(
                    "frame {index} plane {plane}: {psnr:.2} dB, below the {PLANE_PSNR_FLOOR_DB} dB floor"
                ));
            }
        }
        for (plane, other) in [(1, 2), (2, 1)] {
            let own = plane_sad(&frame[plane], &source[plane], chroma_width, 0);
            let swapped = plane_sad(&frame[plane], &source[other], chroma_width, 0);
            let shifted = plane_sad(&frame[plane], &source[plane], chroma_width, 1);
            // A swap or shift that leaves the source unchanged cannot be detected.
            let swap_visible = plane_sad(&source[plane], &source[other], chroma_width, 0) > 0;
            let shift_visible = plane_sad(&source[plane], &source[plane], chroma_width, 1) > 0;
            if (swap_visible && own >= swapped) || (shift_visible && own >= shifted) {
                return Err(format!(
                    "frame {index} plane {plane}: {own} from its source plane, {swapped} from the \
                     other chroma plane, {shifted} from its own one row down"
                ));
            }
        }
    }
    let _ = height;
    Ok(())
}

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

/// No `.` or `..`, and on Windows no `\\?\` prefix from `std::fs::canonicalize`.
fn names_written_file(named: &Path, written: &Path) -> bool {
    named.is_absolute()
        && named.components().all(|part| {
            !matches!(
                part,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
        && !named.to_string_lossy().starts_with(r"\\?\")
        && std::fs::canonicalize(named).ok() == std::fs::canonicalize(written).ok()
        && named.is_file()
}

/// A panicked run keeps its directory and a later process can reuse the pid, so leftovers are
/// removed first.
fn scratch_directory(name: &str) -> ScratchDirectory {
    alpha2_native::require_pinned_ffmpeg_for_tests();
    let directory = env::temp_dir().join(format!("alpha2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch directory");
    ScratchDirectory(directory)
}

/// Removed on drop so that a failed assertion does not leave the directory behind.
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
        providers: alpha2_host_core::providers::ProviderMedia::none(),
        decoding: std::sync::Arc::default(),
        depth: 2,
        byte_budget: crate::memory::ByteBudget::exact(256 << 20),
        concurrency,
        decoder_threads: 1,
        hardware_decoder_threads: 1,
        max_sessions: concurrency,
        reads: Arc::default(),
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

/// Aborts the process if `body` has not returned within `limit`, so a hung wait fails instead of
/// hanging the run.
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

#[test]
fn the_plane_check_refuses_a_swapped_or_shifted_chroma_plane() {
    let (width, height) = (256_usize, 144_usize);
    let (chroma_width, chroma_height) = (width / 2, height / 2);
    // U varies along rows and V along columns, so a swap, row shift or column offset each show.
    let plane = |w: usize, h: usize, sample: &dyn Fn(usize, usize) -> u8| -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).map(move |x| sample(x, y)))
            .collect()
    };
    let frame = |noise: u8| -> Vec<Vec<u8>> {
        vec![
            plane(width, height, &|x, y| {
                u8::try_from((x * 3 + y * 5) % 200 + 16).expect("luma")
                    ^ (noise & u8::try_from(x % 2).expect("bit"))
            }),
            plane(chroma_width, chroma_height, &|_, y| {
                u8::try_from(16 + y * 3 % 200).expect("u")
            }),
            plane(chroma_width, chroma_height, &|x, _| {
                u8::try_from(16 + x * 2 % 200).expect("v")
            }),
        ]
    };
    let (w32, h32) = (
        u32::try_from(width).expect("w"),
        u32::try_from(height).expect("h"),
    );
    let source = vec![frame(0); 3];
    assert_eq!(plane_check(&source, &source, w32, h32), Ok(()));
    assert_eq!(
        plane_check(&vec![frame(1); 3], &source, w32, h32),
        Ok(()),
        "one code of noise"
    );
    let swapped: Vec<Vec<Vec<u8>>> = source
        .iter()
        .map(|planes| vec![planes[0].clone(), planes[2].clone(), planes[1].clone()])
        .collect();
    assert!(
        plane_check(&swapped, &source, w32, h32).is_err(),
        "U and V swapped"
    );
    let shift = |plane: &[u8], rows: usize, columns: usize| -> Vec<u8> {
        (0..chroma_height)
            .flat_map(|y| {
                (0..chroma_width).map(move |x| {
                    plane[(y + rows).min(chroma_height - 1) * chroma_width
                        + (x + columns).min(chroma_width - 1)]
                })
            })
            .collect()
    };
    for (rows, columns, what) in [(1, 0, "one row"), (0, 1, "one column")] {
        let moved: Vec<Vec<Vec<u8>>> = source
            .iter()
            .map(|planes| {
                vec![
                    planes[0].clone(),
                    shift(&planes[1], rows, columns),
                    shift(&planes[2], rows, columns),
                ]
            })
            .collect();
        assert!(
            plane_check(&moved, &source, w32, h32).is_err(),
            "chroma moved {what}"
        );
    }
}
