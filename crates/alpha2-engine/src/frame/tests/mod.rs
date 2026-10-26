use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use alpha2_eval::{
    ActiveClip, ActiveComposition, ActiveEffect, ActiveTransition, CompositionPlan,
    EvaluatedVisual, FrameDesc, Step, StepOp,
};
use alpha2_gpu::{CapabilityTier, GpuContext};
use alpha2_media::{ColorMetadata, SourceId};
use alpha2_model::{Animated, ClipBlendMode, ClipEffect, ClipId, EffectValue, TrackId, Vec2};
use alpha2_render::{
    ColorPolicy, EffectDef, EffectKind, ExternalTextureLease, GpuVideoPlanes, ParamSpec, ParamType,
    ParamUnit, ParamValue, PendingSourceWork, RenderError, RenderSize, RenderedTexture, Renderer,
    SourcePresentation, SourceView, Yuv420Frame,
};
use alpha2_time::MediaTime;

use super::{FrameProvider, FrameRenderer, GpuFrameRenderer};

mod backgrounds;
mod cache;
mod captions_and_text;
mod composition;
mod effects_and_transitions;
mod export_steps;
mod layers;
mod layout;
mod missing_sources;
mod player;
mod shapes;
mod submissions;
mod timing;
mod transition_quality;

/// The pack these tests were written against: chroma centred in each 2x2
/// block, as VP9 and AV1 exports pack it.
fn centred(color: ColorMetadata) -> alpha2_media::YuvEncoding {
    alpha2_media::YuvEncoding::new(color, alpha2_media::ChromaLocation::Center)
}

/// Gives a hand-built frame description the `'static` identities and effects
/// that evaluation would borrow from an index.
fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

/// `effects` applied in order to the clip's picture, as a composition at
/// composition time `time`; `None` without effects.
fn chain(effects: &'static [ClipEffect], time: MediaTime) -> Option<ActiveComposition<'static>> {
    if effects.is_empty() {
        return None;
    }
    let node = |name: String| alpha2_model::NodeId::new(name).expect("valid ID");
    let mut steps = vec![Step {
        node: node("clip".into()),
        op: StepOp::Clip,
        offset: MediaTime::ZERO,
    }];
    steps.extend(effects.iter().enumerate().map(|(index, effect)| Step {
        node: node(format!("effect-{index}")),
        offset: MediaTime::ZERO,
        op: StepOp::Effect {
            input: index,
            inputs: Vec::new(),
            effect: effect.clone(),
        },
    }));
    let plan = leak(CompositionPlan {
        id: alpha2_model::CompositionId::new("chain").expect("valid ID"),
        output: steps.len() - 1,
        steps,
    });
    Some(ActiveComposition::new(plan, time))
}

static GPU_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static SHARED_GPU: OnceLock<GpuContext> = OnceLock::new();

/// Returns a handle to the one device this suite shares.
///
/// Per-test renderer state (texture pool, pipeline cache, effect registry,
/// diagnostics, quality controls) lives on [`Renderer`], not on the context,
/// so sharing the device isolates nothing less than a device per test would.
/// A device per test would make this process tear down as many devices as it
/// has tests, a pattern no alpha2 host uses and the leading suspect for an
/// unreproduced SIGSEGV on a vendor Vulkan driver.
/// `device_creation_survives_repeated_teardown` in `alpha2-render` covers
/// that path on demand.
///
/// It is the adapter a native host renders on, `request_high_performance`'s,
/// not the first one listed: on a machine with an integrated and a discrete
/// GPU those differ, and a failure that only the host's adapter shows would
/// pass here. `WGPU_POWER_PREF` and `WGPU_BACKEND` still choose another. A
/// failing test prints which adapter it ran on.
fn gpu() -> GpuContext {
    let shared = SHARED_GPU
        .get_or_init(|| {
            pollster::block_on(GpuContext::request_high_performance())
                .expect("frame tests require a working GPU adapter")
        })
        .clone();
    let capabilities = shared.capabilities();
    eprintln!(
        "frame tests on {} ({}, {})",
        capabilities.adapter_name, capabilities.backend, capabilities.driver_info
    );
    shared
}

/// Effect parameters that hold one value for the whole clip.
fn constant_params<const N: usize>(
    values: [(&str, EffectValue); N],
) -> BTreeMap<String, Animated<EffectValue>> {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), Animated::Constant { value }))
        .collect()
}

