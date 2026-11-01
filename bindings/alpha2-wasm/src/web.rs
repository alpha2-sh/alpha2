//! `WebEffectRegistry`, the browser GPU renderer (device, effects, uploaded frames and audio,
//! preview, export), plus free functions that publish engine decisions to the page.

use alpha2_engine::{
    EngineProject, ExportStep, Exporter, FramesWithText, GpuFrameRenderer, Player, QualityProfile,
    TextTextures,
};
use alpha2_gpu::GpuContext;
use alpha2_media::{
    ChromaLocation, ColorMetadata, Orientation, Rect, Rotation, SourceId, VideoCodec, YuvEncoding,
};
use alpha2_model::Project;
use alpha2_render::{
    EffectId, Lut3dData, RenderSize, Renderer, SourcePresentation, builtin_catalog_json,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use wasm_bindgen::prelude::*;

use crate::effect::EffectRequest;
use crate::error::{BindingError, Code, EngineTyped};
use crate::frame_store::{FrameStore, WorkingSetOverBudget};
use crate::interop::{
    DeviceHealth, diagnostics_to_js, export_readback_promise, export_status, frame_upload_receipt,
    gpu_diagnostics_json, js_safe_integer, renderer_with_builtins, resolved, set, texture_byte_len,
    validate_texture_extent, watch_device,
};
use crate::present::{
    Presenter, UploadedTexture, UploadedTextureBacking, WebMissingFrame, WebPlaybackResult,
    present_texture,
};
use crate::project_audio::ProjectAudio;
use crate::project_cache::StalePresentations;

/// An `Alpha2Error` with the `code` chosen by the caller, never parsed from the message.
pub(crate) fn coded_error(code: Code, message: impl std::fmt::Display) -> JsValue {
    BindingError::new(code, message.to_string()).into()
}

/// The caller's input is refused: an argument, a frame, a block.
pub(crate) fn invalid_input(message: impl std::fmt::Display) -> JsValue {
    coded_error(Code::INVALID_INPUT, message)
}

/// The call needs state the binding lacks: a project, an export, a canvas.
pub(crate) fn invalid_state(message: impl std::fmt::Display) -> JsValue {
    coded_error(Code::InvalidState, message)
}

/// The binding's own step failed, not the caller's input.
pub(crate) fn internal_error(message: impl std::fmt::Display) -> JsValue {
    coded_error(Code::INTERNAL, message)
}

/// `RESOURCE_LIMIT` for an upload of `source_id` that would push the export frame's pinned
/// sources over the uploaded-frame budget. Its `failure` has step `upload`, stage
/// `working-set-over-budget`, and `requestedBytes` (the minimum the frame needs).
pub(crate) fn working_set_refusal(refusal: WorkingSetOverBudget, source_id: &str) -> JsValue {
    let error = coded_error(
        Code::ResourceLimit,
        format!(
            "the export frame needs at least {} bytes of uploaded sources at once ({} held, {} \
             for {source_id}), over the {}-byte uploaded-frame budget (setUploadedFrameBudget)",
            refusal.needed(),
            refusal.held,
            refusal.requested,
            refusal.budget,
        ),
    );
    let failure = web_sys::js_sys::Object::new();
    let _ = set(&failure, "code", &JsValue::from_str("RESOURCE_LIMIT"));
    let _ = set(&failure, "phase", &JsValue::from_str("upload"));
    let _ = set(
        &failure,
        "stage",
        &JsValue::from_str("working-set-over-budget"),
    );
    let _ = set(&failure, "resource", &JsValue::from_str("unknown"));
    #[allow(clippy::cast_precision_loss)] // Byte counts stay far below 2^53.
    let _ = set(
        &failure,
        "requestedBytes",
        &JsValue::from_f64(refusal.needed() as f64),
    );
    let _ = web_sys::js_sys::Reflect::set(&error, &JsValue::from_str("failure"), &failure);
    error
}

/// Pins the sources the export frame at `time` shows, so the uploads that refill it cannot
/// evict one another ([`FrameStore::pin`]).
pub(crate) fn pin_export_frame<T>(
    frames: &mut FrameStore<T>,
    project: &EngineProject,
    time: MediaTime,
) -> Result<(), JsValue> {
    let needs = project
        .frame_needs(time)
        .map_err(|error| typed_error(&error))?;
    frames.pin(
        needs
            .into_iter()
            .map(|need| (need.source_id.as_str().to_owned(), need.media_time.ticks())),
    );
    Ok(())
}

/// One fallback the renderer applied, for `diagnostics().degradations`.
fn degradation_json(diagnostic: &alpha2_render::CapabilityDiagnostic) -> serde_json::Value {
    use alpha2_render::CapabilityDiagnostic;
    match diagnostic {
        CapabilityDiagnostic::Rgba8Intermediate => {
            serde_json::json!({ "kind": "rgba8Intermediate" })
        }
        CapabilityDiagnostic::TextureBudgetReduced { requested, applied } => {
            serde_json::json!({ "kind": "textureBudgetReduced", "requested": requested, "applied": applied })
        }
        CapabilityDiagnostic::DecoderSessionLimit { limit } => {
            serde_json::json!({ "kind": "decoderSessionLimit", "limit": limit })
        }
        CapabilityDiagnostic::FilterSamplesReduced { requested, applied } => {
            serde_json::json!({ "kind": "filterSamplesReduced", "requested": requested, "applied": applied })
        }
    }
}

/// An engine error with the code its variant names ([`alpha2_engine::ErrorCode::of`]), as the
/// native host answers it.
pub(crate) fn typed_error<E: EngineTyped>(error: &E) -> JsValue {
    BindingError::typed(error).into()
}

/// How many ticks after a source time a decoded frame's timestamp may fall and still be the
/// frame shown there ([`alpha2_time::presentation_slack`]). The stream's timestamps are whole
/// multiples of `num / den` seconds rounded to `quantum` units (0 for no rounding); an unknown
/// time base gives zero. Decoder adapters allow this much past a requested time before taking
/// the last frame at or before it, as the native host does.
#[wasm_bindgen(js_name = presentationSlackTicks)]
#[must_use]
pub fn presentation_slack_ticks(num: i64, den: i64, quantum: i64) -> i64 {
    alpha2_time::presentation_slack(num, den, quantum).ticks()
}

/// The unit a stream's first timestamps (in time base units) are rounded to, or 0 for none
/// ([`alpha2_time::rounding_quantum`]).
#[wasm_bindgen(js_name = roundingQuantum)]
#[must_use]
pub fn rounding_quantum(timestamps: &[i64]) -> i64 {
    alpha2_time::rounding_quantum(timestamps)
}

/// Resolves a quality level into the rate control every sink encodes with, as JSON
/// `{ "bitrate", "mode", "quantizer" }`. The native host calls the same engine function.
#[wasm_bindgen(js_name = resolveRateControl)]
pub fn resolve_rate_control(
    codec: &str,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    level: &str,
) -> Result<String, JsValue> {
    crate::rate_control::resolve_rate_control_json(
        codec,
        width,
        height,
        frame_rate_num,
        frame_rate_den,
        level,
    )
    .map_err(invalid_input)
}

/// Replaces the GPU watchdog, in milliseconds, that losses are classified against (zero
/// restores the default of 2000). A page has no environment, so tests set it here.
#[wasm_bindgen(js_name = setGpuWatchdogMs)]
pub fn set_gpu_watchdog_ms(milliseconds: u32) {
    alpha2_gpu::set_watchdog_ms(milliseconds);
}

/// The engine's quality level names, lowest first, as a JSON array.
#[wasm_bindgen(js_name = qualityLevels)]
pub fn quality_levels() -> String {
    serde_json::to_string(&alpha2_media::QualityLevel::ALL.map(alpha2_media::QualityLevel::as_str))
        .unwrap_or_else(|_| "[]".to_owned())
}

/// Resolves a quality level into a stereo audio bitrate in bits per second. `codec` is `aac`
/// or `opus`.
#[wasm_bindgen(js_name = resolveAudioBitrate)]
pub fn resolve_audio_bitrate(codec: &str, level: &str) -> Result<u32, JsValue> {
    crate::rate_control::resolve_audio_bitrate_for(codec, level).map_err(invalid_input)
}

/// A runtime effect compiled by [`WebEffectRegistry::compile_effect`],
/// waiting for [`WebEffectRegistry::register_compiled_effect`].
#[wasm_bindgen(js_name = CompiledEffect)]
pub struct WebCompiledEffect(alpha2_render::CompiledEffect);

#[wasm_bindgen(js_class = CompiledEffect)]
impl WebCompiledEffect {
    /// The `id@version` the effect registers under.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}@{}", self.0.id().as_str(), self.0.id().version())
    }
}

/// The browser GPU renderer (WebGPU or WebGL2), with its runtime effect registry.
#[wasm_bindgen]
pub struct WebEffectRegistry {
    renderer: Renderer,
    device: wgpu::Device,
    /// Every queue access that can submit goes through this context.
    gpu: GpuContext,
    frames: FrameStore<UploadedTexture>,
    project: Option<EngineProject>,
    player: Player,
    device_lost: Arc<AtomicBool>,
    health: DeviceHealth,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    presenter: Option<Presenter>,
    browser_webgpu: bool,
    adapter_info: BrowserAdapterInfo,
    video_frame_copy: crate::interop::VideoFrameCopy,
    audio: ProjectAudio,
    exporter: Option<Exporter>,
    /// Fonts, and the captions and text generators drawn with them.
    texts: TextTextures,
    source_uploads: u64,
    physical_source_reuses: u64,
    /// Sources held only as the browser's own conversion (`uploadDegradedVideoFrame`): shown in
    /// previews, refused by every export.
    degraded_sources: std::collections::BTreeSet<String>,
    _context_loss: Option<ContextLossListener>,
}

/// Marks the device lost when a WebGL canvas loses its context, and cancels the event so the
/// browser may restore it.
///
/// WebGL context loss arrives as a DOM event, not through wgpu's device-lost callback, so
/// without this `deviceLost` stays false and a pending readback polls a dead context forever.
/// The WebGL spec restores a lost context only if `webglcontextlost` was cancelled.
struct ContextLossListener {
    target: web_sys::EventTarget,
    callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
}

impl ContextLossListener {
    fn new(target: web_sys::EventTarget, health: DeviceHealth) -> Result<Self, JsValue> {
        let callback = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(
            move |event: web_sys::Event| {
                event.prevent_default();
                health.mark_lost("webglcontextlost", "the WebGL context was lost");
            },
        );
        target.add_event_listener_with_callback(
            "webglcontextlost",
            wasm_bindgen::JsCast::unchecked_ref(callback.as_ref()),
        )?;
        Ok(Self { target, callback })
    }
}

/// The browser's `GPUAdapterInfo` for the renderer's adapter. wgpu keeps only `description`,
/// which Chrome leaves empty, so this is what tells two machines apart. Empty on WebGL2.
#[derive(Clone, Debug, Default)]
struct BrowserAdapterInfo {
    vendor: String,
    architecture: String,
    device: String,
    description: String,
    is_fallback_adapter: bool,
}

impl BrowserAdapterInfo {
    /// Requests an adapter from `navigator.gpu` with wgpu's (empty) options, which yields the
    /// renderer's adapter, and reads its `info`. Empty without WebGPU or an answer.
    async fn read() -> Self {
        use wasm_bindgen::JsCast;
        let get = |target: &JsValue, name: &str| {
            web_sys::js_sys::Reflect::get(target, &JsValue::from_str(name))
                .ok()
                .filter(|value| !value.is_undefined() && !value.is_null())
        };
        let Some(gpu) = get(&web_sys::js_sys::global(), "navigator").and_then(|n| get(&n, "gpu"))
        else {
            return Self::default();
        };
        let Some(request) = get(&gpu, "requestAdapter")
            .and_then(|value| value.dyn_into::<web_sys::js_sys::Function>().ok())
        else {
            return Self::default();
        };
        let Some(promise) = request
            .call0(&gpu)
            .ok()
            .and_then(|value| value.dyn_into::<web_sys::js_sys::Promise>().ok())
        else {
            return Self::default();
        };
        let Some(adapter) = wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .ok()
            .filter(|value| !value.is_null() && !value.is_undefined())
        else {
            return Self::default();
        };
        let info = get(&adapter, "info").unwrap_or(JsValue::NULL);
        let text = |name: &str| {
            get(&info, name)
                .and_then(|value| value.as_string())
                .unwrap_or_default()
        };
        // `isFallbackAdapter` moved from the adapter to its info; either answers.
        let is_fallback_adapter = get(&info, "isFallbackAdapter")
            .or_else(|| get(&adapter, "isFallbackAdapter"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        Self {
            vendor: text("vendor"),
            architecture: text("architecture"),
            device: text("device"),
            description: text("description"),
            is_fallback_adapter,
        }
    }

    /// wgpu's adapter name, or else `<vendor> <architecture>` (Chrome leaves a WebGPU adapter's
    /// description empty).
    fn name(&self, wgpu_name: &str) -> String {
        if wgpu_name.is_empty() {
            [self.vendor.as_str(), self.architecture.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            wgpu_name.to_owned()
        }
    }

    /// Whether the browser flags the adapter as fallback or any field names a software
    /// rasterizer (Chrome's `SwiftShader` has architecture `swiftshader`).
    fn software(&self) -> bool {
        self.is_fallback_adapter
            || [
                &self.vendor,
                &self.architecture,
                &self.device,
                &self.description,
            ]
            .iter()
            .any(|field| alpha2_gpu::software_renderer_name(field))
    }

    /// wgpu's account of `adapter` with this one's fields and [`Self::name`].
    fn json(&self, adapter: &alpha2_render::AdapterDiagnostics) -> serde_json::Value {
        serde_json::json!({
            "name": self.name(&adapter.name),
            "vendor": self.vendor,
            "architecture": self.architecture,
            "device": self.device,
            "description": self.description,
            "isFallbackAdapter": self.is_fallback_adapter,
            "backend": adapter.backend,
            "driver": adapter.driver,
            "driverInfo": adapter.driver_info,
            "shaderCompiler": adapter.shader_compiler,
            "tier": format!("{:?}", adapter.tier),
            "timestampQueries": adapter.timestamp_queries,
        })
    }
}

enum WebCanvas {
    Offscreen(web_sys::OffscreenCanvas),
    Html(web_sys::HtmlCanvasElement),
}

impl WebCanvas {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Offscreen(canvas) => (canvas.width(), canvas.height()),
            Self::Html(canvas) => (canvas.width(), canvas.height()),
        }
    }

    fn target(&self) -> wgpu::SurfaceTarget<'static> {
        match self {
            Self::Offscreen(canvas) => wgpu::SurfaceTarget::OffscreenCanvas(canvas.clone()),
            Self::Html(canvas) => wgpu::SurfaceTarget::Canvas(canvas.clone()),
        }
    }

