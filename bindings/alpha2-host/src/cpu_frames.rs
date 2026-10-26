//! The export frame provider of the CPU renderer: decoded frames lent to it
//! as they are, their planes never copied.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use alpha2_engine::FrameProvider;
use alpha2_media::{DecodedVideoFrame, PixelFormat, SourceId};
use alpha2_model::MediaKind;
use alpha2_native::NativeVideoFrame;
use alpha2_render::{
    ColorPolicy, CpuRenderer, CpuSource, RenderCacheKey, RenderSize, RgbaPixels, SourceAlpha,
    SourcePlane, SourcePresentation, SourceView, YuvFrame, YuvPlanes,
};
use alpha2_time::MediaTime;

use crate::decode::{DecodedFrameSet, DecodedSourceFrame};
use crate::error_message;
use crate::frames::{
    FrameInstallMetrics, PhysicalFrameKey, RequestedFrames, export_source_key, refuse_hdr,
    yuv_source,
};

/// A decoded frame's planes, held for as long as a source shows them.
#[derive(Debug)]
struct DecodedPlanes {
    frame: Arc<NativeVideoFrame>,
    format: alpha2_media::YuvFormat,
    size: RenderSize,
}

impl DecodedPlanes {
    fn decoded(&self) -> &DecodedVideoFrame {
        match &*self.frame {
            NativeVideoFrame::Cpu(frame) => frame,
            NativeVideoFrame::Hardware { .. } => unreachable!("only CPU frames are held"),
        }
    }
}

impl YuvPlanes for DecodedPlanes {
    fn frame(&self) -> YuvFrame<'_> {
        yuv_source(self.decoded(), self.format, self.size)
    }
}

/// A decoded frame's one RGBA plane.
#[derive(Debug)]
struct DecodedRgba(Arc<NativeVideoFrame>);

impl RgbaPixels for DecodedRgba {
    fn plane(&self) -> SourcePlane<'_> {
        match &*self.0 {
            NativeVideoFrame::Cpu(frame) => frame.planes.first().map_or(
                SourcePlane {
                    bytes: &[],
                    stride: 0,
                },
                |plane| SourcePlane {
                    bytes: &plane.bytes,
                    stride: plane.stride,
                },
            ),
            NativeVideoFrame::Hardware { .. } => SourcePlane {
                bytes: &[],
                stride: 0,
            },
        }
    }
}

/// A source of `frame` presented at `size`, converted when a graph first
/// draws it: [`crate::frames::upload_frame`] for the CPU renderer.
///
/// # Errors
/// An HDR frame the color policy refuses, a hardware frame (the CPU
/// renderer decodes in software), a plane count that does not match the
/// pixel format, or an unsupported format.
pub fn present_frame(
    renderer: &CpuRenderer,
    size: RenderSize,
    color: ColorPolicy,
    frame: &Arc<NativeVideoFrame>,
) -> Result<CpuSource, String> {
    let NativeVideoFrame::Cpu(decoded) = &**frame else {
        return Err(format!(
            "{}the CPU renderer was given a hardware-decoded frame",
            crate::protocol::internal_tag()
        ));
    };
    refuse_hdr(color, decoded.color)?;
    let coded = decoded
        .planes
        .first()
        .ok_or_else(|| "native decoder frame has no planes".to_owned())?;
    let coded_size = RenderSize::new(coded.width, coded.height).map_err(error_message)?;
    let presentation = SourcePresentation {
        output_size: size,
        visible_rect: decoded.visible_rect,
        orientation: decoded.orientation,
        sample_aspect_ratio: (
            decoded.sample_aspect_ratio.num,
            decoded.sample_aspect_ratio.den,
        ),
        chroma_siting: decoded.chroma_location,
    };
    match decoded.pixel_format {
        PixelFormat::Yuv(format) => {
            if decoded.planes.len() != format.plane_count() {
                return Err(format!(
                    "native YUV decoder frame must contain {} planes",
                    format.plane_count()
                ));
            }
            let planes = Arc::new(DecodedPlanes {
                frame: Arc::clone(frame),
                format,
                size: coded_size,
            });
            let (source, diagnostics) = renderer
                .present_yuv(planes, presentation, color)
                .map_err(error_message)?;
            if diagnostics.iter().any(|diagnostic| {
                matches!(diagnostic, alpha2_render::ColorDiagnostic::UnsupportedHdr)
            }) {
                return Err("the project skips HDR sources".to_owned());
            }
            Ok(source)
        }
        PixelFormat::Rgba8 | PixelFormat::LinearRgbaF16 => {
            if decoded.planes.len() != 1 {
                return Err("a native RGBA frame must contain one plane".to_owned());
            }
            let pixels = Arc::new(DecodedRgba(Arc::clone(frame)));
            let stride = coded.stride;
            if decoded.pixel_format == PixelFormat::Rgba8 {
                renderer.present_rgba8(
                    pixels,
                    coded_size,
                    stride,
                    presentation,
                    SourceAlpha::Straight,
                )
            } else {
                renderer.present_linear_rgba16f(pixels, coded_size, stride, presentation)
            }
            .map_err(error_message)
        }
        other => Err(format!("native decoder returned {other:?}")),
    }
}

/// A video frame installed for the times it is asked for at, sharing its
/// source with every other time that shows the same picture.
pub struct CpuInstalledFrame {
    picture: PhysicalFrameKey,
    source: Arc<CpuSource>,
}

