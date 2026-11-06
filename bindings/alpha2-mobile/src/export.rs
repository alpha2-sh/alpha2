//! Rendering a project into a file with a platform's decoders and encoders: the container layer
//! reads sources and writes the output, and the platform's codecs do the rest.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_container::{MatroskaFlavor, MatroskaWriter, Mp4Flavor, Mp4Writer};
use alpha2_engine::{EngineProject, Exporter, QualityProfile, TextTextures};
use alpha2_host_core::audio_stream::{AudioBackends, AudioStream};
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
use alpha2_host_core::{DecodePolicy, DecodedAudioSources, ProjectAudioRequest};
use alpha2_media::{
    AudioBlock, CancelToken, ColorMetadata, Container, DecodeError, DecodeEvent, Decoder,
    EncodeError, OutputPixels, QualityLevel, SourceId, SpeakerLayout,
};
use alpha2_model::{MediaKind, MediaSource, Project};
use alpha2_render::{CpuRenderer, Renderer};
use serde_json::json;

use crate::error::Alpha2Error;
use crate::platform::ExportJob;
use crate::session::{ExportOutcome, ExportProgressEvent, RendererMode, VideoDecodeMode};

/// A platform's codec factories.
pub(crate) struct MediaBackends {
    pub video_decoders: Arc<dyn VideoBackends>,
    pub audio_decoders: Arc<dyn AudioBackends>,
    pub video_encoders: Arc<dyn VideoEncoders>,
    pub audio_encoders: Arc<dyn AudioEncoders>,
}

/// Samples per block of the offline mix.
const AUDIO_BLOCK: usize = 4096;

/// Bytes the open extension sources may hold, as the desktop host's default.
const PROVIDER_SOURCE_BYTES: u64 = 512 << 20;

/// Exports `job`, once more on the software video encoder when the hardware one fails while
/// encoding.
pub(crate) fn export(job: &ExportJob, media: &MediaBackends) -> Result<ExportOutcome, Alpha2Error> {
    job.paused.wait_while_paused(&job.cancel);
    if job.cancel.is_cancelled() {
        return Err(Alpha2Error::Aborted);
    }
    match attempt(job, media, false) {
        Err(Failure::Encode {
            hardware: true,
            error,
        }) if !job.cancel.is_cancelled() => {
            alpha2_os::diagnostic!(
                "alpha2 mobile export: the hardware encoder failed ({error}); exporting again in software"
            );
            attempt(job, media, true).map_err(Failure::into_error)
        }
        outcome => outcome.map_err(Failure::into_error),
    }
}

/// Why an attempt stopped.
enum Failure {
    /// The encoders or the output, with whether the video encoder ran on hardware.
    Encode { hardware: bool, error: EncodeError },
    /// Anything else.
    Other(Alpha2Error),
}

impl Failure {
    fn into_error(self) -> Alpha2Error {
        match self {
            Self::Encode { error, .. } => error.into(),
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

/// The renderer an export draws with.
pub(crate) enum Drawing {
    Gpu(Box<Renderer>),
    Cpu(Box<CpuRenderer>),
}

impl Drawing {
    pub(crate) fn open(mode: RendererMode) -> Result<Self, Alpha2Error> {
        let cpu = || {
            let threads = std::thread::available_parallelism().map_or(2, usize::from);
            Self::Cpu(Box::new(CpuRenderer::new(threads)))
        };
        if mode == RendererMode::Cpu {
            return Ok(cpu());
        }
        match pollster::block_on(alpha2_gpu::GpuContext::request_product()) {
            Ok(gpu) => {
                let mut renderer = Renderer::new(gpu);
                // The CPU renderer has the built-in effects; the GPU one compiles them here.
                renderer.register_builtins().map_err(|diagnostics| {
                    failed(
                        "RENDER_FAILED",
                        format!("built-in effects failed to compile: {diagnostics:?}"),
                    )
                })?;
                Ok(Self::Gpu(Box::new(renderer)))
            }
            Err(error) if mode == RendererMode::Gpu => Err(Alpha2Error::GpuInitFailed {
                reason: error.to_string(),
            }),
            Err(_) => Ok(cpu()),
        }
    }

    pub(crate) fn as_export(&self) -> ExportRenderer<'_> {
        match self {
            Self::Gpu(renderer) => ExportRenderer::Gpu(renderer),
            Self::Cpu(renderer) => ExportRenderer::Cpu(renderer),
        }
    }

    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Self::Gpu(_) => "gpu",
            Self::Cpu(_) => "cpu",
        }
    }