    fn event_target(&self) -> web_sys::EventTarget {
        match self {
            Self::Offscreen(canvas) => canvas.clone().into(),
            Self::Html(canvas) => canvas.clone().into(),
        }
    }
}

/// Whether a canvas renderer may draw on a software adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SoftwareAdapter {
    Allow,
    Refuse,
}

impl SoftwareAdapter {
    /// `options.softwareAdapter`: `"allow"` (also when absent) or `"refuse"`.
    fn from_options(options: Option<&JsValue>) -> Result<Self, JsValue> {
        let Some(options) = options.filter(|value| !value.is_undefined() && !value.is_null())
        else {
            return Ok(Self::Allow);
        };
        let value = web_sys::js_sys::Reflect::get(options, &JsValue::from_str("softwareAdapter"))
            .unwrap_or(JsValue::UNDEFINED);
        if value.is_undefined() {
            return Ok(Self::Allow);
        }
        match value.as_string().as_deref() {
            Some("allow") => Ok(Self::Allow),
            Some("refuse") => Ok(Self::Refuse),
            _ => Err(invalid_input(
                "options.softwareAdapter must be \"allow\" or \"refuse\"",
            )),
        }
    }
}

/// The unmasked renderer of a throwaway `OffscreenCanvas` WebGL2 context when it is a software
/// rasterizer, so a refusal is known before the caller's canvas is permanently bound to WebGL2.
/// `None` for hardware, or when no WebGL2 context can be made.
fn software_webgl2_renderer() -> Option<String> {
    use wasm_bindgen::JsCast;
    let global = web_sys::js_sys::global();
    let get = |target: &JsValue, name: &str| {
        web_sys::js_sys::Reflect::get(target, &JsValue::from_str(name))
            .ok()
            .filter(|value| !value.is_undefined() && !value.is_null())
    };
    let call = |target: &JsValue, name: &str, arguments: &[JsValue]| {
        let method = get(target, name)?
            .dyn_into::<web_sys::js_sys::Function>()
            .ok()?;
        let arguments: web_sys::js_sys::Array = arguments.iter().collect();
        method
            .apply(target, &arguments)
            .ok()
            .filter(|value| !value.is_undefined() && !value.is_null())
    };
    get(&global, "OffscreenCanvas")?;
    let canvas = web_sys::OffscreenCanvas::new(1, 1).ok()?;
    let context = call(
        canvas.as_ref(),
        "getContext",
        &[JsValue::from_str("webgl2")],
    )?;
    // Without the debug extension the masked renderer is all there is.
    let renderer = call(
        &context,
        "getExtension",
        &[JsValue::from_str("WEBGL_debug_renderer_info")],
    )
    .and_then(|_| {
        call(
            &context,
            "getParameter",
            &[JsValue::from(GL_UNMASKED_RENDERER)],
        )
    })
    .or_else(|| call(&context, "getParameter", &[JsValue::from(GL_RENDERER)]))
    .and_then(|value| value.as_string());
    // Release the context now; browsers cap live WebGL contexts per page.
    if let Some(lose) = call(
        &context,
        "getExtension",
        &[JsValue::from_str("WEBGL_lose_context")],
    ) {
        let _ = call(&lose, "loseContext", &[]);
    }
    renderer.filter(|name| alpha2_gpu::software_renderer_name(name))
}

/// `WEBGL_debug_renderer_info.UNMASKED_RENDERER_WEBGL`.
const GL_UNMASKED_RENDERER: u32 = 0x9246;
/// `RENDERER`.
const GL_RENDERER: u32 = 0x1F01;

/// How long a binding on a canvas with a lost WebGL context waits for restoration before
/// failing as a lost device.
const CONTEXT_RESTORE_TIMEOUT_MS: i32 = 5_000;

/// Whether `canvas` holds a lost WebGL2 context.
///
/// Called once the surface exists, when the context mode is fixed, so `getContext("webgl2")`
/// returns the surface's context (or `null` for WebGPU) and creates nothing.
fn webgl_context_lost(canvas: &JsValue) -> bool {
    use wasm_bindgen::JsCast;
    let method = |target: &JsValue, name: &str| {
        web_sys::js_sys::Reflect::get(target, &JsValue::from_str(name))
            .ok()
            .and_then(|value| value.dyn_into::<web_sys::js_sys::Function>().ok())
    };
    let Some(context) = method(canvas, "getContext")
        .and_then(|get| get.call1(canvas, &JsValue::from_str("webgl2")).ok())
        .filter(JsValue::is_object)
    else {
        return false;
    };
    method(&context, "isContextLost")
        .and_then(|is_lost| is_lost.call0(&context).ok())
        .and_then(|lost| lost.as_bool())
        .unwrap_or(false)
}

/// Waits up to `timeout_ms` for `canvas`'s `webglcontextrestored` event; returns whether it
/// came.
async fn webgl_context_restored(
    canvas: &web_sys::EventTarget,
    timeout_ms: i32,
) -> Result<bool, JsValue> {
    use wasm_bindgen::JsCast;
    let mut resolver = None;
    let settled = web_sys::js_sys::Promise::new(&mut |resolve, _reject| resolver = Some(resolve));
    let Some(resolve) = resolver else {
        return Ok(false);
    };
    let on_restore = {
        let resolve = resolve.clone();
        wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            let _ = resolve.call1(&JsValue::NULL, &JsValue::TRUE);
        })
    };
    let on_timeout = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        let _ = resolve.call1(&JsValue::NULL, &JsValue::FALSE);
    });
    canvas.add_event_listener_with_callback(
        "webglcontextrestored",
        on_restore.as_ref().unchecked_ref(),
    )?;
    let global = web_sys::js_sys::global();
    let timer_function = |name: &str| -> Result<web_sys::js_sys::Function, JsValue> {
        web_sys::js_sys::Reflect::get(&global, &JsValue::from_str(name))?.dyn_into()
    };
    let timer = timer_function("setTimeout")
        .and_then(|set| set.call2(&global, on_timeout.as_ref(), &JsValue::from(timeout_ms)));
    let restored = match &timer {
        Ok(_) => wasm_bindgen_futures::JsFuture::from(settled)
            .await
            .map(|value| value.is_truthy()),
        Err(error) => Err(error.clone()),
    };
    let _ = canvas.remove_event_listener_with_callback(
        "webglcontextrestored",
        on_restore.as_ref().unchecked_ref(),
    );
    if let (Ok(id), Ok(clear)) = (&timer, timer_function("clearTimeout")) {
        let _ = clear.call1(&global, id);
    }
    restored
}

impl Drop for ContextLossListener {
    fn drop(&mut self) {
        let _ = self.target.remove_event_listener_with_callback(
            "webglcontextlost",
            wasm_bindgen::JsCast::unchecked_ref(self.callback.as_ref()),
        );
    }
}

/// `free()` destroys the registry's own device. Otherwise browser WebGPU frees nothing until
/// garbage collection, and an export retry would build its registry on top of the failed one's
/// memory.
impl Drop for WebEffectRegistry {
    fn drop(&mut self) {
        // Destroying the device frees everything; releasing the pools first keeps the counters
        // consistent.
        let _ = self.renderer.release_idle_memory();
        self.device.destroy();
    }
}

#[wasm_bindgen]
impl WebEffectRegistry {
    /// Acquires a surface-independent WebGPU device.
    #[wasm_bindgen(js_name = create)]
    pub async fn create() -> Result<WebEffectRegistry, JsValue> {
        let gpu = GpuContext::request()
            .await
            .map_err(|error| coded_error(Code::GpuInitFailed, error.to_string()))?;
        let device = gpu.device().clone();
        let browser_webgpu = gpu.capabilities().backend == "BrowserWebGpu";
        let adapter_info = if browser_webgpu {
            BrowserAdapterInfo::read().await
        } else {
            BrowserAdapterInfo::default()
        };
        let context = gpu.clone();
        let health = watch_device(&device);
        let device_lost = Arc::clone(&health.lost);
        Ok(Self {
            renderer: renderer_with_builtins(gpu, &health)?,
            device,
            gpu: context,
            frames: FrameStore::sized(
                UploadedTexture::retained_bytes,
                crate::frame_store::DEFAULT_VIDEO_FRAME_BYTES,
            ),
            project: None,
            player: Player::default(),
            device_lost,
            health,
            audio: ProjectAudio::default(),
            exporter: None,
            texts: TextTextures::new(),
            source_uploads: 0,
            physical_source_reuses: 0,
            degraded_sources: std::collections::BTreeSet::new(),
            _context_loss: None,
            surface: None,
            surface_config: None,
            presenter: None,
            browser_webgpu,
            adapter_info,
            video_frame_copy: crate::interop::VideoFrameCopy::default(),
        })
    }

    /// Creates the CPU renderer, for a browser with no WebGPU adapter or WebGL2 context, or on
    /// request. It presents preview frames on `canvas`'s 2D context (`OffscreenCanvas`, page
    /// canvas, or `undefined` for none) and packs export frames in wasm, single-threaded,
    /// byte-identical to the native CPU renderer. Runtime WGSL effects are refused.
    ///
    /// # Errors
    /// Rejects (`INVALID_INPUT`) a canvas with no 2D context, such as one already holding a
    /// WebGL or WebGPU context.
    #[wasm_bindgen(js_name = createCpu)]
    pub fn create_cpu(canvas: &JsValue) -> Result<crate::web_cpu::WebCpuRenderer, JsValue> {
        crate::web_cpu::WebCpuRenderer::new(canvas)
    }

    /// Which renderer this is: `"gpu"` (WebGPU or WebGL2).
    #[wasm_bindgen(getter)]
    #[must_use]
    #[allow(clippy::unused_self)] // A JS getter.
    pub fn renderer(&self) -> String {
        "gpu".to_owned()
    }

    /// Creates a worker-owned renderer for an `OffscreenCanvas`.
    ///
    /// Tries browser WebGPU first, then WebGL2. On WebGPU the device opens before the canvas is
    /// bound, so a failure leaves the canvas free for another context kind (such as the CPU
    /// renderer's 2D one). `options.softwareAdapter` is `"allow"` (default) or `"refuse"`;
    /// refused, a software adapter (WebGPU fallback, `SwiftShader`, llvmpipe, WARP) is skipped
    /// and the call rejects before binding the canvas if nothing else is offered.
    ///
    /// # Errors
    /// Rejects `GPU_INIT_FAILED` when no device opens or only a refused software adapter is
    /// offered; `GPU_DEVICE_LOST` when the canvas's WebGL context is lost and not restored;
    /// `INVALID_INPUT` for a canvas that takes no GPU surface or bad options.
    #[wasm_bindgen(js_name = createForCanvas)]
    pub async fn create_for_canvas(
        canvas: web_sys::OffscreenCanvas,
        options: Option<JsValue>,
    ) -> Result<WebEffectRegistry, JsValue> {
        Self::create_for_surface(
            WebCanvas::Offscreen(canvas),
            SoftwareAdapter::from_options(options.as_ref())?,
        )
        .await
    }

    /// Creates a renderer presenting directly to a main-thread HTML canvas, as
    /// [`Self::create_for_canvas`] does for an `OffscreenCanvas`, with the same `options`.
    ///
    /// # Errors
    /// As [`Self::create_for_canvas`].
    #[wasm_bindgen(js_name = createForHtmlCanvas)]
    pub async fn create_for_html_canvas(
        canvas: web_sys::HtmlCanvasElement,
        options: Option<JsValue>,
    ) -> Result<WebEffectRegistry, JsValue> {
        Self::create_for_surface(
            WebCanvas::Html(canvas),
            SoftwareAdapter::from_options(options.as_ref())?,
        )
        .await
    }

