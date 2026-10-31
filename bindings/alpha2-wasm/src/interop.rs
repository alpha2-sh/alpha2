//! JavaScript value helpers for the web binding.

use alpha2_gpu::{GpuContext, GpuDiagnostic, GpuDiagnosticLog};
use alpha2_render::{RenderError, RenderSize, Renderer, ShaderDiagnostics};
use alpha2_time::MediaTime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;

use crate::error::Code;
use crate::web::{coded_error, internal_error, invalid_input, typed_error};

// A readback yields to the event loop with a message, not a timer: chained messages are not clamped
// to 4 ms or throttled on a hidden page. `performance.now()` ignores wall-clock changes.
#[wasm_bindgen(inline_js = r#"
let channel;
const waiting = [];
export function alpha2YieldTask() {
  if (channel === undefined) {
    channel = new MessageChannel();
    channel.port1.onmessage = () => waiting.shift()?.();
  }
  return new Promise((resolve) => {
    waiting.push(resolve);
    channel.port2.postMessage(undefined);
  });
}
export function alpha2Now() { return performance.now(); }
export function alpha2Sleep(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }
"#)]
extern "C" {
    #[wasm_bindgen(js_name = alpha2YieldTask)]
    fn yield_task() -> web_sys::js_sys::Promise;

    #[wasm_bindgen(js_name = alpha2Sleep)]
    fn sleep_ms(milliseconds: f64) -> web_sys::js_sys::Promise;

    #[wasm_bindgen(js_name = alpha2Now)]
    fn now_ms() -> f64;
}

/// A WebGL readback's wait: the browser's clock and event loop, the
/// readback's mapping, and its device.
struct WebGlWait<'a> {
    readback: &'a alpha2_render::Yuv420pReadback,
    completion: &'a crate::completion::Completion,
    health: &'a DeviceHealth,
}

impl crate::readback_wait::MappingWait for WebGlWait<'_> {
    type Yield = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JsValue>>>>;
    type Error = JsValue;

    fn now_ms(&self) -> f64 {
        now_ms()
    }

    fn poll(&mut self) {
        self.readback.poll();
    }

    fn done(&self) -> bool {
        self.completion.is_done()
    }

    fn lost(&self) -> bool {
        self.health.lost.load(Ordering::Acquire)
    }

    fn yield_task(&mut self) -> Self::Yield {
        let task = wasm_bindgen_futures::JsFuture::from(yield_task());
        Box::pin(async move { task.await.map(drop) })
    }
}

/// A browser WebGPU readback's wait: the browser maps the buffer on its own
/// event loop, so a poll does nothing, and a turn ends when the mapping
/// finishes or after [`WEBGPU_TURN_MS`], whichever is first.
struct WebGpuWait<'a> {
    completion: &'a crate::completion::Completion,
    health: &'a DeviceHealth,
}

/// How long a WebGPU readback's wait sleeps between looks at its deadline:
/// long enough to cost nothing, short enough that the deadline is met within
/// a quarter second.
const WEBGPU_TURN_MS: f64 = 250.0;

impl crate::readback_wait::MappingWait for WebGpuWait<'_> {
    type Yield = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JsValue>>>>;
    type Error = JsValue;

    fn now_ms(&self) -> f64 {
        now_ms()
    }

    fn poll(&mut self) {}

    fn done(&self) -> bool {
        self.completion.is_done()
    }

    fn lost(&self) -> bool {
        self.health.lost.load(Ordering::Acquire)
    }

    fn yield_task(&mut self) -> Self::Yield {
        Box::pin(FirstOf {
            completion: self.completion.clone(),
            timer: wasm_bindgen_futures::JsFuture::from(sleep_ms(WEBGPU_TURN_MS)),
        })
    }
}

/// Ready when the mapping finishes or the timer fires, whichever is first.
struct FirstOf {
    completion: crate::completion::Completion,
    timer: wasm_bindgen_futures::JsFuture,
}

impl std::future::Future for FirstOf {
    type Output = Result<(), JsValue>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if std::pin::Pin::new(&mut self.completion)
            .poll(context)
            .is_ready()
        {
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.timer)
            .poll(context)
            .map(|fired| fired.map(drop))
    }
}

