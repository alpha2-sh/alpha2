//! Frames of a project rendered to PNG files through the steps an export takes: the picture is
//! read before YUV conversion, with premultiplied alpha divided out for PNG.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alpha2_engine::{EngineProject, ExportTextureStep, Exporter, FramesWithText, QualityProfile};
use alpha2_host_core::DecodePolicy;
use alpha2_host_core::export::{FrameBudget, SourceFrameCache, SourceStream, StreamSources};
use alpha2_host_core::partial::{ExistingOutput, OutputTarget};
use alpha2_host_core::providers::ProviderMedia;
use alpha2_host_core::video_stream::{DecodePreference, StreamOptions, VideoBackends};
use alpha2_media::{CancelToken, SourceId};
use alpha2_model::{MediaKind, Project};
use alpha2_time::MediaTime;
use serde_json::{Value, json};

use crate::error::Alpha2Error;
use crate::export::{Drawing, failed, files_of, open_source, open_still, register_resources};
use crate::session::{EngineConfig, LoadedFont, StillsRequest, VideoFrameMode};

/// Bytes the open extension sources may hold while stills render.
const PROVIDER_SOURCE_BYTES: u64 = 256 << 20;

/// Renders `request`'s stills in time order, so every decoder only moves forward, and answers in
/// request order.
pub(crate) fn render(
    request: &StillsRequest,
    config: &EngineConfig,
    fonts: &[LoadedFont],
    video: Option<Arc<dyn VideoBackends>>,
    preference: DecodePreference,
    budget: &FrameBudget,
) -> Result<Value, Alpha2Error> {
    let project = Project::from_json(&request.project_json)
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let snapshot = EngineProject::new(project.clone())
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let settings = &project.settings;
    let size = QualityProfile::export()
        .render_size(settings)
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?;
    let color = alpha2_engine::color_policy(&settings.color);
    let assets = request
        .assets
        .iter()
        .map(|(id, path)| (id.clone(), PathBuf::from(path)))
        .collect::<HashMap<_, _>>();
    // A few frames: decoded pictures go through memory rather than a device that imports them.
    let mut drawing = Drawing::open(config.renderer, VideoFrameMode::Cpu)?;
    register_resources(&project, &assets, &mut drawing)?;
    let mut texts = crate::export::text_textures(fonts)?;
    let providers = Arc::new(ProviderMedia::new(
        &project,
        files_of(&request.assets, &request.frames, &request.resources, fonts),
        (size.width, size.height),
        PROVIDER_SOURCE_BYTES,
    ));
    let mut sources =
        StreamSources::new(opener(&project, &assets, video, preference)).with_providers(providers);
    let mut cache = SourceFrameCache::for_renderer(drawing.as_export(), budget.bytes());
    let existing = if request.overwrite {
        ExistingOutput::Replace
    } else {
        ExistingOutput::Refuse
    };
    let cancel = CancelToken::default();
    let mut order = (0..request.stills.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| (request.stills[index].time_ticks, index));
    let mut answers = vec![Value::Null; request.stills.len()];
    for (step, index) in order.into_iter().enumerate() {
        let still = &request.stills[index];
        let mut exporter = Exporter::still(
            &snapshot,
            MediaTime::from_ticks(still.time_ticks),
            settings.frame_rate,
            QualityProfile::export(),
        )
        .map_err(|error| Alpha2Error::invalid(error.to_string()))?
        .with_background(!request.alpha);
        let frame_time = exporter
            .pending_time()
            .ok_or_else(|| failed("RENDER_FAILED", "a still has no frame"))?;
        let needs = exporter
            .prefetch_needs(&snapshot, 1)
            .map_err(|error| failed("RENDER_FAILED", error.to_string()))?;
        let needs = needs.first().map_or(&[][..], |frame| &frame.needs[..]);
        cache.follow(budget);
        cache
            .fill(
                drawing.as_export(),
                size,
                color,
                (step as u64, needs),
                &mut sources,
                &cancel,
            )
            .map_err(|error| failed("SOURCE_FAILED", error.to_string()))?;
        let pixels = draw(
            &drawing,
            &mut exporter,
            &snapshot,
            &mut texts,
            &cache,
            (frame_time, size),
        )?;
        cache.unpin();
        alpha2_host_core::write_png(
            OutputTarget {
                path: PathBuf::from(&still.output_path),
                existing,
            },
            size.width,
            size.height,
            &alpha2_host_core::tools::unpremultiplied(pixels),
        )
        .map_err(Alpha2Error::from)?;
        answers[index] = json!({
            "output": still.output_path,
            "frameTime": frame_time.ticks(),
            "width": size.width,
            "height": size.height,
        });
    }
    Ok(json!({ "stills": answers, "renderer": drawing.name() }))
}

/// Opens a source for the stills: audio-only sources have no pictures, video files go to
/// `video`'s decoders, and a platform without them reads stills only.
fn opener<'a>(
    project: &Project,
    assets: &'a HashMap<String, PathBuf>,
    video: Option<Arc<dyn VideoBackends>>,
    preference: DecodePreference,
) -> impl FnMut(&SourceId) -> Result<SourceStream, String> + 'a {
    let kinds = project
        .sources
        .iter()
        .map(|source| (source.id.clone(), (source.kind, source.color_metadata)))
        .collect::<HashMap<_, _>>();
    move |id: &SourceId| {
        let (kind, color) = kinds.get(id).copied().unwrap_or((MediaKind::Video, None));
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
            None => open_still(path)
                .ok_or_else(|| format!("source {id}: this platform has no video decoders"))?,
        }
    }
}

/// The still `exporter` stands at, drawn and read back as premultiplied RGBA8.
fn draw(
    drawing: &Drawing,
    exporter: &mut Exporter,
    snapshot: &EngineProject,
    texts: &mut alpha2_engine::TextTextures,
    cache: &SourceFrameCache,
    (time, size): (MediaTime, alpha2_render::RenderSize),
) -> Result<Vec<u8>, Alpha2Error> {
    let text_failed =
        |error: alpha2_engine::EngineError| failed("RENDER_FAILED", error.to_string());
    match drawing {
        Drawing::Gpu(renderer, _) => {
            texts
                .prepare_at(&**renderer, snapshot, time, size)
                .map_err(text_failed)?;
            let frames = FramesWithText::new(cache.provider(), texts);
            let texture = rendered(exporter.render_next_texture(snapshot, &**renderer, &frames))?;
            renderer
                .read_rgba8(&texture)
                .map_err(|error| failed("RENDER_FAILED", error.to_string()))
        }
        Drawing::Cpu(renderer) => {
            texts
                .prepare_at(&**renderer, snapshot, time, size)
                .map_err(text_failed)?;
            let frames = FramesWithText::new(cache.provider(), texts);
            Ok(
                rendered(exporter.render_next_texture(snapshot, &**renderer, &frames))?
                    .read_rgba8(),
            )
        }
    }
}

fn rendered<I>(
    step: Result<ExportTextureStep<I>, alpha2_engine::EngineError>,
) -> Result<I, Alpha2Error> {
    match step.map_err(|error| failed("RENDER_FAILED", error.to_string()))? {
        ExportTextureStep::Rendered { texture, .. } => Ok(texture),
        ExportTextureStep::Waiting {
            missing_sources, ..
        } => Err(failed(
            "MISSING_INPUT",
            format!("the still is missing sources: {missing_sources:?}"),
        )),
        ExportTextureStep::Complete => Err(failed("RENDER_FAILED", "a still rendered no frame")),
    }
}