    async fn create_for_surface(
        canvas: WebCanvas,
        software: SoftwareAdapter,
    ) -> Result<WebEffectRegistry, JsValue> {
        let (width, height) = canvas.size();
        // A software adapter refused on WebGPU, named in the refusal if WebGL2 has nothing
        // better.
        let mut refused = None;
        if wgpu::util::is_browser_webgpu_supported().await {
            let instance = alpha2_gpu::new_instance(wgpu::Backends::BROWSER_WEBGPU);
            // Binding the canvas to `webgpu` is permanent, so open (or refuse) the device first.
            let gpu = GpuContext::request_compatible(&instance, None)
                .await
                .map_err(|error| coded_error(Code::GpuInitFailed, error.to_string()))?;
            let adapter_info = BrowserAdapterInfo::read().await;
            if software == SoftwareAdapter::Refuse
                && (gpu.capabilities().software_rasterizer || adapter_info.software())
            {
                refused = Some(adapter_info.name(&gpu.capabilities().adapter_name));
                gpu.device().destroy();
            } else {
                let surface = instance.create_surface(canvas.target()).map_err(|error| {
                    invalid_input(format!("the canvas takes no GPU surface: {error}"))
                })?;
                return Self::on_surface(gpu, surface, &canvas, width, height, adapter_info, true);
            }
        }
        if software == SoftwareAdapter::Refuse
            && let Some(renderer) = software_webgl2_renderer()
        {
            return Err(coded_error(
                Code::GpuInitFailed,
                format!(
                    "the browser offers only the software adapter {}",
                    refused.unwrap_or(renderer)
                ),
            ));
        }
        // WebGL2 needs the surface during adapter selection; a `getContext("webgl2")` that fails
        // binds nothing.
        let instance = alpha2_gpu::new_instance(wgpu::Backends::GL);
        let surface = instance
            .create_surface(canvas.target())
            .map_err(|error| invalid_input(format!("the canvas takes no GPU surface: {error}")))?;
        // glow panics on a lost WebGL context's null extensions during adapter selection, so
        // wait for restoration or fail as a lost device.
        let target = canvas.event_target();
        if webgl_context_lost(target.as_ref())
            && !webgl_context_restored(&target, CONTEXT_RESTORE_TIMEOUT_MS).await?
        {
            return Err(coded_error(
                Code::GPU_DEVICE_LOST,
                format!(
                    "the canvas's WebGL context is lost and the browser did not restore it within {} s; make the renderer on a new canvas",
                    CONTEXT_RESTORE_TIMEOUT_MS / 1000
                ),
            ));
        }
        let gpu = GpuContext::request_compatible(&instance, Some(&surface))
            .await
            .map_err(|error| coded_error(Code::GpuInitFailed, error.to_string()))?;
        Self::on_surface(
            gpu,
            surface,
            &canvas,
            width,
            height,
            BrowserAdapterInfo::default(),
            false,
        )
    }

    fn on_surface(
        gpu: GpuContext,
        surface: wgpu::Surface<'static>,
        canvas: &WebCanvas,
        width: u32,
        height: u32,
        adapter_info: BrowserAdapterInfo,
        browser_webgpu: bool,
    ) -> Result<WebEffectRegistry, JsValue> {
        let config = gpu
            .surface_config(&surface, width.max(1), height.max(1))
            .ok_or_else(|| {
                coded_error(
                    Code::GpuInitFailed,
                    "the GPU device cannot present to this canvas",
                )
            })?;
        // The renderer that caches the limits does not exist yet.
        #[allow(clippy::disallowed_methods)]
        let maximum = gpu.device().limits().max_texture_dimension_2d;
        validate_texture_extent(maximum, width.max(1), height.max(1))?;
        surface.configure(gpu.device(), &config);
        let device = gpu.device().clone();
        let context = gpu.clone();
        let health = watch_device(&device);
        let device_lost = Arc::clone(&health.lost);
        let context_loss = if browser_webgpu {
            None
        } else {
            Some(ContextLossListener::new(
                canvas.event_target(),
                health.clone(),
            )?)
        };
        let presenter = Presenter::new(&device, config.format);
        let renderer = renderer_with_builtins(gpu, &health)?;
        Ok(Self {
            renderer,
            device,
            gpu: context,
            frames: FrameStore::sized(
                UploadedTexture::retained_bytes,
                crate::frame_store::DEFAULT_VIDEO_FRAME_BYTES,
            ),
            project: None,
            player: Player::default(),
            device_lost,
            health,
            audio: ProjectAudio::default(),
            exporter: None,
            texts: TextTextures::new(),
            source_uploads: 0,
            physical_source_reuses: 0,
            degraded_sources: std::collections::BTreeSet::new(),
            _context_loss: context_loss,
            surface: Some(surface),
            surface_config: Some(config),
            presenter: Some(presenter),
            browser_webgpu,
            adapter_info,
            video_frame_copy: crate::interop::VideoFrameCopy::default(),
        })
    }

