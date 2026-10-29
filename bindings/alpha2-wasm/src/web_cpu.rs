//! `WebCpuRenderer`, the browser binding's renderer for a page that has no
//! GPU: neither a WebGPU adapter nor a WebGL2 context. It draws the same
//! render graphs as `WebEffectRegistry` with the engine's CPU renderer, on
//! the calling thread, and keeps every picture in wasm memory.
//!
//! Frames arrive as a `VideoFrame`'s planes (`uploadYuvFrame`) or as RGBA
//! pixels and become CPU sources with no upload. A preview frame is drawn
//! at the preview quality and put on the canvas's 2D context; an export
//! frame is packed into I420 planes the page hands to `WebCodecs`. Runtime
//! WGSL effects are refused: only a GPU runs them.

use alpha2_engine::{
    EngineProject, ExportPlaneStep, Exporter, FrameProvider, FrameRenderer, FramesWithText, Player,
    QualityProfile, TextTextures,
};
use alpha2_media::{
    ChromaLocation, ColorMetadata, Orientation, Rect, SourceId, VideoCodec, YuvEncoding,
};
use alpha2_model::Project;
use alpha2_render::{
    CpuImage, CpuRenderer, CpuSource, Lut3dData, PlaneTarget, RenderCacheKey, RenderControls,
    RenderSize, RgbaPixels, SourceAlpha, SourcePlane, SourcePresentation, SourceView,
    Yuv420pTargets, YuvFrame, YuvPlanes, builtin_catalog_json,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use std::sync::Arc;
use wasm_bindgen::prelude::*;

use crate::error::Code;
use crate::frame_store::FrameStore;
use crate::interop::{
    export_status, frame_upload_receipt, js_safe_integer, resolved, set, video_color_space,
};
use crate::present::{WebPlaybackResult, upload_source_key};
use crate::project_audio::ProjectAudio;
use crate::project_cache::StalePresentations;
use crate::web::{
    WebCompiledEffect, chroma_siting_named, coded_error, export_waiting, internal_error,
    invalid_input, invalid_state, missing_frames, pin_export_frame, presented_placement,
    result_cache_json, rotation_from_degrees, typed_error, webcodecs_yuv_format,
    working_set_refusal,
};

/// Bytes of uploaded frames the CPU renderer keeps by default. Its pictures
/// live in wasm memory, which never shrinks and ends at 4 GiB, so the budget
/// is a quarter of the GPU renderer's: a dozen 1080p frames, each its
/// planes and its picture once drawn.
const DEFAULT_CPU_FRAME_BYTES: u64 = 256 << 20;

/// Why a runtime effect does not register here.
const RUNTIME_EFFECT_REFUSAL: &str = "runtime WGSL effects run only on a GPU, and this renderer \
     draws on the CPU (the browser offers no WebGPU adapter or WebGL2 context, or the CPU \
     renderer was asked for); remove the effect, or render where a GPU is available";

// The 2D canvas the renderer presents on, and the one it reads a `VideoFrame` or `ImageBitmap`
// through. Both are the browser's own CPU paths: no WebGL or WebGPU context is made.
#[wasm_bindgen(inline_js = r#"
export function alpha2Canvas2d(canvas) {
  const context = canvas.getContext("2d");
  if (context === null)
    throw new Error("the canvas has no 2D context: it already holds a WebGL or WebGPU one");
  return context;
}
let scratch;
export function alpha2PutRgba(context, pixels, width, height) {
  // Copied out of wasm memory, which a later allocation may move.
  const image = new ImageData(new Uint8ClampedArray(pixels), width, height);
  const canvas = context.canvas;
  if (canvas.width === width && canvas.height === height) {
    context.putImageData(image, 0, 0);
    return;
  }
  if (scratch === undefined || scratch.canvas.width !== width || scratch.canvas.height !== height)
    scratch = new OffscreenCanvas(width, height).getContext("2d");
  scratch.putImageData(image, 0, 0);
  context.imageSmoothingQuality = "high";
  context.globalCompositeOperation = "copy";
  context.drawImage(scratch.canvas, 0, 0, canvas.width, canvas.height);
}
let reader;
export function alpha2ImagePixels(image, width, height) {
  if (reader === undefined || reader.canvas.width !== width || reader.canvas.height !== height) {
    reader = new OffscreenCanvas(width, height).getContext("2d", { willReadFrequently: true });
    if (reader === null) throw new Error("the frame import could not create a 2D canvas");
    reader.globalCompositeOperation = "copy";
  }
  reader.drawImage(image, 0, 0, width, height);
  return new Uint8Array(reader.getImageData(0, 0, width, height).data.buffer);
}
export function alpha2ResizeCanvas(canvas, width, height) {
  canvas.width = width;
  canvas.height = height;
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = alpha2Canvas2d, catch)]
    fn canvas_2d(canvas: &JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_name = alpha2PutRgba, catch)]
    fn put_rgba(context: &JsValue, pixels: &[u8], width: u32, height: u32) -> Result<(), JsValue>;

    #[wasm_bindgen(js_name = alpha2ImagePixels, catch)]
    fn image_pixels(image: &JsValue, width: u32, height: u32) -> Result<Vec<u8>, JsValue>;

    #[wasm_bindgen(js_name = alpha2ResizeCanvas, catch)]
    fn resize_canvas(canvas: &JsValue, width: u32, height: u32) -> Result<(), JsValue>;
}

/// One uploaded frame: the source the graph draws, and what it holds.
pub(crate) struct CpuFrame {
    source: CpuSource,
    /// The bytes the frame keeps: its input, and its picture once drawn.
    bytes: u64,
    /// Kept at its own size, not fitted to the project: a new canvas size or
    /// color pipeline leaves it valid.
    raw: bool,
}

impl CpuFrame {
    fn retained_bytes(&self) -> u64 {
        self.bytes
    }
}

impl FrameProvider for FrameStore<CpuFrame> {
    fn texture_for(&self, source: &SourceId, time: MediaTime) -> Option<SourceView<'_>> {
        self.get(source.as_str(), time.ticks())
            .map(|frame| SourceView::cpu(&frame.source))
    }

    fn source_key(&self, source: &SourceId, time: MediaTime) -> Option<RenderCacheKey> {
        upload_source_key(self, source, time)
    }
}

/// A `VideoFrame`'s planes as `copyTo` wrote them, held for as long as a
/// source shows them.
#[derive(Debug)]
struct UploadedPlanes {
    data: Vec<u8>,
    format: alpha2_media::YuvFormat,
    size: RenderSize,
    /// Luma, chroma, chroma, alpha: `(offset, length, stride)`, all zero for
    /// a plane the format does not store.
    planes: [(usize, usize, u32); 4],
    color: ColorMetadata,
}

impl YuvPlanes for UploadedPlanes {
    fn frame(&self) -> YuvFrame<'_> {
        let plane = |index: usize| {
            let (offset, length, stride) = self.planes[index];
            SourcePlane {
                bytes: &self.data[offset..offset + length],
                stride,
            }
        };
        YuvFrame {
            format: self.format,
            planes: [plane(0), plane(1), plane(2)],
            alpha_plane: self.format.alpha.then(|| plane(3)),
            size: self.size,
            alpha: 1.0,
            color: Some(self.color),
        }
    }
}

/// An export frame drawn into I420 planes, packed back to back.
struct ExportedFrame {
    index: u64,
    time: MediaTime,
    planes: Vec<u8>,
    size: RenderSize,
}

/// RGBA pixels, 8-bit or half-float bits, rows packed.
#[derive(Debug)]
struct PackedPixels(Vec<u8>);

impl RgbaPixels for PackedPixels {
    fn plane(&self) -> SourcePlane<'_> {
        SourcePlane {
            bytes: &self.0,
            stride: 0,
        }
    }
}

/// Bytes of a picture of `size` as the CPU renderer stores it (half floats).
fn picture_bytes(size: RenderSize) -> u64 {
    u64::from(size.width) * u64::from(size.height) * 8
}