fn visual(scale: Vec2, opacity: f32) -> EvaluatedVisual {
    EvaluatedVisual {
        position: Vec2::new(0.5, 0.5),
        anchor: Vec2::new(0.5, 0.5),
        scale,
        rotation_degrees: 0.0,
        opacity,
    }
}

fn runtime_tint_effect(wgsl_body: &str) -> EffectDef {
    EffectDef {
        id: "test.tint".into(),
        version: 1,
        kind: EffectKind::Effect,
        inputs: Vec::new(),
        params: vec![ParamSpec {
            name: "tint".into(),
            param_type: ParamType::Color,
            default: ParamValue::Color([1.0; 4]),
            min: None,
            max: None,
            step: None,
            unit: ParamUnit::Color,
            enum_values: Vec::new(),
        }],
        resources: Vec::new(),
        wgsl_body: wgsl_body.into(),
        tier_min: CapabilityTier::TierB,
        license: Some("MIT".into()),
    }
}

fn rgba_texture(renderer: &Renderer, pixel: [u8; 4]) -> wgpu::Texture {
    rgba_image_texture(
        renderer,
        RenderSize::new(1, 1).expect("non-zero size"),
        &pixel,
    )
}

fn assert_rgba8_near(actual: &[u8], expected: &[u8], tolerance: u8) {
    assert_eq!(actual.len(), expected.len(), "RGBA byte lengths differ");
    if let Some((index, (actual, expected))) = actual
        .iter()
        .zip(expected)
        .enumerate()
        .find(|(_, (actual, expected))| actual.abs_diff(**expected) > tolerance)
    {
        panic!(
            "RGBA mismatch at byte {index}: actual {actual}, expected {expected}, tolerance {tolerance}"
        );
    }
}

/// Uploads on the shared [`gpu`] device, as [`nv12_test_planes`] does.
fn rgba_image_texture(_renderer: &Renderer, size: RenderSize, pixels: &[u8]) -> wgpu::Texture {
    let gpu = gpu();
    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("alpha2 test source texture"),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size.width * 4),
            rows_per_image: Some(size.height),
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    texture
}

fn read_rgba8(renderer: &Renderer, target: &RenderedTexture) -> Vec<u8> {
    renderer
        .read_rgba8(target)
        .expect("test texture readback succeeds")
}

struct MissingFrames;

impl FrameProvider for MissingFrames {
    fn texture_for(&self, _: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        None
    }
}

struct SingleFrame<'a> {
    source: SourceId,
    texture: &'a wgpu::TextureView,
}

struct TimedFrames<'a> {
    source: SourceId,
    first: &'a wgpu::TextureView,
    second: &'a wgpu::TextureView,
}

impl FrameProvider for TimedFrames<'_> {
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.source).then_some(if time == MediaTime::ZERO {
            SourceView::from_wgpu(self.first)
        } else {
            SourceView::from_wgpu(self.second)
        })
    }
}

impl FrameProvider for SingleFrame<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        (source == &self.source).then_some(SourceView::from_wgpu(self.texture))
    }
}

/// Answers only captions, from the host's caption cache.
struct CaptionFrames<'a>(&'a crate::TextTextures);

impl FrameProvider for CaptionFrames<'_> {
    fn texture_for(&self, _: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        None
    }

    fn text(&self, key: u64) -> Option<crate::TextTexture<'_>> {
        self.0.get(key)
    }
}

/// A grey backdrop, a caption track over it with two cues that meet at one
/// second, and a red square over the caption on the track after it.
fn caption_project() -> crate::EngineProject {
    caption_project_with("", CaptionExtraTracks::default())
}

/// Tracks added to [`caption_project`], each slot empty or one track's JSON
/// followed by a comma.
#[derive(Clone, Copy, Default)]
struct CaptionExtraTracks<'a> {
    /// Before the backdrop's track.
    bottom: &'a str,
    /// Between the backdrop's track and the captions.
    under_captions: &'a str,
    /// Between the captions and the square's track.
    over_captions: &'a str,
    /// After the square's track.
    top: &'a str,
}