    /// The graphics API this renderer draws through: `"webgpu"` or `"webgl2"`.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn backend(&self) -> String {
        if self.browser_webgpu {
            "webgpu".to_owned()
        } else {
            "webgl2".to_owned()
        }
    }

    /// The adapter as the browser names it: wgpu's name (the unmasked WebGL renderer or a
    /// WebGPU adapter's description), else `<vendor> <architecture>`; empty when unnamed.
    #[wasm_bindgen(getter)]
    #[must_use]
    pub fn adapter(&self) -> String {
        self.adapter_info
            .name(&self.gpu.capabilities().adapter_name)
    }

    /// Parses one JSON effect definition and compiles it on this binding's device. The promise
    /// resolves to the compiled effect for [`Self::register_compiled_effect`], or rejects with
    /// the shader's diagnostics.
    ///
    /// Returns once the shader and pipeline are created; while the browser compiles, the
    /// binding stays fully usable and may be freed.
    ///
    /// # Errors
    /// Throws `EFFECT_ERROR` for a definition the engine refuses before compiling.
    #[wasm_bindgen(js_name = compileEffect)]
    pub fn compile_effect(&self, json: &str) -> Result<web_sys::js_sys::Promise, JsValue> {
        let definition =
            EffectRequest::parse(json).map_err(|error| coded_error(Code::EFFECT_ERROR, error))?;
        let pending = self
            .renderer
            .compile_effect(definition)
            .map_err(|diagnostics| diagnostics_to_js(Code::EFFECT_ERROR, diagnostics))?;
        Ok(wasm_bindgen_futures::future_to_promise(async move {
            let compiled = pending
                .compiled()
                .await
                .map_err(|diagnostics| diagnostics_to_js(Code::EFFECT_ERROR, diagnostics))?;
            Ok(WebCompiledEffect(compiled).into())
        }))
    }

    /// Adds an effect [`Self::compile_effect`] compiled on this binding and returns its
    /// `id@version`. The compiled effect is consumed either way.
    ///
    /// # Errors
    /// Throws `EFFECT_ERROR` for an effect compiled by another binding, one registered while it
    /// compiled, or a full registry.
    #[wasm_bindgen(js_name = registerCompiledEffect)]
    #[allow(clippy::needless_pass_by_value)] // The compiled effect moves into the registry.
    pub fn register_compiled_effect(
        &mut self,
        compiled: WebCompiledEffect,
    ) -> Result<String, JsValue> {
        let id = self
            .renderer
            .register_compiled(compiled.0)
            .map_err(|diagnostics| diagnostics_to_js(Code::EFFECT_ERROR, diagnostics))?;
        Ok(format!("{}@{}", id.as_str(), id.version()))
    }

    /// Releases one compiled runtime effect by exact identity and version.
    #[wasm_bindgen(js_name = unregisterEffect)]
    pub fn unregister_effect(&mut self, id: String, version: u32) -> bool {
        self.renderer.unregister_effect(&EffectId::new(id, version))
    }

    /// Releases every host-registered runtime effect.
    #[wasm_bindgen(js_name = clearEffects)]
    pub fn clear_effects(&mut self) {
        self.renderer.clear_effects();
    }

    /// Returns the engine-owned effect and transition authoring catalog.
    #[wasm_bindgen(js_name = builtinCatalog)]
    #[allow(clippy::unused_self)] // An instance method in the JS API.
    pub fn builtin_catalog(&self) -> String {
        builtin_catalog_json()
    }

    /// Parses and uploads one `.cube` 3D LUT resource.
    #[wasm_bindgen(js_name = registerLut3d)]
    pub fn register_lut_3d(&mut self, id: String, cube: &str) -> Result<(), JsValue> {
        let data = Lut3dData::from_cube(cube).map_err(|error| typed_error(&error))?;
        self.renderer
            .register_lut3d(id, &data)
            .map_err(|error| typed_error(&error))
    }

    /// Releases one uploaded external effect resource.
    #[wasm_bindgen(js_name = unregisterEffectResource)]
    pub fn unregister_effect_resource(&mut self, id: &str) -> bool {
        self.renderer.unregister_effect_resource(id)
    }

    /// Releases every uploaded external effect resource.
    #[wasm_bindgen(js_name = clearEffectResources)]
    pub fn clear_effect_resources(&mut self) {
        self.renderer.clear_effect_resources();
    }

    /// Uploads a decoded browser frame as [`Self::upload_presented_video_frame`] does, upright,
    /// fitted to the loaded project's size (or the frame's display size with no project), under
    /// `source_id` at `media_time_ticks`.
    ///
    /// Takes ownership of `frame` and always closes it. HDR frames are refused.
    #[wasm_bindgen(js_name = uploadVideoFrame)]
    #[allow(clippy::needless_pass_by_value)] // The API takes ownership and closes the frame.
    pub fn upload_video_frame(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        frame: web_sys::VideoFrame,
    ) -> Result<JsValue, JsValue> {
        let output_size = match self.project.as_ref().map_or_else(
            || RenderSize::new(frame.display_width(), frame.display_height()),
            |project| {
                let settings = &project.project().settings;
                RenderSize::new(settings.width, settings.height)
            },
        ) {
            Ok(size) => size,
            Err(error) => {
                frame.close();
                return Err(typed_error(&error));
            }
        };
        self.upload_presented_inner(
            source_id,
            media_time_ticks,
            PresentedImage::Frame(frame),
            output_size,
            Orientation::UPRIGHT,
            false,
        )
    }

    /// Keeps a frame prepared for presentation under `key` and returns its upload receipt.
    fn insert_presented_frame(
        &mut self,
        key: (String, i64),
        prepared: alpha2_render::PreparedSourceTexture,
    ) -> Result<JsValue, JsValue> {
        let retained_bytes = prepared.retained_bytes();
        let (presented, pending) = prepared.into_parts();
        let view = presented.view().clone();
        let output_bytes = texture_byte_len(presented.size(), presented.format());
        let source_id = key.0.clone();
        let generation = self
            .frames
            .insert(
                key,
                UploadedTexture {
                    backing: UploadedTextureBacking::Rendered {
                        _texture: presented,
                    },
                    view,
                    output_bytes,
                    pending,
                },
            )
            .map_err(|refusal| working_set_refusal(refusal, &source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        // A frame that reaches the engine itself ends a degraded preview of the source.
        self.degraded_sources.remove(&source_id);
        frame_upload_receipt(generation, retained_bytes)
    }

    /// Imports a decoded `VideoFrame` without a CPU readback, then applies contain placement,
    /// rotation, filtering, and alpha premultiplication in a GPU pass. The frame is mirrored
    /// left to right when `mirror` is set, then turned clockwise by `rotation_degrees`.
    ///
    /// Browser WebGPU copies the frame directly, or through a reused 2D canvas bitmap where the
    /// browser does not take a `VideoFrame` (Firefox). WebGL2 always uploads the bitmap.
    /// `videoFrameCopy` says which.
    ///
    /// Both paths use the page's own color conversion, so an HDR frame (PQ or HLG, by the
    /// project's declared color for the source, else the frame's `colorSpace`) is refused with
    /// `INVALID_INPUT`; send its planes to [`Self::upload_yuv_frame`] instead.
    #[wasm_bindgen(js_name = uploadPresentedVideoFrame)]
    // Takes ownership and closes the frame; the arguments mirror the JS call.
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
        let image = PresentedImage::Frame(frame);
        let (output_size, orientation) =
            match presented_placement(output_width, output_height, rotation_degrees, mirror) {
                Ok(placement) => placement,
                Err(error) => {
                    image.close();
                    return Err(error);
                }
            };
        self.upload_presented_inner(
            source_id,
            media_time_ticks,
            image,
            output_size,
            orientation,
            false,
        )
    }

    /// [`Self::upload_presented_video_frame`] for a bitmap from
    /// `createImageBitmap(frame, { premultiplyAlpha: "none" })`, skipping the canvas draw where
    /// `videoFrameCopy` is `"bitmap"`. Always closes `bitmap`; HDR sources are refused.
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
        let image = PresentedImage::Bitmap(bitmap);
        let (output_size, orientation) =
            match presented_placement(output_width, output_height, rotation_degrees, mirror) {
                Ok(placement) => placement,
                Err(error) => {
                    image.close();
                    return Err(error);
                }
            };
        self.upload_presented_inner(
            source_id,
            media_time_ticks,
            image,
            output_size,
            orientation,
            false,
        )
    }

    /// [`Self::upload_presented_video_frame`] for a frame the engine should convert from planes
    /// (HDR, or declared color) but the browser decodes only without planes. The browser's
    /// conversion (and tone mapping) imports it, drawn as SDR BT.709.
    ///
    /// The source is then degraded: `renderAt` lists it in `degradedSources`, and any export
    /// frame showing it is refused with `PLANAR_DECODE_UNAVAILABLE`, so browser and native
    /// exports never differ by the browser's conversion. A planar or presented upload of the
    /// source clears this. Always closes `frame`.
    #[wasm_bindgen(js_name = uploadDegradedVideoFrame)]
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    pub fn upload_degraded_video_frame(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        frame: web_sys::VideoFrame,
        output_width: u32,
        output_height: u32,
        rotation_degrees: u32,
        mirror: bool,
    ) -> Result<JsValue, JsValue> {
        let image = PresentedImage::Frame(frame);
        let (output_size, orientation) =
            match presented_placement(output_width, output_height, rotation_degrees, mirror) {
                Ok(placement) => placement,
                Err(error) => {
                    image.close();
                    return Err(error);
                }
            };
        self.upload_presented_inner(
            source_id,
            media_time_ticks,
            image,
            output_size,
            orientation,
            true,
        )
    }

    /// How [`Self::upload_presented_video_frame`] copies a frame: `"direct"`, `"bitmap"` (once
    /// WebGPU refused a frame), `"untried"`, or `"canvas"` on WebGL2.
    #[wasm_bindgen(getter, js_name = videoFrameCopy)]
    #[must_use]
    pub fn video_frame_copy_mode(&self) -> String {
        match (self.browser_webgpu, self.video_frame_copy) {
            (false, _) => "canvas",
            (true, crate::interop::VideoFrameCopy::Bitmap) => "bitmap",
            (true, crate::interop::VideoFrameCopy::Direct) => "direct",
            (true, crate::interop::VideoFrameCopy::Untried) => "untried",
        }
        .to_owned()
    }

    #[allow(clippy::needless_pass_by_value)] // Takes ownership and closes the image.
    fn upload_presented_inner(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        image: PresentedImage,
        output_size: RenderSize,
        orientation: Orientation,
        degraded: bool,
    ) -> Result<JsValue, JsValue> {
        let hdr = match &image {
            PresentedImage::Frame(frame) => self.shows_hdr(&source_id, frame),
            PresentedImage::Bitmap(_) => self.hdr_source(&source_id),
        };
        if hdr && !degraded {
            image.close();
            return Err(invalid_input(format!(
                "source {source_id} is HDR (PQ or HLG): upload its frames' planes with \
                 uploadYuvFrame, which converts them under the project's color pipeline; a \
                 {} upload would show the browser's own conversion",
                image.kind()
            )));
        }
        let (width, height) = image.size();
        if width == 0 || height == 0 {
            image.close();
            return Err(invalid_input(format!(
                "{} dimensions must be non-zero",
                image.kind()
            )));
        }
        if let Err(error) = validate_texture_extent(
            self.renderer.limits().max_texture_dimension_2d,
            width,
            height,
        ) {
            image.close();
            return Err(error);
        }
        let source_size = match RenderSize::new(width, height) {
            Ok(size) => size,
            Err(error) => {
                image.close();
                return Err(typed_error(&error));
            }
        };
        let texture = match self.renderer.acquire_rgba8_upload_texture(source_size) {
            Ok(texture) => texture,
            Err(error) => {
                image.close();
                return Err(typed_error(&error));
            }
        };
        let alpha = match &image {
            PresentedImage::Frame(frame) if self.browser_webgpu => {
                crate::interop::upload_frame_webgpu(
                    self.gpu.queue(),
                    frame,
                    texture.texture(),
                    width,
                    height,
                    &mut self.video_frame_copy,
                )
            }
            PresentedImage::Frame(frame) => crate::interop::upload_frame_through_canvas(
                &self.gpu,
                frame,
                texture.texture(),
                width,
                height,
            ),
            PresentedImage::Bitmap(bitmap) => crate::interop::upload_bitmap(
                &self.gpu,
                self.browser_webgpu,
                bitmap,
                texture.texture(),
                width,
                height,
            ),
        };
        image.close();
        let alpha = alpha?;
        let prepared = self
            .renderer
            .prepare_rgba_presented(
                texture,
                source_size,
                SourcePresentation {
                    output_size,
                    visible_rect: Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    orientation,
                    sample_aspect_ratio: (1, 1),
                    chroma_siting: ChromaLocation::Unspecified,
                },
                alpha,
            )
            .map_err(|error| typed_error(&error))?;
        let receipt =
            self.insert_presented_frame((source_id.clone(), media_time_ticks), prepared)?;
        if degraded {
            self.degraded_sources.insert(source_id);
        }
        Ok(receipt)
    }

    /// Uploads a decoded frame's planar YCbCr samples as `VideoFrame.copyTo` writes them and
    /// converts them with the engine's shader under the project's color pipeline. This is how
    /// HDR (PQ or HLG) frames reach the engine, since texture copies go through the page's
    /// color conversion.
    ///
    /// `format` is a `WebCodecs` pixel format: `I420`, `I422`, or `I444` at 8, 10 (`P10`), or
    /// 12 bits (`P12`), the same with alpha (`I420A`, `I420AP10`, ...), or `NV12`. Samples
    /// above eight bits are 16-bit words with the value in the low bits. `layout` is
    /// `[offset, stride]` per plane, `visible` is `[x, y, width, height]`, and `color` is the
    /// frame's color metadata as project JSON. `chroma_siting` is `left`, `center`, `top-left`,
    /// or `unspecified` (`WebCodecs` leaves it to the demuxer). `display_width` by
    /// `display_height` is the visible rectangle's display size before rotation, which gives
    /// the pixel aspect ratio. The frame is mirrored when `mirror` is set, then turned
    /// clockwise by `rotation_degrees`.
    ///
    /// # Errors
    /// Rejects (`INVALID_INPUT`) an unknown format or chroma siting, a layout or plane that does
    /// not fit the data, a bad size or rotation, and an HDR frame the project's color pipeline
    /// refuses.
    #[wasm_bindgen(js_name = uploadYuvFrame)]
    #[allow(clippy::too_many_arguments)]
    pub fn upload_yuv_frame(
        &mut self,
        source_id: String,
        media_time_ticks: i64,
        format: &str,
        coded_width: u32,
        coded_height: u32,
        data: &[u8],
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
        // Luma, chroma, then alpha at luma size; planes the format does not store stay empty.
        let plane = |index: usize| -> Result<alpha2_render::SourcePlane<'_>, JsValue> {
            if index >= yuv_format.plane_count() {
                return Ok(alpha2_render::SourcePlane {
                    bytes: &[],
                    stride: 0,
                });
            }
            let rows = if index == 0 || index == plane_count {
                coded_height
            } else {
                chroma_rows
            };
            let stride = layout[index * 2 + 1];
            let offset = usize::try_from(layout[index * 2]).unwrap_or(usize::MAX);
            let bytes = usize::try_from(u64::from(stride) * u64::from(rows))
                .ok()
                .and_then(|length| data.get(offset..offset.checked_add(length)?))
                .ok_or_else(|| invalid_input("YUV plane runs past the frame data"))?;
            Ok(alpha2_render::SourcePlane { bytes, stride })
        };
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
        let policy = self.project.as_ref().map_or_else(
            || alpha2_engine::color_policy(&alpha2_model::ColorPipeline::default()),
            |project| alpha2_engine::color_policy(&project.project().settings.color),
        );
        if policy == alpha2_render::ColorPolicy::RejectHdr && alpha2_render::is_hdr(color) {
            return Err(invalid_input(format!(
                "the project refuses HDR sources (its color pipeline's hdr is \"reject\"): {source_id}"
            )));
        }
        let (prepared, _) = self
            .renderer
            .prepare_yuv_presented(
                alpha2_render::YuvFrame {
                    format: yuv_format,
                    planes: [
                        plane(0)?,
                        plane(1)?,
                        if plane_count == 3 {
                            plane(2)?
                        } else {
                            alpha2_render::SourcePlane {
                                bytes: &[],
                                stride: 0,
                            }
                        },
                    ],
                    alpha_plane: yuv_format.alpha.then(|| plane(plane_count)).transpose()?,
                    size: coded,
                    alpha: 1.0,
                    color: Some(color),
                },
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
        self.insert_presented_frame((source_id, media_time_ticks), prepared)
    }

    /// Uploads RGBA8 pixels at their own size and presents them into the output as the engine
    /// presents every source (contained, on the GPU), so a page never scales a source itself.
    /// `premultiplied` says whether color is already multiplied by alpha. Extension providers'
    /// frames arrive this way.
    #[wasm_bindgen(js_name = uploadPresentedRgbaFrame)]
    #[allow(clippy::too_many_arguments)]
    pub fn upload_presented_rgba_frame(
        &mut self,
        source_id: &str,
        media_time_ticks: i64,
        width: u32,
        height: u32,
        pixels: &[u8],
        premultiplied: bool,
        output_width: u32,
        output_height: u32,
    ) -> Result<JsValue, JsValue> {
        let texture = self.presented_rgba(
            width,
            height,
            PresentedPixels::Rgba8 {
                pixels,
                premultiplied,
            },
            output_width,
            output_height,
        )?;
        let retained_bytes = texture.retained_bytes();
        let generation = self
            .frames
            .insert((source_id.to_owned(), media_time_ticks), texture)
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, retained_bytes)
    }

    /// Like [`Self::upload_presented_rgba_frame`], for premultiplied linear-light RGBA half
    /// floats (as bits) in BT.709 primaries, as `OpenEXR` sources arrive. Values outside
    /// `0.0..=1.0` are kept as far as the intermediate format allows.
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
        let texture = self.presented_rgba(
            width,
            height,
            PresentedPixels::Linear(pixels),
            output_width,
            output_height,
        )?;
        let retained_bytes = texture.retained_bytes();
        let generation = self
            .frames
            .insert((source_id.to_owned(), media_time_ticks), texture)
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, retained_bytes)
    }

    /// Like [`Self::upload_presented_linear_frame`], for a still shown at every time.
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
        let texture = self.presented_rgba(
            width,
            height,
            PresentedPixels::Linear(pixels),
            output_width,
            output_height,
        )?;
        let retained_bytes = texture.retained_bytes();
        let generation = self
            .frames
            .insert_image(source_id.to_owned(), texture)
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, retained_bytes)
    }

    /// Like [`Self::upload_presented_rgba_frame`], for a still shown at every time.
    #[wasm_bindgen(js_name = uploadPresentedRgbaImage)]
    #[allow(clippy::too_many_arguments)]
    pub fn upload_presented_rgba_image(
        &mut self,
        source_id: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
        premultiplied: bool,
        output_width: u32,
        output_height: u32,
    ) -> Result<JsValue, JsValue> {
        let texture = self.presented_rgba(
            width,
            height,
            PresentedPixels::Rgba8 {
                pixels,
                premultiplied,
            },
            output_width,
            output_height,
        )?;
        let retained_bytes = texture.retained_bytes();
        let generation = self
            .frames
            .insert_image(source_id.to_owned(), texture)
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, retained_bytes)
    }

    fn presented_rgba(
        &self,
        width: u32,
        height: u32,
        pixels: PresentedPixels<'_>,
        output_width: u32,
        output_height: u32,
    ) -> Result<UploadedTexture, JsValue> {
        validate_texture_extent(
            self.renderer.limits().max_texture_dimension_2d,
            width,
            height,
        )?;
        let size = RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
        let output_size =
            RenderSize::new(output_width, output_height).map_err(|error| typed_error(&error))?;
        let presentation = SourcePresentation {
            output_size,
            visible_rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            orientation: Orientation::UPRIGHT,
            sample_aspect_ratio: (1, 1),
            chroma_siting: ChromaLocation::Unspecified,
        };
        let prepared = match pixels {
            PresentedPixels::Rgba8 {
                pixels,
                premultiplied,
            } => {
                let upload = self
                    .renderer
                    .upload_rgba8(size, pixels)
                    .map_err(|error| typed_error(&error))?;
                self.renderer.prepare_rgba_presented(
                    upload,
                    size,
                    presentation,
                    if premultiplied {
                        alpha2_render::SourceAlpha::Premultiplied
                    } else {
                        alpha2_render::SourceAlpha::Straight
                    },
                )
            }
            PresentedPixels::Linear(pixels) => {
                let upload = self
                    .renderer
                    .upload_linear_rgba16f(size, pixels)
                    .map_err(|error| typed_error(&error))?;
                self.renderer
                    .prepare_linear_presented(upload, size, presentation)
            }
        }
        .map_err(|error| typed_error(&error))?;
        let (presented, pending) = prepared.into_parts();
        let view = presented.view().clone();
        let output_bytes = texture_byte_len(presented.size(), presented.format());
        Ok(UploadedTexture {
            backing: UploadedTextureBacking::Rendered {
                _texture: presented,
            },
            view,
            output_bytes,
            pending,
        })
    }

    /// Whether `frame` of `source_id` is HDR, by the project's declared color for the source
    /// if any (as natively), else by the frame's own transfer.
    fn shows_hdr(&self, source_id: &str, frame: &web_sys::VideoFrame) -> bool {
        let declared = self.project.as_ref().and_then(|project| {
            project
                .project()
                .sources
                .iter()
                .find(|source| source.id.as_str() == source_id)
                .and_then(|source| source.color_metadata)
        });
        declared.map_or_else(
            || {
                matches!(
                    frame.color_space().transfer(),
                    Some(
                        web_sys::VideoTransferCharacteristics::Pq
                            | web_sys::VideoTransferCharacteristics::Hlg
                    )
                )
            },
            alpha2_render::is_hdr,
        )
    }

    /// Whether the loaded project declares `source_id` an HDR (PQ or HLG) source, whose frames
    /// must go through [`Self::upload_yuv_frame`].
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

    /// The color metadata the loaded project declares for `source_id`, as JSON, or `undefined`.
    #[wasm_bindgen(js_name = sourceColorMetadata)]
    #[must_use]
    pub fn source_color_metadata(&self, source_id: &str) -> Option<String> {
        self.project.as_ref().and_then(|project| {
            project
                .project()
                .sources
                .iter()
                .find(|source| source.id.as_str() == source_id)
                .and_then(|source| source.color_metadata)
                .and_then(|color| serde_json::to_string(&color).ok())
        })
    }

    /// Uploads tightly packed premultiplied RGBA8 pixels. Used by WebGL2, whose wgpu backend
    /// cannot import a `VideoFrame` directly.
    #[wasm_bindgen(js_name = uploadRgbaFrame)]
    pub fn upload_rgba_frame(
        &mut self,
        source_id: &str,
        media_time_ticks: i64,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<JsValue, JsValue> {
        let texture = self.rgba_texture(width, height, pixels)?;
        let retained_bytes = texture.retained_bytes();
        let generation = self
            .frames
            .insert((source_id.to_owned(), media_time_ticks), texture)
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, retained_bytes)
    }

    /// Uploads one time-independent still image, kept until its generation is released.
    #[wasm_bindgen(js_name = uploadRgbaImage)]
    pub fn upload_rgba_image(
        &mut self,
        source_id: &str,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<JsValue, JsValue> {
        let texture = self.rgba_texture(width, height, pixels)?;
        let retained_bytes = texture.retained_bytes();
        let generation = self
            .frames
            .insert_image(source_id.to_owned(), texture)
            .map_err(|refusal| working_set_refusal(refusal, source_id))?;
        self.source_uploads = self.source_uploads.saturating_add(1);
        frame_upload_receipt(generation, retained_bytes)
    }

    /// Aliases a new exact source-time request to an already uploaded frame. Returns `false`
    /// when no frame is stored at the existing time.
    ///
    /// # Errors
    /// Refuses with `RESOURCE_LIMIT` an alias that would push the export frame's sources over
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

    fn rgba_texture(
        &self,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<UploadedTexture, JsValue> {
        validate_texture_extent(
            self.renderer.limits().max_texture_dimension_2d,
            width,
            height,
        )?;
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid_input("RGBA frame dimensions overflow"))?;
        if width == 0 || height == 0 || pixels.len() != expected {
            return Err(invalid_input(
                "RGBA frame byte length does not match its dimensions",
            ));
        }
        let size = RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
        let texture = self
            .renderer
            .acquire_rgba8_upload_texture(size)
            .map_err(|error| typed_error(&error))?;
        self.gpu.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: texture.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.view().clone();
        Ok(UploadedTexture {
            backing: UploadedTextureBacking::Raw { _texture: texture },
            view,
            output_bytes: u64::try_from(expected)
                .map_err(|_| invalid_input("RGBA frame byte length exceeds u64"))?,
            pending: None,
        })
    }

    /// Releases an uploaded source texture after decode-cache eviction.
    #[wasm_bindgen(js_name = releaseFrame)]
    pub fn release_frame(&mut self, source_id: &str, media_time_ticks: i64) -> bool {
        self.frames.remove(source_id, media_time_ticks)
    }

    /// Releases exactly one uploaded generation, leaving any newer texture under the same
    /// source and timestamp.
    #[wasm_bindgen(js_name = releaseFrameGeneration)]
    pub fn release_frame_generation(&mut self, generation: u64) -> bool {
        self.frames.release_generation(generation)
    }

    /// Whether the still `sourceId` is still the upload `generation` names. The store evicts
    /// stills on its own, so a host checks before skipping a re-upload.
    #[wasm_bindgen(js_name = holdsImage)]
    pub fn holds_image(&self, source_id: &str, generation: u64) -> bool {
        self.frames.holds_image(source_id, generation)
    }

    /// Releases every uploaded frame and the still of one source whose media changed under the
    /// same ID and locator. Retired generations stay until released.
    #[wasm_bindgen(js_name = releaseSourceFrames)]
    pub fn release_source_frames(&mut self, source_id: &str) -> bool {
        self.degraded_sources.remove(source_id);
        self.frames.release_source(source_id)
    }

    /// Releases all uploaded video frames, images, and retired generations.
    #[wasm_bindgen(js_name = clearFrames)]
    pub fn clear_frames(&mut self) {
        self.degraded_sources.clear();
        self.frames.clear();
    }

    /// Sets the cache budget for live, image, and retired generations (default 1 GiB); aliases
    /// count once and the oldest generations go first. A new frame larger than the budget is
    /// kept alone.
    ///
    /// While an export frame waits for its sources, those it shows are pinned. The budget
    /// evicts everything else first, and an upload or alias that would push them over is
    /// refused with `RESOURCE_LIMIT` (`failure.stage` `working-set-over-budget`,
    /// `failure.requestedBytes` the minimum the frame needs).
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

    /// Lets rendered results (each clip's finished picture and the frame) use up to `bytes` of
    /// GPU memory for reuse when a frame repeats; zero (the default) disables it. Only results
    /// from reused uploads hit: a still, or a frame uploaded once and drawn at several times.
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

    /// Releases `source_id`'s video frames older than `media_time_ticks`, a time on that
    /// source's own timeline. A source two clips read at different times is trimmed only below
    /// what both need.
    #[wasm_bindgen(js_name = trimFramesBefore)]
    pub fn trim_frames_before(&mut self, source_id: &str, media_time_ticks: i64) {
        self.frames.trim_before(source_id, media_time_ticks);
    }

    /// Returns replaced generations still awaiting explicit release.
    #[wasm_bindgen(getter, js_name = retiredFrameCount)]
    pub fn retired_frame_count(&self) -> usize {
        self.frames.retired_len()
    }

    /// Returns the number of browser frames currently retained by the binding.
    #[wasm_bindgen(getter, js_name = uploadedFrameCount)]
    pub fn uploaded_frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Returns GPU texture bytes retained by uploaded source generations.
    #[wasm_bindgen(getter, js_name = uploadedFrameBytes)]
    pub fn uploaded_frame_bytes(&self) -> u64 {
        self.frames.retained_bytes(UploadedTexture::retained_bytes)
    }

    /// Whether decoded `VideoFrame`s can be imported without CPU copies. Both backends can,
    /// directly or through a canvas bitmap (see `uploadPresentedVideoFrame`).
    #[wasm_bindgen(getter, js_name = supportsExternalVideoFrame)]
    #[allow(clippy::unused_self, clippy::missing_const_for_fn)] // A JS getter.
    pub fn supports_external_video_frame(&self) -> bool {
        true
    }

    /// Destroys GPU memory kept only for reuse (idle pooled textures and buffers, kept graph
    /// results) so the browser frees it now rather than at garbage collection, and returns the
    /// bytes destroyed. Nothing a render or readback holds is released. An export calls it
    /// when its byte budget stops it submitting.
    #[wasm_bindgen(js_name = releaseIdleMemory)]
    #[allow(clippy::cast_precision_loss)] // Byte counts stay far below 2^53.
    pub fn release_idle_memory(&self) -> f64 {
        self.renderer.release_idle_memory() as f64
    }

    /// Whether the owning worker must recreate this binding: its device or context was lost,
    /// or the GPU stopped running work, after which every readback is refused. A frame the GPU
    /// rejected with an error is not a loss; the binding renders on.
    #[wasm_bindgen(getter, js_name = deviceLost)]
    pub fn device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Acquire) || self.renderer.gpu_lost().is_some()
    }

    /// After a loss ([`Self::device_lost`]), what the renderer's submission times say about it,
    /// as JSON `{ hung, gpuMs, watchdogMs, timeTicks, frameIndex, effects, predictedMs }`.
    /// `hung` means the longest recent submission ran at least half the GPU watchdog, so the
    /// loss was most likely this work's and rerunning it would stop the GPU again. `undefined`
    /// before any loss.
    #[wasm_bindgen(getter, js_name = lossReport)]
    pub fn loss_report(&self) -> Option<String> {
        if !self.device_lost() {
            return None;
        }
        let longest = self.renderer.longest_submission();
        let hung = longest
            .as_ref()
            .is_some_and(|report| report.gpu_ms >= f64::from(alpha2_gpu::hung_threshold_ms()));
        let frame = longest.as_ref().and_then(|report| report.frame);
        Some(
            serde_json::json!({
                "hung": hung,
                "gpuMs": longest.as_ref().map(|report| (report.gpu_ms * 10.0).round() / 10.0),
                "watchdogMs": alpha2_gpu::watchdog_ms(),
                "timeTicks": frame.map(|frame| frame.time_ticks.to_string()),
                "frameIndex": frame.and_then(|frame| frame.frame_index),
                "predictedMs": longest
                    .as_ref()
                    .and_then(|report| report.predicted_ms)
                    .map(|ms| (ms * 10.0).round() / 10.0),
                "effects": longest.map(|report| report.effects).unwrap_or_default(),
            })
            .to_string(),
        )
    }

    /// Reconfigures the presentation surface after the host changes its canvas size.
    #[wasm_bindgen(js_name = resizeSurface)]
    pub fn resize_surface(&mut self, width: u32, height: u32) -> Result<(), JsValue> {
        validate_texture_extent(
            self.renderer.limits().max_texture_dimension_2d,
            width,
            height,
        )?;
        let surface = self
            .surface
            .as_ref()
            .ok_or_else(|| invalid_state("renderer was created without a canvas"))?;
        let config = self
            .surface_config
            .as_mut()
            .ok_or_else(|| invalid_state("surface configuration is unavailable"))?;
        config.width = width;
        config.height = height;
        surface.configure(&self.device, config);
        self.player.reset();
        Ok(())
    }

    /// The shared capability report JSON (`alpha2-capabilities`) with runtime `browser`.
    /// `decode` and `encode` are `null` for the page to fill; `features` names only
    /// `renderer.gpu`, `renderer.cpu`, `effects.runtime`, and on WebGL2
    /// `uploads.webgl2-bitmap` (unavailable where frames upload as CPU pixels).
    ///
    /// # Errors
    /// Returns an error if the report cannot be serialized, or on WebGL2 when the upload probe
    /// throws.
    #[wasm_bindgen(js_name = capabilityReport)]
    pub fn capability_report(&self) -> Result<String, JsValue> {
        let report = alpha2_engine::CapabilityReport::new(
            alpha2_engine::Runtime::Browser,
            &self.renderer,
        )
        .with_feature("renderer.gpu", alpha2_engine::Feature::available())
        .with_feature(
            "renderer.cpu",
            alpha2_engine::Feature::unavailable(
                "this renderer draws on a GPU; createCpu makes the one that draws on the CPU",
            ),
        )
        .with_feature("effects.runtime", alpha2_engine::Feature::available());
        let report = if self.browser_webgpu {
            report
        } else {
            report.with_feature(
                "uploads.webgl2-bitmap",
                match crate::interop::webgl2_uploads()? {
                    crate::interop::WebGl2Uploads::Bitmap { .. } => {
                        alpha2_engine::Feature::available()
                    }
                    crate::interop::WebGl2Uploads::CpuPixels => alpha2_engine::Feature::unavailable(
                        "this browser's WebGL2 does not upload canvas bitmaps; frames are uploaded as CPU pixels",
                    ),
                },
            )
        };
        serde_json::to_string(&report).map_err(|error| internal_error(error.to_string()))
    }

    /// How frames rendered since the last call were split into GPU submissions to stay under
    /// the watchdog budget, as JSON `{ frames, splitFrames, maxPerFrame, maxPredictedMs }` (the
    /// native host's `performance.gpuSubmissions`). Resets the counts.
    #[wasm_bindgen(js_name = takeGpuSubmissionStats)]
    pub fn take_gpu_submission_stats(&self) -> String {
        let stats = self.renderer.take_gpu_submission_stats();
        serde_json::json!({
            "frames": stats.frames,
            "splitFrames": stats.split_frames,
            "maxPerFrame": stats.max_per_frame,
            "maxPredictedMs": stats.max_predicted_ms.map(|ms| (ms * 10.0).round() / 10.0),
        })
        .to_string()
    }

    /// Returns a JSON snapshot suitable for browser developer tooling.
    #[wasm_bindgen(js_name = diagnostics)]
    pub fn diagnostics(&self) -> Result<String, JsValue> {
        let diagnostics = self.renderer.diagnostics();
        let value = serde_json::json!({
            "gpu": gpu_diagnostics_json(&self.health.log),
            "resultCache": result_cache_json(diagnostics.result_cache),
            "adapter": self.adapter_info.json(&diagnostics.adapter),
            "passes": diagnostics.passes.iter().map(|pass| serde_json::json!({
                "label": pass.label,
                "nanoseconds": pass.nanoseconds,
                "source": format!("{:?}", pass.source),
            })).collect::<Vec<_>>(),
            "textures": {
                "created": diagnostics.textures.created,
                "reused": diagnostics.textures.reused,
                "active": diagnostics.textures.active,
                "activeBytes": diagnostics.textures.active_bytes,
                "highWaterBytes": diagnostics.textures.high_water_bytes,
                "available": diagnostics.textures.available,
                "availableBytes": diagnostics.textures.available_bytes,
                "budgetBytes": diagnostics.textures.budget_bytes,
                "evicted": diagnostics.textures.evicted,
            },
            "uploadTextures": {
                "created": diagnostics.upload_textures.created,
                "reused": diagnostics.upload_textures.reused,
                "discarded": diagnostics.upload_textures.discarded,
                "active": diagnostics.upload_textures.active,
                "activeBytes": diagnostics.upload_textures.active_bytes,
                "highWaterBytes": diagnostics.upload_textures.high_water_bytes,
                "available": diagnostics.upload_textures.available,
                "availableBytes": diagnostics.upload_textures.available_bytes,
                "budgetBytes": diagnostics.upload_textures.budget_bytes,
            },
            "readbackBuffers": {
                "created": diagnostics.readback_buffers.created,
                "reused": diagnostics.readback_buffers.reused,
                "discarded": diagnostics.readback_buffers.discarded,
                "active": diagnostics.readback_buffers.active,
                "activeBytes": diagnostics.readback_buffers.active_bytes,
                "highWaterBytes": diagnostics.readback_buffers.high_water_bytes,
                "available": diagnostics.readback_buffers.available,
                "availableBytes": diagnostics.readback_buffers.available_bytes,
            },
            "uniformBuffers": {
                "created": diagnostics.uniform_buffers.created,
                "reused": diagnostics.uniform_buffers.reused,
                "discarded": diagnostics.uniform_buffers.discarded,
                "active": diagnostics.uniform_buffers.active,
                "activeBytes": diagnostics.uniform_buffers.active_bytes,
                "highWaterBytes": diagnostics.uniform_buffers.high_water_bytes,
                "available": diagnostics.uniform_buffers.available,
                "availableBytes": diagnostics.uniform_buffers.available_bytes,
            },
            "pipelines": {
                "runtimeEffects": diagnostics.pipelines.runtime_effects,
                "builtins": diagnostics.pipelines.builtins,
            },
            "frames": {
                "presented": diagnostics.presented_frames,
                "dropped": diagnostics.dropped_frames,
                "repeated": diagnostics.repeated_frames,
            },
            "sourceFrames": {
                "count": self.frames.len(),
                "retired": self.frames.retired_len(),
                "retainedBytes": self.frames.retained_bytes(UploadedTexture::retained_bytes),
                "uploads": self.source_uploads,
                "physicalReuses": self.physical_source_reuses,
            },
            "decoderWaitNanoseconds": u64::try_from(diagnostics.decoder_wait.as_nanos())
                .unwrap_or(u64::MAX),
            "maxFilterSamples": diagnostics.max_filter_samples,
            "webgl2Uploads": crate::interop::measured_webgl2_uploads()
                .filter(|_| !self.browser_webgpu)
                .map(crate::interop::WebGl2Uploads::name),
            "motionBlurSubframes": diagnostics.motion_blur_subframes,
            "degradations": diagnostics
                .degradations
                .iter()
                .map(degradation_json)
                .collect::<Vec<_>>(),
            "work": {
                "graphNodes": diagnostics.graph_nodes,
                "graphRenderPasses": diagnostics.graph_passes,
                "renderPasses": diagnostics.render_passes,
                "sourceSubmissions": diagnostics.source_submissions,
                "graphSubmissions": diagnostics.graph_submissions,
                "readbackSubmissions": diagnostics.readback_submissions,
                "frameSubmissions": diagnostics.frame_submissions,
                "gpuCopyOperations": diagnostics.copy_operations,
                "gpuCopyBytes": diagnostics.copy_bytes,
            },
        });
        serde_json::to_string(&value).map_err(|error| internal_error(error.to_string()))
    }

    /// Draws an uploaded frame to the configured worker canvas.
    #[wasm_bindgen(js_name = presentUploadedFrame)]
    pub fn present_uploaded_frame(
        &self,
        source_id: &str,
        media_time_ticks: i64,
    ) -> Result<(), JsValue> {
        let source = self
            .frames
            .get(source_id, media_time_ticks)
            .ok_or_else(|| coded_error(Code::MISSING_INPUT, "uploaded frame key was not found"))?;
        if let Some(pending) = source.pending.as_ref() {
            self.renderer.submit_pending_source(pending);
        }
        present_texture(
            self.surface.as_ref(),
            self.surface_config.as_ref(),
            self.presenter.as_ref(),
            &self.device,
            &self.gpu,
            &self.device_lost,
            &source.view,
        )
    }

    /// Enables or disables per-pass GPU/CPU timing in `diagnostics()`. Off by default because
    /// it reads a clock per pass and allocates GPU timestamp resources.
    #[wasm_bindgen(js_name = setPassTiming)]
    pub fn set_pass_timing(&self, enabled: bool) {
        self.renderer.set_pass_timing_enabled(enabled);
    }

    /// Loads and validates a versioned project document.
    ///
    /// Uploaded frames and audio are kept for every source whose ID and locator are unchanged,
    /// so an edit needs no re-decode. Presentation frames are invalidated by a new canvas size
    /// or color pipeline (`settings.color`), or per source by new declared color
    /// (`color_metadata`). All audio is dropped when the project sample rate changes. Media
    /// that changes under an unchanged locator needs a new source ID.
    ///
    /// Loading ends a running export (`endExport`), so its next step is refused.
    #[wasm_bindgen(js_name = loadProject)]
    pub fn load_project(&mut self, json: &str) -> Result<(), JsValue> {
        let project = Project::from_json(json).map_err(|error| typed_error(&error))?;
        let snapshot = EngineProject::new(project).map_err(|error| typed_error(&error))?;
        let audio = self.audio.mixer_for(&snapshot)?;
        // An editor reloads on every edit, so keep media for sources whose identity is
        // unchanged.
        if let Some(previous) = self.project.as_ref() {
            let stale = crate::project_cache::stale_sources(previous.project(), snapshot.project());
            self.frames.retain_sources(|source| !stale.contains(source));
            self.degraded_sources
                .retain(|source| !stale.contains(source));
            let raw = |texture: &UploadedTexture| {
                matches!(&texture.backing, UploadedTextureBacking::Raw { .. })
            };
            match crate::project_cache::stale_presentations(previous.project(), snapshot.project())
            {
                StalePresentations::All => self.frames.retain_values(raw),
                StalePresentations::Sources(sources) => {
                    if !sources.is_empty() {
                        self.frames.retain_entries(|source, texture| {
                            raw(texture) || !sources.contains(source)
                        });
                    }
                }
            }
        } else {
            self.degraded_sources.clear();
            self.frames.clear();
        }
        self.audio.install(self.project.as_ref(), &snapshot, audio);
        self.project = Some(snapshot);
        self.player.reset();
        self.exporter = None;
        Ok(())
    }

    /// Uploads project-rate planar audio for one source.
    ///
    /// `start_sample` is the first sample's index in the source at the project rate. `layout`
    /// names each plane's speaker: an array of `FFmpeg` channel names in plane order
    /// (`["FC", "FL", "FR"]`, `"UNK"` for none), or a channel count for `FFmpeg`'s default
    /// layout (3 is 2.1, 4 is 4.0); 1 to 24 planes. `samples` holds the planes end to end. An
    /// upload replaces windows it overlaps and joins same-speaker windows it meets.
    ///
    /// The mixer reads a window only where it holds every sample a block needs, so a host
    /// decoding past a source's end uploads zeros there. Alternatively pass `ends_source` with
    /// an upload that runs to the source's end, so the source is silent after it.
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

    /// Total retained audio samples across every source, for host budgeting.
    #[wasm_bindgen(js_name = audioRetainedFrames)]
    pub fn audio_retained_frames(&self) -> u32 {
        self.audio.retained_frames()
    }

    /// Allocated audio sample bytes, including spare capacity.
    #[wasm_bindgen(js_name = audioRetainedBytes)]
    pub fn audio_retained_bytes(&self) -> u32 {
        self.audio.retained_bytes()
    }

    /// Half-open project-rate sample intervals of one source's windows, most recently uploaded
    /// first, as JSON `[{startSample, endSample}]`. A host decodes only what these do not
    /// cover, since the store may have extended, replaced or dropped windows.
    #[wasm_bindgen(js_name = audioRetainedWindows)]
    pub fn audio_retained_windows(&self, source_id: &str) -> Result<String, JsValue> {
        self.audio.retained_windows(source_id)
    }

    /// Discards `source_id`'s audio that ends before `ticks`, a time on that source's own
    /// timeline. A source two clips read at different times is trimmed only below what both
    /// need. Call it as playback moves on, a few seconds behind the earliest time still mixed.
    #[wasm_bindgen(js_name = trimAudioBefore)]
    pub fn trim_audio_before(&mut self, source_id: &str, ticks: i64) {
        self.audio.trim_before(source_id, ticks);
    }

    /// Chooses what `renderAudio` does, outside an export, with a block that does not start
    /// where the previous one ended.
    ///
    /// `"exact"` (default) resets and replays the settling history, mixing what a fresh
    /// renderer would. `"continue-small-gaps"` is for live preview: a jump of at most
    /// `audioMaxBlock` samples continues from the previous state instead of replaying up to
    /// 30 s, so its output is not reproducible. Exports are always exact. Changing the policy
    /// resets the audio, as `resetAudio` does.
    ///
    /// # Errors
    /// Rejects any other policy name (`INVALID_INPUT`, not fatal).
    #[wasm_bindgen(js_name = setAudioContinuity)]
    pub fn set_audio_continuity(&mut self, policy: &str) -> Result<(), JsValue> {
        self.audio.set_continuity(policy, self.exporter.is_some())
    }

    /// Clears every processor history after a seek or a transport stop.
    #[wasm_bindgen(js_name = resetAudio)]
    pub fn reset_audio(&mut self) {
        self.audio.reset();
    }

    /// Source intervals a host must decode before mixing a sample range, including
    /// interpolation neighbours, read-ahead, delay lines, and after a seek the settling history
    /// effect stages replay. Tick values are strings to preserve the full i64 range.
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

    /// Mixes one project audio block of at most `audioMaxBlock` samples per channel and returns
    /// its planar samples, channels end to end. Missing sources are reported by
    /// `audioMissingSources`.
    ///
    /// # Errors
    /// Rejects (`INVALID_INPUT`, not fatal) more than `audioMaxBlock` frames.
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

    /// Starts (from empty) or stops measuring loudness of the mix before the master limiter,
    /// over subsequent `renderAudio` blocks. Starting removes the normalization gain so the mix
    /// is measured as authored; blocks with a missing source are not measured.
    ///
    /// # Errors
    /// Returns an error without an audio renderer.
    #[wasm_bindgen(js_name = setLoudnessMetering)]
    pub fn set_loudness_metering(&mut self, enabled: bool) -> Result<(), JsValue> {
        self.audio.set_loudness_metering(enabled)
    }

    /// The loudness measured since metering started as JSON (`LoudnessReport` in
    /// `alpha2/loudness`), or `null` while off. `limiterThreshold` (or `null`) is the master
    /// limiter it measured in front of, which `planLoudnessNormalization` keeps the mix under.
    #[wasm_bindgen(js_name = loudnessReport)]
    #[must_use]
    pub fn loudness_report(&self) -> String {
        self.audio.loudness_report()
    }

    /// Applies a static gain in dB after the master gain and before the limiter; zero or
    /// `setLoudnessMetering(true)` removes it. A normalized export sets the gain
    /// `planLoudnessNormalization` returns; it persists until set again.
    ///
    /// # Errors
    /// Returns an error without an audio renderer, or for a gain that is not finite or whose
    /// factor overflows a 32-bit float (`INVALID_INPUT`).
    #[wasm_bindgen(js_name = setNormalizationGainDb)]
    pub fn set_normalization_gain_db(&mut self, gain_db: f64) -> Result<(), JsValue> {
        self.audio.set_normalization_gain_db(gain_db)
    }

    /// Source IDs the most recent `renderAudio` call could not mix, as a JSON array.
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

    /// Starts a deterministic export over a half-open timeline range.
    ///
    /// Resets the audio renderer so export audio inherits no preview DSP state, and keeps it
    /// exact until `endExport`. Until then, an audio block missing a source mixes silence and
    /// advances nothing (`audioMissingSources` names them), so re-rendering it after upload
    /// continues exactly.
    ///
    /// Renders offscreen at [`QualityProfile::export`], independent of the canvas. Frames are
    /// packed with the chroma siting of `codec` (`h264`, `hevc`, `vp9`, `av1`;
    /// [`alpha2_media::YuvEncoding::export`]).
    ///
    /// # Errors
    /// Returns an error without a loaded project, for an empty range, an invalid frame rate, or
    /// an unknown codec.
    #[wasm_bindgen(js_name = beginExport)]
    pub fn begin_export(
        &mut self,
        start_ticks: i64,
        end_ticks: i64,
        rate_num: u32,
        rate_den: u32,
        codec: &str,
    ) -> Result<(), JsValue> {
        let codec: VideoCodec =
            codec
                .parse()
                .map_err(|error: <VideoCodec as std::str::FromStr>::Err| {
                    invalid_input(error.to_string())
                })?;
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let range = TimeRange::new(MediaTime(start_ticks), MediaTime(end_ticks))
            .map_err(|error| typed_error(&error))?;
        let frame_rate = if rate_num > 0 && rate_den > 0 {
            FrameRate::new(rate_num, rate_den).map_err(|error| typed_error(&error))?
        } else {
            project.project().settings.frame_rate
        };
        self.exporter = Some(
            Exporter::new(project, range, frame_rate, QualityProfile::export(), codec)
                .map_err(|error| typed_error(&error))?,
        );
        self.frames.unpin();
        self.audio.begin_export();
        Ok(())
    }

    /// Ends the current export and releases its scheduler.
    #[wasm_bindgen(js_name = endExport)]
    pub fn end_export(&mut self) {
        self.exporter = None;
        self.frames.unpin();
        self.audio.end_export();
    }

    /// Completed and total frame counts for the running export and the range it covers, as
    /// JSON.
    ///
    /// `effectiveRange` is the requested range widened to the frame grid; its start is the
    /// output's zero. `audioWindow` is the project-rate samples the export's audio covers:
    /// `startSample` (a decimal string) at the output's zero, and `frames`. Hosts take audio
    /// from these, never from the request.
    ///
    /// # Errors
    /// Returns an error when no export is running or the range does not fit in samples.
    #[wasm_bindgen(js_name = exportProgress)]
    pub fn export_progress(&self) -> Result<String, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
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

    /// Renders the next export frame and resolves with its pixels.
    ///
    /// Resolves to `{ kind: "complete" }`; to `{ kind: "waiting", timeTicks, missingSources,
    /// missingFrames }` when a source is missing (`missingFrames` as `renderAt` reports it); or
    /// to `{ kind: "rendered", index, timeTicks, width, height, format: "I420", colorSpace,
    /// layout, pixels }` with padded planes at `layout`'s offsets and strides. Waiting never
    /// advances the cursor.
    ///
    /// # Errors
    /// Returns an error without a running export or loaded project. The promise rejects with a
    /// `failure` when the readback times out, the device is lost, or the mapping fails.
    #[wasm_bindgen(js_name = exportFrame)]
    pub fn export_frame(&mut self) -> Result<web_sys::js_sys::Promise, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let exporter = self
            .exporter
            .as_mut()
            .ok_or_else(|| invalid_state("no export is running"))?;
        if let Some(time) = exporter.pending_time() {
            refuse_degraded(&self.degraded_sources, project, time)?;
            let size = exporter
                .render_size(project)
                .map_err(|error| typed_error(&error))?;
            self.texts
                .prepare_at(&self.renderer, project, time, size)
                .map_err(|error| typed_error(&error))?;
        }
        self.renderer.mark_frame(Some(alpha2_render::FrameMark {
            time_ticks: exporter.pending_time().map_or(0, MediaTime::ticks),
            frame_index: Some(exporter.progress().completed),
        }));
        let step = exporter
            .render_next(
                project,
                &self.renderer,
                &FramesWithText::new(&self.frames, &self.texts),
            )
            .map_err(|error| typed_error(&error))?;

        let (index, time, readback) = match step {
            ExportStep::Complete => {
                self.frames.unpin();
                return Ok(resolved(&export_status("complete", None, &[])?));
            }
            ExportStep::Waiting {
                time,
                missing_sources,
            } => {
                pin_export_frame(&mut self.frames, project, time)?;
                return Ok(resolved(&export_waiting(project, time, &missing_sources)?));
            }
            ExportStep::Rendered {
                index,
                time,
                readback,
            } => (index, time, readback),
        };
        self.frames.unpin();

        let (readback, _) = export_readback_promise(
            readback,
            self.health.clone(),
            self.renderer.loss_classifier(),
            self.browser_webgpu,
            self.renderer.gpu_wait_timeout(),
            Some(index),
            time,
        );
        Ok(readback)
    }

    /// Starts the next export render and returns before its GPU readback maps.
    ///
    /// A rendered result carries a `readback` promise; calling again while it is pending keeps
    /// two or three readbacks in flight. Waiting results do not advance the cursor.
    #[wasm_bindgen(js_name = beginExportFrame)]
    pub fn begin_export_frame(&mut self) -> Result<JsValue, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let exporter = self
            .exporter
            .as_mut()
            .ok_or_else(|| invalid_state("no export is running"))?;
        if let Some(time) = exporter.pending_time() {
            refuse_degraded(&self.degraded_sources, project, time)?;
            let size = exporter
                .render_size(project)
                .map_err(|error| typed_error(&error))?;
            self.texts
                .prepare_at(&self.renderer, project, time, size)
                .map_err(|error| typed_error(&error))?;
        }
        self.renderer.mark_frame(Some(alpha2_render::FrameMark {
            time_ticks: exporter.pending_time().map_or(0, MediaTime::ticks),
            frame_index: Some(exporter.progress().completed),
        }));
        let step = exporter
            .render_next(
                project,
                &self.renderer,
                &FramesWithText::new(&self.frames, &self.texts),
            )
            .map_err(|error| typed_error(&error))?;
        let (index, time, readback) = match step {
            ExportStep::Complete => {
                self.frames.unpin();
                return export_status("complete", None, &[]);
            }
            ExportStep::Waiting {
                time,
                missing_sources,
            } => {
                pin_export_frame(&mut self.frames, project, time)?;
                return export_waiting(project, time, &missing_sources);
            }
            ExportStep::Rendered {
                index,
                time,
                readback,
            } => (index, time, readback),
        };
        self.frames.unpin();
        let (readback, retained_bytes) = export_readback_promise(
            readback,
            self.health.clone(),
            self.renderer.loss_classifier(),
            self.browser_webgpu,
            self.renderer.gpu_wait_timeout(),
            Some(index),
            time,
        );
        let result = web_sys::js_sys::Object::new();
        set(&result, "kind", &JsValue::from_str("rendered"))?;
        #[allow(clippy::cast_precision_loss)] // A frame index stays below 2^53.
        set(&result, "index", &JsValue::from_f64(index as f64))?;
        set(
            &result,
            "timeTicks",
            &JsValue::from_str(&time.ticks().to_string()),
        )?;
        set(
            &result,
            "retainedBytes",
            &js_safe_integer(retained_bytes, "readback byte count")?,
        )?;
        set(&result, "readback", readback.as_ref())?;
        Ok(result.into())
    }

    /// Renders the next export frame and presents it on this renderer's canvas instead of
    /// reading it back (experimental): the page captures it with `new VideoFrame(canvas)`.
    ///
    /// The canvas must be sized to the export (`resizeSurface`) and serve only this export.
    /// Capture it in the same task, before the next call replaces the drawing buffer. Returns
    /// `{kind: "rendered", index, timeTicks}`, or the `waiting` and `complete` results of
    /// `beginExportFrame`.
    ///
    /// # Errors
    /// Returns an error without a loaded project, a running export, or a canvas, or when
    /// rendering or presentation fails.
    #[wasm_bindgen(js_name = beginExportPresent)]
    pub fn begin_export_present(&mut self) -> Result<JsValue, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let exporter = self
            .exporter
            .as_mut()
            .ok_or_else(|| invalid_state("no export is running"))?;
        if let Some(time) = exporter.pending_time() {
            refuse_degraded(&self.degraded_sources, project, time)?;
            let size = exporter
                .render_size(project)
                .map_err(|error| typed_error(&error))?;
            self.texts
                .prepare_at(&self.renderer, project, time, size)
                .map_err(|error| typed_error(&error))?;
        }
        let step = exporter
            .render_next_texture(
                project,
                &self.renderer,
                &FramesWithText::new(&self.frames, &self.texts),
            )
            .map_err(|error| typed_error(&error))?;
        let (index, time, texture) = match step {
            alpha2_engine::ExportTextureStep::Complete => {
                self.frames.unpin();
                return export_status("complete", None, &[]);
            }
            alpha2_engine::ExportTextureStep::Waiting {
                time,
                missing_sources,
            } => {
                pin_export_frame(&mut self.frames, project, time)?;
                return export_waiting(project, time, &missing_sources);
            }
            alpha2_engine::ExportTextureStep::Rendered {
                index,
                time,
                texture,
            } => (index, time, texture),
        };
        self.frames.unpin();
        present_texture(
            self.surface.as_ref(),
            self.surface_config.as_ref(),
            self.presenter.as_ref(),
            &self.device,
            &self.gpu,
            &self.device_lost,
            texture.view(),
        )?;
        if self.browser_webgpu {
            // The presentation pass reads the texture after this returns, so keep it until the
            // queue is done.
            let completion = crate::completion::Completion::default();
            let signal = completion.clone();
            self.gpu.on_submitted_work_done(move || signal.finish());
            wasm_bindgen_futures::spawn_local(async move {
                completion.await;
                drop(texture);
            });
        }
        let result = web_sys::js_sys::Object::new();
        set(&result, "kind", &JsValue::from_str("rendered"))?;
        #[allow(clippy::cast_precision_loss)] // A frame index stays below 2^53.
        set(&result, "index", &JsValue::from_f64(index as f64))?;
        set(
            &result,
            "timeTicks",
            &JsValue::from_str(&time.ticks().to_string()),
        )?;
        Ok(result.into())
    }

    /// Starts one exact timeline timestamp offscreen at export quality: the stateless
    /// counterpart of [`Self::export_frame`] for a JavaScript-scheduled exporter.
    ///
    /// The readback promise is exposed separately so later frames can be submitted while
    /// earlier mappings are in flight. Frames are packed for `codec` as [`Self::begin_export`]
    /// does, in `pixels` (an `FFmpeg` pixel format: `yuva420p` keeps alpha and draws no
    /// background, `yuv420p10le` is 10-bit; I420 when absent). Non-I420 layouts are packed into
    /// tight planes with the `WebCodecs` `format` (`I420A`, `I420P10`).
    ///
    /// # Errors
    /// Returns an error without a loaded project, for an unknown codec or pixel format, or when
    /// rendering or submission fails.
    #[wasm_bindgen(js_name = beginExportAt)]
    #[allow(clippy::needless_pass_by_value)] // wasm-bindgen hands an optional string over owned.
    pub fn begin_export_at(
        &mut self,
        time_ticks: i64,
        codec: &str,
        pixels: Option<String>,
    ) -> Result<JsValue, JsValue> {
        let pixels = crate::interop::parse_pixels(pixels.as_deref())?;
        let codec: VideoCodec =
            codec
                .parse()
                .map_err(|error: <VideoCodec as std::str::FromStr>::Err| {
                    invalid_input(error.to_string())
                })?;
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let time = MediaTime(time_ticks);
        refuse_degraded(&self.degraded_sources, project, time)?;
        let mut frame = project
            .evaluate(time)
            .map_err(|error| typed_error(&error))?;
        if pixels.alpha {
            frame.background = None;
        }
        let quality = QualityProfile::export();
        // An odd 4:2:0 size is refused before rendering, naming the nearest even size.
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
        let provider = FramesWithText::new(&self.frames, &self.texts);
        self.renderer.mark_frame(Some(alpha2_render::FrameMark {
            time_ticks,
            frame_index: None,
        }));
        let (readback, missing) = if pixels.is_yuv420p() {
            let attempt = self
                .renderer
                .render_frame_yuv420p(size, &frame, &provider, YuvEncoding::export(codec))
                .map_err(|error| typed_error(&error))?;
            (attempt.readback, attempt.report.missing_sources)
        } else {
            let render =
                alpha2_engine::FrameRenderer::render_frame(&self.renderer, size, &frame, &provider)
                    .map_err(|error| typed_error(&error))?;
            let readback = render
                .texture
                .map(|texture| {
                    self.renderer.begin_layout_readback(
                        &texture,
                        YuvEncoding::export(codec),
                        pixels,
                    )
                })
                .transpose()
                .map_err(|error| typed_error(&error))?;
            (readback, render.report.missing_sources)
        };
        let Some(readback_handle) = readback else {
            pin_export_frame(&mut self.frames, project, time)?;
            return export_waiting(project, time, &missing);
        };
        self.frames.unpin();
        let (readback, retained_bytes) = export_readback_promise(
            readback_handle,
            self.health.clone(),
            self.renderer.loss_classifier(),
            self.browser_webgpu,
            self.renderer.gpu_wait_timeout(),
            None,
            time,
        );
        let result = web_sys::js_sys::Object::new();
        set(&result, "kind", &JsValue::from_str("rendered"))?;
        set(
            &result,
            "timeTicks",
            &JsValue::from_str(&time.ticks().to_string()),
        )?;
        set(
            &result,
            "retainedBytes",
            &js_safe_integer(retained_bytes, "readback byte count")?,
        )?;
        set(&result, "readback", readback.as_ref())?;
        Ok(result.into())
    }

    /// Drops the held preview frame after a seek without changing the project.
    #[wasm_bindgen(js_name = resetPlayer)]
    pub fn reset_player(&mut self) {
        self.player.reset();
    }

    /// Renders preview frames at `scale` times the project size from now on (default 0.5).
    /// Drops the held frame so the next `renderAt` presents; resize the surface to
    /// `previewSize` afterwards.
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

    /// The loaded project's render size at the preview scale, `[width, height]`: what
    /// `renderAt` renders and the surface should be.
    ///
    /// # Errors
    /// Fails when no project is loaded.
    #[wasm_bindgen(getter, js_name = previewSize)]
    pub fn preview_size(&self) -> Result<Vec<u32>, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let size = self
            .player
            .render_size(project)
            .map_err(|error| typed_error(&error))?;
        Ok(vec![size.width, size.height])
    }

    /// Exact clip/source/timestamp requests from the same snapshot `renderAt` uses. Hosts keep
    /// separate decoder cursors per clip ID. Tick values are strings to preserve i64.
    #[wasm_bindgen(js_name = frameNeeds)]
    pub fn frame_needs(&self, time_ticks: i64) -> Result<String, JsValue> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid_state("no project is loaded"))?;
        let needs = project
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

    /// Resolves when submitted WebGPU preview work completes, so the host can pace frames. The
    /// returned promise releases the wasm borrow at once, allowing edits while waiting.
    #[wasm_bindgen(js_name = submittedWorkDone)]
    pub fn submitted_work_done(&self) -> web_sys::js_sys::Promise {
        let completion = crate::completion::Completion::default();
        if self.browser_webgpu {
            let signal = completion.clone();
            self.gpu.on_submitted_work_done(move || signal.finish());
        } else {
            completion.finish();
        }
        let lost = Arc::clone(&self.device_lost);
        wasm_bindgen_futures::future_to_promise(async move {
            completion.await;
            if lost.load(Ordering::Acquire) {
                Err(coded_error(
                    Code::GPU_DEVICE_LOST,
                    "GPU device was lost; make the renderer again",
                ))
            } else {
                Ok(JsValue::UNDEFINED)
            }
        })
    }

    /// Appends an OpenType font to the fallback stack for captions, text generators and
    /// layouts. With the same fonts as the export host, preview text matches exported text
    /// pixel for pixel.
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

    /// Appends every face of an OpenType font file (each weight of a `.ttc`) to the fallback
    /// stack in file order, all checked before any is added.
    ///
    /// # Errors
    /// Returns an error for malformed font bytes; no face is added.
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

    /// Evaluates and presents one project frame on the worker-owned canvas.
    ///
    /// Returns JSON `{ requestedTimeTicks, presentedTimeTicks, missingSources, missingFrames,
    /// repeated, unchanged }`. `presentedTimeTicks` is `null` until a frame is presented;
    /// `missingFrames` gives the exact source time each missing source was needed at;
    /// `unchanged` means the frame equals the last one, so nothing was rendered or presented.
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
        self.renderer.mark_frame(Some(alpha2_render::FrameMark {
            time_ticks,
            frame_index: None,
        }));
        let playback = self
            .player
            .render_at(
                project,
                &self.renderer,
                &FramesWithText::new(&self.frames, &self.texts),
                MediaTime(time_ticks),
            )
            .map_err(|error| typed_error(&error))?;
        let missing_frames =
            missing_frames(project, playback.requested_time, &playback.missing_sources)?;
        let degraded_sources =
            degraded_at(&self.degraded_sources, project, playback.requested_time)?;
        let result = WebPlaybackResult {
            requested_time_ticks: playback.requested_time.ticks(),
            presented_time_ticks: playback.presented_time.map(MediaTime::ticks),
            missing_sources: playback
                .missing_sources
                .iter()
                .map(ToString::to_string)
                .collect(),
            missing_frames,
            degraded_sources,
            repeated: playback.repeated,
            unchanged: playback.unchanged,
        };
        if let Some(texture) = playback.texture.filter(|_| !playback.unchanged) {
            present_texture(
                self.surface.as_ref(),
                self.surface_config.as_ref(),
                self.presenter.as_ref(),
                &self.device,
                &self.gpu,
                &self.device_lost,
                texture.view(),
            )?;
            if self.browser_webgpu {
                let retained = texture.clone();
                let completion = crate::completion::Completion::default();
                let signal = completion.clone();
                self.gpu.on_submitted_work_done(move || signal.finish());
                wasm_bindgen_futures::spawn_local(async move {
                    completion.await;
                    drop(retained);
                });
            }
        }
        serde_json::to_string(&result).map_err(|error| internal_error(error.to_string()))
    }
}

