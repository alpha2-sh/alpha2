//! Hardware video encode from renderer-packed planes.
//!
//! This is the contract every GPU-resident export backend implements, so the
//! host's export loop names no platform type. A backend is opened on the
//! renderer's own device and splits into two halves that live on different
//! threads:
//!
//! - [`HardwareEncodeInput`], the coordinator's half: it hands out encoder
//!   surfaces, names the plane textures the renderer packs a frame into, and
//!   moves those planes into the surface on the GPU.
//! - [`HardwareEncodeSession`], the encode worker's half: it submits surfaces
//!   to the encoder and muxes the packets, beside an audio stream, into a
//!   transactional output file.
//!
//! A frame's surface moves `acquire -> begin -> (render) -> submit -> encode`, and
//! it is only reused once every GPU access and every codec reference to it has
//! been released. The backend owns that rule: dropping the host's last handle
//! is not, by itself, a promise that nothing still reads the image.
// A platform without a backend compiles the whole contract with nothing behind it, so the host's
// loop stays one loop on every platform.
#![cfg_attr(
    not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
    allow(dead_code, unused_variables, clippy::needless_pass_by_value)
)]

#[cfg(target_os = "windows")]
mod d3d12va;
#[cfg(target_os = "macos")]
mod videotoolbox;
#[cfg(target_os = "linux")]
mod vulkan;

use std::sync::atomic::{AtomicU64, Ordering};

use alpha2_media::{ColorMetadata, EncodeError, VideoRateControl};
use alpha2_time::{FrameRate, MediaTime};

#[cfg(all(test, target_os = "windows"))]
pub(crate) use d3d12va::encode_no_rate_control_frames;
#[cfg(target_os = "windows")]
pub(crate) use d3d12va::probe_d3d12va_rate_control;
#[cfg(target_os = "windows")]
pub use d3d12va::{
    D3D12EncodeProbe, D3D12RateControlOpen, D3D12RateControlRunner, D3D12TestEncodeRunner,
    D3D12VA_TEST_ENCODE_SIZE, d3d12va_encoder, open_d3d12va_rate_control, probe_d3d12va_encode,
    set_d3d12va_rate_control_runner, set_d3d12va_test_encode_runner,
};
#[cfg(target_os = "macos")]
pub use videotoolbox::{VideoToolboxEncodeProbe, probe_videotoolbox_encode};
#[cfg(target_os = "linux")]
pub(crate) use vulkan::probe_vulkan_rate_control;
#[cfg(target_os = "linux")]
pub use vulkan::{VulkanEncodeProbe, probe_vulkan_encode};

use crate::{EncodeStageTimings, NativeVideoCodec, NativeVideoEncoderSelection};

/// The native API a hardware encode session drives.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HardwareEncodeBackend {
    /// `FFmpeg`'s Vulkan video encoders on the renderer's Vulkan device.
    Vulkan,
    /// `FFmpeg`'s `VideoToolbox` encoders reading `IOSurface`s the renderer's
    /// `Metal` device draws into.
    VideoToolbox,
    /// `FFmpeg`'s D3D12 Video encoders on the renderer's D3D12 device.
    D3D12Va,
}

impl HardwareEncodeBackend {
    /// Stable identifier used by capability reports and export results.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vulkan => "vulkan",
            Self::VideoToolbox => "videotoolbox",
            Self::D3D12Va => "d3d12va",
        }
    }
}

/// How the pixels of an encoder-input surface are laid out.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EncoderInputLayout {
    /// 8-bit 4:2:0: a full-size luma plane and a half-size plane of
    /// interleaved chroma pairs.
    Nv12,
}

impl EncoderInputLayout {
    /// Stable identifier used by capability reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nv12 => "nv12",
        }
    }

    /// Bytes one visible frame of this layout holds, without padding.
    #[must_use]
    pub const fn frame_bytes(self, width: u32, height: u32) -> u64 {
        match self {
            Self::Nv12 => {
                let luma = width as u64 * height as u64;
                luma + luma / 2
            }
        }
    }
}

/// How a packed frame reaches the encoder's surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EncoderInputHandoff {
    /// The renderer packs into its own plane textures, and a GPU copy moves
    /// them into the surface. No pixel crosses the CPU.
    GpuCopy,
    /// The renderer packs straight into the surface's own planes: no plane
    /// ring and no copy.
    DirectTarget,
}