/// The frames of the export being rendered on the CPU, as
/// [`crate::frames::NativeGpuFrames`] holds them for the GPU.
pub struct NativeCpuFrames {
    pub size: RenderSize,
    /// How HDR frames come into the working space: the project's color pipeline.
    pub color: ColorPolicy,
    generation: Option<u64>,
    frames: RequestedFrames<CpuInstalledFrame>,
    physical_frames: HashMap<PhysicalFrameKey, Arc<CpuSource>>,
    /// The stills the last frame showed, and no others.
    images: HashMap<SourceId, CpuSource>,
    /// Stills whose picture arrived with a frame the render cache held, kept
    /// for the next frame that is rendered.
    waiting_images: HashMap<SourceId, Arc<NativeVideoFrame>>,
}

impl NativeCpuFrames {
    pub fn new(size: RenderSize) -> Self {
        Self {
            size,
            color: alpha2_engine::color_policy(&alpha2_model::ColorPipeline::default()),
            generation: None,
            frames: RequestedFrames::default(),
            physical_frames: HashMap::new(),
            images: HashMap::new(),
            waiting_images: HashMap::new(),
        }
    }

    fn keep_stills(&mut self, stills: &[SourceId]) {
        self.images.retain(|source, _| stills.contains(source));
        self.waiting_images
            .retain(|source, _| stills.contains(source));
    }

    /// Takes a decoded frame set whose frame is not rendered, because the
    /// render cache holds it.
    pub fn pass(&mut self, decoded: DecodedFrameSet) {
        self.keep_stills(&decoded.stills);
        for frame in decoded.frames {
            if frame.kind == MediaKind::Image && !self.images.contains_key(&frame.source) {
                self.waiting_images.insert(frame.source, frame.frame);
            }
        }
    }

    /// Takes the sources one rendered frame reads and keeps nothing it does
    /// not: video frames of other times and stills it does not show.
    pub fn install(
        &mut self,
        renderer: &CpuRenderer,
        decoded: DecodedFrameSet,
    ) -> Result<FrameInstallMetrics, String> {
        let started = Instant::now();
        let mut metrics = FrameInstallMetrics::default();
        let generation = decoded.generation;
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.frames.clear();
            self.physical_frames.clear();
        }
        let needed = decoded
            .frames
            .iter()
            .filter(|frame| frame.kind != MediaKind::Image)
            .map(|frame| (&frame.source, frame.requested))
            .collect::<HashSet<_>>();
        let physical_needed = decoded
            .frames
            .iter()
            .filter(|frame| frame.kind != MediaKind::Image)
            .map(|frame| PhysicalFrameKey::new(generation, &frame.source, &frame.frame))
            .collect::<HashSet<_>>();
        self.frames.retain_needed(&needed);
        self.physical_frames
            .retain(|key, _| physical_needed.contains(key));
        self.keep_stills(&decoded.stills);
        let DecodedFrameSet {
            index,
            frames,
            stills,
            ..
        } = decoded;
        for decoded in frames {
            if decoded.kind == MediaKind::Image {
                self.waiting_images.remove(&decoded.source);
                if self.images.contains_key(&decoded.source) {
                    continue;
                }
                let source = present_frame(renderer, self.size, self.color, &decoded.frame)?;
                metrics.rgba_fallback_frames = metrics.rgba_fallback_frames.saturating_add(1);
                self.images.insert(decoded.source, source);
            } else {
                self.install_video(renderer, generation, &decoded, &mut metrics)?;
            }
        }
        for (source, frame) in std::mem::take(&mut self.waiting_images) {
            let presented = present_frame(renderer, self.size, self.color, &frame)?;
            self.images.insert(source, presented);
        }
        if let Some(missing) = stills
            .iter()
            .find(|source| !self.images.contains_key(*source))
        {
            return Err(format!(
                "{}still {} reached frame {index} without its picture",
                crate::protocol::internal_tag(),
                missing.as_str(),
            ));
        }
        metrics.resident_stills = self.images.len();
        metrics.duration = started.elapsed();
        Ok(metrics)
    }

    fn install_video(
        &mut self,
        renderer: &CpuRenderer,
        generation: u64,
        decoded: &DecodedSourceFrame,
        metrics: &mut FrameInstallMetrics,
    ) -> Result<(), String> {
        if self.frames.contains(&decoded.source, decoded.requested) {
            return Ok(());
        }
        let picture = PhysicalFrameKey::new(generation, &decoded.source, &decoded.frame);
        let source = if let Some(source) = self.physical_frames.get(&picture) {
            metrics.physical_reuses = metrics.physical_reuses.saturating_add(1);
            Arc::clone(source)
        } else {
            let source = Arc::new(present_frame(
                renderer,
                self.size,
                self.color,
                &decoded.frame,
            )?);
            metrics.direct_yuv_frames = metrics.direct_yuv_frames.saturating_add(1);
            self.physical_frames
                .insert(picture.clone(), Arc::clone(&source));
            source
        };
        self.frames.insert(
            &decoded.source,
            decoded.requested,
            CpuInstalledFrame { picture, source },
        );
        Ok(())
    }
}

impl FrameProvider for NativeCpuFrames {
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        self.frames
            .get(source, time)
            .map(|frame| SourceView::cpu(&frame.source))
            .or_else(|| self.images.get(source).map(SourceView::cpu))
    }

    fn source_key(&self, source: &SourceId, time: MediaTime) -> Option<RenderCacheKey> {
        export_source_key(
            source,
            self.frames.get(source, time).map(|frame| &frame.picture),
            self.images.contains_key(source),
        )
    }
}