// WebGL2 has no `copyExternalImageToTexture`, and its `texSubImage2D(VideoFrame)` converts a
// CPU-backed frame on the CPU (12 ms at 1080p on Chrome). A 2D canvas draws it on the GPU and hands
// over a bitmap without a copy. One canvas per size (at most four sizes) is kept for reuse.
#[wasm_bindgen(inline_js = r#"
const canvases = new Map();
export function alpha2FrameBitmap(frame, width, height) {
  const key = `${width}x${height}`;
  let entry = canvases.get(key);
  if (entry === undefined) {
    const canvas = new OffscreenCanvas(width, height);
    const context = canvas.getContext("2d");
    if (context === null) throw new Error("the VideoFrame import could not create a 2D canvas");
    context.globalCompositeOperation = "copy";
    entry = { canvas, context };
    if (canvases.size >= 4) canvases.delete(canvases.keys().next().value);
  } else {
    canvases.delete(key);
  }
  canvases.set(key, entry);
  entry.context.drawImage(frame, 0, 0, width, height);
  return entry.canvas.transferToImageBitmap();
}
export function alpha2ImagePixels(image, width, height) {
  const canvas = new OffscreenCanvas(width, height);
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (context === null) throw new Error("the VideoFrame import could not create a 2D canvas");
  context.drawImage(image, 0, 0, width, height);
  return new Uint8Array(context.getImageData(0, 0, width, height).data.buffer);
}
let uploadProbe;
export function alpha2BitmapUploadProbe() {
  if (uploadProbe !== undefined) return uploadProbe;
  // Without a scratch context nothing can show that bitmaps upload: frames go as pixels.
  const gl = new OffscreenCanvas(1, 1).getContext("webgl2");
  if (gl === null) {
    uploadProbe = { works: false, premultiplied: false };
    return uploadProbe;
  }
  // A `size`-square canvas bitmap of opaque red (filled, or drawn from a VideoFrame as frames are)
  // uploaded into immutable storage as wgpu uploads one: whether WebGL took it without an error and
  // its centre reads back red. With `color`, a 1x1 fill of that colour, and its pixel.
  const upload = (size, video, color) => {
    const source = new OffscreenCanvas(size, size);
    const context = source.getContext("2d");
    if (video) {
      const red = new Uint8Array(size * size * 4);
      for (let at = 0; at < red.length; at += 4) red.set([255, 0, 0, 255], at);
      const frame = new VideoFrame(red, { format: "RGBA", codedWidth: size, codedHeight: size, timestamp: 0 });
      context.drawImage(frame, 0, 0);
      frame.close();
    } else {
      context.fillStyle = color ?? "rgb(255, 0, 0)";
      context.fillRect(0, 0, size, size);
    }
    const bitmap = source.transferToImageBitmap();
    try {
      gl.getError();
      const texture = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, texture);
      gl.texStorage2D(gl.TEXTURE_2D, 1, gl.RGBA8, size, size);
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, gl.RGBA, gl.UNSIGNED_BYTE, bitmap);
      const taken = gl.getError() === gl.NO_ERROR;
      gl.bindFramebuffer(gl.FRAMEBUFFER, gl.createFramebuffer());
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, texture, 0);
      const pixel = new Uint8Array(4);
      gl.readPixels(size >> 1, size >> 1, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
      gl.deleteTexture(texture);
      const red = pixel[0] >= 250 && pixel[1] <= 5 && pixel[2] <= 5 && pixel[3] >= 250;
      return { works: taken && gl.getError() === gl.NO_ERROR && red, pixel };
    } finally {
      bitmap.close();
    }
  };
  try {
    // Some drivers refuse some sizes (GL_INVALID_VALUE) or take the upload without an error and
    // leave the texture black, depending on the size and on how the canvas was drawn.
    const works = [1, 16, 64, 256].every((size) => upload(size, false).works && upload(size, true).works);
    // Half-transparent red reads back as 128 premultiplied, 255 straight.
    const premultiplied = upload(1, false, "rgba(255, 0, 0, 0.5)").pixel[0] < 192;
    uploadProbe = { works, premultiplied };
    return uploadProbe;
  } finally {
    gl.getExtension("WEBGL_lose_context")?.loseContext();
  }
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = alpha2FrameBitmap, catch)]
    fn frame_bitmap(
        frame: &web_sys::VideoFrame,
        width: u32,
        height: u32,
    ) -> Result<web_sys::ImageBitmap, JsValue>;

    #[wasm_bindgen(js_name = alpha2ImagePixels, catch)]
    fn image_pixels(image: &JsValue, width: u32, height: u32) -> Result<Vec<u8>, JsValue>;

    #[wasm_bindgen(js_name = alpha2BitmapUploadProbe, catch)]
    fn bitmap_upload_probe() -> Result<JsValue, JsValue>;
}

/// How the WebGL2 backend uploads a `VideoFrame`: as a canvas bitmap, which
/// WebGL takes premultiplied or straight as the browser chooses, or, where a
/// bitmap upload fails or leaves the texture black (ANGLE on some Vulkan
/// drivers), as the 2D canvas's pixels, read back on the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebGl2Uploads {
    Bitmap { premultiplied: bool },
    CpuPixels,
}

impl WebGl2Uploads {
    /// The name diagnostics and the capability report give it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Bitmap { .. } => "bitmap",
            Self::CpuPixels => "cpu-pixels",
        }
    }
}

thread_local! {
    /// What [`webgl2_uploads`] measured in this realm, once.
    static WEBGL2_UPLOADS: std::cell::Cell<Option<WebGl2Uploads>> =
        const { std::cell::Cell::new(None) };
}

/// How this browser's WebGL2 takes a frame, measured once on a scratch
/// `OffscreenCanvas` context: opaque red bitmaps of four sizes, filled and
/// drawn from a `VideoFrame`, must upload without a GL error and read back
/// red, and a half-transparent one tells premultiplied from straight. Without
/// a scratch context, frames go as CPU pixels.
pub(crate) fn webgl2_uploads() -> Result<WebGl2Uploads, JsValue> {
    if let Some(measured) = WEBGL2_UPLOADS.with(std::cell::Cell::get) {
        return Ok(measured);
    }
    let probe = bitmap_upload_probe()?;
    let flag = |name: &str| {
        web_sys::js_sys::Reflect::get(&probe, &JsValue::from_str(name))
            .map(|value| value.as_bool() == Some(true))
    };
    let measured = if flag("works")? {
        WebGl2Uploads::Bitmap {
            premultiplied: flag("premultiplied")?,
        }
    } else {
        WebGl2Uploads::CpuPixels
    };
    WEBGL2_UPLOADS.with(|cell| cell.set(Some(measured)));
    Ok(measured)
}