/// [`caption_project`] with `sources` (each followed by a comma) and
/// `tracks` added.
fn caption_project_with(sources: &str, tracks: CaptionExtraTracks) -> crate::EngineProject {
    let document = r#"{
          "schema_version": 1, "id": "p", "name": "P",
          "settings": { "width": 160, "height": 90, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48000 },
          "sources": [ SOURCES
            { "id": "grey", "kind": "generated", "generator": { "type": "fill", "paint": {
                "type": "solid", "color": { "red": 0.3, "green": 0.3, "blue": 0.3, "alpha": 1 } } } },
            { "id": "square", "kind": "generated", "generator": { "type": "shapes", "items": [
                { "type": "rectangle", "center": { "mode": "constant", "value": { "x": 0.5, "y": 0.9 } },
                  "size": { "mode": "constant", "value": { "x": 0.05, "y": 0.1 } } },
                { "type": "fill", "paint": { "mode": "constant", "value": { "type": "solid", "color": { "red": 1, "green": 0, "blue": 0, "alpha": 1 } } } } ] } }
          ],
          "timeline": { "tracks": [ BOTTOM
            { "id": "back", "name": "Back", "kind": "video", "clips": [
              { "id": "c1", "source_id": "grey", "timeline_range": { "start": 0, "end": 360000 },
                "source_range": { "start": 0, "end": 360000 } } ] }, UNDER_CAPTIONS
            { "id": "subs", "name": "Subtitles", "kind": "caption", "captions": {
              "style": { "font_size": 0.12, "position": { "x": 0.5, "y": 0.95 },
                "outline": { "color": { "red": 0, "green": 0, "blue": 0, "alpha": 1 }, "width": 0.1 },
                "background": { "color": { "red": 0, "green": 0, "blue": 0.4, "alpha": 0.6 }, "padding": 0.2 } },
              "cues": [
                { "timeline_range": { "start": 0, "end": 120000 }, "text": "First cue" },
                { "timeline_range": { "start": 120000, "end": 240000 }, "text": "Second" } ] } }, OVER_CAPTIONS
            { "id": "front", "name": "Front", "kind": "video", "clips": [
              { "id": "c2", "source_id": "square", "timeline_range": { "start": 0, "end": 360000 },
                "source_range": { "start": 0, "end": 360000 } } ] } TOP
          ] }
        }"#
    .replace("SOURCES", sources)
    .replace("BOTTOM", tracks.bottom)
    .replace("UNDER_CAPTIONS", tracks.under_captions)
    .replace("OVER_CAPTIONS", tracks.over_captions)
    .replace("TOP", tracks.top);
    let project = alpha2_model::Project::from_json(&document).expect("a valid project");
    crate::EngineProject::new(project).expect("indexed")
}

/// A provider that has its one source only once it is told to.
struct ArrivingFrame<'a> {
    source: SourceId,
    texture: &'a wgpu::TextureView,
    arrived: std::cell::Cell<bool>,
}

impl FrameProvider for ArrivingFrame<'_> {
    fn texture_for(&self, source: &SourceId, _: MediaTime) -> Option<SourceView<'_>> {
        (self.arrived.get() && source == &self.source)
            .then_some(SourceView::from_wgpu(self.texture))
    }
}

/// A 16x16, 30 fps project showing one video clip over its first second.
fn one_clip_snapshot() -> crate::EngineProject {
    use alpha2_model::{Clip, MediaKind, MediaSource, Project, ProjectId, ProjectSettings, Track};
    use alpha2_time::{FrameRate, TimeRange};
    let source = SourceId::new("video").expect("valid ID");
    let mut project = Project::new(
        ProjectId::new("waiting").expect("valid ID"),
        "waiting",
        ProjectSettings::new(16, 16, FrameRate::new(30, 1).expect("rate"), 48_000)
            .expect("settings"),
    );
    project.sources = vec![MediaSource::new(
        source.clone(),
        "asset://video",
        MediaKind::Video,
    )];
    let mut track = Track::new(
        TrackId::new("v").expect("valid ID"),
        "v",
        alpha2_model::TrackKind::Video,
    );
    let second = TimeRange::new(MediaTime::ZERO, MediaTime::from_seconds(1)).expect("range");
    track.clips.push(Clip::new(
        ClipId::new("clip").expect("valid ID"),
        source,
        second,
        second,
    ));
    project.timeline.tracks.push(track);
    crate::EngineProject::new(project).expect("valid project")
}