impl EncoderInputHandoff {
    /// Stable identifier used by capability reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GpuCopy => "gpu-copy",
            Self::DirectTarget => "direct-target",
        }
    }
}

/// What one hardware encoder accepts on this device, read from the device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HardwareEncodeCapability {
    /// The native API the session drives.
    pub backend: HardwareEncodeBackend,
    /// The `FFmpeg` encoder a session opens.
    pub encoder: &'static str,
    /// The codec it encodes.
    pub codec: NativeVideoCodec,
    /// Whether the encoder takes GPU frames only. Pinning such an encoder is
    /// asking for this path; an encoder the readback path can also feed
    /// (`h264_videotoolbox`) can be pinned on either.
    pub gpu_input_only: bool,
    /// The surface layout it takes.
    pub input_layout: EncoderInputLayout,
    /// How a packed frame reaches its surface.
    pub handoff: EncoderInputHandoff,
    /// Whether the encoder runs on the renderer's own device. A backend that
    /// would need a copy between devices is not offered as GPU-resident.
    pub shares_renderer_device: bool,
    /// Smallest accepted coded width and height.
    pub minimum: (u32, u32),
    /// Largest accepted coded width and height.
    pub maximum: (u32, u32),
    /// Granularity the output size is aligned up to before the limits apply.
    pub granularity: (u32, u32),
    /// References a P picture may use, when the device reports it.
    pub max_p_picture_l0_references: Option<u32>,
    /// Forward references a B picture may use; zero together with
    /// `max_l1_references` means no B pictures. `None` when the device does
    /// not report it.
    pub max_b_picture_l0_references: Option<u32>,
    /// Backward references a B picture may use, when the device reports it.
    pub max_l1_references: Option<u32>,
    /// The rate-control modes the device reports: `cbr`, `vbr`, `disabled`
    /// (a constant quantizer), and on D3D12 `qvbr`.
    pub rate_control_modes: Vec<&'static str>,
    /// Plane-texture pairs the renderer packs into, allocated for the whole
    /// session whatever the pipeline depth. Zero when it packs into the
    /// surfaces themselves.
    pub plane_ring_slots: usize,
    /// Surfaces the handoff may still hold after the host has let go of
    /// them: a copying backend keeps each surface it copied into until its
    /// slot comes round again. They are allocated on top of the frames the
    /// host holds, so a budget has to count them.
    pub handoff_retained_surfaces: usize,
    /// Surfaces the encoder itself still holds after it has accepted them,
    /// until it has consumed them. Also allocated on top of the host's.
    pub codec_retained_surfaces: usize,
}

impl HardwareEncodeCapability {
    /// Every surface the pool may hold beyond those the host has leased.
    #[must_use]
    pub const fn retained_surfaces(&self) -> usize {
        self.handoff_retained_surfaces
            .saturating_add(self.codec_retained_surfaces)
    }
}

impl HardwareEncodeCapability {
    /// The coded extent an output of this size is encoded at.
    #[must_use]
    pub const fn coded(&self, width: u32, height: u32) -> (u32, u32) {
        (
            align_up(width, self.granularity.0),
            align_up(height, self.granularity.1),
        )
    }

    /// Whether an output of this size can be encoded, and if not, why.
    ///
    /// # Errors
    /// Names the limit the size misses.
    pub fn accepts(&self, width: u32, height: u32) -> Result<(), String> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!(
                "a {width}x{height} output has no 4:2:0 chroma plane"
            ));
        }
        let coded = self.coded(width, height);
        if coded.0 < self.minimum.0
            || coded.1 < self.minimum.1
            || coded.0 > self.maximum.0
            || coded.1 > self.maximum.1
        {
            return Err(format!(
                "a {width}x{height} output codes as {}x{}, outside {}'s {}x{}..{}x{}",
                coded.0,
                coded.1,
                self.encoder,
                self.minimum.0,
                self.minimum.1,
                self.maximum.0,
                self.maximum.1
            ));
        }
        Ok(())
    }
}

const fn align_up(value: u32, granularity: u32) -> u32 {
    if granularity <= 1 {
        return value;
    }
    value.div_ceil(granularity).saturating_mul(granularity)
}