/// The upload mode [`webgl2_uploads`] measured, if a frame or the capability
/// report has asked yet.
pub(crate) fn measured_webgl2_uploads() -> Option<WebGl2Uploads> {
    WEBGL2_UPLOADS.with(std::cell::Cell::get)
}

/// Uploads a `VideoFrame` into `texture` on the WebGL2 backend, returning
/// whether the texture now holds premultiplied or straight color: a canvas
/// bitmap where this browser's WebGL2 uploads one, else the canvas's straight
/// pixels through `write_texture` ([`webgl2_uploads`]).
pub(crate) fn upload_frame_through_canvas(
    gpu: &alpha2_gpu::GpuContext,
    frame: &web_sys::VideoFrame,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<alpha2_render::SourceAlpha, JsValue> {
    match webgl2_uploads()? {
        WebGl2Uploads::Bitmap { premultiplied } => {
            let bitmap = frame_bitmap(frame, width, height)?;
            upload_bitmap_through_gl(gpu, &bitmap, texture, width, height);
            bitmap.close();
            Ok(if premultiplied {
                alpha2_render::SourceAlpha::Premultiplied
            } else {
                alpha2_render::SourceAlpha::Straight
            })
        }
        WebGl2Uploads::CpuPixels => {
            upload_pixels_through_canvas(gpu, frame, texture, width, height)?;
            Ok(alpha2_render::SourceAlpha::Straight)
        }
    }
}

/// Draws `image` on a 2D canvas and uploads its straight RGBA pixels into
/// `texture` with `write_texture`: the WebGL2 upload where canvas bitmaps fail.
fn upload_pixels_through_canvas(
    gpu: &alpha2_gpu::GpuContext,
    image: &JsValue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<(), JsValue> {
    let pixels = image_pixels(image, width, height)?;
    gpu.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
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
    Ok(())
}

/// Uploads `bitmap` into `texture` on the WebGL2 backend.
/// The caller closes the bitmap.
fn upload_bitmap_through_gl(
    gpu: &alpha2_gpu::GpuContext,
    bitmap: &web_sys::ImageBitmap,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) {
    gpu.queue().copy_external_image_to_texture(
        &wgpu::CopyExternalImageSourceInfo {
            source: wgpu::ExternalImageSource::ImageBitmap(bitmap.clone()),
            origin: wgpu::Origin2d::ZERO,
            flip_y: false,
        },
        wgpu::CopyExternalImageDestInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
            color_space: wgpu::PredefinedColorSpace::Srgb,
            premultiplied_alpha: false,
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    // wgpu's GL backend records the copy and runs it at the next submission; the bitmap has to
    // stay open until then, and no longer.
    gpu.submit([]);
}

// wgpu's WebGPU backend unwraps the browser's `copyExternalImageToTexture` result, so a source the
// browser refuses panics the module. Calling the queue here keeps the exception a `Result`.
#[wasm_bindgen(inline_js = r#"
export function alpha2CopyExternalImage(queue, source, texture, width, height) {
  queue.copyExternalImageToTexture(
    { source, origin: [0, 0], flipY: false },
    { texture, mipLevel: 0, origin: [0, 0, 0], aspect: "all", colorSpace: "srgb", premultipliedAlpha: false },
    [width, height, 1],
  );
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = alpha2CopyExternalImage, catch)]
    fn copy_external_image(
        queue: &JsValue,
        source: &JsValue,
        texture: &JsValue,
        width: u32,
        height: u32,
    ) -> Result<(), JsValue>;
}

/// How browser WebGPU takes a `VideoFrame`. The WebGPU spec lets
/// `copyExternalImageToTexture` read one directly, but not every browser
/// does: Firefox rejects it with a `TypeError` (Mozilla bug 1922100) and
/// takes an `ImageBitmap` instead. The first frame finds out which.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum VideoFrameCopy {
    /// No frame has been copied yet.
    #[default]
    Untried,
    /// The browser took a frame directly; later failures are the frame's own.
    Direct,
    /// The browser refused the type; every frame goes through a bitmap.
    Bitmap,
}

/// Uploads a `VideoFrame` into `texture` on browser WebGPU, directly when
/// the browser takes the frame and through a 2D canvas's bitmap when it
/// refuses the type. The texture holds straight color either way.
pub(crate) fn upload_frame_webgpu(
    queue: &wgpu::Queue,
    frame: &web_sys::VideoFrame,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    copy: &mut VideoFrameCopy,
) -> Result<alpha2_render::SourceAlpha, JsValue> {
    let (Some(queue), Some(texture)) = (queue.as_webgpu(), texture.as_webgpu()) else {
        return Err(internal_error(
            "the WebGPU upload has no browser queue or texture",
        ));
    };
    if *copy != VideoFrameCopy::Bitmap {
        match copy_external_image(queue, frame, texture, width, height) {
            Ok(()) => {
                *copy = VideoFrameCopy::Direct;
                return Ok(alpha2_render::SourceAlpha::Straight);
            }
            // Only a browser that never took a frame is refusing the type; after one direct copy
            // a `TypeError` is this frame's failure.
            Err(error)
                if *copy == VideoFrameCopy::Untried
                    && error.is_instance_of::<web_sys::js_sys::TypeError>() =>
            {
                *copy = VideoFrameCopy::Bitmap;
            }
            Err(error) => return Err(browser_rejected("VideoFrame", &error)),
        }
    }
    let bitmap =
        frame_bitmap(frame, width, height).map_err(|error| browser_rejected("canvas", &error))?;
    // The copy reads the bitmap when it is issued, so it can close right after.
    let copied = copy_external_image(queue, &bitmap, texture, width, height);
    bitmap.close();
    copied.map_err(|error| browser_rejected("ImageBitmap", &error))?;
    Ok(alpha2_render::SourceAlpha::Straight)
}

