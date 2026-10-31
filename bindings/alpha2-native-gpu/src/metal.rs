//! `VideoToolbox`/`CoreVideo` surface import into the renderer's `Metal` device.

use std::{error::Error, ffi::c_void, fmt, ptr::NonNull, sync::Arc};

use alpha2_ffmpeg_bridge::{BridgeError, VideoToolboxFrameLease};
use alpha2_gpu::GpuContext;
use objc2::{
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_metal::{MTLTexture, MTLTextureType};

use crate::{NativeGpuIdentity, NativePlaneLayout};

use crate::interop::Alpha2MetalFrameInfo as MetalFrameInfo;

/// A codec `VideoToolbox` may decode in hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoToolboxCodec {
    /// H.264 (`avc1`).
    H264,
    /// HEVC (`hvc1`).
    Hevc,
    /// VP9 (`vp09`), through the supplemental decoder.
    Vp9,
    /// AV1 (`av01`), through the supplemental decoder.
    Av1,
}

impl VideoToolboxCodec {
    /// Every codec, in the order a device's list names them.
    const ALL: [Self; 4] = [Self::H264, Self::Hevc, Self::Vp9, Self::Av1];

    /// The codec's `FFmpeg` name, as conform and `capabilities.decode` spell it.
    const fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Vp9 => "vp9",
            Self::Av1 => "av1",
        }
    }

    /// The `CMVideoCodecType` four-character code.
    const fn four_cc(self) -> u32 {
        u32::from_be_bytes(*match self {
            Self::H264 => b"avc1",
            Self::Hevc => b"hvc1",
            Self::Vp9 => b"vp09",
            Self::Av1 => b"av01",
        })
    }
}

/// Whether `VideoToolbox` decodes `codec` in hardware on this machine, as
/// `VTIsHardwareDecodeSupported` answers after registering the supplemental
/// decoders. It answers per codec, not per profile.
#[must_use]
pub fn videotoolbox_hardware_decodes(codec: VideoToolboxCodec) -> bool {
    // SAFETY: takes a plain integer and calls two VideoToolbox queries.
    unsafe { alpha2_native_gpu_vt_hardware_decodes(codec.four_cc()) != 0 }
}

/// The codecs `VideoToolbox` decodes in hardware on this machine, by `FFmpeg`
/// name, asked once per process: the answer is the machine's, not a device's.
fn hardware_decode_codecs() -> &'static [&'static str] {
    static CODECS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    CODECS.get_or_init(|| {
        VideoToolboxCodec::ALL
            .into_iter()
            .filter(|codec| videotoolbox_hardware_decodes(*codec))
            .map(VideoToolboxCodec::name)
            .collect()
    })
}

/// The system-wide `IOSurface` ID behind a `VideoToolbox` frame's pixel
/// buffer, or `None` for a frame without one. The ID names the surface even
/// when `CoreVideo` hands it over in a new pixel buffer, so a count of IDs is
/// a count of the decoder's surfaces.
#[must_use]
pub fn videotoolbox_surface_id(frame: &VideoToolboxFrameLease) -> Option<u32> {
    let pixel_buffer = usize::try_from(frame.info().ok()?.pixel_buffer).ok()? as *mut c_void;
    // SAFETY: the lease holds the frame, and with it the pixel buffer, for the call; the helper
    // accepts null.
    let id = unsafe { alpha2_native_gpu_iosurface_id(pixel_buffer) };
    (id != 0).then_some(id)
}

/// `alpha2_native_gpu_metal_encoder_plane`'s refusals in words.
fn encoder_plane_reason(code: i32) -> &'static str {
    match code {
        -1 => "bad argument",
        -2 => "not 8-bit video-range NV12",
        -3 => "not two planes",
        -4 => "no IOSurface",
        -5 => "Metal returned no texture",
        -6 => "not an Apple-family GPU",
        _ => "unknown refusal",
    }
}

