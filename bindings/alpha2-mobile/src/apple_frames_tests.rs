//! `VideoToolbox` pixel buffers sampled as `Metal` textures against frames copied through memory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use alpha2_apple_media::AppleVideo;
use alpha2_engine::{EngineProject, Exporter, QualityProfile, TextTextures};
use alpha2_gpu::GpuContext;
use alpha2_host_core::encode::PlaneBuffer;
use alpha2_host_core::export::{
    ExportRenderer, FrameBudget, StreamSources, SurfaceImport, VideoSink, export_video,
};
use alpha2_host_core::video_stream::{
    BackendImage, DecodePreference, StreamOptions, VideoBackends, VideoStream,
};
use alpha2_media::{CancelToken, EncodeError, PixelLayout, SourceId};
use alpha2_render::Renderer;
use alpha2_test_support::{Capability, skip};
use alpha2_time::MediaTime;

use crate::zero_copy::{GpuFrames, hooks};
use crate::{
    Alpha2, EngineConfig, ExportProgress, ExportProgressEvent, ExportRequest, ProgressFailure,
    VideoFrameMode,
};

static SERIAL: Mutex<()> = Mutex::new(());

const FRAMES: i64 = 12;

fn tests_dir() -> PathBuf {
    std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests")),
        PathBuf::from,
    )
}

fn corpus(name: &str) -> PathBuf {
    tests_dir().join("corpus").join(name)
}

fn project(width: u32, height: u32, frames: i64) -> String {
    let end = frames * alpha2_time::TICKS_PER_SECOND / 30;
    serde_json::json!({
        "schema_version": 1,
        "id": "apple-frames",
        "name": "Apple frames",
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

fn importing() -> Option<(GpuContext, Renderer, GpuFrames)> {
    let gpu = match pollster::block_on(GpuContext::request_product()) {
        Ok(gpu) => gpu,
        Err(error) => {
            skip!(Capability::Metal, "no GPU device: {error}");
            return None;
        }
    };
    let (gpu, Some(frames)) = crate::zero_copy::open(gpu, true) else {
        skip!(
            Capability::Metal,
            "the GPU device does not import pixel buffers"
        );
        return None;
    };
    let mut renderer = Renderer::new(gpu.clone());
    renderer.register_builtins().expect("built-in effects");
    Some((gpu, renderer, frames))
}

fn render(
    renderer: &Renderer,
    name: &str,
    decoders: Arc<dyn VideoBackends>,
    import: Option<Arc<dyn SurfaceImport>>,
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
        alpha2_model::Project::from_json(&project(320, 180, FRAMES)).expect("project"),
    )
    .expect("snapshot");
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let path = corpus(name);
    let mut sources = StreamSources::new(move |_: &SourceId| {
        crate::export::open_source(
            &path,
            Arc::clone(&decoders),
            StreamOptions {
                preference: DecodePreference::Auto,
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
        (ExportRenderer::Gpu(renderer), None),
        &mut TextTextures::new(),
        &mut sources,
        &mut sink,
        (&FrameBudget::new(1 << 30), &CancelToken::default()),
        &mut |_| Ok(()),
        &mut |_, _| {},
    )
    .unwrap_or_else(|error| panic!("{name}: {error}"));
    sink.0
}

fn difference(name: &str, memory: &[Vec<u8>], surfaces: &[Vec<u8>]) -> (u8, usize) {
    assert_eq!(memory.len(), surfaces.len(), "{name}: frame count");
    let pairs = || {
        memory
            .iter()
            .zip(surfaces)
            .flat_map(|(a, b)| a.iter().zip(b))
    };
    let most = pairs().map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
    (most, pairs().filter(|(a, b)| a != b).count())
}

const FIXTURES: [&str; 5] = [
    "h264-padded.mp4",
    "hevc-padded.mp4",
    "hevc-main10-padded.mp4",
    "h264-bt709-crop-bottom.mp4",
    "h264-bt709-crop-left.mp4",
];

#[test]
fn rendered_frames_match_between_the_memory_and_metal_paths() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((_, renderer, frames)) = importing() else {
        return;
    };
    for name in FIXTURES {
        let memory = render(&renderer, name, Arc::new(AppleVideo::default()), None);
        let before = (frames.imported(), frames.fallbacks());
        let surfaces = render(
            &renderer,
            name,
            Arc::clone(&frames.decoders),
            Some(Arc::clone(&frames.import)),
        );
        let imported = frames.imported() - before.0;
        let (most, differing) = difference(name, &memory, &surfaces);
        eprintln!(
            "{name}: {} rendered frames, {imported} imported, largest difference {most}, \
             {differing} samples differ",
            memory.len()
        );
        assert!(most <= 1, "{name}: largest difference {most}");
        assert_eq!(frames.fallbacks(), before.1, "{name}: fallbacks");
        let expected = if name.contains("crop-left") {
            0
        } else {
            memory.len() as u64
        };
        assert_eq!(imported, expected, "{name}: imported frames");
    }
}

#[test]
fn a_refused_import_shows_the_copy_and_is_counted() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((_, renderer, frames)) = importing() else {
        return;
    };
    let name = "h264-padded.mp4";
    let memory = render(&renderer, name, Arc::new(AppleVideo::default()), None);
    hooks::refuse_imports(1);
    let surfaces = render(
        &renderer,
        name,
        Arc::clone(&frames.decoders),
        Some(Arc::clone(&frames.import)),
    );
    hooks::refuse_imports(0);
    let (most, _) = difference(name, &memory, &surfaces);
    assert!(most <= 1, "{name}: largest difference {most}");
    assert_eq!(frames.fallbacks(), 1);
    assert_eq!(frames.imported(), memory.len() as u64 - 1);
    assert!(
        memory[0] == surfaces[0],
        "{name}: the copied frame differs from the memory path's"
    );
}

#[test]
fn a_dropped_plane_lease_lets_its_pixel_buffer_go() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((gpu, renderer, frames)) = importing() else {
        return;
    };
    let name = "h264-padded.mp4";
    let mut stream = VideoStream::open_file(
        &corpus(name),
        Arc::clone(&frames.decoders),
        StreamOptions::default(),
    )
    .expect(name);
    let size = alpha2_render::RenderSize::new(320, 180).expect("size");
    let cancel = CancelToken::default();
    let mut surfaces = Vec::new();
    for index in 0..3 {
        let frame = stream.next(&cancel).expect("decodes").expect("a frame");
        let BackendImage::Surface(lease) = &frame.image else {
            panic!("frame {index} is not a surface");
        };
        surfaces.push(Arc::downgrade(lease));
        let prepared = frames
            .import
            .prepare(
                &renderer,
                size,
                alpha2_render::ColorPolicy::RejectHdr,
                &frame.facts,
                lease,
            )
            .unwrap_or_else(|error| panic!("frame {index}: {error}"));
        gpu.device()
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(std::time::Duration::from_secs(10)),
            })
            .expect("the renderer's work finishes");
        drop(prepared);
    }
    drop(stream);
    let alive = surfaces
        .iter()
        .filter(|surface| surface.strong_count() > 0)
        .count();
    assert_eq!(alive, 0, "pixel buffers still held");
}