/// Uploads a bitmap made with `premultiplyAlpha: "none"` into `texture`, as
/// straight color. The caller closes the bitmap.
pub(crate) fn upload_bitmap(
    gpu: &alpha2_gpu::GpuContext,
    browser_webgpu: bool,
    bitmap: &web_sys::ImageBitmap,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<alpha2_render::SourceAlpha, JsValue> {
    if !browser_webgpu {
        match webgl2_uploads()? {
            WebGl2Uploads::Bitmap { .. } => {
                upload_bitmap_through_gl(gpu, bitmap, texture, width, height);
            }
            WebGl2Uploads::CpuPixels => {
                upload_pixels_through_canvas(gpu, bitmap, texture, width, height)?;
            }
        }
        return Ok(alpha2_render::SourceAlpha::Straight);
    }
    let (Some(queue), Some(texture)) = (gpu.queue().as_webgpu(), texture.as_webgpu()) else {
        return Err(internal_error(
            "the WebGPU upload has no browser queue or texture",
        ));
    };
    copy_external_image(queue, bitmap, texture, width, height)
        .map_err(|error| browser_rejected("ImageBitmap", &error))?;
    Ok(alpha2_render::SourceAlpha::Straight)
}

fn browser_rejected(source: &str, error: &JsValue) -> JsValue {
    let message = web_sys::js_sys::Reflect::get(error, &JsValue::from_str("message"))
        .ok()
        .and_then(|message| message.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| format!("{error:?}"));
    invalid_input(format!(
        "the browser rejected the {source} upload: {message}"
    ))
}

/// Checks a texture size against the device's `max_texture_dimension_2d`,
/// which callers take from [`alpha2_render::Renderer::limits`]: reading the
/// device's limits crosses into JavaScript once per field.
pub(crate) fn validate_texture_extent(
    maximum: u32,
    width: u32,
    height: u32,
) -> Result<(), JsValue> {
    RenderSize::new(width, height).map_err(|error| typed_error(&error))?;
    if width > maximum || height > maximum {
        return Err(invalid_input(
            "texture dimensions exceed this GPU device limit",
        ));
    }
    Ok(())
}

/// `value` as a JavaScript number; an internal error past 2^53 - 1, where a
/// number would lose precision. `label` names the value in that error.
pub(crate) fn js_safe_integer(value: u64, label: &str) -> Result<JsValue, JsValue> {
    const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
    if value > MAX_SAFE_INTEGER {
        return Err(internal_error(format!(
            "{label} exceeds JavaScript's safe integer range"
        )));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(JsValue::from_f64(value as f64))
}

/// The `{ generation: bigint, retainedBytes: number }` object a frame upload
/// answers with.
pub(crate) fn frame_upload_receipt(
    generation: u64,
    retained_bytes: u64,
) -> Result<JsValue, JsValue> {
    let receipt = web_sys::js_sys::Object::new();
    let generation = web_sys::js_sys::BigInt::from(generation);
    set(&receipt, "generation", generation.as_ref())?;
    set(
        &receipt,
        "retainedBytes",
        &js_safe_integer(retained_bytes, "uploaded texture byte count")?,
    )?;
    Ok(receipt.into())
}

/// Bytes of one texture of `size` in `format`, without row padding; a format
/// with no single block size counts 4 bytes per texel.
pub(crate) fn texture_byte_len(size: RenderSize, format: wgpu::TextureFormat) -> u64 {
    let block = format.block_copy_size(None).unwrap_or(4);
    u64::from(size.width)
        .saturating_mul(u64::from(size.height))
        .saturating_mul(u64::from(block))
}

/// Starts mapping an export frame's I420 readback and returns a promise for
/// the frame, with the staging bytes the readback holds until it settles.
///
/// The promise resolves to `{ kind: "rendered", index?, timeTicks, width,
/// height, format: "I420", colorSpace, layout, pixels }`, where `pixels` holds
/// the padded planes at the offsets and strides `layout` gives. It rejects
/// with an error carrying `failure` (see [`readback_failure`]) on a timeout,
/// a lost device, or a mapping that failed; `loss` says whether a loss or a
/// timeout was the renderer's own long submission (`DEVICE_HUNG`).
#[allow(clippy::too_many_lines)]
pub(crate) fn export_readback_promise(
    readback: alpha2_render::Yuv420pReadback,
    health: DeviceHealth,
    loss: alpha2_render::LossClassifier,
    browser_webgpu: bool,
    timeout: std::time::Duration,
    index: Option<u64>,
    time: MediaTime,
) -> (web_sys::js_sys::Promise, u64) {
    // The readback owns its staging buffer and can finish on the event loop without retaining the
    // renderer or rendered graph target.
    let retained_bytes = readback.staging_bytes();
    let size = readback.size();
    let completion = crate::completion::Completion::default();
    let mapped = Arc::new(Mutex::new(None::<Result<(), RenderError>>));
    let signal = completion.clone();
    let outcome = Arc::clone(&mapped);
    readback.map_async(move |result| {
        *outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        signal.finish();
    });
    let promise = wasm_bindgen_futures::future_to_promise(async move {
        let failure = |code: &str, stage: &str, message: &str| {
            readback_failure(&health, code, stage, message, index, time)
        };
        #[allow(clippy::cast_precision_loss)] // Milliseconds stay far below 2^53.
        let timeout_ms = timeout.as_millis() as f64;
        // Browser WebGPU drives map callbacks from its event loop, and the callback finishes the
        // completion; WebGL needs explicit polls, a task apart. Either way the wait has a deadline
        // of active time, and a lost device, which never completes a mapping, stops it too.
        let waited = if browser_webgpu {
            let mut wait = WebGpuWait {
                completion: &completion,
                health: &health,
            };
            crate::readback_wait::wait_for_mapping(&mut wait, timeout_ms).await?
        } else {
            let mut wait = WebGlWait {
                readback: &readback,
                completion: &completion,
                health: &health,
            };
            crate::readback_wait::wait_for_mapping(&mut wait, timeout_ms).await?
        };
        if waited == crate::readback_wait::Waited::TimedOut {
            return Err(loss_failure(
                &loss,
                RenderError::GpuTimeout,
                &failure,
                ("GPU_TIMEOUT", "timeout"),
                alpha2_render::GPU_TIMEOUT_MESSAGE,
            ));
        }
        if health.lost.load(Ordering::Acquire) {
            return Err(loss_failure(
                &loss,
                RenderError::GpuDeviceLost,
                &failure,
                ("DEVICE_LOST", "device-lost"),
                "GPU device was lost during export",
            ));
        }
        let result = mapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Err((stage, message)) = mapping_outcome(result) {
            return Err(failure("READBACK_FAILED", stage, &message));
        }
        let format = readback.pixel_layout().map_or("I420", webcodecs_format);
        let (layout, pixels) = if let Some(pixels) = readback.pixel_layout() {
            // Another layout is packed on the CPU, into tight planes.
            let (mut memory, layout, offsets) = tight_planes(pixels, size)?;
            readback
                .finish_into_planes(&mut plane_targets(&mut memory, &offsets))
                .map_err(|error| finish_failure(&health, error, &failure))?;
            (layout, web_sys::js_sys::Uint8Array::from(&memory[..]))
        } else {
            let layout = plane_layout_js(&readback)?;
            // The planes stay as the GPU copied them, rows padded, and the caller reads them
            // through `layout`: one copy out of the mapping instead of a repack.
            let pixels = readback
                .finish_mapped(|mapped| {
                    if browser_webgpu {
                        // The mapping is a JS `ArrayBuffer`; reading it from Rust (even its `len`)
                        // would first copy it into the wasm heap.
                        let mapping = mapped.as_uint8array();
                        mapping.slice(0, mapping.length())
                    } else {
                        web_sys::js_sys::Uint8Array::from(&mapped[..])
                    }
                })
                .map_err(|error| finish_failure(&health, error, &failure))?;
            (layout, pixels)
        };
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
        set(&result, "width", &JsValue::from_f64(f64::from(size.width)))?;
        set(
            &result,
            "height",
            &JsValue::from_f64(f64::from(size.height)),
        )?;
        set(&result, "format", &JsValue::from_str(format))?;
        set(
            &result,
            "colorSpace",
            &video_color_space(alpha2_media::ColorMetadata::EXPORT)?,
        )?;
        set(&result, "layout", &layout)?;
        set(&result, "pixels", &pixels)?;
        Ok(result.into())
    });
    (promise, retained_bytes)
}

/// The failure of a readback that timed out or met a lost device (`error`):
/// `DEVICE_HUNG` when the renderer's own long submission explains it
/// (`loss`), otherwise `code` with `message`.
fn loss_failure(
    loss: &alpha2_render::LossClassifier,
    error: RenderError,
    failure: &impl Fn(&str, &str, &str) -> JsValue,
    (code, stage): (&str, &str),
    message: &str,
) -> JsValue {
    match loss.classify(error) {
        hung @ RenderError::GpuDeviceHung { .. } => {
            failure("DEVICE_HUNG", stage, &hung.to_string())
        }
        _ => failure(code, stage, message),
    }
}

/// A mapped readback that did not finish, as its failure.
fn finish_failure(
    health: &DeviceHealth,
    error: RenderError,
    failure: &impl Fn(&str, &str, &str) -> JsValue,
) -> JsValue {
    match error {
        RenderError::GpuWorkDropped | RenderError::GpuDeviceLost => {
            // The renderer refuses every readback from now on, as the native host does by
            // exiting: the binding is lost and its owner must make it again.
            let message = error.to_string();
            health.mark_lost("gpu-work-dropped", &message);
            failure("DEVICE_LOST", "canary", &message)
        }
        // Lost the same way, after a submission that ran long enough to stop the GPU.
        RenderError::GpuDeviceHung { .. } => {
            let message = error.to_string();
            health.mark_lost("gpu-device-hung", &message);
            failure("DEVICE_HUNG", "canary", &message)
        }
        // The GPU rejected the frame's submission and the device reported why: that is the
        // frame's failure, and the binding keeps rendering.
        RenderError::Gpu { kind } => failure(
            match kind {
                alpha2_gpu::GpuErrorKind::OutOfMemory => "OUT_OF_MEMORY",
                alpha2_gpu::GpuErrorKind::Validation => "VALIDATION_FAILED",
                _ => "READBACK_FAILED",
            },
            "rejected",
            &error.to_string(),
        ),
        _ => failure("READBACK_FAILED", readback_stage(error), &error.to_string()),
    }
}

/// Where a readback's mapping went wrong, as a stage name and a message.
fn mapping_outcome(result: Option<Result<(), RenderError>>) -> Result<(), (&'static str, String)> {
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err((
            readback_stage(error),
            format!("export frame readback failed to map: {error}"),
        )),
        None => Err((
            "callback-dropped",
            "export frame readback never reported its mapping".to_owned(),
        )),
    }
}

const fn readback_stage(error: RenderError) -> &'static str {
    match error {
        RenderError::TextureReadback(stage) => stage.as_str(),
        _ => "map-failed",
    }
}

