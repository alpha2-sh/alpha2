//! Rendering a project into a file with a platform's decoders and encoders: the container layer
//! reads sources and writes the output, and the platform's codecs do the rest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_container::{MatroskaFlavor, MatroskaWriter, Mp4Flavor, Mp4Writer};
use alpha2_engine::{
    EngineError, EngineProject, ErrorCode, Exporter, QualityProfile, TextTextures,
};
use alpha2_host_core::audio_stream::{AudioBackends, AudioStream};
use alpha2_host_core::audio_windows::{AudioWindowError, OpenAudio, WindowedAudioSources};
use alpha2_host_core::encode::{
    AudioEncodeConfig, AudioEncoders, EncodeSink, VideoEncodeConfig, VideoEncoders,
};
use alpha2_host_core::export::{
    ExportError, ExportRenderer, SourceStream, StreamSources, export_video,
};
use alpha2_host_core::partial::{OutputTarget, PartialOutput, SeekNeed};
use alpha2_host_core::providers::{ProviderFiles, ProviderMedia};
use alpha2_host_core::stills::{decode_still, still_format};
use alpha2_host_core::video_stream::{DecodePreference, StreamOptions, VideoBackends, VideoStream};
use alpha2_host_core::{DecodePolicy, ProjectAudioRequest};
use alpha2_media::{
    AudioBlock, AudioBlockSink, CancelToken, Container, EncodeError, OutputPixels, QualityLevel,
    SourceId, SpeakerLayout,
};
use alpha2_model::{MediaKind, MediaSource, Project};
use alpha2_render::{CpuRenderer, Renderer};
use serde_json::json;

use crate::error::Alpha2Error;
use crate::platform::ExportJob;
use crate::session::{ExportOutcome, ExportProgressEvent, RendererMode, VideoFrameMode};
use crate::zero_copy::GpuFrames;

/// A platform's codec factories.
pub(crate) struct MediaBackends {
    pub video_decoders: Arc<dyn VideoBackends>,
    pub audio_decoders: Arc<dyn AudioBackends>,
    pub video_encoders: Arc<dyn VideoEncoders>,
    pub audio_encoders: Arc<dyn AudioEncoders>,
}

/// Video streams a source reader may keep open beyond the most its timeline reads at once.
pub(crate) const SPARE_SESSIONS: usize = 2;

/// Samples per block of the offline mix.
const AUDIO_BLOCK: usize = 4096;

/// Bytes the open extension sources may hold, as the desktop host's default.
const PROVIDER_SOURCE_BYTES: u64 = 512 << 20;

/// Exports `job`, again on the software video encoder when the hardware video encoder failed.
pub(crate) fn export(job: &ExportJob, media: &MediaBackends) -> Result<ExportOutcome, Alpha2Error> {
    match attempt(job, media, false) {
        Err(Failure::HardwareEncoder(error)) if !job.cancel.is_cancelled() => {
            alpha2_os::diagnostic!(
                "alpha2 mobile export: the hardware video encoder failed ({error}); exporting again in software"
            );
            attempt(job, media, true).map_err(Failure::into_error)
        }
        outcome => outcome.map_err(Failure::into_error),
    }
}

/// Why an attempt stopped.
enum Failure {
    /// The hardware video encoder failed; the software one may succeed.
    HardwareEncoder(EncodeError),
    /// Anything else.
    Other(Alpha2Error),
}

impl Failure {
    fn into_error(self) -> Alpha2Error {
        match self {
            Self::HardwareEncoder(error) => error.into(),
            Self::Other(error) => error,
        }
    }
}

impl From<Alpha2Error> for Failure {
    fn from(error: Alpha2Error) -> Self {
        Self::Other(error)
    }
}

pub(crate) fn failed(code: &str, reason: impl Into<String>) -> Alpha2Error {
    Alpha2Error::Failed {
        details: None,
        code: code.into(),
        reason: reason.into(),
    }
}

/// The writer of `container` into `file`.
fn writer(container: Container, file: std::fs::File) -> Box<dyn alpha2_container::ContainerWriter> {
    match container {
        Container::Mp4 => Box::new(Mp4Writer::create(file, Mp4Flavor::Mp4)),
        Container::Mov => Box::new(Mp4Writer::create(file, Mp4Flavor::Mov)),
        Container::Mkv => Box::new(MatroskaWriter::create(file, MatroskaFlavor::Matroska)),
        Container::Webm => Box::new(MatroskaWriter::create(file, MatroskaFlavor::WebM)),
    }
}

/// Registers the project's effect resources (3D LUTs) from their asset files with `drawing`.
pub(crate) fn register_resources(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    drawing: &mut Drawing,
) -> Result<(), Alpha2Error> {
    for resource in &project.effect_resources {
        let path = assets.get(resource.id.as_str()).ok_or_else(|| {
            Alpha2Error::invalid(format!("effect resource {} has no asset path", resource.id))
        })?;
        let text = std::fs::read_to_string(path).map_err(|error| {
            Alpha2Error::invalid(format!("effect resource {}: {error}", resource.id))
        })?;
        let data = alpha2_render::Lut3dData::from_cube(&text).map_err(|error| {
            Alpha2Error::invalid(format!("effect resource {}: {error}", resource.id))
        })?;
        drawing
            .register_lut3d(resource.id.as_str(), &data)
            .map_err(|reason| failed("RENDER_FAILED", reason))?;
    }
    Ok(())
}