/// The browser renderer that draws on the CPU, for a page with no GPU.
///
/// It answers the calls `WebEffectRegistry` answers for preview, export,
/// text and audio, and draws the same pictures as the engine's native CPU
/// renderer, byte for byte. Effects registered at run time are refused.
#[wasm_bindgen]
pub struct WebCpuRenderer {
    renderer: CpuRenderer,
    /// The canvas preview frames are put on, with its 2D context; none for a
    /// renderer that only exports.
    canvas: Option<(JsValue, JsValue)>,
    frames: FrameStore<CpuFrame>,
    project: Option<EngineProject>,
    player: Player<Arc<CpuImage>>,
    audio: ProjectAudio,
    exporter: Option<Exporter>,
    texts: TextTextures,
    source_uploads: u64,
    physical_source_reuses: u64,
}

impl WebCpuRenderer {
    /// A renderer presenting on `canvas`'s 2D context (`undefined` or `null`
    /// for none).
    pub(crate) fn new(canvas: &JsValue) -> Result<Self, JsValue> {
        let canvas = if canvas.is_undefined() || canvas.is_null() {
            None
        } else {
            let context = canvas_2d(canvas).map_err(|error| {
                coded_error(
                    Code::INVALID_INPUT,
                    format!(
                        "the CPU renderer cannot draw on this canvas: {}",
                        js_message(&error)
                    ),
                )
            })?;
            Some((canvas.clone(), context))
        };
        Ok(Self {
            // wasm runs one thread: the renderer draws on the calling one.
            renderer: CpuRenderer::new(1),
            canvas,
            frames: FrameStore::sized(CpuFrame::retained_bytes, DEFAULT_CPU_FRAME_BYTES),
            project: None,
            player: Player::default(),
            audio: ProjectAudio::default(),
            exporter: None,
            texts: TextTextures::new(),
            source_uploads: 0,
            physical_source_reuses: 0,
        })
    }

    fn project(&self) -> Result<&EngineProject, JsValue> {
        self.project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))
    }

    /// Keeps `frame` as the frame of `source_id` at `time` (a still when
    /// `None`) and answers its upload receipt.
    fn keep(
        &mut self,
        source_id: &str,
        time: Option<i64>,
        frame: CpuFrame,
    ) -> Result<JsValue, JsValue> {
        let bytes = frame.bytes;
        let generation = match time {
            Some(time) => self.frames.insert((source_id.to_owned(), time), frame),
            None => self.frames.insert_image(source_id.to_owned(), frame),
        }
        .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, bytes)
    }

    /// The project's color policy, the default one with none loaded.
    fn color_policy(&self) -> alpha2_render::ColorPolicy {
        self.project.as_ref().map_or_else(
            || alpha2_engine::color_policy(&alpha2_model::ColorPipeline::default()),
            |project| alpha2_engine::color_policy(&project.project().settings.color),
        )
    }

    /// The color the loaded project declares for `source_id`.
    fn declared_color(&self, source_id: &str) -> Option<ColorMetadata> {
        self.project.as_ref().and_then(|project| {
            project
                .project()
                .sources
                .iter()
                .find(|source| source.id.as_str() == source_id)
                .and_then(|source| source.color_metadata)
        })
    }

    /// A source of straight or premultiplied RGBA8 `pixels` of `size`,
    /// fitted to `output_size` and turned as `orientation` says.
    fn rgba_source(
        &self,
        size: RenderSize,
        pixels: Vec<u8>,
        alpha: SourceAlpha,
        output_size: RenderSize,
        orientation: Orientation,
    ) -> Result<CpuFrame, JsValue> {
        let input = pixels.len() as u64;
        let source = self
            .renderer
            .present_rgba8(
                Arc::new(PackedPixels(pixels)),
                size,
                0,
                presentation(size, output_size, orientation),
                alpha,
            )
            .map_err(|error| typed_error(&error))?;
        Ok(CpuFrame {
            source,
            bytes: input + picture_bytes(output_size),
            raw: false,
        })
    }

    /// A `VideoFrame` or `ImageBitmap` read through a 2D canvas, as the
    /// GPU renderer's WebGL2 path reads one, presented into the output.
    fn image_source(
        &self,
        source_id: &str,
        image: &JsValue,
        (width, height): (u32, u32),
        output_size: RenderSize,
        orientation: Orientation,
    ) -> Result<CpuFrame, JsValue> {
        if self
            .declared_color(source_id)
            .is_some_and(alpha2_render::is_hdr)
        {
            return Err(invalid_input(format!(
                "source {source_id} is HDR (PQ or HLG): upload its frames' planes with \
                 uploadYuvFrame, which converts them under the project's color pipeline"
            )));
        }
        let size = RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
        let pixels = image_pixels(image, width, height).map_err(|error| {
            invalid_input(format!(
                "the browser could not read the frame's pixels: {}",
                js_message(&error)
            ))
        })?;
        self.rgba_source(
            size,
            pixels,
            SourceAlpha::Straight,
            output_size,
            orientation,
        )
    }

    /// Puts `picture` on the canvas, if the renderer has one.
    fn present(&self, picture: &CpuImage) -> Result<(), JsValue> {
        let Some((_, context)) = &self.canvas else {
            return Ok(());
        };
        let size = picture.size();
        let mut pixels = picture.read_rgba8();
        unpremultiply(&mut pixels);
        put_rgba(context, &pixels, size.width, size.height).map_err(|error| {
            internal_error(format!(
                "the canvas refused the frame: {}",
                js_message(&error)
            ))
        })
    }

    /// The export frame's `{ kind: "rendered", ... }` result with its I420
    /// planes, packed rows, and its `retainedBytes`.
    fn rendered_frame(
        planes: &[u8],
        size: RenderSize,
        index: Option<u64>,
        time: MediaTime,
    ) -> Result<web_sys::js_sys::Object, JsValue> {
        let (width, height) = (size.width, size.height);
        let luma = u64::from(width) * u64::from(height);
        let layout = web_sys::js_sys::Array::new();
        for (offset, stride) in [(0, width), (luma, width / 2), (luma + luma / 4, width / 2)] {
            let entry = web_sys::js_sys::Object::new();
            set(&entry, "offset", &js_safe_integer(offset, "plane offset")?)?;
            set(&entry, "stride", &JsValue::from_f64(f64::from(stride)))?;
            layout.push(&entry);
        }
        let result = web_sys::js_sys::Object::new();
        set(&result, "kind", &JsValue::from_str("rendered"))?;
        if let Some(index) = index {
            #[allow(clippy::cast_precision_loss)] // A frame index stays below 2^53.
            set(&result, "index", &JsValue::from_f64(index as f64))?;
        }
        set(
            &result,
            "timeTicks",
            &JsValue::from_str(&time.ticks().to_string()),
        )?;
        set(&result, "width", &JsValue::from_f64(f64::from(width)))?;
        set(&result, "height", &JsValue::from_f64(f64::from(height)))?;
        set(&result, "format", &JsValue::from_str("I420"))?;
        set(
            &result,
            "colorSpace",
            &video_color_space(ColorMetadata::EXPORT)?,
        )?;
        set(&result, "layout", &layout)?;
        set(
            &result,
            "pixels",
            &web_sys::js_sys::Uint8Array::from(planes),
        )?;
        Ok(result)
    }

    /// The next frame of the running export, packed into I420:
    /// `Ok(Err(status))` when it waits or the export is complete.
    fn next_export_frame(&mut self) -> Result<Result<ExportedFrame, JsValue>, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let exporter = self
            .exporter
            .as_mut()
            .ok_or_else(|| invalid_state("no export is running"))?;
        let size = exporter
            .render_size(project)
            .map_err(|error| typed_error(&error))?;
        if let Some(time) = exporter.pending_time() {
            self.texts
                .prepare_at(&self.renderer, project, time, size)
                .map_err(|error| typed_error(&error))?;
        }
        let mut planes = i420_buffer(size);
        let step = {
            let mut targets = i420_targets(&mut planes, size);
            exporter
                .render_next_into_yuv420p(
                    project,
                    &self.renderer,
                    &FramesWithText::new(&self.frames, &self.texts),
                    &mut targets,
                    None,
                )
                .map_err(|error| cpu_engine_error(&error))?
        };
        match step {
            ExportPlaneStep::Complete => {
                self.frames.unpin();
                Ok(Err(export_status("complete", None, &[])?))
            }
            ExportPlaneStep::Waiting {
                time,
                missing_sources,
            } => {
                pin_export_frame(&mut self.frames, project, time)?;
                Ok(Err(export_waiting(project, time, &missing_sources)?))
            }
            ExportPlaneStep::Rendered { index, time } => {
                self.frames.unpin();
                Ok(Ok(ExportedFrame {
                    index,
                    time,
                    planes,
                    size,
                }))
            }
        }
    }
}