/// Each plane's offset and stride in the mapped staging buffer.
/// The output pictures an export asks for by `FFmpeg` name (`yuva420p`,
/// `yuv420p10le`), 8-bit 4:2:0 when it names none. A browser encodes 4:2:0
/// only, so any other layout is refused.
///
/// # Errors
/// `INVALID_INPUT` for a name that is no layout, or one a browser cannot
/// encode.
pub(crate) fn parse_pixels(name: Option<&str>) -> Result<alpha2_media::PixelLayout, JsValue> {
    let Some(name) = name else {
        return Ok(alpha2_media::PixelLayout::YUV420P);
    };
    let layout = alpha2_media::PixelLayout::from_ffmpeg_name(name)
        .ok_or_else(|| invalid_input(format!("unknown pixel format {name:?}")))?;
    if layout.chroma != alpha2_media::Chroma::Yuv420
        || layout.planes != alpha2_media::PlaneLayout::Planar
    {
        return Err(invalid_input(format!(
            "a browser encodes 4:2:0 planar pictures, not {name}"
        )));
    }
    Ok(layout)
}

/// The `WebCodecs` `VideoPixelFormat` of a 4:2:0 `layout`.
pub(crate) fn webcodecs_format(layout: alpha2_media::PixelLayout) -> &'static str {
    match (layout.bit_depth, layout.alpha) {
        (alpha2_media::BitDepth::Eight, false) => "I420",
        (alpha2_media::BitDepth::Eight, true) => "I420A",
        (alpha2_media::BitDepth::Ten, false) => "I420P10",
        (alpha2_media::BitDepth::Ten, true) => "I420AP10",
    }
}