/// Text rendering with the session's fonts, each under every family name it has.
pub(crate) fn text_textures(
    fonts: &[crate::session::LoadedFont],
) -> Result<TextTextures, Alpha2Error> {
    let mut texts = TextTextures::new();
    for font in fonts {
        for family in &font.families {
            texts
                .add_font(family.clone(), Arc::clone(&font.bytes), font.face_index)
                .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
        }
    }
    Ok(texts)
}

/// The renderer an export draws with; the GPU renderer's frames stay on its device when it
/// imports decoder surfaces. The CPU renderer keeps why no GPU was used when one was allowed.
pub(crate) enum Drawing {
    Gpu(Box<Renderer>, Option<GpuFrames>),
    Cpu(Box<CpuRenderer>, Option<String>),
}

impl Drawing {
    pub(crate) fn open(mode: RendererMode, frames: VideoFrameMode) -> Result<Self, Alpha2Error> {
        let cpu = |unavailable| {
            let threads = std::thread::available_parallelism().map_or(2, usize::from);
            Self::Cpu(Box::new(CpuRenderer::new(threads)), unavailable)
        };
        if mode == RendererMode::Cpu {
            return Ok(cpu(None));
        }
        match pollster::block_on(alpha2_gpu::GpuContext::request_product()) {
            Ok(gpu) => {
                let (gpu, frames) = crate::zero_copy::open(gpu, frames == VideoFrameMode::Auto);
                gpu.set_diagnostic_handler(|diagnostic| {
                    alpha2_os::diagnostic!("alpha2 export: GPU: {diagnostic}");
                });
                let mut renderer = Renderer::new(gpu);
                // The CPU renderer has the built-in effects; the GPU one compiles them here.
                renderer.register_builtins().map_err(|diagnostics| {
                    failed(
                        "RENDER_FAILED",
                        format!("built-in effects failed to compile: {diagnostics:?}"),
                    )
                })?;
                Ok(Self::Gpu(Box::new(renderer), frames))
            }
            Err(error) if mode == RendererMode::Gpu => Err(Alpha2Error::GpuInitFailed {
                reason: error.to_string(),
            }),
            Err(error) => {
                alpha2_os::diagnostic!("alpha2 export: no GPU, drawing on the CPU: {error}");
                Ok(cpu(Some(error.to_string())))
            }
        }
    }

    /// Why no GPU draws, when one was allowed and none could be used.
    pub(crate) fn gpu_unavailable(&self) -> Option<&str> {
        match self {
            Self::Cpu(_, unavailable) => unavailable.as_deref(),
            Self::Gpu(..) => None,
        }
    }

    pub(crate) fn as_export(&self) -> ExportRenderer<'_> {
        match self {
            Self::Gpu(renderer, _) => ExportRenderer::Gpu(renderer),
            Self::Cpu(renderer, _) => ExportRenderer::Cpu(renderer),
        }
    }

    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Gpu(..) => "gpu",
            Self::Cpu(..) => "cpu",
        }
    }

    pub(crate) const fn gpu_frames(&self) -> Option<&GpuFrames> {
        match self {
            Self::Gpu(_, frames) => frames.as_ref(),
            Self::Cpu(..) => None,
        }
    }

    fn register_lut3d(&mut self, id: &str, data: &alpha2_render::Lut3dData) -> Result<(), String> {
        match self {
            Self::Gpu(renderer, _) => renderer.register_lut3d(id, data),
            Self::Cpu(renderer, _) => renderer.register_lut3d(id, data),
        }
        .map_err(|error| error.to_string())
    }
}