/// Checks that `cube` is a `.cube` 3D LUT the engine accepts, as `registerLut3d` parses it,
/// without a renderer, so a host can refuse a file at import time.
///
/// # Errors
/// Rejects text the parser refuses, with its line and reason.
#[wasm_bindgen(js_name = checkLut3d)]
pub fn check_lut_3d(cube: &str) -> Result<(), JsValue> {
    Lut3dData::from_cube(cube)
        .map(|_| ())
        .map_err(|error| typed_error(&error))
}

/// The sources a frame at `time` shows that `degraded` names, in ID order.
fn degraded_at(
    degraded: &std::collections::BTreeSet<String>,
    project: &EngineProject,
    time: MediaTime,
) -> Result<Vec<String>, JsValue> {
    if degraded.is_empty() {
        return Ok(Vec::new());
    }
    let mut sources: Vec<String> = project
        .frame_needs(time)
        .map_err(|error| typed_error(&error))?
        .into_iter()
        .map(|need| need.source_id.as_str().to_owned())
        .filter(|source| degraded.contains(source))
        .collect();
    sources.sort_unstable();
    sources.dedup();
    Ok(sources)
}

/// Refuses an export frame at `time` that shows a degraded source, so a browser export never
/// contains the browser's conversion of a source.
fn refuse_degraded(
    degraded: &std::collections::BTreeSet<String>,
    project: &EngineProject,
    time: MediaTime,
) -> Result<(), JsValue> {
    let sources = degraded_at(degraded, project, time)?;
    if sources.is_empty() {
        return Ok(());
    }
    Err(coded_error(
        Code::PlanarDecodeUnavailable,
        format!(
            "this browser gives {} only as frames the engine cannot convert, shown in previews              through the browser's own conversion: a browser export cannot render them; export              with the native host",
            sources.join(", ")
        ),
    ))
}