#[wasm_bindgen]
impl WebCpuRenderer {
    /// Which renderer this is: `"cpu"`.
    #[wasm_bindgen(getter)]
    #[must_use]
    #[allow(clippy::unused_self)] // A JS getter.
    pub fn renderer(&self) -> String {
        "cpu".to_owned()
    }

    /// Always `false`: the CPU renderer has no device to lose.
    #[wasm_bindgen(getter, js_name = deviceLost)]
    #[must_use]
    #[allow(clippy::unused_self, clippy::missing_const_for_fn)] // A JS getter.
    pub fn device_lost(&self) -> bool {
        false
    }

    /// Refused: a runtime effect is WGSL, which only a GPU runs.
    ///
    /// # Errors
    /// Always rejects with `EFFECT_ERROR`.
    #[wasm_bindgen(js_name = compileEffect)]
    #[allow(clippy::unused_self)]
    pub fn compile_effect(&self, json: &str) -> Result<web_sys::js_sys::Promise, JsValue> {
        let definition = crate::effect::EffectRequest::parse(json)
            .map_err(|error| coded_error(Code::EFFECT_ERROR, error))?;
        Err(coded_error(
            Code::EFFECT_ERROR,
            format!(
                "effect {}@{}: {RUNTIME_EFFECT_REFUSAL}",
                definition.id, definition.version
            ),
        ))
    }

    /// Refused, as [`Self::compile_effect`] is.
    ///
    /// # Errors
    /// Always throws `EFFECT_ERROR`.
    #[wasm_bindgen(js_name = registerCompiledEffect)]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
    pub fn register_compiled_effect(
        &mut self,
        compiled: WebCompiledEffect,
    ) -> Result<String, JsValue> {
        Err(coded_error(
            Code::EFFECT_ERROR,
            format!("effect {}: {RUNTIME_EFFECT_REFUSAL}", compiled.id()),
        ))
    }

    /// `false`: no runtime effect registers here.
    #[wasm_bindgen(js_name = unregisterEffect)]
    #[allow(clippy::unused_self, clippy::needless_pass_by_value)]
    pub fn unregister_effect(&mut self, _id: String, _version: u32) -> bool {
        false
    }

    /// Does nothing: no runtime effect registers here.
    #[wasm_bindgen(js_name = clearEffects)]
    #[allow(clippy::unused_self)]
    pub fn clear_effects(&mut self) {}

    /// The engine's effect and transition authoring catalog: every built-in
    /// draws on the CPU.
    #[wasm_bindgen(js_name = builtinCatalog)]
    #[allow(clippy::unused_self)] // An instance method in the JS API.
    pub fn builtin_catalog(&self) -> String {
        builtin_catalog_json()
    }

    /// Parses and keeps one `.cube` 3D LUT resource.
    ///
    /// # Errors
    /// Rejects a cube the engine's parser refuses.
    #[wasm_bindgen(js_name = registerLut3d)]
    pub fn register_lut_3d(&mut self, id: String, cube: &str) -> Result<(), JsValue> {
        let data = Lut3dData::from_cube(cube).map_err(|error| typed_error(&error))?;
        self.renderer
            .register_lut3d(id, &data)
            .map_err(|error| typed_error(&error))
    }

    /// Releases one effect resource.
    #[wasm_bindgen(js_name = unregisterEffectResource)]
    pub fn unregister_effect_resource(&mut self, id: &str) -> bool {
        self.renderer.unregister_effect_resource(id)
    }

    /// Releases every effect resource.
    #[wasm_bindgen(js_name = clearEffectResources)]
    pub fn clear_effect_resources(&mut self) {
        self.renderer.clear_effect_resources();
    }

    /// `WebEffectRegistry.uploadVideoFrame`: the frame read through a 2D
    /// canvas, fitted to the loaded project. Always closes `frame`.
    ///
    /// # Errors
    /// Rejects an HDR source's frame and a frame the browser cannot read.
    #[wasm_bindgen(js_name = uploadVideoFrame)]
    #[allow(clippy::needless_pass_by_value)] // The API takes ownership and closes the frame.
    pub fn upload_video_frame(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        frame: web_sys::VideoFrame,
    ) -> Result<JsValue, JsValue> {
        let size = (frame.display_width(), frame.display_height());
        let output_size = self.project.as_ref().map_or_else(
            || RenderSize::new(size.0, size.1),
            |project| {
                let settings = &project.project().settings;
                RenderSize::new(settings.width, settings.height)
            },
        );
        let made = output_size
            .map_err(|error| typed_error(&error))
            .and_then(|output_size| {
                self.image_source(
                    &source_id,
                    frame.as_ref(),
                    size,
                    output_size,
                    Orientation::UPRIGHT,
                )
            });
        frame.close();
        self.keep(&source_id, Some(media_time_ticks), made?)
    }

    /// `WebEffectRegistry.uploadPresentedVideoFrame`, the frame read through
    /// a 2D canvas (the browser's conversion, as WebGL2's is). Always closes
    /// `frame`; an HDR source's frame is refused (`uploadYuvFrame` takes it).
    ///
    /// # Errors
    /// Rejects an HDR source's frame, a bad placement, and a frame the
    /// browser cannot read.
    #[wasm_bindgen(js_name = uploadPresentedVideoFrame)]
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub fn upload_presented_video_frame(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        frame: web_sys::VideoFrame,
        output_width: u32,
        output_height: u32,
        rotation_degrees: u32,
        mirror: bool,
    ) -> Result<JsValue, JsValue> {
        let size = (frame.display_width(), frame.display_height());
        let made = presented_placement(output_width, output_height, rotation_degrees, mirror)
            .and_then(|(output_size, orientation)| {
                self.image_source(&source_id, frame.as_ref(), size, output_size, orientation)
            });
        frame.close();
        self.keep(&source_id, Some(media_time_ticks), made?)
    }

    /// [`Self::upload_presented_video_frame`] for a bitmap made with
    /// `premultiplyAlpha: "none"`. Always closes `bitmap`.
    ///
    /// # Errors
    /// As [`Self::upload_presented_video_frame`].
    #[wasm_bindgen(js_name = uploadPresentedImageBitmap)]
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub fn upload_presented_image_bitmap(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        bitmap: web_sys::ImageBitmap,
        output_width: u32,
        output_height: u32,
        rotation_degrees: u32,
        mirror: bool,
    ) -> Result<JsValue, JsValue> {
        let size = (bitmap.width(), bitmap.height());
        let made = presented_placement(output_width, output_height, rotation_degrees, mirror)
            .and_then(|(output_size, orientation)| {
                self.image_source(&source_id, bitmap.as_ref(), size, output_size, orientation)
            });
        bitmap.close();
        self.keep(&source_id, Some(media_time_ticks), made?)
    }

    /// `"canvas"`: frames passed as `VideoFrame`s are read through a 2D
    /// canvas.
    #[wasm_bindgen(getter, js_name = videoFrameCopy)]
    #[must_use]
    #[allow(clippy::unused_self)] // A JS getter.
    pub fn video_frame_copy_mode(&self) -> String {
        "canvas".to_owned()
    }

    /// `true`: decoded `VideoFrame`s are taken, as planes (`uploadYuvFrame`)
    /// or through a 2D canvas.
    #[wasm_bindgen(getter, js_name = supportsExternalVideoFrame)]
    #[allow(clippy::unused_self, clippy::missing_const_for_fn)] // A JS getter.
    pub fn supports_external_video_frame(&self) -> bool {
        true
    }