struct Quiet;

impl ExportProgress for Quiet {
    fn on_progress(&self, _event: ExportProgressEvent) -> Result<(), ProgressFailure> {
        Ok(())
    }
}

#[test]
fn an_export_samples_pixel_buffers_unless_its_session_copies_frames() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let imports = importing().is_some();
    let path = std::env::temp_dir().join("apple-frames.mp4");
    for (mode, surfaces) in [
        (VideoFrameMode::Cpu, false),
        (VideoFrameMode::Auto, imports),
    ] {
        let engine = Alpha2::new(EngineConfig {
            video_frames: mode,
            ..EngineConfig::default()
        });
        let request = ExportRequest {
            project_json: project(320, 180, FRAMES),
            assets: HashMap::from([(
                "v".to_owned(),
                corpus("h264-padded.mp4").to_string_lossy().into_owned(),
            )]),
            frames: HashMap::new(),
            resources: HashMap::new(),
            output_path: path.to_string_lossy().into_owned(),
            overwrite: true,
            container: None,
            video_codec: None,
            audio_codec: None,
            include_audio: false,
            start_ticks: None,
            end_ticks: None,
        };
        let handle = engine
            .export(request, Arc::new(Quiet))
            .expect("export starts");
        let outcome = pollster::block_on(handle.outcome()).expect("export");
        let report: serde_json::Value = serde_json::from_str(&outcome.report_json).expect("report");
        let frames = report["frames"].as_u64().expect("frames");
        let imported = report["surfaceFrames"].as_u64().expect("surfaceFrames");
        assert_eq!(report["surfaceFallbacks"].as_u64(), Some(0), "{report}");
        assert_eq!(
            imported,
            if surfaces { frames } else { 0 },
            "{mode:?}: {report}"
        );
    }
    let _ = std::fs::remove_file(path);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferLockBaseAddress(buffer: *mut std::ffi::c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut std::ffi::c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(buffer: *mut std::ffi::c_void, plane: usize)
    -> *const u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buffer: *mut std::ffi::c_void, plane: usize) -> usize;
}