/// The exact frame each of `missing` was needed at, from the snapshot that rendered `time`.
pub(crate) fn missing_frames(
    project: &EngineProject,
    time: MediaTime,
    missing: &[SourceId],
) -> Result<Vec<WebMissingFrame>, JsValue> {
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    Ok(project
        .frame_needs(time)
        .map_err(|error| typed_error(&error))?
        .into_iter()
        .filter(|need| missing.contains(&need.source_id))
        .map(|need| WebMissingFrame {
            clip_id: need.clip_id.as_str().to_owned(),
            reader_id: need.reader.as_ref().map(|node| node.as_str().to_owned()),
            source_id: need.source_id.as_str().to_owned(),
            media_time_ticks: need.media_time.ticks().to_string(),
        })
        .collect())
}

/// `{ kind: "waiting", timeTicks, missingSources, missingFrames }` for an export frame at
/// `time` that lacks `missing`.
pub(crate) fn export_waiting(
    project: &EngineProject,
    time: MediaTime,
    missing: &[SourceId],
) -> Result<JsValue, JsValue> {
    let sources: Vec<String> = missing.iter().map(ToString::to_string).collect();
    let status = export_status("waiting", Some(time.ticks()), &sources)?;
    let frames = serde_json::to_string(&missing_frames(project, time, missing)?)
        .map_err(|error| internal_error(error.to_string()))?;
    web_sys::js_sys::Reflect::set(
        &status,
        &JsValue::from_str("missingFrames"),
        &web_sys::js_sys::JSON::parse(&frames)?,
    )?;
    Ok(status)
}