#[allow(clippy::too_many_lines)]
fn attempt(
    job: &ExportJob,
    media: &MediaBackends,
    software: bool,
) -> Result<ExportOutcome, Failure> {
    let request = &job.request;
    let project = Project::from_json(&request.project_json)
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let snapshot = EngineProject::new(project.clone())
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let output = PathBuf::from(&request.output_path);
    let (container, video_codec, audio_codec) = alpha2_media::resolve_export_format(
        request
            .container
            .as_deref()
            .map(parse_container)
            .transpose()?,
        output.extension().and_then(std::ffi::OsStr::to_str),
        request
            .video_codec
            .as_deref()
            .map(parse_video)
            .transpose()?,
        request
            .audio_codec
            .as_deref()
            .map(parse_audio)
            .transpose()?,
    )
    .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let pixels = alpha2_media::resolve_pixels(video_codec, container, OutputPixels::default())
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let settings = &project.settings;
    let rate = settings.frame_rate;
    let range = export_range(&snapshot, request.start_ticks, request.end_ticks)?;
    let mut exporter = Exporter::new(
        &snapshot,
        range,
        rate,
        QualityProfile::export(),
        video_codec,
    )
    .map_err(|error| match &error {
        // A composition that cannot draw fails with the engine's code, as on the desktop.
        EngineError::Evaluate(_) => failed(
            ErrorCode::of(&error).map_or("RENDER_FAILED", ErrorCode::as_str),
            error.to_string(),
        ),
        _ => Alpha2Error::invalid(error.to_string()),
    })?
    .with_pixels(pixels);
    let size = exporter
        .render_size(&snapshot)
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let assets = request
        .assets
        .iter()
        .map(|(id, path)| (id.clone(), PathBuf::from(path)))
        .collect::<HashMap<_, _>>();

    hold(job)?;
    let mut drawing = Drawing::open(job.config.renderer, job.config.video_frames)?;
    register_resources(&project, &assets, &mut drawing)?;
    let mut texts = text_textures(&job.fonts)?;

    // The mix is encoded first; the sink holds its packets and writes them in step with the video.
    let has_audio = request.include_audio && project.sources.iter().any(MediaSource::is_audible);
    let mut audio_sources = if has_audio {
        Some(audio_windows(
            &project,
            &assets,
            Arc::clone(&media.audio_decoders),
            (settings.sample_rate, AUDIO_WINDOW_BYTES),
        )?)
    } else {
        None
    };

    let target = OutputTarget {
        path: output.clone(),
        existing: if request.overwrite {
            alpha2_host_core::partial::ExistingOutput::Replace
        } else {
            alpha2_host_core::partial::ExistingOutput::Refuse
        },
    };
    let encode = |error: EncodeError| Failure::Other(error.into());
    let mut partial = PartialOutput::claim(&target, SeekNeed::Required).map_err(encode)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(partial.write_path())
        .map_err(|error| encode(EncodeError::Output(error.to_string())))?;

    let video_config = VideoEncodeConfig {
        codec: video_codec,
        width: size.width,
        height: size.height,
        frame_rate: rate,
        layout: exporter.pixels(),
        rate: alpha2_media::resolve_video_rate_control(
            video_codec,
            size.width,
            size.height,
            rate,
            QualityLevel::default(),
        )
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?,
        color: exporter.encoding().color,
        keyframe_interval: u32::try_from(alpha2_host_core::encode_policy::keyframe_interval(rate))
            .unwrap_or(1),
        software,
    };
    let video_encoder = media.video_encoders.open(&video_config).map_err(encode)?;
    let hardware = video_encoder.hardware();
    let video_encoder_name = video_encoder.name().to_owned();
    let encode = |error: EncodeError| match error {
        EncodeError::EncoderRejected { .. } if hardware => Failure::HardwareEncoder(error),
        error => Failure::Other(error.into()),
    };
    let audio = if has_audio {
        let config = AudioEncodeConfig {
            codec: audio_codec,
            project_rate: settings.sample_rate,
            speakers: SpeakerLayout::STEREO,
            bitrate: alpha2_media::resolve_audio_bitrate(audio_codec, QualityLevel::default()),
        };
        Some((media.audio_encoders.open(&config).map_err(encode)?, config))
    } else {
        None
    };
    let audio_encoder_name = audio.as_ref().map(|(encoder, _)| encoder.name().to_owned());
    let audio_bitrate = audio.as_ref().map(|(encoder, _)| encoder.bitrate());
    let mut sink = EncodeSink::new(
        writer(container, file),
        video_encoder,
        video_config,
        audio,
        job.cancel.clone(),
    );

    if let Some(sources) = audio_sources.as_mut() {
        let window = exporter
            .audio_window(settings.sample_rate)
            .map_err(|error| failed("RENDER_FAILED", error.to_string()))?;
        alpha2_host_core::render_project_audio(
            &snapshot,
            &ProjectAudioRequest {
                channels: 2,
                start_sample: window.start_sample,
                total_frames: window.frames,
                block_sizes: &[AUDIO_BLOCK],
            },
            sources,
            &mut Held {
                sink: &mut sink,
                job,
            },
            &job.cancel,
        )
        .map_err(|error| match error {
            alpha2_host_core::OfflineAudioError::Sink(error) => encode(error),
            alpha2_host_core::OfflineAudioError::Sources(AudioWindowError::MissingInput(why)) => {
                Failure::Other(failed("MISSING_INPUT", why))
            }
            alpha2_host_core::OfflineAudioError::Sources(AudioWindowError::ResourceLimit(why)) => {
                Failure::Other(failed("RESOURCE_LIMIT", why))
            }
            error => Failure::Other(failed("AUDIO_RENDER_FAILED", error.to_string())),
        })?;
    }

    let preference = crate::session::decode_preference(job.config.video_decode);
    let providers = Arc::new(ProviderMedia::new(
        &project,
        provider_files(job),
        (size.width, size.height),
        PROVIDER_SOURCE_BYTES,
    ));
    let gpu_frames = drawing.gpu_frames();
    let decoders = gpu_frames.map_or_else(
        || Arc::clone(&media.video_decoders),
        |frames| Arc::clone(&frames.decoders),
    );
    // The limit sits above the peak of cursors reading at once, so it only catches a leak.
    let (peak, reads) = alpha2_host_core::cursor::visual_decode_reads(&project, range);
    let mut sources = StreamSources::new(source_opener(
        &project,
        assets.clone(),
        Some(decoders),
        preference,
    ))
    .with_providers(providers)
    .with_reads(reads)
    .with_session_limit(peak + SPARE_SESSIONS);
    if let Some(gpu_frames) = gpu_frames {
        sources = sources.with_surface_import(Arc::clone(&gpu_frames.import));
    }
    let mut targets = gpu_frames
        .and_then(GpuFrames::encoder_targets)
        .filter(|_| !withheld_targets());
    let mut frames = 0;
    export_video(
        &snapshot,
        &mut exporter,
        (drawing.as_export(), targets.as_deref_mut()),
        &mut texts,
        &mut sources,
        &mut sink,
        (&job.budget, &job.cancel),
        &mut |sink| {
            // No frame stays inside the encoder while the app is away.
            if job.paused.is_paused() {
                sink.settle()?;
            }
            job.paused.wait_while_paused(&job.cancel, &job.held);
            Ok(())
        },
        &mut |done, total| {
            frames = done;
            job.progress.report(
                ExportProgressEvent {
                    frames_done: done,
                    frames_total: total,
                },
                &job.cancel,
            );
        },
    )
    .map_err(|error| match error {
        ExportError::Cancelled => Failure::Other(Alpha2Error::Aborted),
        ExportError::Encode(error) => encode(error),
        ExportError::Source { source, reason } => Failure::Other(failed(
            "SOURCE_FAILED",
            format!("source {source}: {reason}"),
        )),
        error => Failure::Other(failed("RENDER_FAILED", error.to_string())),
    })?;
    let decode_diagnostics = layer_diagnostics(sources.dropped_layers());
    drop(sources);
    let surface_frames = drawing.gpu_frames().map_or(0, GpuFrames::imported);
    let surface_fallbacks = drawing.gpu_frames().map_or(0, GpuFrames::fallbacks);
    let in_place = drawing.gpu_frames().map_or(0, GpuFrames::rendered_in_place);
    sink.finish().map_err(encode)?;
    partial.finish(&job.cancel).map_err(encode)?;
    let report = json!({
        "container": container.as_str(),
        "videoCodec": video_codec.as_str(),
        "videoEncoder": video_encoder_name,
        "hardwareEncoder": hardware,
        "softwareRetry": software,
        "audioCodec": audio_encoder_name.as_ref().map(|_| audio_codec.as_str()),
        "audioEncoder": audio_encoder_name,
        "audioBitrate": audio_bitrate,
        "renderer": drawing.name(),
        "gpuUnavailable": drawing.gpu_unavailable(),
        "surfaceFrames": surface_frames,
        "surfaceFallbacks": surface_fallbacks,
        "framesRenderedInPlace": in_place,
        "decodeDiagnostics": decode_diagnostics,
        "frames": frames,
        "width": size.width,
        "height": size.height,
    });
    Ok(ExportOutcome {
        output_path: request.output_path.clone(),
        report_json: report.to_string(),
    })
}