    /// `WebEffectRegistry.uploadYuvFrame`: a decoded frame's planes, as
    /// `VideoFrame.copyTo` writes them, kept as they are and converted under
    /// the project's color pipeline when a frame first draws them.
    ///
    /// # Errors
    /// Rejects (`INVALID_INPUT`) an unknown format or chroma siting, a layout
    /// or plane that does not fit the data, a bad size or rotation, and an
    /// HDR frame when the project's color pipeline refuses HDR sources.
    #[wasm_bindgen(js_name = uploadYuvFrame)]
    #[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
    pub fn upload_yuv_frame(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        format: &str,
        coded_width: u32,
        coded_height: u32,
        data: Vec<u8>,
        layout: &[u32],
        visible: &[u32],
        display_width: u32,
        display_height: u32,
        color: &str,
        chroma_siting: &str,
        output_width: u32,
        output_height: u32,
        rotation_degrees: u32,
        mirror: bool,
    ) -> Result<JsValue, JsValue> {
        let chroma_siting = chroma_siting_named(chroma_siting)?;
        let yuv_format = webcodecs_yuv_format(format)?;
        let color: ColorMetadata = serde_json::from_str(color)
            .map_err(|error| invalid_input(format!("invalid frame color metadata: {error}")))?;
        let plane_count = yuv_format.color_plane_count();
        if layout.len() != yuv_format.plane_count() * 2 || visible.len() != 4 {
            return Err(invalid_input("YUV frame layout does not match its format"));
        }
        let (_, chroma_rows) = yuv_format.chroma_size(coded_width, coded_height);
        // Luma, chroma, then alpha at the luma's size; a plane the format does not store is
        // empty.
        let mut planes = [(0, 0, 0); 4];
        for index in 0..yuv_format.plane_count() {
            let rows = if index == 0 || index == plane_count {
                coded_height
            } else {
                chroma_rows
            };
            let stride = layout[index * 2 + 1];
            let offset = usize::try_from(layout[index * 2]).unwrap_or(usize::MAX);
            let length = usize::try_from(u64::from(stride) * u64::from(rows))
                .ok()
                .filter(|length| {
                    offset
                        .checked_add(*length)
                        .is_some_and(|end| end <= data.len())
                })
                .ok_or_else(|| invalid_input("YUV plane runs past the frame data"))?;
            // The alpha plane comes after the color planes, whichever their count.
            let slot = if index == plane_count { 3 } else { index };
            planes[slot] = (offset, length, stride);
        }
        let coded =
            RenderSize::new(coded_width, coded_height).map_err(|error| typed_error(&error))?;
        let output_size =
            RenderSize::new(output_width, output_height).map_err(|error| typed_error(&error))?;
        let sample_aspect_ratio = alpha2_media::SampleAspectRatio::of_display(
            (visible[2], visible[3]),
            (display_width, display_height),
        )
        .ok_or_else(|| {
            invalid_input(format!(
                "YUV frame shows its {}x{} picture at {display_width}x{display_height}",
                visible[2], visible[3]
            ))
        })?;
        let orientation = Orientation {
            rotation: rotation_from_degrees(rotation_degrees)?,
            mirror,
        };
        let policy = self.color_policy();
        if policy == alpha2_render::ColorPolicy::RejectHdr && alpha2_render::is_hdr(color) {
            return Err(invalid_input(format!(
                "the project refuses HDR sources (its color pipeline's hdr is \"reject\"): {source_id}"
            )));
        }
        let input = data.len() as u64;
        let (source, _) = self
            .renderer
            .present_yuv(
                Arc::new(UploadedPlanes {
                    data,
                    format: yuv_format,
                    size: coded,
                    planes,
                    color,
                }),
                SourcePresentation {
                    output_size,
                    visible_rect: Rect {
                        x: visible[0],
                        y: visible[1],
                        width: visible[2],
                        height: visible[3],
                    },
                    orientation,
                    sample_aspect_ratio: (sample_aspect_ratio.num, sample_aspect_ratio.den),
                    chroma_siting,
                },
                policy,
            )
            .map_err(|error| typed_error(&error))?;
        self.keep(
            &source_id,
            Some(media_time_ticks),
            CpuFrame {
                source,
                bytes: input + picture_bytes(output_size),
                raw: false,
            },
        )
    }

    /// `WebEffectRegistry.uploadPresentedRgbaFrame`.
    ///
    /// # Errors
    /// Rejects pixels that do not hold the size, and a bad output size.
    #[wasm_bindgen(js_name = uploadPresentedRgbaFrame)]
    #[allow(clippy::too_many_arguments)]
    pub fn upload_presented_rgba_frame(
        &mut self,
        source_id: &str,
        media_time_ticks: i64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        premultiplied: bool,
        output_width: u32,
        output_height: u32,
    ) -> Result<JsValue, JsValue> {
        let frame = self.presented_rgba8(
            width,
            height,
            pixels,
            premultiplied,
            output_width,
            output_height,
        )?;
        self.keep(source_id, Some(media_time_ticks), frame)
    }

    /// `WebEffectRegistry.uploadPresentedRgbaImage`.
    ///
    /// # Errors
    /// As [`Self::upload_presented_rgba_frame`].
    #[wasm_bindgen(js_name = uploadPresentedRgbaImage)]
    #[allow(clippy::too_many_arguments)]
    pub fn upload_presented_rgba_image(
        &mut self,
        source_id: &str,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        premultiplied: bool,
        output_width: u32,
        output_height: u32,
    ) -> Result<JsValue, JsValue> {
        let frame = self.presented_rgba8(
            width,
            height,
            pixels,
            premultiplied,
            output_width,
            output_height,
        )?;
        self.keep(source_id, None, frame)
    }

    /// `WebEffectRegistry.uploadPresentedLinearFrame`.
    ///
    /// # Errors
    /// Rejects pixels that do not hold the size, and a bad output size.
    #[wasm_bindgen(js_name = uploadPresentedLinearFrame)]
    #[allow(clippy::too_many_arguments)]
    pub fn upload_presented_linear_frame(
        &mut self,
        source_id: &str,
        media_time_ticks: i64,
        width: u32,
        height: u32,
        pixels: &[u16],
        output_width: u32,
        output_height: u32,
    ) -> Result<JsValue, JsValue> {
        let frame = self.presented_linear(width, height, pixels, output_width, output_height)?;
        self.keep(source_id, Some(media_time_ticks), frame)
    }

    /// `WebEffectRegistry.uploadPresentedLinearImage`.
    ///
    /// # Errors
    /// As [`Self::upload_presented_linear_frame`].
    #[wasm_bindgen(js_name = uploadPresentedLinearImage)]
    pub fn upload_presented_linear_image(
        &mut self,
        source_id: &str,
        width: u32,
        height: u32,
        pixels: &[u16],
        output_width: u32,
        output_height: u32,
    ) -> Result<JsValue, JsValue> {
        let frame = self.presented_linear(width, height, pixels, output_width, output_height)?;
        self.keep(source_id, None, frame)
    }

    /// Whether the loaded project declares `source_id` an HDR source.
    #[wasm_bindgen(js_name = hdrSource)]
    #[must_use]
    pub fn hdr_source(&self, source_id: &str) -> bool {
        self.project.as_ref().is_some_and(|project| {
            project.project().sources.iter().any(|source| {
                source.id.as_str() == source_id
                    && alpha2_render::is_hdr(source.effective_color_metadata())
            })
        })
    }

    /// The color the loaded project declares for `source_id`, as JSON.
    #[wasm_bindgen(js_name = sourceColorMetadata)]
    #[must_use]
    pub fn source_color_metadata(&self, source_id: &str) -> Option<String> {
        self.declared_color(source_id)
            .and_then(|color| serde_json::to_string(&color).ok())
    }