/// The engine's YUV layout for a `WebCodecs` planar pixel format.
pub(crate) fn webcodecs_yuv_format(format: &str) -> Result<alpha2_media::YuvFormat, JsValue> {
    Ok(match format {
        "I420" => alpha2_media::YuvFormat::I420,
        "NV12" => alpha2_media::YuvFormat::NV12,
        "I420P10" => alpha2_media::YuvFormat::planar(1, 1, 10),
        "I420P12" => alpha2_media::YuvFormat::planar(1, 1, 12),
        "I422" => alpha2_media::YuvFormat::planar(1, 0, 8),
        "I422P10" => alpha2_media::YuvFormat::planar(1, 0, 10),
        "I444" => alpha2_media::YuvFormat::planar(0, 0, 8),
        "I444P10" => alpha2_media::YuvFormat::planar(0, 0, 10),
        "I444P12" => alpha2_media::YuvFormat::planar(0, 0, 12),
        "I422P12" => alpha2_media::YuvFormat::planar(1, 0, 12),
        "I420A" => alpha2_media::YuvFormat::I420.with_alpha(),
        "I420AP10" => alpha2_media::YuvFormat::planar(1, 1, 10).with_alpha(),
        "I420AP12" => alpha2_media::YuvFormat::planar(1, 1, 12).with_alpha(),
        "I422A" => alpha2_media::YuvFormat::planar(1, 0, 8).with_alpha(),
        "I422AP10" => alpha2_media::YuvFormat::planar(1, 0, 10).with_alpha(),
        "I422AP12" => alpha2_media::YuvFormat::planar(1, 0, 12).with_alpha(),
        "I444A" => alpha2_media::YuvFormat::planar(0, 0, 8).with_alpha(),
        "I444AP10" => alpha2_media::YuvFormat::planar(0, 0, 10).with_alpha(),
        "I444AP12" => alpha2_media::YuvFormat::planar(0, 0, 12).with_alpha(),
        other => {
            return Err(invalid_input(format!(
                "unsupported YUV frame format {other}"
            )));
        }
    })
}