#[cfg(test)]
fn withheld_targets() -> bool {
    hooks::WITHHOLD_TARGETS.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(test))]
const fn withheld_targets() -> bool {
    false
}

#[cfg(test)]
pub(crate) mod hooks {
    use std::sync::atomic::AtomicBool;

    pub(crate) static WITHHOLD_TARGETS: AtomicBool = AtomicBool::new(false);
}

/// Waits while the session is paused; [`Alpha2Error::Aborted`] once the export is cancelled.
fn hold(job: &ExportJob) -> Result<(), Alpha2Error> {
    job.paused.wait_while_paused(&job.cancel, &job.held);
    if job.cancel.is_cancelled() {
        return Err(Alpha2Error::Aborted);
    }
    Ok(())
}

/// The sink of the audio phase, which waits while the session is paused before each block.
struct Held<'a> {
    sink: &'a mut EncodeSink,
    job: &'a ExportJob,
}

impl AudioBlockSink for Held<'_> {
    fn write_audio(&mut self, block: &AudioBlock, cancel: &CancelToken) -> Result<(), EncodeError> {
        self.job
            .paused
            .wait_while_paused(&self.job.cancel, &self.job.held);
        self.sink.write_audio(block, cancel)
    }

    fn finish_audio(&mut self, cancel: &CancelToken) -> Result<(), EncodeError> {
        self.sink.finish_audio(cancel)
    }

    fn abort_audio(&mut self) -> Result<(), EncodeError> {
        self.sink.abort_audio()
    }
}

/// The desktop export report's `layerDropped` entries for `dropped`.
fn layer_diagnostics(
    dropped: &std::collections::BTreeMap<SourceId, alpha2_host_core::video_stream::DroppedLayer>,
) -> serde_json::Value {
    dropped
        .iter()
        .map(|(source, dropped)| {
            json!({
                "type": "layerDropped",
                "code": dropped.code,
                "source": source.as_str(),
                "reason": dropped.reason,
            })
        })
        .collect()
}

/// The files the request names for the extension sources.
fn provider_files(job: &ExportJob) -> ProviderFiles {
    let request = &job.request;
    files_of(
        &request.assets,
        &request.frames,
        &request.resources,
        &job.fonts,
    )
}

/// The provider files of a request's assets, sequence frames and resources, with the session's
/// fonts.
pub(crate) fn files_of(
    assets: &HashMap<String, String>,
    frames: &HashMap<String, Vec<String>>,
    resources: &HashMap<String, HashMap<String, String>>,
    fonts: &[crate::session::LoadedFont],
) -> ProviderFiles {
    ProviderFiles {
        originals: assets
            .iter()
            .map(|(id, path)| (id.clone(), PathBuf::from(path)))
            .collect(),
        frames: frames
            .iter()
            .map(|(id, paths)| {
                let paths = paths
                    .iter()
                    .map(|path| (!path.is_empty()).then(|| PathBuf::from(path)))
                    .collect();
                (id.clone(), paths)
            })
            .collect(),
        fonts: fonts.iter().map(|font| Arc::clone(&font.bytes)).collect(),
        resources: resources
            .iter()
            .map(|(id, files)| {
                let files = files
                    .iter()
                    .map(|(locator, path)| (locator.clone(), PathBuf::from(path)))
                    .collect();
                (id.clone(), files)
            })
            .collect(),
    }
}