    /// `WebEffectRegistry.uploadRgbaFrame`: premultiplied RGBA8 kept at its
    /// own size.
    ///
    /// # Errors
    /// Rejects pixels that do not hold the size.
    #[wasm_bindgen(js_name = uploadRgbaFrame)]
    pub fn upload_rgba_frame(
        &mut self,
        source_id: &str,
        media_time_ticks: i64,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<JsValue, JsValue> {
        let frame = raw_frame(width, height, pixels)?;
        self.keep(source_id, Some(media_time_ticks), frame)
    }

    /// `WebEffectRegistry.uploadRgbaImage`: a still, premultiplied RGBA8 kept
    /// at its own size.
    ///
    /// # Errors
    /// Rejects pixels that do not hold the size.
    #[wasm_bindgen(js_name = uploadRgbaImage)]
    pub fn upload_rgba_image(
        &mut self,
        source_id: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<JsValue, JsValue> {
        let frame = raw_frame(width, height, pixels)?;
        self.keep(source_id, None, frame)
    }

    /// `WebEffectRegistry.aliasVideoFrame`.
    ///
    /// # Errors
    /// Refuses an alias that would make the export frame's sources outgrow
    /// the uploaded-frame budget.
    #[wasm_bindgen(js_name = aliasVideoFrame)]
    pub fn alias_video_frame(
        &mut self,
        source_id: &str,
        existing_media_time_ticks: i64,
        requested_media_time_ticks: i64,
    ) -> Result<bool, JsValue> {
        let aliased = self
            .frames
            .alias(
                &(source_id.to_owned(), existing_media_time_ticks),
                (source_id.to_owned(), requested_media_time_ticks),
            )
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        if aliased {
            self.physical_source_reuses = self.physical_source_reuses.saturating_add(1);
        }
        Ok(aliased)
    }

    /// Releases an uploaded frame.
    #[wasm_bindgen(js_name = releaseFrame)]
    pub fn release_frame(&mut self, source_id: &str, media_time_ticks: i64) -> bool {
        self.frames.remove(source_id, media_time_ticks)
    }

    /// Releases exactly one uploaded generation.
    #[wasm_bindgen(js_name = releaseFrameGeneration)]
    pub fn release_frame_generation(&mut self, generation: u64) -> bool {
        self.frames.release_generation(generation)
    }

    /// Whether the still `sourceId` is still the upload `generation` names.
    #[wasm_bindgen(js_name = holdsImage)]
    pub fn holds_image(&self, source_id: &str, generation: u64) -> bool {
        self.frames.holds_image(source_id, generation)
    }

    /// Releases every uploaded frame and the still of one source.
    #[wasm_bindgen(js_name = releaseSourceFrames)]
    pub fn release_source_frames(&mut self, source_id: &str) -> bool {
        self.frames.release_source(source_id)
    }

    /// Releases all uploaded frames, images, and retired generations.
    #[wasm_bindgen(js_name = clearFrames)]
    pub fn clear_frames(&mut self) {
        self.frames.clear();
    }

    /// Sets the byte budget of uploaded frames (default 256 MiB, wasm
    /// memory), as `WebEffectRegistry.setUploadedFrameBudget` does.
    ///
    /// # Errors
    /// Rejects a negative or non-finite budget.
    #[wasm_bindgen(js_name = setUploadedFrameBudget)]
    pub fn set_uploaded_frame_budget(&mut self, bytes: f64) -> Result<(), JsValue> {
        if !(bytes.is_finite() && bytes >= 0.0) {
            return Err(invalid_input(
                "the uploaded frame budget must be a non-negative number",
            ));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Checked above.
        self.frames.set_video_byte_budget(bytes as u64);
        Ok(())
    }

    /// Lets drawn results occupy up to `bytes` of wasm memory to be found
    /// again when a frame repeats; zero, the default, turns that off.
    ///
    /// # Errors
    /// Rejects a negative or non-finite budget.
    #[wasm_bindgen(js_name = setResultCacheBudget)]
    pub fn set_result_cache_budget(&mut self, bytes: f64) -> Result<(), JsValue> {
        if !(bytes.is_finite() && bytes >= 0.0) {
            return Err(invalid_input(
                "the result cache budget must be a non-negative number",
            ));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Checked above.
        self.renderer.set_result_cache_budget(bytes as u64);
        Ok(())
    }

    /// Releases `source_id`'s video frames older than `media_time_ticks`.
    #[wasm_bindgen(js_name = trimFramesBefore)]
    pub fn trim_frames_before(&mut self, source_id: &str, media_time_ticks: i64) {
        self.frames.trim_before(source_id, media_time_ticks);
    }

    /// Replaced generations still awaiting explicit release.
    #[wasm_bindgen(getter, js_name = retiredFrameCount)]
    pub fn retired_frame_count(&self) -> usize {
        self.frames.retired_len()
    }

    /// Frames the binding holds.
    #[wasm_bindgen(getter, js_name = uploadedFrameCount)]
    pub fn uploaded_frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Bytes the uploaded frames hold, their pictures counted once drawn.
    #[wasm_bindgen(getter, js_name = uploadedFrameBytes)]
    pub fn uploaded_frame_bytes(&self) -> u64 {
        self.frames.retained_bytes(CpuFrame::retained_bytes)
    }

    /// Lets go of the drawn results kept for reuse and returns their bytes.
    #[wasm_bindgen(js_name = releaseIdleMemory)]
    #[allow(clippy::cast_precision_loss)] // Byte counts stay far below 2^53.
    pub fn release_idle_memory(&self) -> f64 {
        self.renderer.release_idle_memory() as f64
    }

    /// Sizes the canvas to `width` by `height`, which preview frames are
    /// scaled to fill, and drops the held frame.
    ///
    /// # Errors
    /// Fails without a canvas, or for a zero or oversized dimension.
    #[wasm_bindgen(js_name = resizeSurface)]
    pub fn resize_surface(&mut self, width: u32, height: u32) -> Result<(), JsValue> {
        RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
        let (canvas, _) = self
            .canvas
            .as_ref()
            .ok_or_else(|| invalid_state("renderer was created without a canvas"))?;
        resize_canvas(canvas, width, height).map_err(|error| {
            internal_error(format!(
                "the canvas was not resized: {}",
                js_message(&error)
            ))
        })?;
        self.player.reset();
        Ok(())
    }

    /// What this renderer can do, in the shared capability report's JSON
    /// with runtime `browser`: its "GPU" is the CPU (backend `cpu`),
    /// `features` says `renderer.cpu` is available, `renderer.gpu` is not,
    /// and runtime effects (`effects.runtime`) do not run.
    ///
    /// # Errors
    /// Returns an error if the report cannot be serialized.
    #[wasm_bindgen(js_name = capabilityReport)]
    pub fn capability_report(&self) -> Result<String, JsValue> {
        let report =
            alpha2_engine::CapabilityReport::cpu(alpha2_engine::Runtime::Browser, &self.renderer)
                .with_feature(
                    "renderer.gpu",
                    alpha2_engine::Feature::unavailable(
                        "this renderer draws on the CPU; the browser offers no WebGPU adapter or \
                         WebGL2 context, or the CPU renderer was asked for",
                    ),
                )
                .with_feature("renderer.cpu", alpha2_engine::Feature::available())
                .with_feature(
                    "effects.runtime",
                    alpha2_engine::Feature::unavailable(RUNTIME_EFFECT_REFUSAL),
                );
        serde_json::to_string(&report).map_err(|error| internal_error(error.to_string()))
    }

    /// A JSON snapshot for developer tooling: what the CPU renderer drew,
    /// its result cache, and the frames it holds.
    ///
    /// # Errors
    /// Returns an error if the snapshot cannot be serialized.
    #[wasm_bindgen(js_name = diagnostics)]
    pub fn diagnostics(&self) -> Result<String, JsValue> {
        let stats = self.renderer.stats();
        let (presented, repeated) = self.renderer.frame_outcomes();
        let value = serde_json::json!({
            "renderer": "cpu",
            "resultCache": result_cache_json(self.renderer.result_cache_stats()),
            "frames": { "presented": presented, "repeated": repeated },
            "cpu": {
                "threads": self.renderer.threads(),
                "graphs": stats.graphs,
                "nodes": stats.nodes,
                "renderMicros": stats.render_micros,
                "presentedSources": stats.presented_sources,
                "imageHighWaterBytes": stats.image_high_water_bytes,
            },
            "sourceFrames": {
                "count": self.frames.len(),
                "retired": self.frames.retired_len(),
                "retainedBytes": self.frames.retained_bytes(CpuFrame::retained_bytes),
                "uploads": self.source_uploads,
                "physicalReuses": self.physical_source_reuses,
            },
        });
        serde_json::to_string(&value).map_err(|error| internal_error(error.to_string()))
    }

    /// Does nothing: the CPU renderer times nothing per pass.
    #[wasm_bindgen(js_name = setPassTiming)]
    #[allow(clippy::unused_self)]
    pub fn set_pass_timing(&self, _enabled: bool) {}

    /// A promise that resolves at once: the CPU renderer's work is done when
    /// its call returns.
    #[wasm_bindgen(js_name = submittedWorkDone)]
    #[allow(clippy::unused_self)]
    pub fn submitted_work_done(&self) -> web_sys::js_sys::Promise {
        resolved(&JsValue::UNDEFINED)
    }

    /// Loads and validates a versioned project document, keeping uploads
    /// as `WebEffectRegistry.loadProject` does, and ends a running export.
    ///
    /// # Errors
    /// Rejects a document the engine refuses.
    #[wasm_bindgen(js_name = loadProject)]
    pub fn load_project(&mut self, json: &str) -> Result<(), JsValue> {
        let project = Project::from_json(json).map_err(|error| typed_error(&error))?;
        let snapshot = EngineProject::new(project).map_err(|error| typed_error(&error))?;
        let audio = self.audio.mixer_for(&snapshot)?;
        if let Some(previous) = self.project.as_ref() {
            let stale = crate::project_cache::stale_sources(previous.project(), snapshot.project());
            self.frames.retain_sources(|source| !stale.contains(source));
            match crate::project_cache::stale_presentations(previous.project(), snapshot.project())
            {
                StalePresentations::All => self.frames.retain_values(|frame| frame.raw),
                StalePresentations::Sources(sources) => {
                    if !sources.is_empty() {
                        self.frames
                            .retain_entries(|source, frame| frame.raw || !sources.contains(source));
                    }
                }
            }
        } else {
            self.frames.clear();
        }
        self.audio.install(self.project.as_ref(), &snapshot, audio);
        self.project = Some(snapshot);
        self.player.reset();
        self.exporter = None;
        Ok(())
    }

    /// `WebEffectRegistry.uploadAudioBlock`.
    ///
    /// # Errors
    /// As `WebEffectRegistry.uploadAudioBlock`.
    #[wasm_bindgen(js_name = uploadAudioBlock)]
    pub fn upload_audio_block(
        &mut self,
        source_id: &str,
        start_sample: i64,
        sample_rate: u32,
        layout: &JsValue,
        samples: &web_sys::js_sys::Float32Array,
        ends_source: Option<bool>,
    ) -> Result<(), JsValue> {
        self.audio.upload_block(
            source_id,
            start_sample,
            sample_rate,
            layout,
            samples,
            ends_source,
        )
    }

    /// Drops one source's retained audio.
    #[wasm_bindgen(js_name = releaseAudioSource)]
    pub fn release_audio_source(&mut self, source_id: &str) -> bool {
        self.audio.release_source(source_id)
    }

    /// Drops every source's retained audio.
    #[wasm_bindgen(js_name = clearAudioSources)]
    pub fn clear_audio_sources(&mut self) {
        self.audio.clear_sources();
    }

    /// Total retained audio samples across every source.
    #[wasm_bindgen(js_name = audioRetainedFrames)]
    pub fn audio_retained_frames(&self) -> u32 {
        self.audio.retained_frames()
    }

    /// Allocated audio sample bytes.
    #[wasm_bindgen(js_name = audioRetainedBytes)]
    pub fn audio_retained_bytes(&self) -> u32 {
        self.audio.retained_bytes()
    }

    /// One source's retained audio windows, as JSON.
    ///
    /// # Errors
    /// Returns an error if the windows cannot be serialized.
    #[wasm_bindgen(js_name = audioRetainedWindows)]
    pub fn audio_retained_windows(&self, source_id: &str) -> Result<String, JsValue> {
        self.audio.retained_windows(source_id)
    }

    /// Discards `source_id`'s retained audio that ends before `ticks`.
    #[wasm_bindgen(js_name = trimAudioBefore)]
    pub fn trim_audio_before(&mut self, source_id: &str, ticks: i64) {
        self.audio.trim_before(source_id, ticks);
    }

    /// `WebEffectRegistry.setAudioContinuity`.
    ///
    /// # Errors
    /// Rejects any other policy name.
    #[wasm_bindgen(js_name = setAudioContinuity)]
    pub fn set_audio_continuity(&mut self, policy: &str) -> Result<(), JsValue> {
        self.audio.set_continuity(policy, self.exporter.is_some())
    }

    /// Clears every processor history after a seek or a transport stop.
    #[wasm_bindgen(js_name = resetAudio)]
    pub fn reset_audio(&mut self) {
        self.audio.reset();
    }

    /// `WebEffectRegistry.audioNeeds`.
    ///
    /// # Errors
    /// Fails without a project, or for a window out of range.
    #[wasm_bindgen(js_name = audioNeeds)]
    pub fn audio_needs(&self, start_sample: i64, frames: u32) -> Result<String, JsValue> {
        self.audio
            .needs(self.project.as_ref(), start_sample, frames)
    }

    /// The most samples per channel one `renderAudio` call mixes.
    #[wasm_bindgen(getter, js_name = audioMaxBlock)]
    #[allow(clippy::unused_self, clippy::missing_const_for_fn)] // A JS getter.
    pub fn audio_max_block(&self) -> u32 {
        u32::try_from(crate::AUDIO_BLOCK).unwrap_or(u32::MAX)
    }

    /// `WebEffectRegistry.renderAudio`: the same mix, with no GPU.
    ///
    /// # Errors
    /// As `WebEffectRegistry.renderAudio`.
    #[wasm_bindgen(js_name = renderAudio)]
    pub fn render_audio(
        &mut self,
        start_sample: i64,
        frames: u32,
        playing: bool,
    ) -> Result<Vec<f32>, JsValue> {
        self.audio
            .render(self.project.as_ref(), start_sample, frames, playing)
    }

    /// `WebEffectRegistry.setLoudnessMetering`.
    ///
    /// # Errors
    /// Returns an error without an audio renderer.
    #[wasm_bindgen(js_name = setLoudnessMetering)]
    pub fn set_loudness_metering(&mut self, enabled: bool) -> Result<(), JsValue> {
        self.audio.set_loudness_metering(enabled)
    }

    /// `WebEffectRegistry.loudnessReport`.
    #[wasm_bindgen(js_name = loudnessReport)]
    #[must_use]
    pub fn loudness_report(&self) -> String {
        self.audio.loudness_report()
    }

    /// `WebEffectRegistry.setNormalizationGainDb`.
    ///
    /// # Errors
    /// As `WebEffectRegistry.setNormalizationGainDb`.
    #[wasm_bindgen(js_name = setNormalizationGainDb)]
    pub fn set_normalization_gain_db(&mut self, gain_db: f64) -> Result<(), JsValue> {
        self.audio.set_normalization_gain_db(gain_db)
    }

    /// Sources the most recent `renderAudio` call could not mix, as JSON.
    ///
    /// # Errors
    /// Returns an error if the list cannot be serialized.
    #[wasm_bindgen(js_name = audioMissingSources)]
    pub fn audio_missing_sources(&self) -> Result<String, JsValue> {
        self.audio.missing_sources()
    }

    /// Channel count of every mixed block.
    #[wasm_bindgen(js_name = audioChannels)]
    pub fn audio_channels(&self) -> u32 {
        self.audio.channels()
    }

    /// Project sample rate used by uploads and mixed blocks.
    #[wasm_bindgen(js_name = audioSampleRate)]
    pub fn audio_sample_rate(&self) -> u32 {
        self.audio.sample_rate()
    }

    /// `WebEffectRegistry.beginExport`, drawing on the CPU.
    ///
    /// # Errors
    /// Returns an error without a loaded project, for an empty range, an
    /// invalid frame rate, or an unknown codec.
    #[wasm_bindgen(js_name = beginExport)]
    pub fn begin_export(
        &mut self,
        start_ticks: i64,
        end_ticks: i64,
        rate_num: u32,
        rate_den: u32,
        codec: &str,
    ) -> Result<(), JsValue> {
        let codec = parse_codec(codec)?;
        let project = self.project()?;
        let range = TimeRange::new(MediaTime(start_ticks), MediaTime(end_ticks))
            .map_err(|error| typed_error(&error))?;
        let frame_rate = if rate_num > 0 && rate_den > 0 {
            FrameRate::new(rate_num, rate_den).map_err(|error| typed_error(&error))?
        } else {
            project.project().settings.frame_rate
        };
        let exporter = Exporter::new(project, range, frame_rate, QualityProfile::export(), codec)
            .map_err(|error| typed_error(&error))?;
        self.exporter = Some(exporter);
        self.frames.unpin();
        self.audio.begin_export();
        Ok(())
    }

    /// Ends the current export.
    #[wasm_bindgen(js_name = endExport)]
    pub fn end_export(&mut self) {
        self.exporter = None;
        self.frames.unpin();
        self.audio.end_export();
    }

    /// `WebEffectRegistry.exportProgress`.
    ///
    /// # Errors
    /// Returns an error when no export is running.
    #[wasm_bindgen(js_name = exportProgress)]
    pub fn export_progress(&self) -> Result<String, JsValue> {
        let project = self.project()?;
        let exporter = self
            .exporter
            .as_ref()
            .ok_or_else(|| invalid_state("no export is running"))?;
        let progress = exporter.progress();
        let range = exporter.effective_range();
        let audio = exporter
            .audio_window(project.project().settings.sample_rate)
            .map_err(|error| typed_error(&error))?;
        serde_json::to_string(&serde_json::json!({
            "completed": progress.completed,
            "total": progress.total,
            "effectiveRange": {
                "startTicks": range.start().ticks().to_string(),
                "endTicks": range.end().ticks().to_string(),
            },
            "audioWindow": {
                "startSample": audio.start_sample.to_string(),
                "frames": audio.frames,
            },
        }))
        .map_err(|error| internal_error(error.to_string()))
    }

    /// `WebEffectRegistry.exportFrame`: the next export frame, drawn on the
    /// CPU before the call returns.
    ///
    /// # Errors
    /// Returns an error without a running export or a loaded project, or
    /// when the frame cannot be drawn.
    #[wasm_bindgen(js_name = exportFrame)]
    pub fn export_frame(&mut self) -> Result<web_sys::js_sys::Promise, JsValue> {
        Ok(resolved(&match self.next_export_frame()? {
            Ok(frame) => {
                Self::rendered_frame(&frame.planes, frame.size, Some(frame.index), frame.time)?
                    .into()
            }
            Err(status) => status,
        }))
    }

    /// `WebEffectRegistry.beginExportFrame`: its `readback` is already
    /// settled.
    ///
    /// # Errors
    /// As [`Self::export_frame`].
    #[wasm_bindgen(js_name = beginExportFrame)]
    pub fn begin_export_frame(&mut self) -> Result<JsValue, JsValue> {
        let exported = match self.next_export_frame()? {
            Ok(exported) => exported,
            Err(status) => return Ok(status),
        };
        let frame = Self::rendered_frame(
            &exported.planes,
            exported.size,
            Some(exported.index),
            exported.time,
        )?;
        ticket(
            Some(exported.index),
            exported.time,
            exported.planes.len() as u64,
            &frame,
        )
    }

    /// `WebEffectRegistry.beginExportAt`: one exact timeline timestamp at
    /// export quality, drawn on the CPU, its `readback` already settled.
    ///
    /// # Errors
    /// Returns an error without a loaded project, for an unknown codec or an
    /// odd size, an effect that does not draw on the CPU (`EFFECT_ERROR`),
    /// or when the frame cannot be drawn.
    #[wasm_bindgen(js_name = beginExportAt)]
    pub fn begin_export_at(&mut self, time_ticks: i64, codec: &str) -> Result<JsValue, JsValue> {
        let codec = parse_codec(codec)?;
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let time = MediaTime(time_ticks);
        let frame = project
            .evaluate(time)
            .map_err(|error| typed_error(&error))?;
        alpha2_engine::check_frame_effects(&self.renderer, &frame)
            .map_err(|error| cpu_engine_error(&error))?;
        let quality = QualityProfile::export();
        // An odd 4:2:0 size is refused before anything renders, naming the nearest even size.
        let size = quality
            .export_size(&project.project().settings)
            .map_err(|error| typed_error(&error))?;
        self.renderer
            .set_texture_budget_bytes(quality.texture_budget_bytes);
        self.renderer
            .set_quality_controls(quality.max_filter_samples, quality.motion_blur_subframes);
        self.texts
            .prepare(&self.renderer, &frame, size)
            .map_err(|error| typed_error(&error))?;
        let mut planes = i420_buffer(size);
        let report = {
            let mut targets = i420_targets(&mut planes, size);
            self.renderer
                .render_frame_into_yuv420p(
                    size,
                    &frame,
                    &FramesWithText::new(&self.frames, &self.texts),
                    YuvEncoding::export(codec),
                    &mut targets,
                )
                .map_err(|error| typed_error(&error))?
        };
        if !report.is_complete() {
            pin_export_frame(&mut self.frames, project, time)?;
            return export_waiting(project, time, &report.missing_sources);
        }
        self.frames.unpin();
        let rendered = Self::rendered_frame(&planes, size, None, time)?;
        ticket(None, time, planes.len() as u64, &rendered)
    }

    /// Drops the held preview frame after a seek.
    #[wasm_bindgen(js_name = resetPlayer)]
    pub fn reset_player(&mut self) {
        self.player.reset();
    }

    /// Renders preview frames at `scale` times the project's size from now
    /// on (0.5 until set).
    ///
    /// # Errors
    /// Rejects a scale that is not finite, above 0 and at most 8.
    #[wasm_bindgen(js_name = setPreviewScale)]
    pub fn set_preview_scale(&mut self, scale: f32) -> Result<(), JsValue> {
        let player = Player::new(QualityProfile::preview().with_resolution_scale(scale))
            .map_err(|error| typed_error(&error))?
            .with_missing_frame_policy(self.player.missing_frame_policy())
            .with_prefetch_frames(self.player.prefetch_frames());
        self.player = player;
        Ok(())
    }

    /// The loaded project's render size at the preview scale.
    ///
    /// # Errors
    /// Fails when no project is loaded.
    #[wasm_bindgen(getter, js_name = previewSize)]
    pub fn preview_size(&self) -> Result<Vec<u32>, JsValue> {
        let size = self
            .player
            .render_size(self.project()?)
            .map_err(|error| typed_error(&error))?;
        Ok(vec![size.width, size.height])
    }

    /// `WebEffectRegistry.frameNeeds`.
    ///
    /// # Errors
    /// Fails without a project, or when the frame cannot be evaluated.
    #[wasm_bindgen(js_name = frameNeeds)]
    pub fn frame_needs(&self, time_ticks: i64) -> Result<String, JsValue> {
        let needs = self
            .project()?
            .frame_needs(MediaTime(time_ticks))
            .map_err(|error| typed_error(&error))?;
        let needs: Vec<_> = needs
            .iter()
            .map(|need| {
                serde_json::json!({
                    "clipId": need.clip_id.as_str(),
                    "readerId": need.reader.as_ref().map(alpha2_model::NodeId::as_str),
                    "sourceId": need.source_id.as_str(),
                    "mediaTimeTicks": need.media_time.ticks().to_string(),
                })
            })
            .collect();
        serde_json::to_string(&needs).map_err(|error| internal_error(error.to_string()))
    }

    /// Appends an OpenType font to the fallback stack text draws with.
    ///
    /// # Errors
    /// Returns an error for malformed font bytes.
    #[wasm_bindgen(js_name = addFont)]
    pub fn add_font(
        &mut self,
        family: String,
        bytes: Vec<u8>,
        face_index: u32,
    ) -> Result<(), JsValue> {
        self.texts
            .add_font(family, bytes, face_index)
            .map_err(|error| typed_error(&error))
    }

    /// Appends every face of an OpenType font file to the fallback stack, in
    /// the file's order: each weight of a collection (`.ttc`), all checked
    /// before any is added, as the export host registers a file.
    ///
    /// # Errors
    /// Returns an error for malformed font bytes, with no face added.
    #[wasm_bindgen(js_name = addFontFile)]
    pub fn add_font_file(&mut self, family: &str, bytes: Vec<u8>) -> Result<(), JsValue> {
        let faces = crate::text::file_faces(family, bytes)
            .map_err(|error| typed_error(&alpha2_engine::TextError::Raster(error)))?;
        for face in faces {
            self.texts
                .add_font(face.family, face.bytes, face.face_index)
                .map_err(|error| typed_error(&error))?;
        }
        Ok(())
    }

    /// Drops every font, and the text drawn with them.
    #[wasm_bindgen(js_name = clearFonts)]
    pub fn clear_fonts(&mut self) {
        self.texts = TextTextures::new();
    }

    /// Draws one project frame at the preview quality and puts it on the
    /// canvas's 2D context, scaled to the canvas, answering the JSON
    /// `WebEffectRegistry.renderAt` answers. A frame equal to the one shown
    /// is neither drawn nor put (`unchanged`).
    ///
    /// # Errors
    /// Fails without a project, or when the frame cannot be drawn.
    #[wasm_bindgen(js_name = renderAt)]
    pub fn render_at(&mut self, time_ticks: i64) -> Result<String, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let size = self
            .player
            .render_size(project)
            .map_err(|error| typed_error(&error))?;
        self.texts
            .prepare_at(&self.renderer, project, MediaTime(time_ticks), size)
            .map_err(|error| typed_error(&error))?;
        let playback = self
            .player
            .render_at(
                project,
                &self.renderer,
                &FramesWithText::new(&self.frames, &self.texts),
                MediaTime(time_ticks),
            )
            .map_err(|error| cpu_engine_error(&error))?;
        let missing_frames =
            missing_frames(project, playback.requested_time, &playback.missing_sources)?;
        let result = WebPlaybackResult {
            requested_time_ticks: playback.requested_time.ticks(),
            presented_time_ticks: playback.presented_time.map(MediaTime::ticks),
            missing_sources: playback
                .missing_sources
                .iter()
                .map(ToString::to_string)
                .collect(),
            missing_frames,
            repeated: playback.repeated,
            unchanged: playback.unchanged,
        };
        let picture = playback
            .texture
            .filter(|_| !playback.unchanged)
            .map(Arc::clone);
        if let Some(picture) = picture {
            self.present(&picture)?;
        }
        serde_json::to_string(&result).map_err(|error| internal_error(error.to_string()))
    }
}

impl WebCpuRenderer {
    fn presented_rgba8(
        &self,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        premultiplied: bool,
        output_width: u32,
        output_height: u32,
    ) -> Result<CpuFrame, JsValue> {
        let size = RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
        let output_size =
            RenderSize::new(output_width, output_height).map_err(|error| typed_error(&error))?;
        self.rgba_source(
            size,
            pixels,
            if premultiplied {
                SourceAlpha::Premultiplied
            } else {
                SourceAlpha::Straight
            },
            output_size,
            Orientation::UPRIGHT,
        )
    }

    fn presented_linear(
        &self,
        width: u32,
        height: u32,
        pixels: &[u16],
        output_width: u32,
        output_height: u32,
    ) -> Result<CpuFrame, JsValue> {
        let size = RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
        let output_size =
            RenderSize::new(output_width, output_height).map_err(|error| typed_error(&error))?;
        let bytes: Vec<u8> = pixels
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let input = bytes.len() as u64;
        let source = self
            .renderer
            .present_linear_rgba16f(
                Arc::new(PackedPixels(bytes)),
                size,
                0,
                presentation(size, output_size, Orientation::UPRIGHT),
            )
            .map_err(|error| typed_error(&error))?;
        Ok(CpuFrame {
            source,
            bytes: input + picture_bytes(output_size),
            raw: false,
        })
    }
}

/// A frame of premultiplied RGBA8 kept at its own size.
fn raw_frame(width: u32, height: u32, pixels: &[u8]) -> Result<CpuFrame, JsValue> {
    let size = RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
    if pixels.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        return Err(invalid_input(
            "RGBA frame byte length does not match its dimensions",
        ));
    }
    let picture =
        CpuImage::from_premultiplied_rgba8(size, pixels).map_err(|error| typed_error(&error))?;
    Ok(CpuFrame {
        bytes: picture.retained_bytes(),
        source: CpuSource::image(Arc::new(picture)),
        raw: true,
    })
}

/// How a picture of `size` is shown in an output of `output_size`: all of
/// it, square pixels, turned as `orientation` says.
fn presentation(
    size: RenderSize,
    output_size: RenderSize,
    orientation: Orientation,
) -> SourcePresentation {
    SourcePresentation {
        output_size,
        visible_rect: Rect {
            x: 0,
            y: 0,
            width: size.width,
            height: size.height,
        },
        orientation,
        sample_aspect_ratio: (1, 1),
        chroma_siting: ChromaLocation::Unspecified,
    }
}

/// A buffer for one I420 frame of `size`, planes packed back to back.
fn i420_buffer(size: RenderSize) -> Vec<u8> {
    let luma = size.width as usize * size.height as usize;
    vec![0; luma + luma / 2]
}

/// [`i420_buffer`]'s three planes as render targets.
fn i420_targets(planes: &mut [u8], size: RenderSize) -> Yuv420pTargets<'_> {
    let width = size.width as usize;
    let luma = width * size.height as usize;
    let (y, chroma) = planes.split_at_mut(luma);
    let (u, v) = chroma.split_at_mut(luma / 4);
    Yuv420pTargets {
        y: PlaneTarget::new(y, width),
        u: PlaneTarget::new(u, width / 2),
        v: PlaneTarget::new(v, width / 2),
    }
}

/// `{ kind: "rendered", index?, timeTicks, retainedBytes, readback }` with
/// the readback settled to `frame`.
fn ticket(
    index: Option<u64>,
    time: MediaTime,
    bytes: u64,
    frame: &web_sys::js_sys::Object,
) -> Result<JsValue, JsValue> {
    let result = web_sys::js_sys::Object::new();
    set(&result, "kind", &JsValue::from_str("rendered"))?;
    if let Some(index) = index {
        #[allow(clippy::cast_precision_loss)] // A frame index stays below 2^53.
        set(&result, "index", &JsValue::from_f64(index as f64))?;
    }
    set(
        &result,
        "timeTicks",
        &JsValue::from_str(&time.ticks().to_string()),
    )?;
    set(
        &result,
        "retainedBytes",
        &js_safe_integer(bytes, "frame byte count")?,
    )?;
    set(&result, "readback", resolved(frame).as_ref())?;
    Ok(result.into())
}

fn parse_codec(codec: &str) -> Result<VideoCodec, JsValue> {
    codec
        .parse()
        .map_err(|error: <VideoCodec as std::str::FromStr>::Err| invalid_input(error.to_string()))
}

/// An engine error on the CPU renderer. Every built-in draws on the CPU,
/// so an effect it does not know is one a GPU would compile at run time:
/// the refusal says so, as the native host's CPU renderer does.
fn cpu_engine_error(error: &alpha2_engine::EngineError) -> JsValue {
    if let alpha2_engine::EngineError::UnregisteredEffect { effect, version } = error {
        return coded_error(
            Code::EFFECT_ERROR,
            format!(
                "effect {effect}@{version} is not built in and has no CPU implementation: \
                 {RUNTIME_EFFECT_REFUSAL}"
            ),
        );
    }
    typed_error(error)
}

/// Divides each pixel's color by its alpha: a canvas's `ImageData` holds
/// straight alpha.
fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = u8::try_from(straight.min(255)).unwrap_or(u8::MAX);
        }
    }
}

/// A thrown JavaScript value's message.
fn js_message(error: &JsValue) -> String {
    web_sys::js_sys::Reflect::get(error, &JsValue::from_str("message"))
        .ok()
        .and_then(|message| message.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| format!("{error:?}"))
}