/// A chroma siting as the SDK names it.
pub(crate) fn chroma_siting_named(name: &str) -> Result<ChromaLocation, JsValue> {
    Ok(match name {
        "left" => ChromaLocation::Left,
        "center" => ChromaLocation::Center,
        "top-left" => ChromaLocation::TopLeft,
        "unspecified" => ChromaLocation::Unspecified,
        other => return Err(invalid_input(format!("unknown chroma siting {other}"))),
    })
}

pub(crate) fn rotation_from_degrees(degrees: u32) -> Result<Rotation, JsValue> {
    Ok(match degrees {
        0 => Rotation::Deg0,
        90 => Rotation::Deg90,
        180 => Rotation::Deg180,
        270 => Rotation::Deg270,
        _ => return Err(invalid_input("frame rotation must be 0, 90, 180, or 270")),
    })
}

/// The output size and orientation a presented upload is shown at.
pub(crate) fn presented_placement(
    output_width: u32,
    output_height: u32,
    rotation_degrees: u32,
    mirror: bool,
) -> Result<(RenderSize, Orientation), JsValue> {
    let output_size =
        RenderSize::new(output_width, output_height).map_err(|error| typed_error(&error))?;
    let rotation = rotation_from_degrees(rotation_degrees)?;
    Ok((output_size, Orientation { rotation, mirror }))
}

enum PresentedImage {
    Frame(web_sys::VideoFrame),
    Bitmap(web_sys::ImageBitmap),
}

impl PresentedImage {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Frame(frame) => (frame.display_width(), frame.display_height()),
            Self::Bitmap(bitmap) => (bitmap.width(), bitmap.height()),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Frame(_) => "VideoFrame",
            Self::Bitmap(_) => "ImageBitmap",
        }
    }

    fn close(&self) {
        match self {
            Self::Frame(frame) => frame.close(),
            Self::Bitmap(bitmap) => bitmap.close(),
        }
    }
}

/// Pixels a page hands over for GPU presentation.
#[derive(Copy, Clone)]
enum PresentedPixels<'a> {
    /// sRGB-encoded RGBA8, straight or premultiplied.
    Rgba8 {
        pixels: &'a [u8],
        premultiplied: bool,
    },
    /// Premultiplied linear-light RGBA half floats in BT.709 primaries.
    Linear(&'a [u16]),
}

pub(crate) fn result_cache_json(cache: alpha2_render::ResultCacheStats) -> serde_json::Value {
    serde_json::json!({
        "budgetBytes": cache.budget_bytes,
        "bytes": cache.bytes,
        "entries": cache.entries,
        "pinned": cache.pinned,
        "hits": cache.hits,
        "misses": cache.misses,
        "ineligible": cache.ineligible,
        "inserts": cache.inserts,
        "evictions": cache.evictions,
        "skippedNodes": cache.skipped_nodes,
        "invalidations": {
            "effects": cache.invalidations[0],
            "effectResources": cache.invalidations[1],
            "quality": cache.invalidations[2],
            "explicit": cache.invalidations[3],
            "gpuError": cache.invalidations[4],
        },
    })
}