fn parse_container(name: &str) -> Result<Container, Alpha2Error> {
    Container::ALL
        .into_iter()
        .find(|container| container.as_str() == name)
        .ok_or_else(|| Alpha2Error::invalid(format!("container: unknown container {name}")))
}

fn parse_video(name: &str) -> Result<alpha2_media::VideoCodec, Alpha2Error> {
    serde_json::from_value(json!(name))
        .map_err(|_| Alpha2Error::invalid(format!("videoCodec: unknown codec {name}")))
}

fn parse_audio(name: &str) -> Result<alpha2_media::AudioCodec, Alpha2Error> {
    serde_json::from_value(json!(name))
        .map_err(|_| Alpha2Error::invalid(format!("audioCodec: unknown codec {name}")))
}

/// The request's ticks to export, each unset end taken from the project's default export range.
fn export_range(
    snapshot: &EngineProject,
    start: Option<i64>,
    end: Option<i64>,
) -> Result<alpha2_time::TimeRange, Alpha2Error> {
    let whole = snapshot
        .project()
        .default_export_range()
        .ok_or_else(|| Alpha2Error::invalid("the timeline is empty"))?;
    let start = start.map_or(whole.start(), alpha2_time::MediaTime::from_ticks);
    let end = end.map_or(whole.end(), alpha2_time::MediaTime::from_ticks);
    alpha2_time::TimeRange::new(start, end)
        .map_err(|error| Alpha2Error::invalid(format!("export range: {error}")))
}

/// The still picture at `path`, if the file is one.
///
/// # Errors
/// The file does not read, or a still does not decode.
pub(crate) fn open_still(path: &Path) -> Result<Option<SourceStream>, String> {
    let mut head = [0; 12];
    let read = std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if still_format(&head[..read]).is_none() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let still = decode_still(&bytes).map_err(|error| error.to_string())?;
    Ok(Some(SourceStream::Still(Arc::new(
        still.into_video_frame(),
    ))))
}

/// A source file as frames: a still picture, or the default video track of a container.
pub(crate) fn open_source(
    path: &Path,
    decoders: Arc<dyn VideoBackends>,
    options: StreamOptions,
) -> Result<SourceStream, String> {
    if let Some(still) = open_still(path)? {
        return Ok(still);
    }
    VideoStream::open_file(path, decoders, options)
        .map(|stream| SourceStream::Video(Box::new(stream)))
        .map_err(|error| error.to_string())
}

/// Opens the project's sources as frames, for an export, a preview or stills alike: audio-only
/// sources have no pictures, files open as stills or through `video`'s decoders, and without
/// decoders only stills open.
pub(crate) fn source_opener(
    project: &Project,
    assets: HashMap<String, PathBuf>,
    video: Option<Arc<dyn VideoBackends>>,
    preference: DecodePreference,
) -> impl FnMut(&SourceId) -> Result<SourceStream, String> + Send + 'static {
    let declared = project
        .sources
        .iter()
        .map(|source| (source.id.clone(), (source.kind, source.color_metadata)))
        .collect::<HashMap<_, _>>();
    move |id: &SourceId| {
        let (kind, color) = declared
            .get(id)
            .copied()
            .unwrap_or((MediaKind::Video, None));
        if kind == MediaKind::Audio {
            return Ok(SourceStream::AudioOnly);
        }
        let path = assets
            .get(id.as_str())
            .ok_or_else(|| format!("source {id} has no asset path"))?;
        match &video {
            Some(video) => open_source(
                path,
                Arc::clone(video),
                StreamOptions {
                    preference,
                    policy: DecodePolicy::default(),
                    declared_color: color,
                },
            ),
            None => open_still(path)?
                .ok_or_else(|| format!("source {id}: this platform has no video decoders")),
        }
    }
}

/// Whether the file at `path` opens as a still, a video or an audio source here.
pub(crate) fn probe_source(path: &Path, media: &MediaBackends) -> Result<(), String> {
    let video = open_source(
        path,
        Arc::clone(&media.video_decoders),
        StreamOptions::default(),
    );
    if video.is_ok() {
        return Ok(());
    }
    AudioStream::open_file(
        path,
        media.audio_decoders.as_ref(),
        48_000,
        None,
        DecodePolicy::default(),
    )
    .map(|_| ())
    .map_err(|audio| {
        format!(
            "no video ({}) and no audio ({audio})",
            video.err().unwrap_or_default()
        )
    })
}

/// Most bytes of decoded audio an export's mix holds at once, as the desktop host's default.
const AUDIO_WINDOW_BYTES: u64 = 256 << 20;

/// Windows over the audio of every audible source the timeline uses, decoded at the project rate
/// as the mix reaches it, within a quarter of the memory free now and at most `ceiling` bytes.
///
/// # Errors
/// `MISSING_INPUT` for a used audible source without an asset.
pub(crate) fn audio_windows(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    decoders: Arc<dyn AudioBackends>,
    (project_rate, ceiling): (u32, u64),
) -> Result<WindowedAudioSources<AudioStream>, Alpha2Error> {
    let mut paths = HashMap::new();
    for source in project
        .used_sources()
        .into_iter()
        .filter(|source| source.is_audible())
    {
        let path = assets.get(source.id.as_str()).ok_or_else(|| {
            failed(
                "MISSING_INPUT",
                format!("audio source {} has no asset path", source.id),
            )
        })?;
        paths.insert(source.id.clone(), path.clone());
    }
    let bytes = alpha2_os::memory_capacity()
        .headroom()
        .map_or(ceiling, |free| (free / 4).min(ceiling));
    let open: OpenAudio<AudioStream> = Box::new(move |source, stream| {
        let path = paths.get(source).ok_or_else(|| {
            AudioWindowError::MissingInput(format!("audio source {source} has no asset path"))
        })?;
        AudioStream::open_file(
            path,
            decoders.as_ref(),
            project_rate,
            stream,
            DecodePolicy::default(),
        )
        .map_err(AudioWindowError::Decode)
    });
    Ok(WindowedAudioSources::new(project_rate, bytes, open))
}