/// The renderer's device, as a hardware encoder can use it.
#[derive(Clone)]
pub struct HardwareEncodeDevice {
    #[cfg(target_os = "linux")]
    vulkan: alpha2_native_gpu::SharedVulkanContext,
    #[cfg(target_os = "macos")]
    metal: alpha2_native_gpu::SharedMetalContext,
    #[cfg(target_os = "windows")]
    d3d12: alpha2_native_gpu::SharedD3D12Context,
}

impl std::fmt::Debug for HardwareEncodeDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HardwareEncodeDevice")
            .finish_non_exhaustive()
    }
}

/// One export's encoder settings and output.
#[derive(Clone, Debug)]
pub struct HardwareEncodeRequest<'a> {
    /// Output width, which must be even.
    pub width: u32,
    /// Output height, which must be even.
    pub height: u32,
    /// The codec to encode, one the device's
    /// [`HardwareEncodeDevice::capability`] takes.
    pub codec: NativeVideoCodec,
    /// The constant frame rate the stream is timed in.
    pub frame_rate: FrameRate,
    /// The resolved rate control.
    pub rate_control: VideoRateControl,
    /// The color the pack encodes, which the stream is tagged with.
    pub color: ColorMetadata,
    /// Where the file goes, and what it does with a file there; a partial
    /// file sits beside it until it finishes.
    pub output: &'a crate::OutputTarget,
    /// The file format written there, which must offer `codec` and the audio
    /// codec.
    pub container: alpha2_media::Container,
    /// The audio stream muxed beside the video, if any.
    pub audio: Option<crate::AudioStreamConfig>,
}

/// A session's frame rate as `FFmpeg` takes it, numerator and denominator.
/// Either one past `i32` is refused, as the readback sinks refuse it, so the
/// export falls back to them and is refused there for the same reason
/// instead of the session quietly running at another rate.
pub(crate) fn session_frame_rate(rate: FrameRate) -> Result<(i32, i32), EncodeError> {
    match (
        i32::try_from(rate.numerator()),
        i32::try_from(rate.denominator()),
    ) {
        (Ok(numerator), Ok(denominator)) => Ok((numerator, denominator)),
        _ => Err(EncodeError::Adapter(format!(
            "frame rate {}/{} is out of range",
            rate.numerator(),
            rate.denominator()
        ))),
    }
}

/// Why a hardware session did not open.
#[derive(Debug)]
pub enum HardwareEncodeOpenError {
    /// The encoder refused before anything was written: another path can
    /// still take the export.
    Refused(String),
    /// The output was being created when this failed; the export has failed.
    Failed(EncodeError),
}

impl std::fmt::Display for HardwareEncodeOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => formatter.write_str(reason),
            Self::Failed(error) => write!(formatter, "{error}"),
        }
    }
}