/// Tight plane memory for a picture, the planes' `{ offset, stride }` as a
/// JS array, and the same as `(offset, stride)` pairs, in storage order.
pub(crate) type TightPlanes = (Vec<u8>, web_sys::js_sys::Array, Vec<(usize, usize)>);

/// [`TightPlanes`] for a `size` picture of `layout`.
pub(crate) fn tight_planes(
    layout: alpha2_media::PixelLayout,
    size: RenderSize,
) -> Result<TightPlanes, JsValue> {
    let mut offsets = Vec::new();
    let mut total = 0_usize;
    let js = web_sys::js_sys::Array::new();
    for (width, rows, bytes) in layout.planes(size.width, size.height) {
        let stride = (width * bytes) as usize;
        let entry = web_sys::js_sys::Object::new();
        set(
            &entry,
            "offset",
            &js_safe_integer(total as u64, "plane offset")?,
        )?;
        #[allow(clippy::cast_precision_loss)]
        set(&entry, "stride", &JsValue::from_f64(stride as f64))?;
        js.push(&entry);
        offsets.push((total, stride));
        total += stride * rows as usize;
    }
    Ok((vec![0; total], js, offsets))
}

/// `memory` split at `offsets` into one target per plane.
pub(crate) fn plane_targets<'a>(
    memory: &'a mut [u8],
    offsets: &[(usize, usize)],
) -> Vec<alpha2_render::PlaneTarget<'a>> {
    let mut targets = Vec::with_capacity(offsets.len());
    let mut rest = memory;
    let mut consumed = 0;
    for (index, &(offset, stride)) in offsets.iter().enumerate() {
        let end = offsets
            .get(index + 1)
            .map_or(consumed + rest.len(), |next| next.0);
        let (plane, tail) = std::mem::take(&mut rest).split_at_mut(end - offset);
        targets.push(alpha2_render::PlaneTarget::new(plane, stride));
        rest = tail;
        consumed = end;
    }
    targets
}

fn plane_layout_js(
    readback: &alpha2_render::Yuv420pReadback,
) -> Result<web_sys::js_sys::Array, JsValue> {
    let layout = web_sys::js_sys::Array::new();
    for plane in readback.plane_layout() {
        let entry = web_sys::js_sys::Object::new();
        set(
            &entry,
            "offset",
            &js_safe_integer(plane.offset, "plane offset")?,
        )?;
        set(
            &entry,
            "stride",
            &JsValue::from_f64(f64::from(plane.stride)),
        )?;
        layout.push(&entry);
    }
    Ok(layout)
}