/// `alpha2_native_gpu_metal_frame_info`'s refusals in words.
fn frame_info_reason(code: i32) -> &'static str {
    match code {
        -1 => "null argument",
        -2 => "pixel format is not 420v/420f/x420/xf20",
        -3 => "not two planes",
        -4 => "no IOSurface",
        _ => "unknown refusal",
    }
}

/// A `CoreVideo` pixel format as its four characters, or in hex when they are
/// not all printable ASCII.
fn four_cc_text(format: u32) -> String {
    let bytes = format.to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        bytes.iter().map(|byte| char::from(*byte)).collect()
    } else {
        format!("0x{format:08x}")
    }
}

unsafe extern "C" {
    fn alpha2_native_gpu_vt_hardware_decodes(codec_type: u32) -> i32;
    fn alpha2_native_gpu_iosurface_id(pixel_buffer: *mut c_void) -> u32;
    fn alpha2_native_gpu_metal_cache_create(device: *mut c_void, output: *mut *mut c_void) -> i32;
    fn alpha2_native_gpu_metal_cache_destroy(cache: *mut c_void);
    fn alpha2_native_gpu_metal_frame_info(
        pixel_buffer: *mut c_void,
        output: *mut MetalFrameInfo,
    ) -> i32;
    fn alpha2_native_gpu_metal_import(
        cache: *mut c_void,
        pixel_buffer: *mut c_void,
        plane: u32,
        texture: *mut *mut c_void,
        metal_texture: *mut *mut c_void,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_metal_texture_destroy(texture: *mut c_void);
    fn alpha2_native_gpu_metal_encoder_plane(
        device: *mut c_void,
        pixel_buffer: *mut c_void,
        plane: u32,
        width: *mut u32,
        height: *mut u32,
        texture: *mut *mut c_void,
    ) -> i32;
}

/// `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange`.
const PIXEL_FORMAT_420_10_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"x420");
/// `kCVPixelFormatType_420YpCbCr10BiPlanarFullRange`.
const PIXEL_FORMAT_420_10_FULL_RANGE: u32 = u32::from_be_bytes(*b"xf20");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`.
const PIXEL_FORMAT_420_8_FULL_RANGE: u32 = u32::from_be_bytes(*b"420f");

/// How long a wait on the renderer's queue may take, in milliseconds.
static WAIT_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30_000);

pub(crate) fn set_wait_timeout(timeout: std::time::Duration) {
    WAIT_TIMEOUT_MS.store(
        u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// How long a wait on the renderer's queue for work handed to another
/// engine may take before it is reported as a timeout.
#[must_use]
pub fn gpu_wait_timeout() -> std::time::Duration {
    std::time::Duration::from_millis(WAIT_TIMEOUT_MS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Wraps `plane` of `pixel_buffer` as a `CoreVideo` Metal texture through
/// the helper, or fails the way [`inject`] asks.
///
/// # Safety
/// As for the helper: the cache and pixel buffer are live for the call, and
/// the outputs are writable.
unsafe fn import_plane(
    cache: *mut c_void,
    pixel_buffer: *mut c_void,
    plane: u32,
    texture: &mut *mut c_void,
    metal_texture: &mut *mut c_void,
    status: &mut i32,
) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::fails(plane, status) {
        return step;
    }
    // SAFETY: the caller's contract.
    let step = unsafe {
        alpha2_native_gpu_metal_import(cache, pixel_buffer, plane, texture, metal_texture, status)
    };
    #[cfg(test)]
    if step == 0 {
        inject::made();
    }
    step
}

/// Releases a `CoreVideo` texture [`import_plane`] made.
///
/// # Safety
/// `texture` is a +1 reference from the helper, released once.
unsafe fn destroy_texture(texture: *mut c_void) {
    #[cfg(test)]
    inject::released();
    // SAFETY: the caller's contract.
    unsafe { alpha2_native_gpu_metal_texture_destroy(texture) };
}

/// Fails a plane of the next import on this thread with a `CVReturn` a test
/// chooses, at the texture cache call, as only `CoreVideo` would, and counts
/// the `CoreVideo` textures this thread holds, so tests reach the branches
/// that handle a failure and see what they keep.
#[cfg(test)]
pub(crate) mod inject {
    use std::cell::Cell;

    thread_local! {
        static NEXT_IMPORT_FAILS: Cell<Option<(u32, i32)>> = const { Cell::new(None) };
        static LIVE_TEXTURES: Cell<u64> = const { Cell::new(0) };
    }

    /// Fails `plane` of the next import on this thread with `status`, at
    /// `CVMetalTextureCacheCreateTextureFromImage`; the planes before it
    /// import.
    pub(crate) fn fail_next_import_with(plane: u32, status: i32) {
        NEXT_IMPORT_FAILS.with(|next| next.set(Some((plane, status))));
    }

    /// The `CoreVideo` textures made on this thread and not yet released.
    pub(crate) fn live_textures() -> u64 {
        LIVE_TEXTURES.with(Cell::get)
    }

    /// Whether this plane is the one to fail; a failure returns the status
    /// the test chose at the texture cache step.
    pub(super) fn fails(plane: u32, status: &mut i32) -> Option<i32> {
        let chosen = NEXT_IMPORT_FAILS.with(Cell::get)?;
        (chosen.0 == plane).then(|| {
            NEXT_IMPORT_FAILS.with(|next| next.set(None));
            *status = chosen.1;
            crate::interop::ALPHA2_GPU_STEP_CREATE_TEXTURE_FROM_IMAGE
        })
    }

    pub(super) fn made() {
        LIVE_TEXTURES.with(|live| live.set(live.get() + 1));
    }

    pub(super) fn released() {
        LIVE_TEXTURES.with(|live| live.set(live.get().saturating_sub(1)));
    }
}

/// Failure while creating or using a same-device `CoreVideo` texture cache.
#[derive(Debug)]
pub struct SharedMetalError(String);

impl fmt::Display for SharedMetalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SharedMetalError {}

struct MetalCache {
    raw: NonNull<c_void>,
}

// SAFETY: CoreVideo texture caches may be used by independent decode/render workers; CoreVideo
// serializes access to the cache object.
unsafe impl Send for MetalCache {}
// SAFETY: as for `Send`.
unsafe impl Sync for MetalCache {}

impl Drop for MetalCache {
    fn drop(&mut self) {
        // SAFETY: `raw` is the +1 reference returned by cache_create.
        unsafe { alpha2_native_gpu_metal_cache_destroy(self.raw.as_ptr()) };
    }
}

struct CoreVideoTexture(NonNull<c_void>);

// SAFETY: a uniquely owned +1 CoreVideo reference, released once on drop; CoreFoundation reference
// counting is thread-safe.
unsafe impl Send for CoreVideoTexture {}
// SAFETY: shared references never touch the pointer.
unsafe impl Sync for CoreVideoTexture {}

impl Drop for CoreVideoTexture {
    fn drop(&mut self) {
        // SAFETY: this is the +1 CVMetalTextureRef returned by the import helper.
        unsafe { destroy_texture(self.0.as_ptr()) };
    }
}

/// `Metal` device and `CoreVideo` texture cache shared with the renderer.
#[derive(Clone)]
pub struct SharedMetalContext {
    gpu: GpuContext,
    cache: Arc<MetalCache>,
    identity: NativeGpuIdentity,
}

impl fmt::Debug for SharedMetalContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedMetalContext")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Imported bi-planar `CoreVideo` image exposed as two wgpu texture views.
#[derive(Debug)]
pub struct ImportedMetalFrame {
    /// Plane-zero luma view.
    pub y: wgpu::TextureView,
    /// Plane-one interleaved chroma view.
    pub uv: wgpu::TextureView,
    /// Sample layout of the imported planes.
    pub layout: NativePlaneLayout,
    /// `CoreVideo` clean aperture in coded pixel coordinates.
    pub visible_rect: [u32; 4],
    /// Luma plane extent, which the visible rectangle is placed inside.
    ///
    /// `CoreVideo` may hand back a buffer larger than the display size, so this
    /// is the extent presentation must normalize sample coordinates against.
    pub coded_size: [u32; 2],
    /// Whether the planes hold full-range samples, from the buffer's pixel
    /// format (`420f`/`xf20` full, `420v`/`x420` video range). The buffer is
    /// what the samples are, whatever the stream's tag says: `VideoToolbox`
    /// hands back an HEVC-with-alpha base layer converted to video range
    /// from a full-range tag.
    pub full_range: bool,
    /// The buffer's pixel format as its four characters, for a report.
    pub pixel_format: String,
    /// Decoder and `CoreVideo` ownership retained through queue completion.
    pub lease: Arc<MetalImportLease>,
}

/// Owns the decoder frame, `CoreVideo` plane wrappers, and wgpu textures.
pub struct MetalImportLease {
    _frame: Arc<VideoToolboxFrameLease>,
    _cv_textures: [CoreVideoTexture; 2],
    _textures: [wgpu::Texture; 2],
    retained_bytes: u64,
}

impl fmt::Debug for MetalImportLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MetalImportLease")
            .field("retained_bytes", &self.retained_bytes)
            .finish_non_exhaustive()
    }
}

impl MetalImportLease {
    /// Approximate decoded-surface bytes retained by this import.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// Records that the sampling submission has been queued.
    ///
    /// `Metal` tracks `CoreVideo` texture hazards implicitly. The host keeps this
    /// lease alive through its queue-completion callback.
    pub const fn submitted(&self) {}
}

/// The two planes of one encoder-input pixel buffer, as render targets.
///
/// Each texture aliases the buffer's `IOSurface`, so drawing into it is
/// writing the frame the encoder reads. The textures keep the surface alive;
/// the pixel buffer itself is its pool's to recycle.
#[derive(Debug)]
pub struct MetalEncoderPlanes {
    /// Plane-zero luma, `R8Unorm`, full size.
    pub luma: wgpu::Texture,
    /// Plane-one interleaved chroma, `Rg8Unorm`, half size each way.
    pub chroma: wgpu::Texture,
}

impl SharedMetalContext {
    /// Creates a `CoreVideo` texture cache for an existing `Metal`-backed renderer.
    ///
    /// # Errors
    /// Returns an error if the renderer is not `Metal` or `CoreVideo` rejects its device.
    pub fn from_gpu(gpu: GpuContext) -> Result<Self, SharedMetalError> {
        // SAFETY: this call only borrows the device for the duration of the guard.
        let hal = unsafe { gpu.device().as_hal::<wgpu_hal::api::Metal>() }
            .ok_or_else(|| SharedMetalError("renderer is not using Metal".into()))?;
        let device = Retained::as_ptr(hal.raw_device())
            .cast_mut()
            .cast::<c_void>();
        let mut cache = std::ptr::null_mut();
        // SAFETY: `device` is the renderer's live id<MTLDevice> and output is writable.
        let result = unsafe { alpha2_native_gpu_metal_cache_create(device, &raw mut cache) };
        if result != 0 {
            return Err(SharedMetalError(format!(
                "CVMetalTextureCacheCreate failed with {result}"
            )));
        }
        let cache = NonNull::new(cache)
            .ok_or_else(|| SharedMetalError("CoreVideo returned no texture cache".into()))?;
        let identity = NativeGpuIdentity {
            physical_device: device.addr() as u64,
            device: device.addr() as u64,
            epoch: 1,
        };
        drop(hal);
        // Asked here, before any export, so a session never pays for the query.
        hardware_decode_codecs();
        Ok(Self {
            gpu,
            cache: Arc::new(MetalCache { raw: cache }),
            identity,
        })
    }

    /// Renderer context that owns this `Metal` device.
    #[must_use]
    pub fn gpu(&self) -> GpuContext {
        self.gpu.clone()
    }

    /// Whether this device can present 10-bit decoded planes.
    ///
    /// `CoreVideo` hands each plane over separately, so this is the renderer's
    /// standalone 16-bit norm support rather than a planar format feature.
    #[must_use]
    pub fn supports_ten_bit(&self) -> bool {
        self.gpu
            .device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
    }

    /// The codecs `VideoToolbox` decodes in hardware here, by `FFmpeg` name
    /// (`h264`, `hevc`, `vp9`, `av1`), as `VTIsHardwareDecodeSupported`
    /// answers per codec, not per profile.
    #[must_use]
    pub fn decode_codecs(&self) -> &'static [&'static str] {
        hardware_decode_codecs()
    }

    /// Raw-device cache identity.
    #[must_use]
    pub const fn identity(&self) -> NativeGpuIdentity {
        self.identity
    }

    /// Imports an `IOSurface`-backed 4:2:0 pixel buffer as two `Metal` textures.
    ///
    /// Both 8-bit (`420v`/`420f`) and 10-bit (`x420`/`xf20`) bi-planar buffers
    /// are supported; a 10-bit buffer keeps its samples left-aligned in 16-bit
    /// containers, which presentation corrects for.
    ///
    /// # Errors
    /// Returns an error for unsupported/non-`IOSurface` buffers or failed
    /// `Metal` wrapping.
    #[allow(clippy::too_many_lines)]
    pub fn import_surface(
        &self,
        frame: Arc<VideoToolboxFrameLease>,
    ) -> Result<ImportedMetalFrame, BridgeError> {
        let decoded = frame.info()?;
        let pixel_buffer = usize::try_from(decoded.pixel_buffer)
            .map_err(|_| BridgeError::InvalidInput("invalid CoreVideo pointer"))?
            as *mut c_void;
        let mut info = MetalFrameInfo::default();
        // SAFETY: the retained AVFrame owns `pixel_buffer`, and output is writable.
        let result = unsafe { alpha2_native_gpu_metal_frame_info(pixel_buffer, &raw mut info) };
        if result != 0 {
            return Err(BridgeError::UnsupportedSurface(format!(
                "VideoToolbox frame {} ({}x{}, {} planes) is not an IOSurface-backed 4:2:0 \
                 buffer: {}",
                four_cc_text(info.pixel_format),
                info.width,
                info.height,
                info.plane_count,
                frame_info_reason(result)
            )));
        }
        let layout = match info.pixel_format {
            PIXEL_FORMAT_420_10_VIDEO_RANGE | PIXEL_FORMAT_420_10_FULL_RANGE => {
                NativePlaneLayout::P010
            }
            _ => NativePlaneLayout::Nv12,
        };
        // CoreVideo hands each plane over separately, so a 10-bit buffer needs the renderer's
        // standalone 16-bit norm formats rather than P010.
        if layout == NativePlaneLayout::P010
            && !self
                .gpu
                .device()
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        {
            return Err(BridgeError::InvalidInput(
                "renderer does not expose 16-bit plane textures",
            ));
        }
        let mut cv = [std::ptr::null_mut(); 2];
        let mut metal = [std::ptr::null_mut(); 2];
        for plane in 0..2_u32 {
            let mut status = 0;
            // SAFETY: cache and pixel buffer are live; outputs belong to this call.
            let step = unsafe {
                import_plane(
                    self.cache.raw.as_ptr(),
                    pixel_buffer,
                    plane,
                    &mut cv[plane as usize],
                    &mut metal[plane as usize],
                    &mut status,
                )
            };
            if step != 0 {
                for texture in cv.into_iter().filter(|texture| !texture.is_null()) {
                    // SAFETY: each non-null entry is a +1 reference from the helper.
                    unsafe { destroy_texture(texture) };
                }
                return Err(crate::status::helper_failure(
                    alpha2_ffmpeg_bridge::NativeGpuBackend::Metal,
                    "texture import",
                    step,
                    status,
                ));
            }
        }
        let cv_textures = [
            CoreVideoTexture(
                NonNull::new(cv[0])
                    .ok_or(BridgeError::InvalidInput("CoreVideo returned no Y texture"))?,
            ),
            CoreVideoTexture(NonNull::new(cv[1]).ok_or(BridgeError::InvalidInput(
                "CoreVideo returned no UV texture",
            ))?),
        ];
        let y = self.wrap_plane(
            metal[0],
            info.plane_width[0],
            info.plane_height[0],
            0,
            layout,
        )?;
        let uv = self.wrap_plane(
            metal[1],
            info.plane_width[1],
            info.plane_height[1],
            1,
            layout,
        )?;
        let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
        let retained_bytes = u64::from(info.width)
            .saturating_mul(u64::from(info.height))
            .saturating_mul(layout.bytes_per_pixel_numerator())
            .div_ceil(2);
        let lease = Arc::new(MetalImportLease {
            _frame: frame,
            _cv_textures: cv_textures,
            _textures: [y, uv],
            retained_bytes,
        });
        Ok(ImportedMetalFrame {
            y: y_view,
            uv: uv_view,
            layout,
            visible_rect: [
                info.clean_x,
                info.clean_y,
                info.clean_width,
                info.clean_height,
            ],
            coded_size: [info.plane_width[0], info.plane_height[0]],
            full_range: matches!(
                info.pixel_format,
                PIXEL_FORMAT_420_8_FULL_RANGE | PIXEL_FORMAT_420_10_FULL_RANGE
            ),
            pixel_format: four_cc_text(info.pixel_format),
            lease,
        })
    }

    /// Wraps the planes of an encoder-input `VideoToolbox` frame's pixel
    /// buffer as render targets on the renderer's device.
    ///
    /// # Errors
    /// Returns an error for a frame that is not a `VideoToolbox` hardware
    /// frame, or whose buffer is not `IOSurface`-backed 8-bit video-range
    /// NV12 that `Metal` can alias.
    pub fn encoder_planes(
        &self,
        frame: &rsmpeg::avutil::AVFrame,
    ) -> Result<MetalEncoderPlanes, SharedMetalError> {
        let info = alpha2_ffmpeg_bridge::videotoolbox_frame_info(frame)
            .map_err(|error| SharedMetalError(format!("encoder frame: {error}")))?;
        let pixel_buffer = usize::try_from(info.pixel_buffer)
            .ok()
            .filter(|pointer| *pointer != 0)
            .ok_or_else(|| SharedMetalError("encoder frame has no pixel buffer".into()))?
            as *mut c_void;
        // SAFETY: the guard only borrows the device for this call.
        let hal = unsafe { self.gpu.device().as_hal::<wgpu_hal::api::Metal>() }
            .ok_or_else(|| SharedMetalError("renderer is not using Metal".into()))?;
        let device = Retained::as_ptr(hal.raw_device())
            .cast_mut()
            .cast::<c_void>();
        let mut planes = Vec::with_capacity(2);
        for plane in 0..2_u32 {
            let (mut width, mut height, mut raw) = (0, 0, std::ptr::null_mut());
            // SAFETY: the device is live for the guard, `frame` holds the pixel buffer for this
            // call, and the outputs belong to this call.
            let result = unsafe {
                alpha2_native_gpu_metal_encoder_plane(
                    device,
                    pixel_buffer,
                    plane,
                    &raw mut width,
                    &raw mut height,
                    &raw mut raw,
                )
            };
            if result != 0 {
                return Err(SharedMetalError(format!(
                    "encoder pixel buffer plane {plane} cannot be a Metal render target: {} \
                     ({result})",
                    encoder_plane_reason(result)
                )));
            }
            // SAFETY: a non-null result is the +1 id<MTLTexture> the helper transferred to us.
            let object = unsafe { Retained::<AnyObject>::from_raw(raw.cast()) }
                .ok_or_else(|| SharedMetalError("Metal returned no plane texture".into()))?;
            // SAFETY: the helper created this object with
            // -newTextureWithDescriptor:iosurface:plane:.
            let texture: Retained<ProtocolObject<dyn MTLTexture>> =
                unsafe { Retained::cast_unchecked(object) };
            planes.push(self.wrap_target(texture, width, height, plane));
        }
        drop(hal);
        let chroma = planes.pop();
        let luma = planes.pop();
        luma.zip(chroma)
            .map(|(luma, chroma)| MetalEncoderPlanes { luma, chroma })
            .ok_or_else(|| SharedMetalError("Metal returned no plane texture".into()))
    }

    fn wrap_target(
        &self,
        raw: Retained<ProtocolObject<dyn MTLTexture>>,
        width: u32,
        height: u32,
        plane: u32,
    ) -> wgpu::Texture {
        let format = if plane == 0 {
            wgpu::TextureFormat::R8Unorm
        } else {
            wgpu::TextureFormat::Rg8Unorm
        };
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        // SAFETY: a two-dimensional, single-mip, single-sample texture of this format and size on
        // the renderer's device.
        let hal_texture = unsafe {
            wgpu_hal::metal::Device::texture_from_raw(
                raw,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu_hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                None,
            )
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some(if plane == 0 {
                "alpha2 VideoToolbox encoder Y"
            } else {
                "alpha2 VideoToolbox encoder UV"
            }),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        };
        // SAFETY: the texture was created on this wgpu device's own Metal device with the usage
        // above, and every frame's pack overwrites it whole, so its prior contents may be
        // discarded.
        unsafe {
            self.gpu
                .device()
                .create_texture_from_hal::<wgpu_hal::api::Metal>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::UNINITIALIZED,
                )
        }
    }

    fn wrap_plane(
        &self,
        raw: *mut c_void,
        width: u32,
        height: u32,
        plane: usize,
        layout: NativePlaneLayout,
    ) -> Result<wgpu::Texture, BridgeError> {
        // SAFETY: `raw` is the Metal texture the import helper returned for a live CoreVideo
        // texture, or null, which `retain` maps to `None`.
        let object = unsafe { Retained::<AnyObject>::retain(raw.cast()) }.ok_or(
            BridgeError::InvalidInput("CoreVideo returned no Metal texture"),
        )?;
        // SAFETY: CoreVideo returned this object as id<MTLTexture>.
        let raw_texture: Retained<ProtocolObject<dyn MTLTexture>> =
            unsafe { Retained::cast_unchecked(object) };
        let format = match (plane, layout) {
            (0, NativePlaneLayout::P010) => wgpu::TextureFormat::R16Unorm,
            (0, _) => wgpu::TextureFormat::R8Unorm,
            (_, NativePlaneLayout::P010) => wgpu::TextureFormat::Rg16Unorm,
            (_, _) => wgpu::TextureFormat::Rg8Unorm,
        };
        // SAFETY: the retained object is a two-dimensional single-mip Metal texture.
        let hal_texture = unsafe {
            wgpu_hal::metal::Device::texture_from_raw(
                raw_texture,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu_hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                None,
            )
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some(if plane == 0 {
                "alpha2 VideoToolbox Y"
            } else {
                "alpha2 VideoToolbox UV"
            }),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: the texture originates from the same raw Metal device used by this wgpu device
        // and CoreVideo initializes its contents.
        Ok(unsafe {
            self.gpu
                .device()
                .create_texture_from_hal::<wgpu_hal::api::Metal>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::RESOURCE,
                )
        })
    }
}

#[cfg(test)]
#[path = "metal_tests.rs"]
mod tests;