impl HardwareEncodeDevice {
    /// The shared Vulkan device the renderer runs on.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub const fn vulkan(context: alpha2_native_gpu::SharedVulkanContext) -> Self {
        Self { vulkan: context }
    }

    /// The `Metal` device the renderer runs on.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub const fn metal(context: alpha2_native_gpu::SharedMetalContext) -> Self {
        Self { metal: context }
    }

    /// The shared D3D12 device the renderer runs on.
    #[cfg(target_os = "windows")]
    #[must_use]
    pub const fn d3d12(context: alpha2_native_gpu::SharedD3D12Context) -> Self {
        Self { d3d12: context }
    }

    /// What this device's hardware encoder accepts for `codec`.
    ///
    /// A device can encode one codec and not another, so each export asks
    /// about its own. Linux and macOS have one hardware encoder, for H.264.
    ///
    /// # Errors
    /// Says why this device has no usable hardware encoder for `codec`.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
        allow(clippy::unused_self)
    )]
    pub fn capability(&self, codec: NativeVideoCodec) -> Result<HardwareEncodeCapability, String> {
        #[cfg(target_os = "windows")]
        return d3d12va::capability(&self.d3d12, codec);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let capability = self.preferred_capability()?;
            if capability.codec == codec {
                Ok(capability)
            } else {
                Err(format!(
                    "{} encodes {}, not {}",
                    capability.encoder,
                    capability.codec.as_str(),
                    codec.as_str()
                ))
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err("this platform has no hardware encode backend yet".to_owned())
    }

    /// The capability a report leads with: on Windows the first of H.264,
    /// HEVC and AV1 this device encodes, elsewhere the one encoder there is.
    ///
    /// # Errors
    /// Says why this device has no usable hardware encoder at all.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
        allow(clippy::unused_self)
    )]
    pub fn preferred_capability(&self) -> Result<HardwareEncodeCapability, String> {
        #[cfg(target_os = "linux")]
        return vulkan::capability(&self.vulkan);
        #[cfg(target_os = "macos")]
        return videotoolbox::capability(&self.metal);
        #[cfg(target_os = "windows")]
        return d3d12va::preferred_capability(&self.d3d12);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        Err("this platform has no hardware encode backend yet".to_owned())
    }

    /// Opens a session writing `request.output` and its input half.
    ///
    /// Everything that can refuse does so before the output file exists.
    ///
    /// # Errors
    /// [`HardwareEncodeOpenError::Refused`] before any output exists,
    /// [`HardwareEncodeOpenError::Failed`] once creating it went wrong.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", target_os = "windows")),
        allow(clippy::unused_self)
    )]
    pub fn open(
        &self,
        request: &HardwareEncodeRequest<'_>,
    ) -> Result<(HardwareEncodeSession, HardwareEncodeInput), HardwareEncodeOpenError> {
        let generation = NEXT_POOL_GENERATION.fetch_add(1, Ordering::Relaxed);
        // These backends encode one codec; the D3D12VA one checks the codec it is asked for itself.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.capability(request.codec)
            .map_err(HardwareEncodeOpenError::Refused)?;
        #[cfg(target_os = "linux")]
        {
            let (session, input) = vulkan::open(&self.vulkan, request, generation)?;
            Ok((
                HardwareEncodeSession(SessionBackend::Vulkan(session)),
                HardwareEncodeInput {
                    backend: InputBackend::Vulkan(input),
                    generation,
                },
            ))
        }
        #[cfg(target_os = "macos")]
        {
            let (session, input) = videotoolbox::open(&self.metal, request, generation)?;
            Ok((
                HardwareEncodeSession(SessionBackend::VideoToolbox(session)),
                HardwareEncodeInput {
                    backend: InputBackend::VideoToolbox(input),
                    generation,
                },
            ))
        }
        #[cfg(target_os = "windows")]
        {
            let (session, input) = d3d12va::open(&self.d3d12, request, generation)?;
            Ok((
                HardwareEncodeSession(SessionBackend::D3D12(session)),
                HardwareEncodeInput {
                    backend: InputBackend::D3D12(input),
                    generation,
                },
            ))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = (request, generation);
            Err(HardwareEncodeOpenError::Refused(
                "this platform has no hardware encode backend yet".to_owned(),
            ))
        }
    }
}

/// Identifies the pool a surface came from, so a surface can never be
/// written or encoded by a session other than its own: after a cancel, a
/// resize, or a device loss, an old surface must not reach a new pool.
static NEXT_POOL_GENERATION: AtomicU64 = AtomicU64::new(1);

/// What one encoder-input surface is, independent of its backing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct EncoderSurfaceDescriptor {
    /// The backend whose pool allocated it.
    pub backend: HardwareEncodeBackend,
    /// The session's pool; see [`HardwareEncodeInput::acquire_surface`].
    pub pool_generation: u64,
    /// Its pixel layout.
    pub layout: EncoderInputLayout,
    /// The picture's size.
    pub visible: (u32, u32),
    /// The color its pixels are encoded in.
    pub color: ColorMetadata,
}

/// One encoder-input surface on loan from its session's pool.
///
/// Holding it keeps the backing alive; dropping it gives the backend's
/// reference back. The backend still decides when the backing is reusable.
pub struct EncoderSurface {
    descriptor: EncoderSurfaceDescriptor,
    backing: SurfaceBacking,
}

enum SurfaceBacking {
    #[cfg(target_os = "linux")]
    Vulkan(vulkan::VulkanEncoderFrame),
    #[cfg(target_os = "macos")]
    VideoToolbox(videotoolbox::VideoToolboxEncoderFrame),
    #[cfg(target_os = "windows")]
    D3D12(d3d12va::D3D12EncoderFrame),
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Unsupported(std::convert::Infallible),
}