/// `color` as a `WebCodecs` `VideoColorSpaceInit`, so a browser encoder tags
/// what the pack encoded, as the native encoder does from the same value.
pub(crate) fn video_color_space(color: alpha2_media::ColorMetadata) -> Result<JsValue, JsValue> {
    use alpha2_media::{ColorMatrix, ColorPrimaries, ColorRange, TransferFunction};
    let space = web_sys::js_sys::Object::new();
    let primaries = match color.primaries {
        ColorPrimaries::Bt601 => "smpte170m",
        ColorPrimaries::Bt709 => "bt709",
        ColorPrimaries::Bt2020 => "bt2020",
    };
    let transfer = match color.transfer {
        TransferFunction::Bt709 => "bt709",
        TransferFunction::Srgb => "iec61966-2-1",
        TransferFunction::Pq => "pq",
        TransferFunction::Hlg => "hlg",
    };
    let matrix = match color.matrix {
        ColorMatrix::Bt601 => "smpte170m",
        ColorMatrix::Bt709 => "bt709",
        ColorMatrix::Bt2020Ncl => "bt2020-ncl",
        ColorMatrix::Rgb => "rgb",
    };
    set(&space, "primaries", &JsValue::from_str(primaries))?;
    set(&space, "transfer", &JsValue::from_str(transfer))?;
    set(&space, "matrix", &JsValue::from_str(matrix))?;
    set(
        &space,
        "fullRange",
        &JsValue::from_bool(color.range == ColorRange::Full),
    )?;
    Ok(space.into())
}

/// What the browser has said about one device: whether it is lost, and its
/// recent errors, typed and in bounded memory, and also told to the renderer
/// (`reports`), which cannot scope a browser frame's errors itself.
#[derive(Clone, Debug)]
pub(crate) struct DeviceHealth {
    pub(crate) lost: Arc<AtomicBool>,
    pub(crate) log: Arc<GpuDiagnosticLog>,
    pub(crate) reports: alpha2_render::GpuErrorReports,
}

impl DeviceHealth {
    /// Records a loss the browser reported outside wgpu, such as a WebGL
    /// context loss event.
    pub(crate) fn mark_lost(&self, reason: &str, message: &str) {
        if !self.lost.swap(true, Ordering::AcqRel) {
            self.log.push(GpuDiagnostic::DeviceLost {
                reason: reason.to_owned(),
                message: message.to_owned(),
            });
        }
    }
}

/// Installs `device`'s uncaptured-error and device-lost callbacks, recording
/// both in one typed log the way a native `GpuContext` handler receives them.
pub(crate) fn watch_device(device: &wgpu::Device) -> DeviceHealth {
    let health = DeviceHealth {
        lost: Arc::new(AtomicBool::new(false)),
        log: Arc::new(GpuDiagnosticLog::default()),
        reports: alpha2_render::GpuErrorReports::default(),
    };
    let log = Arc::clone(&health.log);
    let reports = health.reports.clone();
    // Browser WebGPU validation is asynchronous, so a successful `renderAt` does not prove the
    // frame passed; the renderer hears of errors too and stops trusting what it kept.
    device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
        log.push(GpuDiagnostic::uncaptured(&error));
        reports.report(alpha2_gpu::GpuErrorKind::of(&error));
    }));
    let lost = health.clone();
    device.set_device_lost_callback(move |reason, message| {
        // A registry's `free()` destroys its device on purpose; that is not a loss to warn
        // about.
        if reason != wgpu::DeviceLostReason::Destroyed {
            web_sys::console::warn_1(
                &format!("alpha2 GPU device lost: {reason:?} {message}").into(),
            );
        }
        lost.mark_lost(&format!("{reason:?}"), &message);
    });
    health
}

/// The device log as JSON: the events, oldest first, and how many were dropped.
pub(crate) fn gpu_diagnostics_json(log: &GpuDiagnosticLog) -> serde_json::Value {
    let snapshot = log.snapshot();
    serde_json::json!({
        "events": snapshot.entries.iter().map(gpu_diagnostic_json).collect::<Vec<_>>(),
        "dropped": snapshot.dropped,
        "deviceLost": snapshot.device_lost,
        "evidence": snapshot.failure_code(),
    })
}

fn gpu_diagnostic_json(diagnostic: &GpuDiagnostic) -> serde_json::Value {
    match diagnostic {
        GpuDiagnostic::UncapturedError { kind, message } => serde_json::json!({
            "event": diagnostic.event(),
            "kind": kind.as_str(),
            "code": diagnostic.failure_code(),
            "message": message,
        }),
        GpuDiagnostic::DeviceLost { reason, message } => serde_json::json!({
            "event": diagnostic.event(),
            "reason": reason,
            "code": diagnostic.failure_code(),
            "message": message,
        }),
        _ => serde_json::json!({ "event": diagnostic.event(), "message": diagnostic.to_string() }),
    }
}