#[cfg(test)]
mod tests {
    use alpha2_host_core::encode::{AudioEncodeBackend, EncodePoll, VideoEncodeBackend};

    use super::*;

    /// Reads nothing itself; PCM goes through `WithPcm`.
    struct NoDecoders;

    impl AudioBackends for NoDecoders {
        fn open(
            &self,
            _track: &alpha2_container::TrackInfo,
        ) -> Result<
            Box<dyn alpha2_host_core::audio_stream::AudioDecodeBackend>,
            alpha2_host_core::video_stream::BackendError,
        > {
            Err(alpha2_host_core::video_stream::BackendError::Unsupported(
                "PCM only".into(),
            ))
        }
    }

    /// 10 s of 16-bit mono silence at 8 kHz, which decodes to 1.92 MB of samples at 48 kHz.
    fn silence(path: &Path) {
        let bytes: u32 = 8_000 * 10 * 2;
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend((36 + bytes).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16_u32.to_le_bytes());
        wav.extend([1, 0, 1, 0]);
        wav.extend(8_000_u32.to_le_bytes());
        wav.extend(16_000_u32.to_le_bytes());
        wav.extend([2, 0, 16, 0]);
        wav.extend(b"data");
        wav.extend(bytes.to_le_bytes());
        wav.resize(wav.len() + bytes as usize, 0);
        std::fs::write(path, wav).expect("wav");
    }