impl EncoderSurface {
    /// What this surface is.
    #[must_use]
    pub const fn descriptor(&self) -> &EncoderSurfaceDescriptor {
        &self.descriptor
    }
}

/// A frame the coordinator has begun: its planes are the renderer's to write
/// until [`HardwareEncodeInput::submit`] takes it.
#[must_use = "a begun frame must be submitted, or its planes stay claimed"]
pub struct InputFrame {
    /// The plane pair of the copier's ring the frame is packed into.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    slot: usize,
    /// The surface whose own planes the frame is packed into.
    #[cfg(target_os = "macos")]
    pixel_buffer: usize,
}

/// Surface-pool counters for one session.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct EncoderPoolStats {
    /// Surfaces handed out.
    pub acquired: u64,
    /// Distinct backings the pool allocated to hand them out. Beyond the
    /// frames the host holds, this counts what the encoder and the GPU copy
    /// still referenced when the next surface was asked for.
    pub allocated: u64,
    /// Handoffs into a surface: a plane copy each on a copying backend.
    pub copy_operations: u64,
    /// The bytes those handoffs moved.
    pub copy_bytes: u64,
}

/// The coordinator's half of a hardware session.
pub struct HardwareEncodeInput {
    backend: InputBackend,
    generation: u64,
}

enum InputBackend {
    #[cfg(target_os = "linux")]
    Vulkan(vulkan::VulkanEncodeInput),
    #[cfg(target_os = "macos")]
    VideoToolbox(videotoolbox::VideoToolboxEncodeInput),
    #[cfg(target_os = "windows")]
    D3D12(d3d12va::D3D12EncodeInput),
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Unsupported(std::convert::Infallible),
}

impl HardwareEncodeInput {
    /// Takes one surface from the session's pool.
    ///
    /// The pool allocates on demand, so the number of backings it ever holds
    /// is the peak number outstanding; the caller bounds that.
    ///
    /// # Errors
    /// Returns an adapter failure when the pool cannot allocate.
    pub fn acquire_surface(&mut self) -> Result<EncoderSurface, EncodeError> {
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            InputBackend::Vulkan(input) => {
                let frame = input.acquire_frame()?;
                Ok(EncoderSurface {
                    descriptor: input.descriptor(self.generation),
                    backing: SurfaceBacking::Vulkan(frame),
                })
            }
            #[cfg(target_os = "macos")]
            InputBackend::VideoToolbox(input) => {
                let frame = input.acquire_frame()?;
                Ok(EncoderSurface {
                    descriptor: input.descriptor(self.generation),
                    backing: SurfaceBacking::VideoToolbox(frame),
                })
            }
            #[cfg(target_os = "windows")]
            InputBackend::D3D12(input) => {
                let frame = input.acquire_frame()?;
                Ok(EncoderSurface {
                    descriptor: input.descriptor(self.generation),
                    backing: SurfaceBacking::D3D12(frame),
                })
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            InputBackend::Unsupported(never) => match *never {},
        }
    }