    fn register_lut3d(&mut self, id: &str, data: &alpha2_render::Lut3dData) -> Result<(), String> {
        match self {
            Self::Gpu(renderer) => renderer.register_lut3d(id, data),
            Self::Cpu(renderer) => renderer.register_lut3d(id, data),
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
    .map_err(|error| Alpha2Error::invalid(error.to_string()))?
    .with_pixels(pixels);
    let size = exporter
        .render_size(&snapshot)
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let assets = request
        .assets
        .iter()
        .map(|(id, path)| (id.clone(), PathBuf::from(path)))
        .collect::<HashMap<_, _>>();

    let mut drawing = Drawing::open(job.config.renderer)?;
    register_resources(&project, &assets, &mut drawing)?;
    let mut texts = text_textures(&job.fonts)?;

    // The mix is encoded first: its packets are small, and the writer then interleaves them
    // with the video as it arrives.
    let has_audio = request.include_audio && project.sources.iter().any(MediaSource::is_audible);
    let audio_sources = if has_audio {
        Some(decode_audio(
            &project,
            &assets,
            media.audio_decoders.as_ref(),
            settings.sample_rate,
            &job.cancel,
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
    let encode = |error| Failure::Encode {
        hardware: false,
        error,
    };
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
        color: ColorMetadata::EXPORT,
        keyframe_interval: u32::try_from(alpha2_host_core::encode_policy::keyframe_interval(rate))
            .unwrap_or(1),
        software,
    };
    let video_encoder = media.video_encoders.open(&video_config).map_err(encode)?;
    let hardware = video_encoder.hardware();
    let video_encoder_name = video_encoder.name().to_owned();
    let encode = |error| Failure::Encode { hardware, error };
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
    let mut sink = EncodeSink::new(writer(container, file), video_encoder, video_config, audio);

    if let Some(sources) = &audio_sources {
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
            &mut sink,
            &job.cancel,
        )
        .map_err(|error| match error {
            alpha2_host_core::OfflineAudioError::Sink(error) => encode(error),
            error => Failure::Other(failed("AUDIO_RENDER_FAILED", error.to_string())),
        })?;
    }

    let preference = match job.config.video_decode {
        VideoDecodeMode::Auto => DecodePreference::Auto,
        VideoDecodeMode::Hardware => DecodePreference::Hardware,
        VideoDecodeMode::Software => DecodePreference::Software,
    };
    let declared = project
        .sources
        .iter()
        .map(|source| (source.id.clone(), (source.kind, source.color_metadata)))
        .collect::<HashMap<_, _>>();
    let providers = Arc::new(ProviderMedia::new(
        &project,
        provider_files(job),
        (size.width, size.height),
        PROVIDER_SOURCE_BYTES,
    ));
    let decoders = Arc::clone(&media.video_decoders);
    let mut sources = StreamSources::new(|id: &SourceId| {
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
        open_source(
            path,
            Arc::clone(&decoders),
            StreamOptions {
                preference,
                policy: DecodePolicy::default(),
                declared_color: color,
            },
        )
    })
    .with_providers(providers);
    let mut frames = 0;
    export_video(
        &snapshot,
        &mut exporter,
        drawing.as_export(),
        &mut texts,
        &mut sources,
        &mut sink,
        (&job.budget, &job.cancel),
        &mut |done, total| {
            frames = done;
            job.progress.on_progress(ExportProgressEvent {
                frames_done: done,
                frames_total: total,
            });
            job.paused.wait_while_paused(&job.cancel);
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
    drop(sources);
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
        "renderer": drawing.name(),
        "frames": frames,
        "width": size.width,
        "height": size.height,
    });
    Ok(ExportOutcome {
        output_path: request.output_path.clone(),
        report_json: report.to_string(),
    })
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

/// The timeline range to export: the request's ticks within the timeline.
fn export_range(
    snapshot: &EngineProject,
    start: Option<i64>,
    end: Option<i64>,
) -> Result<alpha2_time::TimeRange, Alpha2Error> {
    let timeline = snapshot
        .timeline_range()
        .ok_or_else(|| Alpha2Error::invalid("the timeline is empty"))?;
    let start = start.map_or(timeline.start(), alpha2_time::MediaTime::from_ticks);
    let end = end.map_or(timeline.end(), alpha2_time::MediaTime::from_ticks);
    alpha2_time::TimeRange::new(start, end)
        .map_err(|error| Alpha2Error::invalid(format!("export range: {error}")))
}

/// A still picture's file as a source, `None` for a file that is no still.
pub(crate) fn open_still(path: &Path) -> Option<Result<SourceStream, String>> {
    let mut head = [0; 12];
    let read = std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
        .ok()?;
    still_format(&head[..read])?;
    Some(
        std::fs::read(path)
            .map_err(|error| format!("{}: {error}", path.display()))
            .and_then(|bytes| decode_still(&bytes).map_err(|error| error.to_string()))
            .map(|still| SourceStream::Still(Arc::new(still.into_video_frame()))),
    )
}

/// A source file as frames: a still picture, or the default video track of a container.
pub(crate) fn open_source(
    path: &Path,
    decoders: Arc<dyn VideoBackends>,
    options: StreamOptions,
) -> Result<SourceStream, String> {
    let mut head = [0; 12];
    let read = std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if still_format(&head[..read]).is_some() {
        let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let still = decode_still(&bytes).map_err(|error| error.to_string())?;
        return Ok(SourceStream::Still(Arc::new(still.into_video_frame())));
    }
    VideoStream::open_file(path, decoders, options)
        .map(|stream| SourceStream::Video(Box::new(stream)))
        .map_err(|error| error.to_string())
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

/// Every audible source's audio, decoded whole at the project rate, for each stream its clips
/// play.
pub(crate) fn decode_audio(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    decoders: &dyn AudioBackends,
    project_rate: u32,
    cancel: &CancelToken,
) -> Result<DecodedAudioSources, Alpha2Error> {
    let mut streams: HashMap<&SourceId, BTreeSet<Option<u32>>> = HashMap::new();
    for track in &project.timeline.tracks {
        for clip in &track.clips {
            streams
                .entry(&clip.source_id)
                .or_default()
                .insert(clip.audio.stream);
        }
    }
    let mut decoded = DecodedAudioSources::new();
    for source in project.sources.iter().filter(|source| source.is_audible()) {
        let Some(path) = assets.get(source.id.as_str()) else {
            continue;
        };
        let wanted = streams
            .get(&source.id)
            .cloned()
            .unwrap_or_else(|| BTreeSet::from([None]));
        for stream in wanted {
            let mut stream_audio = AudioStream::open_file(
                path,
                decoders,
                project_rate,
                stream,
                DecodePolicy::default(),
            )?;
            if let Some(block) = whole(&mut stream_audio, cancel)? {
                decoded.insert_stream(source.id.clone(), stream, block);
            }
        }
    }
    Ok(decoded)
}

/// All of a stream's audio as one block; `None` for a stream without samples.
fn whole(
    decoder: &mut AudioStream,
    cancel: &CancelToken,
) -> Result<Option<AudioBlock>, DecodeError> {
    let mut all: Option<AudioBlock> = None;
    loop {
        match decoder.decode_next(cancel)? {
            DecodeEvent::Audio(block) => match all.as_mut() {
                None => all = Some(block),
                Some(all) => {
                    for (into, from) in all.channels.iter_mut().zip(block.channels) {
                        into.extend(from);
                    }
                }
            },
            DecodeEvent::EndOfStream => return Ok(all),
            DecodeEvent::Retry | DecodeEvent::Video(_) => {}
        }
    }
}