    #[test]
    fn an_export_without_a_range_starts_at_zero() {
        let tests = std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests"),
            PathBuf::from,
        );
        let mut project = Project::from_json(
            &std::fs::read_to_string(tests.join("cases/host-video-export-project.json"))
                .expect("project"),
        )
        .expect("loads");
        let two_seconds = 2 * alpha2_time::TICKS_PER_SECOND;
        let clip = &mut project.timeline.tracks[0].clips[0];
        clip.timeline_range = alpha2_time::TimeRange::new(
            alpha2_time::MediaTime::from_ticks(two_seconds),
            alpha2_time::MediaTime::from_ticks(two_seconds + 120_000),
        )
        .expect("range");
        let snapshot = EngineProject::new(project).expect("snapshot");
        let range = |start, end| {
            export_range(&snapshot, start, end)
                .map(|range| (range.start().ticks(), range.end().ticks()))
                .ok()
        };
        assert_eq!(range(None, None), Some((0, two_seconds + 120_000)));
        assert_eq!(
            range(Some(two_seconds), None),
            Some((two_seconds, two_seconds + 120_000))
        );
        assert_eq!(range(None, Some(60_000)), Some((0, 60_000)));
    }

    /// Video encoders whose hardware sessions fail at a frame, and an audio encoder that fails
    /// when asked to; both record what they opened.
    #[derive(Default)]
    struct Encoders {
        /// Hardware video sessions fail to submit this frame.
        hardware_fails_at: Option<u64>,
        /// The audio encoder fails every frame.
        audio_fails: bool,
        /// Whether each video encoder opened was asked for software.
        video_opened: std::sync::Mutex<Vec<bool>>,
        audio_opened: std::sync::atomic::AtomicUsize,
    }

    struct FakeVideo {
        hardware: bool,
        fails_at: Option<u64>,
        submitted: u64,
        queued: std::collections::VecDeque<i64>,
        ended: bool,
        given: bool,
    }

    impl VideoEncodeBackend for FakeVideo {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn hardware(&self) -> bool {
            self.hardware
        }

        fn timescale(&self) -> u32 {
            90_000
        }

        fn submit(
            &mut self,
            pts: i64,
            _planes: &[alpha2_host_core::encode::PlaneBuffer],
            _keyframe: bool,
        ) -> Result<alpha2_host_core::video_stream::Submit, EncodeError> {
            if self.hardware && self.fails_at == Some(self.submitted) {
                return Err(EncodeError::Adapter("the hardware session broke".into()));
            }
            self.submitted += 1;
            self.queued.push_back(pts);
            Ok(alpha2_host_core::video_stream::Submit::Accepted)
        }

        fn receive(&mut self, _wait: bool) -> Result<EncodePoll, EncodeError> {
            Ok(match self.queued.pop_front() {
                Some(pts) => {
                    self.given = true;
                    EncodePoll::Packet(alpha2_host_core::encode::EncodedPacket {
                        pts,
                        dts: pts,
                        keyframe: true,
                        data: bytes::Bytes::from_static(&[1]),
                    })
                }
                None if self.ended => EncodePoll::Drained,
                None => EncodePoll::Pending,
            })
        }

        fn finish_input(&mut self) -> Result<(), EncodeError> {
            self.ended = true;
            Ok(())
        }

        fn codec_private(&self) -> Option<bytes::Bytes> {
            self.given.then(|| bytes::Bytes::from_static(&[1, 0, 0, 0]))
        }

        fn applied_rate_control(&self) -> alpha2_host_core::rate_control::AppliedRateControl {
            alpha2_host_core::rate_control::AppliedRateControl::Bitrate
        }
    }

    struct FakeAudio {
        fails: bool,
        queued: std::collections::VecDeque<i64>,
        ended: bool,
    }

    impl AudioEncodeBackend for FakeAudio {
        fn name(&self) -> &'static str {
            "fake audio"
        }

        fn sample_rate(&self) -> u32 {
            48_000
        }

        fn frame_size(&self) -> usize {
            1_024
        }

        fn bitrate(&self) -> u32 {
            128_000
        }

        fn submit(
            &mut self,
            pts: i64,
            _planes: &[Vec<f32>],
        ) -> Result<alpha2_host_core::video_stream::Submit, EncodeError> {
            if self.fails {
                return Err(EncodeError::Adapter("the audio session broke".into()));
            }
            self.queued.push_back(pts);
            Ok(alpha2_host_core::video_stream::Submit::Accepted)
        }

        fn receive(&mut self, _wait: bool) -> Result<EncodePoll, EncodeError> {
            Ok(match self.queued.pop_front() {
                Some(pts) => EncodePoll::Packet(alpha2_host_core::encode::EncodedPacket {
                    pts,
                    dts: pts,
                    keyframe: true,
                    data: bytes::Bytes::from_static(&[2]),
                }),
                None if self.ended => EncodePoll::Drained,
                None => EncodePoll::Pending,
            })
        }

        fn finish_input(&mut self) -> Result<(), EncodeError> {
            self.ended = true;
            Ok(())
        }

        fn codec_private(&self) -> Option<bytes::Bytes> {
            Some(bytes::Bytes::from_static(&[0x11, 0x90]))
        }

        fn priming(&self) -> u32 {
            0
        }
    }

    impl VideoEncoders for Encoders {
        fn open(
            &self,
            config: &VideoEncodeConfig,
        ) -> Result<Box<dyn VideoEncodeBackend>, EncodeError> {
            self.video_opened
                .lock()
                .expect("lock")
                .push(config.software);
            Ok(Box::new(FakeVideo {
                hardware: !config.software,
                fails_at: self.hardware_fails_at,
                submitted: 0,
                queued: std::collections::VecDeque::new(),
                ended: false,
                given: false,
            }))
        }
    }

    impl AudioEncoders for Encoders {
        fn open(
            &self,
            _config: &AudioEncodeConfig,
        ) -> Result<Box<dyn AudioEncodeBackend>, EncodeError> {
            self.audio_opened
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Box::new(FakeAudio {
                fails: self.audio_fails,
                queued: std::collections::VecDeque::new(),
                ended: false,
            }))
        }
    }

    impl VideoBackends for NoDecoders {
        fn open(
            &self,
            _track: &alpha2_container::TrackInfo,
            _kind: alpha2_host_core::video_stream::BackendKind,
        ) -> Result<
            Box<dyn alpha2_host_core::video_stream::VideoDecodeBackend>,
            alpha2_host_core::video_stream::BackendError,
        > {
            Err(alpha2_host_core::video_stream::BackendError::Unsupported(
                "no decoders".into(),
            ))
        }
    }

    struct Quiet;

    impl crate::ExportProgress for Quiet {
        fn on_progress(
            &self,
            _event: crate::ExportProgressEvent,
        ) -> Result<(), crate::ProgressFailure> {
            Ok(())
        }
    }

    /// Exports a still and `audio` to `name`.mkv with `encoders` and the CPU renderer.
    fn export_with(
        encoders: &Arc<Encoders>,
        name: &str,
        audio: &Path,
    ) -> (PathBuf, Result<ExportOutcome, Alpha2Error>) {
        let tests = std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests"),
            PathBuf::from,
        );
        let output =
            std::env::temp_dir().join(format!("alpha2-mobile-{}-{name}.mkv", std::process::id()));
        let _ = std::fs::remove_file(&output);
        let path = |path: &Path| path.to_string_lossy().into_owned();
        let request = crate::ExportRequest {
            project_json: std::fs::read_to_string(
                tests.join("cases/host-audio-export-project.json"),
            )
            .expect("project"),
            assets: HashMap::from([
                (
                    "image-source".to_owned(),
                    path(&tests.join("corpus/cross_topology_parity.png")),
                ),
                ("audio-source".to_owned(), path(audio)),
            ]),
            output_path: path(&output),
            video_codec: Some("vp9".into()),
            audio_codec: Some("aac".into()),
            include_audio: true,
            ..crate::ExportRequest::default()
        };
        (output, run(encoders, request))
    }

    /// Runs `request` on this thread with `encoders` and the CPU renderer.
    fn run(
        encoders: &Arc<Encoders>,
        request: crate::ExportRequest,
    ) -> Result<ExportOutcome, Alpha2Error> {
        let job = ExportJob {
            request,
            config: crate::EngineConfig {
                renderer: crate::RendererMode::Cpu,
                ..crate::EngineConfig::default()
            },
            fonts: Vec::new(),
            cancel: CancelToken::default(),
            paused: Arc::default(),
            held: Arc::default(),
            budget: alpha2_host_core::export::FrameBudget::new(1 << 30),
            progress: Arc::new(crate::session::Progress::new(Arc::new(Quiet))),
        };
        let media = MediaBackends {
            video_decoders: Arc::new(NoDecoders),
            audio_decoders: Arc::new(alpha2_host_core::audio_stream::WithPcm(NoDecoders)),
            video_encoders: Arc::clone(encoders) as Arc<dyn VideoEncoders>,
            audio_encoders: Arc::clone(encoders) as Arc<dyn AudioEncoders>,
        };
        export(&job, &media)
    }

    fn opened(encoders: &Encoders) -> (Vec<bool>, usize) {
        (
            encoders.video_opened.lock().expect("lock").clone(),
            encoders
                .audio_opened
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    #[test]
    fn an_export_whose_late_composition_cannot_draw_fails_before_its_first_frame() {
        let second = alpha2_time::TICKS_PER_SECOND;
        let white = json!({"type": "fill", "paint": {"type": "solid",
            "color": {"red": 1, "green": 1, "blue": 1, "alpha": 1}}});
        let project = json!({
            "schema_version": 1, "id": "late", "name": "Late",
            "settings": {"width": 64, "height": 48, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [{"id": "white", "kind": "generated", "generator": white}],
            "compositions": [{"id": "g", "nodes": [{"id": "clip", "op": {"type": "clip"}}]}],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [
                {"id": "early", "source_id": "white",
                    "timeline_range": {"start": 0, "end": 9 * second},
                    "source_range": {"start": 0, "end": 9 * second}},
                {"id": "late", "source_id": "white", "composition": {"id": "g"},
                    "timeline_range": {"start": 9 * second, "end": 10 * second},
                    "source_range": {"start": 0, "end": second}}
            ]}]}
        });
        let output =
            std::env::temp_dir().join(format!("alpha2-mobile-{}-late.mp4", std::process::id()));
        let _ = std::fs::remove_file(&output);
        let encoders = Arc::new(Encoders::default());
        let outcome = run(
            &encoders,
            crate::ExportRequest {
                project_json: project.to_string(),
                output_path: output.to_string_lossy().into_owned(),
                video_codec: Some("h264".into()),
                include_audio: false,
                ..crate::ExportRequest::default()
            },
        );
        assert!(
            matches!(&outcome, Err(Alpha2Error::Failed { code, .. })
                if code == "INCOMPLETE_COMPOSITION"),
            "{outcome:?}"
        );
        assert_eq!(opened(&encoders), (vec![], 0), "no encoder opened");
        assert!(!output.exists());
    }

    #[test]
    fn a_dropped_alpha_reads_as_the_desktop_reports_it() {
        let dropped = alpha2_host_core::video_stream::DroppedLayer {
            code: "alpha-dropped",
            reason: "alpha in an HEVC auxiliary layer is not decoded".into(),
        };
        let source = SourceId::new("overlay").expect("id");
        assert_eq!(
            layer_diagnostics(&std::collections::BTreeMap::from([(source, dropped)])),
            json!([{
                "type": "layerDropped",
                "code": "alpha-dropped",
                "source": "overlay",
                "reason": "alpha in an HEVC auxiliary layer is not decoded",
            }])
        );
    }

    #[test]
    fn only_a_failed_hardware_video_encoder_is_retried_in_software() {
        let wav =
            std::env::temp_dir().join(format!("alpha2-mobile-retry-{}.wav", std::process::id()));
        silence(&wav);

        let encoders = Arc::new(Encoders {
            hardware_fails_at: Some(5),
            ..Encoders::default()
        });
        let (output, outcome) = export_with(&encoders, "hardware", &wav);
        let report: serde_json::Value =
            serde_json::from_str(&outcome.expect("exported").report_json).expect("report");
        assert_eq!(report["softwareRetry"], true);
        assert_eq!(report["audioBitrate"], 128_000);
        assert_eq!(report["decodeDiagnostics"], json!([]));
        assert_eq!(report["gpuUnavailable"], serde_json::Value::Null);
        assert_eq!(report["hardwareEncoder"], false);
        assert_eq!(opened(&encoders), (vec![false, true], 2));
        let _ = std::fs::remove_file(output);

        let encoders = Arc::new(Encoders {
            hardware_fails_at: Some(5),
            audio_fails: true,
            ..Encoders::default()
        });
        let (output, outcome) = export_with(&encoders, "audio", &wav);
        assert!(
            matches!(&outcome, Err(Alpha2Error::Failed { code, reason, .. })
                if code == "ENCODE_FAILED" && reason.contains("audio session")),
            "{outcome:?}"
        );
        assert_eq!(opened(&encoders), (vec![false], 1));
        assert!(!output.exists());

        let encoders = Arc::new(Encoders::default());
        let target =
            std::env::temp_dir().join(format!("alpha2-mobile-{}-publish.mkv", std::process::id()));
        alpha2_host_core::partial::hooks::FAIL_PUBLISH_TO
            .with_borrow_mut(|fail| *fail = Some((target.clone(), 28)));
        let (output, outcome) = export_with(&encoders, "publish", &wav);
        alpha2_host_core::partial::hooks::FAIL_PUBLISH_TO.with_borrow_mut(|fail| *fail = None);
        let kept = match outcome {
            Err(Alpha2Error::OutputPublishFailed {
                kept_path: Some(kept),
                ..
            }) => PathBuf::from(kept),
            other => panic!("{other:?}"),
        };
        assert!(kept.exists(), "{}", kept.display());
        assert!(!output.exists());
        assert_eq!(opened(&encoders), (vec![false], 1));
        let _ = std::fs::remove_file(kept);
        let _ = std::fs::remove_file(wav);
    }
}