    /// Begins a frame bound for `surface`: waits until the planes it will be
    /// packed into are free. Call it before recording the render that packs
    /// the frame, render into [`Self::planes`], and submit that render before
    /// [`Self::submit`].
    ///
    /// # Errors
    /// Returns an error for a surface from another pool, or when the wait
    /// fails.
    pub fn begin_frame(&mut self, surface: &EncoderSurface) -> Result<InputFrame, EncodeError> {
        self.check(surface)?;
        match &mut self.backend {
            #[cfg(target_os = "linux")]
            InputBackend::Vulkan(input) => input.begin_frame(),
            #[cfg(target_os = "macos")]
            InputBackend::VideoToolbox(input) => match &surface.backing {
                SurfaceBacking::VideoToolbox(backing) => input.begin_frame(backing),
            },
            #[cfg(target_os = "windows")]
            InputBackend::D3D12(input) => input.begin_frame(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            InputBackend::Unsupported(never) => match *never {},
        }
    }

    /// The luma and interleaved chroma textures `frame` is packed into.
    #[must_use]
    pub fn planes(&self, frame: &InputFrame) -> (&wgpu::Texture, &wgpu::Texture) {
        match &self.backend {
            #[cfg(target_os = "linux")]
            InputBackend::Vulkan(input) => input.planes(frame),
            #[cfg(target_os = "macos")]
            InputBackend::VideoToolbox(input) => input.planes(frame),
            #[cfg(target_os = "windows")]
            InputBackend::D3D12(input) => input.planes(frame),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            InputBackend::Unsupported(never) => match *never {},
        }
    }

    /// Moves `frame`'s packed planes into `surface` on the GPU. The encoder
    /// waits for that on the GPU too, so this does not.
    ///
    /// # Errors
    /// Returns an error for a surface from another pool, or when the handoff
    /// cannot be submitted.
    pub fn submit(
        &mut self,
        frame: InputFrame,
        surface: &mut EncoderSurface,
    ) -> Result<(), EncodeError> {
        self.check(surface)?;
        match (&mut self.backend, &mut surface.backing) {
            #[cfg(target_os = "linux")]
            (InputBackend::Vulkan(input), SurfaceBacking::Vulkan(backing)) => {
                input.submit(frame, backing)
            }
            #[cfg(target_os = "macos")]
            (InputBackend::VideoToolbox(input), SurfaceBacking::VideoToolbox(backing)) => {
                input.submit(frame, backing)
            }
            #[cfg(target_os = "windows")]
            (InputBackend::D3D12(input), SurfaceBacking::D3D12(backing)) => {
                input.submit(frame, backing)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            (InputBackend::Unsupported(never), _) => match *never {},
        }
    }

    /// Stamps `surface` with its presentation time.
    pub fn set_time(&self, surface: &mut EncoderSurface, time: MediaTime) {
        match (&self.backend, &mut surface.backing) {
            #[cfg(target_os = "linux")]
            (InputBackend::Vulkan(input), SurfaceBacking::Vulkan(backing)) => {
                backing.set_pts(input.timestamp(time));
            }
            #[cfg(target_os = "macos")]
            (InputBackend::VideoToolbox(input), SurfaceBacking::VideoToolbox(backing)) => {
                backing.set_pts(input.timestamp(time));
            }
            #[cfg(target_os = "windows")]
            (InputBackend::D3D12(input), SurfaceBacking::D3D12(backing)) => {
                backing.set_pts(input.timestamp(time));
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            (InputBackend::Unsupported(never), _) => match *never {},
        }
    }

    /// This session's pool counters so far.
    #[must_use]
    pub fn stats(&self) -> EncoderPoolStats {
        match &self.backend {
            #[cfg(target_os = "linux")]
            InputBackend::Vulkan(input) => input.stats(),
            #[cfg(target_os = "macos")]
            InputBackend::VideoToolbox(input) => input.stats(),
            #[cfg(target_os = "windows")]
            InputBackend::D3D12(input) => input.stats(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            InputBackend::Unsupported(never) => match *never {},
        }
    }

    /// Waits for every in-flight handoff and releases the input resources.
    ///
    /// # Errors
    /// On timeout or device loss the backend keeps what the GPU may still use
    /// rather than freeing it, and says so.
    pub fn finish(self) -> Result<(), EncodeError> {
        match self.backend {
            #[cfg(target_os = "linux")]
            InputBackend::Vulkan(input) => input.finish(),
            #[cfg(target_os = "macos")]
            InputBackend::VideoToolbox(input) => input.finish(),
            #[cfg(target_os = "windows")]
            InputBackend::D3D12(input) => input.finish(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            InputBackend::Unsupported(never) => match never {},
        }
    }

    fn check(&self, surface: &EncoderSurface) -> Result<(), EncodeError> {
        if surface.descriptor.pool_generation == self.generation {
            Ok(())
        } else {
            Err(EncodeError::Adapter(format!(
                "encoder surface from pool {} reached pool {}",
                surface.descriptor.pool_generation, self.generation
            )))
        }
    }
}

/// The encode worker's half of a hardware session.
pub struct HardwareEncodeSession(SessionBackend);

enum SessionBackend {
    #[cfg(target_os = "linux")]
    Vulkan(vulkan::VulkanEncodeSession),
    #[cfg(target_os = "macos")]
    VideoToolbox(videotoolbox::VideoToolboxEncodeSession),
    #[cfg(target_os = "windows")]
    D3D12(d3d12va::D3D12EncodeSession),
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    Unsupported(std::convert::Infallible),
}

impl HardwareEncodeSession {
    /// The encoder this session opened, reported the way every other export
    /// path reports the encoder it opened.
    #[must_use]
    pub fn selection(&self) -> &NativeVideoEncoderSelection {
        match &self.0 {
            #[cfg(target_os = "linux")]
            SessionBackend::Vulkan(session) => session.selection(),
            #[cfg(target_os = "macos")]
            SessionBackend::VideoToolbox(session) => session.selection(),
            #[cfg(target_os = "windows")]
            SessionBackend::D3D12(session) => session.selection(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            SessionBackend::Unsupported(never) => match *never {},
        }
    }

    /// Where this session's time has gone, split the way the software sinks
    /// split theirs.
    #[must_use]
    pub fn stage_timings(&self) -> EncodeStageTimings {
        match &self.0 {
            #[cfg(target_os = "linux")]
            SessionBackend::Vulkan(session) => session.stage_timings(),
            #[cfg(target_os = "macos")]
            SessionBackend::VideoToolbox(session) => session.stage_timings(),
            #[cfg(target_os = "windows")]
            SessionBackend::D3D12(session) => session.stage_timings(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            SessionBackend::Unsupported(never) => match *never {},
        }
    }

    /// Submits `surface` and muxes whatever packets it produced.
    ///
    /// # Errors
    /// Returns an adapter failure when the encoder rejects the frame or its
    /// packets cannot be written.
    pub fn encode(&mut self, surface: &EncoderSurface) -> Result<(), EncodeError> {
        match (&mut self.0, &surface.backing) {
            #[cfg(target_os = "linux")]
            (SessionBackend::Vulkan(session), SurfaceBacking::Vulkan(frame)) => {
                session.encode(surface.descriptor.pool_generation, frame)
            }
            #[cfg(target_os = "macos")]
            (SessionBackend::VideoToolbox(session), SurfaceBacking::VideoToolbox(frame)) => {
                session.encode(surface.descriptor.pool_generation, frame)
            }
            #[cfg(target_os = "windows")]
            (SessionBackend::D3D12(session), SurfaceBacking::D3D12(frame)) => {
                session.encode(surface.descriptor.pool_generation, frame)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            (SessionBackend::Unsupported(never), _) => match *never {},
        }
    }

    /// Buffers one audio block for the audio stream.
    ///
    /// # Errors
    /// Returns an adapter failure for a format change, a non-monotonic block,
    /// or a failed audio encode.
    pub fn write_audio(&mut self, block: &alpha2_media::AudioBlock) -> Result<(), EncodeError> {
        match &mut self.0 {
            #[cfg(target_os = "linux")]
            SessionBackend::Vulkan(session) => session.write_audio(block),
            #[cfg(target_os = "macos")]
            SessionBackend::VideoToolbox(session) => session.write_audio(block),
            #[cfg(target_os = "windows")]
            SessionBackend::D3D12(session) => session.write_audio(block),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            SessionBackend::Unsupported(never) => match *never {},
        }
    }

    /// Flushes the encoder and the audio, writes the trailer, and renames the
    /// output into place. Only this makes the output visible.
    ///
    /// # Errors
    /// Returns an adapter failure from the flush, the trailer, or the rename.
    pub fn finish(&mut self) -> Result<(), EncodeError> {
        match &mut self.0 {
            #[cfg(target_os = "linux")]
            SessionBackend::Vulkan(session) => session.finish(),
            #[cfg(target_os = "macos")]
            SessionBackend::VideoToolbox(session) => session.finish(),
            #[cfg(target_os = "windows")]
            SessionBackend::D3D12(session) => session.finish(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            SessionBackend::Unsupported(never) => match *never {},
        }
    }

    /// Discards the partial output.
    pub fn abort(&mut self) {
        match &mut self.0 {
            #[cfg(target_os = "linux")]
            SessionBackend::Vulkan(session) => session.abort(),
            #[cfg(target_os = "macos")]
            SessionBackend::VideoToolbox(session) => session.abort(),
            #[cfg(target_os = "windows")]
            SessionBackend::D3D12(session) => session.abort(),
            #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
            SessionBackend::Unsupported(never) => match *never {},
        }
    }
}