const READ_ONLY: u64 = 1;

type Input = (Vec<u8>, Vec<u8>);

struct Inputs {
    encoder: Box<dyn alpha2_host_core::encode::VideoEncodeBackend>,
    frames: Vec<Input>,
    size: (usize, usize),
}

impl VideoSink for Inputs {
    fn write_video(
        &mut self,
        _index: u64,
        _time: MediaTime,
        _layout: PixelLayout,
        planes: &[PlaneBuffer],
    ) -> Result<(), EncodeError> {
        let (width, height) = self.size;
        let rows = |plane: &PlaneBuffer, columns: usize, count: usize| {
            plane
                .bytes
                .chunks(plane.stride)
                .take(count)
                .flat_map(|row| row[..columns].to_vec())
                .collect::<Vec<_>>()
        };
        let (u, v) = (
            rows(&planes[1], width / 2, height / 2),
            rows(&planes[2], width / 2, height / 2),
        );
        let chroma = u.iter().zip(&v).flat_map(|(u, v)| [*u, *v]).collect();
        self.frames.push((rows(&planes[0], width, height), chroma));
        Ok(())
    }

    fn lender(&mut self) -> Option<&mut dyn alpha2_host_core::export::SinkLender> {
        Some(self)
    }
}

impl alpha2_host_core::export::SinkLender for Inputs {
    fn lend(&mut self) -> Result<alpha2_host_core::encode::LentBuffer, EncodeError> {
        match self.encoder.lender().expect("lends").lend()? {
            alpha2_host_core::encode::Lend::Buffer(buffer) => Ok(buffer),
            alpha2_host_core::encode::Lend::Busy => Err(EncodeError::Adapter("busy".into())),
        }
    }

    fn write_lent(
        &mut self,
        _index: u64,
        _time: MediaTime,
        buffer: alpha2_host_core::encode::LentBuffer,
    ) -> Result<(), EncodeError> {
        let (width, height) = self.size;
        let pixels = buffer.surface();
        // SAFETY: the lent buffer keeps its pixel buffer alive; it is unlocked below, and the
        // rows read lie inside each locked plane.
        let planes = unsafe {
            assert_eq!(CVPixelBufferLockBaseAddress(pixels, READ_ONLY), 0);
            let read = |plane: usize, rows: usize, columns: usize| {
                let base = CVPixelBufferGetBaseAddressOfPlane(pixels, plane);
                let stride = CVPixelBufferGetBytesPerRowOfPlane(pixels, plane);
                (0..rows)
                    .flat_map(|row| std::slice::from_raw_parts(base.add(row * stride), columns))
                    .copied()
                    .collect::<Vec<_>>()
            };
            let planes = (read(0, height, width), read(1, height / 2, width));
            CVPixelBufferUnlockBaseAddress(pixels, READ_ONLY);
            planes
        };
        self.frames.push(planes);
        Ok(())
    }
}

fn encoder_inputs(
    renderer: &Renderer,
    frames: &GpuFrames,
    name: &str,
    lend: bool,
) -> (Vec<Input>, u64) {
    use alpha2_host_core::encode::{VideoEncodeConfig, VideoEncoders};
    let config = VideoEncodeConfig {
        codec: alpha2_media::VideoCodec::H264,
        width: 320,
        height: 180,
        frame_rate: alpha2_time::FrameRate::new(30, 1).expect("rate"),
        layout: PixelLayout::YUV420P,
        rate: alpha2_media::VideoRateControl {
            bitrate: 1_000_000,
            mode: alpha2_media::BitrateMode::Variable,
            quantizer: None,
        },
        color: alpha2_media::ColorMetadata::EXPORT,
        keyframe_interval: 30,
        software: false,
    };
    let mut sink = Inputs {
        encoder: alpha2_apple_media::AppleVideoEncoders
            .open(&config)
            .expect("an encoder"),
        frames: Vec::new(),
        size: (320, 180),
    };
    let snapshot = EngineProject::new(
        alpha2_model::Project::from_json(&project(320, 180, FRAMES)).expect("project"),
    )
    .expect("snapshot");
    let mut exporter = Exporter::for_project(
        &snapshot,
        QualityProfile::export(),
        alpha2_media::VideoCodec::H264,
    )
    .expect("exporter");
    let path = corpus(name);
    let decoders = Arc::clone(&frames.decoders);
    let mut sources = StreamSources::new(move |_: &SourceId| {
        crate::export::open_source(&path, Arc::clone(&decoders), StreamOptions::default())
    })
    .with_surface_import(Arc::clone(&frames.import));
    let mut targets = frames.encoder_targets().filter(|_| lend);
    let before = frames.rendered_in_place();
    export_video(
        &snapshot,
        &mut exporter,
        (ExportRenderer::Gpu(renderer), targets.as_deref_mut()),
        &mut TextTextures::new(),
        &mut sources,
        &mut sink,
        (&FrameBudget::new(1 << 30), &CancelToken::default()),
        &mut |_| Ok(()),
        &mut |_, _| {},
    )
    .unwrap_or_else(|error| panic!("{name}: {error}"));
    (sink.frames, frames.rendered_in_place() - before)
}