/// A rejected export readback: a JavaScript error whose `failure` says which
/// class it is, at which step, for which frame, with the device's evidence.
///
/// A mapping that failed while the device log holds an out-of-memory report
/// is classed `OUT_OF_MEMORY` on the GPU; without typed evidence it stays
/// `READBACK_FAILED`, since a refused mapping names no cause of its own.
fn readback_failure(
    health: &DeviceHealth,
    code: &str,
    stage: &str,
    message: &str,
    index: Option<u64>,
    time: MediaTime,
) -> JsValue {
    let evidence = health.log.snapshot().failure_code();
    let code = match (code, evidence) {
        ("READBACK_FAILED", Some(evidence @ ("DEVICE_LOST" | "OUT_OF_MEMORY"))) => evidence,
        _ => code,
    };
    // The error's code follows the native host: a readback that failed for want of the device is
    // the lost device, a timeout a timeout, and any other the binding's own step.
    let error = coded_error(
        match code {
            "DEVICE_LOST" => Code::GPU_DEVICE_LOST,
            "DEVICE_HUNG" => Code::GPU_DEVICE_HUNG,
            "GPU_TIMEOUT" => Code::GPU_TIMEOUT,
            _ => Code::INTERNAL,
        },
        message,
    );
    let failure = web_sys::js_sys::Object::new();
    let _ = set(&failure, "code", &JsValue::from_str(code));
    let _ = set(&failure, "phase", &JsValue::from_str("readback"));
    let _ = set(&failure, "stage", &JsValue::from_str(stage));
    let _ = set(
        &failure,
        "resource",
        &JsValue::from_str(
            if matches!(code, "OUT_OF_MEMORY" | "VALIDATION_FAILED" | "DEVICE_HUNG") {
                "gpu"
            } else {
                "unknown"
            },
        ),
    );
    // A frame rendered outside an export has no index; its caller's context says which frame it
    // was.
    if let Some(index) = index {
        #[allow(clippy::cast_precision_loss)] // A frame index stays below 2^53.
        let _ = set(&failure, "frameIndex", &JsValue::from_f64(index as f64));
    }
    let _ = set(
        &failure,
        "timeTicks",
        &JsValue::from_str(&time.ticks().to_string()),
    );
    if let Some(value) = serde_json::to_string(&gpu_diagnostics_json(&health.log))
        .ok()
        .and_then(|json| web_sys::js_sys::JSON::parse(&json).ok())
    {
        let _ = set(&failure, "gpu", &value);
    }
    let _ = web_sys::js_sys::Reflect::set(&error, &JsValue::from_str("failure"), &failure);
    error
}

/// A renderer on `gpu` that reports GPU errors through `health`, with the
/// built-in effects registered, each compiled the first time it is drawn. A
/// built-in that fails to register is an internal error.
pub(crate) fn renderer_with_builtins(
    gpu: GpuContext,
    health: &DeviceHealth,
) -> Result<Renderer, JsValue> {
    let mut renderer = Renderer::new(gpu).with_gpu_error_reports(health.reports.clone());
    renderer
        .register_builtins()
        .map_err(|diagnostics| diagnostics_to_js(Code::INTERNAL, diagnostics))?;
    Ok(renderer)
}

/// The shader diagnostics as one error: `code` says whose shader failed,
/// the caller's effect or the binding's own.
pub(crate) fn diagnostics_to_js(code: Code, diagnostics: ShaderDiagnostics) -> JsValue {
    let message = diagnostics
        .messages
        .into_iter()
        .map(|diagnostic| match (diagnostic.line, diagnostic.column) {
            (Some(line), Some(column)) => {
                format!("{line}:{column}: {}", diagnostic.message)
            }
            _ => diagnostic.message,
        })
        .collect::<Vec<_>>()
        .join("\n");
    coded_error(code, message)
}

/// Converts an absolute sample window into the timeline span it covers.
pub(crate) fn audio_span(
    sample_rate: u32,
    start_sample: i64,
    frames: u32,
) -> Result<alpha2_time::TimeRange, JsValue> {
    if sample_rate == 0 || frames == 0 || start_sample < 0 {
        return Err(invalid_input("audio window is out of range"));
    }
    let ticks = |sample: i64| -> i64 {
        let value = i128::from(sample) * i128::from(alpha2_time::TICKS_PER_SECOND)
            / i128::from(sample_rate);
        i64::try_from(value).unwrap_or(i64::MAX)
    };
    let start = ticks(start_sample);
    let end_sample = start_sample
        .checked_add(i64::from(frames))
        .ok_or_else(|| invalid_input("audio window is out of range"))?;
    let minimum_end = start
        .checked_add(1)
        .ok_or_else(|| invalid_input("audio window is out of range"))?;
    let end = ticks(end_sample).max(minimum_end);
    alpha2_time::TimeRange::new(MediaTime(start), MediaTime(end))
        .map_err(|error| typed_error(&error))
}

/// Sets one own property on a plain result object.
pub(crate) fn set(
    target: &web_sys::js_sys::Object,
    key: &str,
    value: &JsValue,
) -> Result<(), JsValue> {
    web_sys::js_sys::Reflect::set(target, &JsValue::from_str(key), value)?;
    Ok(())
}

/// Builds a non-pixel export status object.
pub(crate) fn export_status(
    kind: &str,
    time_ticks: Option<i64>,
    missing: &[String],
) -> Result<JsValue, JsValue> {
    let result = web_sys::js_sys::Object::new();
    set(&result, "kind", &JsValue::from_str(kind))?;
    if let Some(ticks) = time_ticks {
        set(&result, "timeTicks", &JsValue::from_str(&ticks.to_string()))?;
    }
    let sources = web_sys::js_sys::Array::new();
    for source in missing {
        sources.push(&JsValue::from_str(source));
    }
    set(&result, "missingSources", &sources.into())?;
    Ok(result.into())
}

/// Wraps an already-known value in a resolved promise.
pub(crate) fn resolved(value: &JsValue) -> web_sys::js_sys::Promise {
    web_sys::js_sys::Promise::resolve(value)
}