#[test]
fn frames_rendered_into_the_encoders_buffers_equal_the_planes_read_back() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let Some((_, renderer, frames)) = importing() else {
        return;
    };
    for name in ["h264-padded.mp4", "hevc-main10-padded.mp4"] {
        let (read, none) = encoder_inputs(&renderer, &frames, name, false);
        let (lent, in_place) = encoder_inputs(&renderer, &frames, name, true);
        assert_eq!(none, 0, "{name}");
        assert_eq!(
            in_place,
            read.len() as u64,
            "{name}: frames rendered in place"
        );
        assert_eq!(read.len(), lent.len(), "{name}");
        let mut most = 0;
        for (index, (read, lent)) in read.iter().zip(&lent).enumerate() {
            for (plane, (a, b)) in [(&read.0, &lent.0), (&read.1, &lent.1)]
                .into_iter()
                .enumerate()
            {
                assert_eq!(a.len(), b.len(), "{name} frame {index} plane {plane}");
                most = a
                    .iter()
                    .zip(b)
                    .map(|(a, b)| a.abs_diff(*b))
                    .fold(most, u8::max);
            }
        }
        eprintln!(
            "{name}: {} frames, largest input difference {most}",
            read.len()
        );
        assert!(most <= 1, "{name}: largest difference {most}");
    }
}

#[test]
fn an_export_renders_into_the_encoders_buffers_unless_its_session_copies_frames() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let imports = importing().is_some();
    let path = std::env::temp_dir().join("apple-frames-lent.mp4");
    for (mode, withheld, in_place) in [
        (VideoFrameMode::Cpu, false, false),
        (VideoFrameMode::Auto, true, false),
        (VideoFrameMode::Auto, false, imports),
    ] {
        crate::export::hooks::WITHHOLD_TARGETS.store(withheld, std::sync::atomic::Ordering::SeqCst);
        let engine = Alpha2::new(EngineConfig {
            video_frames: mode,
            ..EngineConfig::default()
        });
        let request = ExportRequest {
            project_json: project(320, 180, FRAMES),
            assets: HashMap::from([(
                "v".to_owned(),
                corpus("h264-padded.mp4").to_string_lossy().into_owned(),
            )]),
            frames: HashMap::new(),
            resources: HashMap::new(),
            output_path: path.to_string_lossy().into_owned(),
            overwrite: true,
            container: None,
            video_codec: None,
            audio_codec: None,
            include_audio: false,
            start_ticks: None,
            end_ticks: None,
        };
        let handle = engine
            .export(request, Arc::new(Quiet))
            .expect("export starts");
        let outcome = pollster::block_on(handle.outcome());
        crate::export::hooks::WITHHOLD_TARGETS.store(false, std::sync::atomic::Ordering::SeqCst);
        let report: serde_json::Value =
            serde_json::from_str(&outcome.expect("export").report_json).expect("report");
        let frames = report["frames"].as_u64().expect("frames");
        assert_eq!(
            report["framesRenderedInPlace"].as_u64(),
            Some(if in_place { frames } else { 0 }),
            "{mode:?} withheld {withheld}: {report}"
        );
        let decoded = decoded_frames(&path);
        assert_eq!(decoded, frames, "{mode:?} withheld {withheld}");
    }
    let _ = std::fs::remove_file(path);
}

fn decoded_frames(path: &std::path::Path) -> u64 {
    let mut stream = VideoStream::open_file(
        path,
        Arc::new(AppleVideo::default()),
        StreamOptions::default(),
    )
    .expect("the output opens");
    let cancel = CancelToken::default();
    let mut count = 0;
    while stream.next(&cancel).expect("decodes").is_some() {
        count += 1;
    }
    count
}
