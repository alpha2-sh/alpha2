//! D3D12 interop: the D3D12VA decode-surface GPU copy into renderer-owned
//! D3D12 textures, the copy of rendered planes into D3D12VA encoder frames,
//! and the device's D3D12 Video Encode capabilities.

use std::{
    error::Error,
    ffi::c_void,
    fmt,
    mem::ManuallyDrop,
    ptr::NonNull,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use alpha2_ffmpeg_bridge::{BridgeError, D3D12FrameLease, create_d3d12_device_context};
use alpha2_gpu::GpuContext;
use rsmpeg::avutil::AVHWDeviceContext;
use windows::{
    Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource},
    core::Interface,
};

use crate::{NativeGpuIdentity, NativePlaneLayout};

const DXGI_FORMAT_NV12: u32 = 103;
const DXGI_FORMAT_P010: u32 = 104;
/// The step a copy fails at when it runs on the GPU with no fence to say
/// when it ends.
const COPY_SUBMITTED_UNFENCED: i32 = crate::interop::ALPHA2_GPU_STEP_QUEUE_SIGNAL;

use crate::interop::Alpha2D3D12CopyInfo as D3D12CopyInfo;
use crate::interop::{
    Alpha2D3D12EncodeCaps as D3D12EncodeCaps, Alpha2D3D12EncodeCopy as D3D12EncodeCopy,
};

unsafe extern "C" {
    fn alpha2_native_gpu_d3d12_pool_create(device: *mut c_void) -> *mut c_void;
    fn alpha2_native_gpu_d3d12_pool_destroy(pool: *mut c_void);
    fn alpha2_native_gpu_d3d12_pool_trim(pool: *mut c_void) -> u64;
    #[cfg(test)]
    fn alpha2_native_gpu_d3d12_pool_counts(
        pool: *mut c_void,
        targets_made: *mut u32,
        slots_made: *mut u32,
        idle_targets: *mut u32,
        idle_slots: *mut u32,
    );
    fn alpha2_native_gpu_d3d12_copy(
        pool: *mut c_void,
        queue: *mut c_void,
        source: *mut c_void,
        decode_fence: *mut c_void,
        decode_value: u64,
        output_ticket: *mut *mut c_void,
        output: *mut D3D12CopyInfo,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_d3d12_copy_destroy(ticket: *mut c_void) -> i32;
    fn alpha2_native_gpu_d3d12_set_wait_timeout(milliseconds: u32);
    fn alpha2_native_gpu_d3d12_wait_timeout() -> u32;
    fn alpha2_native_gpu_d3d12_encode_copier_create(
        device: *mut c_void,
        output: *mut *mut c_void,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_d3d12_encode_copier_destroy(copier: *mut c_void, status: *mut i32) -> i32;
    fn alpha2_native_gpu_d3d12_encode_copier_slots() -> u32;
    fn alpha2_native_gpu_d3d12_encode_copier_await_slot(
        copier: *mut c_void,
        slot: *mut u32,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_d3d12_encode_copier_submit(
        copier: *mut c_void,
        queue: *mut c_void,
        copy: *const D3D12EncodeCopy,
        signal_value: *mut u64,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_d3d12_encode_caps(
        device: *mut c_void,
        codec: u32,
        caps: *mut D3D12EncodeCaps,
        status: *mut i32,
    ) -> i32;
}

/// Sets how long the C helpers wait on a fence before giving up.
pub(crate) fn set_wait_timeout(timeout: std::time::Duration) {
    let milliseconds = crate::win32_wait_milliseconds(timeout);
    // SAFETY: stores one integer the helpers read on each wait.
    unsafe { alpha2_native_gpu_d3d12_set_wait_timeout(milliseconds) };
}

/// How long a wait on D3D12 work may take before it is reported as a
/// timeout, as [`crate::set_gpu_wait_timeout`] set it. Work that waits
/// without a bound of its own (`FFmpeg`'s D3D12VA encoder waits `INFINITE`)
/// is bounded by this from outside.
#[must_use]
pub fn gpu_wait_timeout() -> std::time::Duration {
    // SAFETY: reads one integer the setter stores.
    std::time::Duration::from_millis(u64::from(unsafe { alpha2_native_gpu_d3d12_wait_timeout() }))
}

/// Calls `alpha2_native_gpu_d3d12_copy`, or in tests fails first where
/// [`inject`] asks.
///
/// # Safety
/// As for the helper: every pointer is live for the call, and the outputs are
/// writable.
#[allow(clippy::too_many_arguments)]
unsafe fn copy(
    pool: *mut c_void,
    queue: *mut c_void,
    source: *mut c_void,
    decode_fence: *mut c_void,
    decode_value: u64,
    ticket: &mut *mut c_void,
    output: &mut D3D12CopyInfo,
    status: &mut i32,
) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::fails(status) {
        return step;
    }
    // SAFETY: the caller's contract.
    unsafe {
        alpha2_native_gpu_d3d12_copy(
            pool,
            queue,
            source,
            decode_fence,
            decode_value,
            ticket,
            output,
            status,
        )
    }
}

/// Calls `alpha2_native_gpu_d3d12_encode_copier_submit`, or in tests fails
/// first where [`inject`] asks.
///
/// # Safety
/// As for the helper: the copier and queue are live, `copy` names live
/// resources of their device, and the outputs are writable.
unsafe fn encode_copy_submit(
    copier: *mut c_void,
    queue: *mut c_void,
    copy: &D3D12EncodeCopy,
    signalled: &mut u64,
    status: &mut i32,
) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::encode_copy_fails(status) {
        return step;
    }
    // SAFETY: the caller's contract.
    unsafe { alpha2_native_gpu_d3d12_encode_copier_submit(copier, queue, copy, signalled, status) }
}

/// Fails the next surface copy on this thread at a step a test names, as
/// only a device fault would, so tests reach the branches that handle it.
#[cfg(test)]
pub(crate) mod inject {
    use std::cell::Cell;

    thread_local! {
        static NEXT_COPY_FAILS_AT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
        static NEXT_ENCODE_COPY_FAILS_AT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    }

    /// Fails the next encode copy on this thread at `step` with a removed
    /// device, as [`fail_next_copy`] fails a decode copy.
    pub(crate) fn fail_next_encode_copy(step: i32) {
        NEXT_ENCODE_COPY_FAILS_AT.with(|next| next.set((step, 0x887A_0005_u32.cast_signed())));
    }

    /// Whether this encode copy is the one to fail; as [`fails`].
    pub(super) fn encode_copy_fails(status: &mut i32) -> Option<i32> {
        let (step, chosen) = NEXT_ENCODE_COPY_FAILS_AT.with(|next| next.replace((0, 0)));
        (step != 0).then(|| {
            *status = chosen;
            step
        })
    }

    /// Fails the next copy on this thread at `step` with a removed device.
    pub(crate) fn fail_next_copy(step: i32) {
        fail_next_copy_with(step, 0x887A_0005_u32.cast_signed());
    }

    /// Fails the next copy on this thread at `step` with the `HRESULT`
    /// `status`.
    pub(crate) fn fail_next_copy_with(step: i32, status: i32) {
        NEXT_COPY_FAILS_AT.with(|next| next.set((step, status)));
    }

    /// Whether this copy is the one to fail; a failure returns the status the
    /// test chose at the step it named.
    pub(super) fn fails(status: &mut i32) -> Option<i32> {
        let (step, chosen) = NEXT_COPY_FAILS_AT.with(|next| next.replace((0, 0)));
        (step != 0).then(|| {
            *status = chosen;
            step
        })
    }
}

/// Failure while binding D3D12VA to the renderer's D3D12 device.
#[derive(Debug)]
pub struct SharedD3D12Error(String);

impl fmt::Display for SharedD3D12Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SharedD3D12Error {}

/// One submitted copy, and the decode surface it reads.
///
/// The surface goes back to `FFmpeg` only once the copy is known to be over.
/// When the helper cannot establish that in time, it leaves its own objects in
/// use, and the surface is left with them: a decoder that reused it would
/// write under a copy still reading it.
struct CopyTicket {
    raw: NonNull<c_void>,
    source: ManuallyDrop<Arc<D3D12FrameLease>>,
}

// SAFETY: the ticket is uniquely owned and only destroyed, once, on drop; the copy it names
// synchronizes through its own fence.
unsafe impl Send for CopyTicket {}
// SAFETY: shared references never touch the ticket.
unsafe impl Sync for CopyTicket {}

impl Drop for CopyTicket {
    fn drop(&mut self) {
        // SAFETY: the pointer is the unique ticket returned by the copy helper.
        let released = unsafe { alpha2_native_gpu_d3d12_copy_destroy(self.raw.as_ptr()) } != 0;
        // SAFETY: taken once, here, and never read again.
        let source = unsafe { ManuallyDrop::take(&mut self.source) };
        if released {
            drop(source);
        } else {
            std::mem::forget(source);
        }
    }
}

/// The copy targets, command allocators and lists, and fences of one
/// renderer device, kept from one copy to the next (`c/d3d12.c`). A copy
/// still out keeps it, so it outlives the contexts that made it as long as
/// any copy does.
struct CopyPool(NonNull<c_void>);

// SAFETY: the C pool locks around everything it shares between copies; the pointer itself is
// only handed to the helpers and destroyed once, on drop.
unsafe impl Send for CopyPool {}
// SAFETY: as above.
unsafe impl Sync for CopyPool {}

impl Drop for CopyPool {
    fn drop(&mut self) {
        // SAFETY: the pool the constructor made, given up once.
        unsafe { alpha2_native_gpu_d3d12_pool_destroy(self.0.as_ptr()) };
    }
}

/// Renderer D3D12 device shared with `FFmpeg`'s D3D12VA decoder and encoders.
#[derive(Clone)]
pub struct SharedD3D12Context {
    gpu: GpuContext,
    ffmpeg_device: AVHWDeviceContext,
    identity: NativeGpuIdentity,
    copies: Arc<CopyPool>,
    /// Whether wgpu exposes NV12 plane views here, which only decode needs.
    decode_planes: bool,
    /// What the device's D3D12 Video Encode answered for each codec, asked
    /// once per context: the answer is the device's, and does not change.
    encode_support: Arc<[OnceLock<Result<D3D12EncodeExtent, String>>; 3]>,
}

impl fmt::Debug for SharedD3D12Context {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedD3D12Context")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// One copied bi-planar D3D12 resource exposed as two wgpu plane views.
#[derive(Debug)]
pub struct ImportedD3D12Frame {
    /// Plane-zero luma view.
    pub y: wgpu::TextureView,
    /// Plane-one interleaved chroma view.
    pub uv: wgpu::TextureView,
    /// Sample layout of the copied planes.
    pub layout: NativePlaneLayout,
    /// Native copied-resource width.
    pub width: u32,
    /// Native copied-resource height.
    pub height: u32,
    /// Copy, frame, and GPU-completion ownership.
    pub lease: Arc<D3D12CopyLease>,
}

/// Keeps the source surface and copied D3D12 resource live through sampling.
pub struct D3D12CopyLease {
    /// Holds the source surface until the native copy is known to be over.
    _ticket: CopyTicket,
    _texture: wgpu::Texture,
    gpu: GpuContext,
    fence: ID3D12Fence,
    fence_value: u64,
    staged: AtomicBool,
    submitted: AtomicBool,
    retained_bytes: u64,
}

impl fmt::Debug for D3D12CopyLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("D3D12CopyLease")
            .field("staged", &self.staged.load(Ordering::Acquire))
            .field("submitted", &self.submitted.load(Ordering::Acquire))
            .field("retained_bytes", &self.retained_bytes)
            .finish_non_exhaustive()
    }
}

impl D3D12CopyLease {
    /// Approximate bytes retained by the decode surface and its copy.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// Stages the GPU-copy completion fence for the next wgpu submission.
    pub fn prepare_submission(&self) {
        if self.staged.swap(true, Ordering::AcqRel) {
            return;
        }
        // SAFETY: this queue is always D3D12 for a successfully-created context.
        if let Some(queue) = unsafe { self.gpu.queue().as_hal::<wgpu_hal::api::Dx12>() } {
            queue.add_wait_fence(self.fence.clone(), self.fence_value);
        }
    }

    /// Marks the sampling submission as committed.
    pub fn submitted(&self) {
        self.submitted.store(true, Ordering::Release);
    }
}

impl Drop for D3D12CopyLease {
    fn drop(&mut self) {
        if !self.staged.load(Ordering::Acquire) || self.submitted.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: the queue belongs to this context and the fence is still retained.
        if let Some(queue) = unsafe { self.gpu.queue().as_hal::<wgpu_hal::api::Dx12>() } {
            queue.remove_wait_fence(&self.fence);
        }
    }
}

impl SharedD3D12Context {
    /// Binds an existing D3D12 renderer device to an `FFmpeg` D3D12VA context.
    ///
    /// An adapter without NV12 plane views is kept: only decode needs them,
    /// to show its copies, and the encode handoff copies raw. Whether it has
    /// them is [`Self::decode_planes`], and decode refuses on it.
    ///
    /// # Errors
    /// Returns an error for a non-D3D12 renderer or an `FFmpeg`
    /// device-context initialization failure.
    pub fn from_gpu(gpu: GpuContext) -> Result<Self, SharedD3D12Error> {
        let decode_planes = gpu
            .device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12);
        // SAFETY: the guard only borrows the renderer's live D3D12 device.
        let hal = unsafe { gpu.device().as_hal::<wgpu_hal::api::Dx12>() }
            .ok_or_else(|| SharedD3D12Error("renderer is not using D3D12".into()))?;
        let raw = hal.raw_device().as_raw();
        let identity = NativeGpuIdentity {
            physical_device: raw.addr() as u64,
            device: raw.addr() as u64,
            epoch: 1,
        };
        // SAFETY: `raw` is the renderer's live `ID3D12Device`, borrowed through `hal`, and the
        // context takes its own COM reference to it.
        let ffmpeg_device = unsafe { create_d3d12_device_context(raw.addr() as u64) }
            .map_err(|error| SharedD3D12Error(error.to_string()))?;
        // SAFETY: as above; the pool takes its own reference to the device.
        let copies = NonNull::new(unsafe { alpha2_native_gpu_d3d12_pool_create(raw) })
            .map(CopyPool)
            .ok_or_else(|| SharedD3D12Error("out of memory for the D3D12 copy pool".into()))?;
        drop(hal);
        Ok(Self {
            gpu,
            ffmpeg_device,
            identity,
            copies: Arc::new(copies),
            decode_planes,
            encode_support: Arc::new([OnceLock::new(), OnceLock::new(), OnceLock::new()]),
        })
    }

    /// [`Self::from_gpu`] for a device that must hardware-decode: it also
    /// refuses an adapter without NV12 plane views.
    ///
    /// # Errors
    /// Returns what [`Self::from_gpu`] does, or that the adapter has no NV12
    /// plane views.
    pub fn for_decode(gpu: GpuContext) -> Result<Self, SharedD3D12Error> {
        let shared = Self::from_gpu(gpu)?;
        if !shared.decode_planes {
            return Err(SharedD3D12Error(
                "D3D12 adapter does not expose NV12 plane views".into(),
            ));
        }
        Ok(shared)
    }

    /// Whether the renderer can show decoded surfaces here: wgpu exposes NV12
    /// plane views on this adapter. Decode refuses without them; encode does
    /// not need them.
    #[must_use]
    pub const fn decode_planes(&self) -> bool {
        self.decode_planes
    }

    /// What this device's D3D12 Video Encode accepts for `codec` with NV12
    /// input, or why it encodes none: the adapter, the codec and the check
    /// that refused, with its `HRESULT`.
    ///
    /// Asked once per context and codec, of the device itself, in the order
    /// `FFmpeg`'s D3D12VA encoder asks as it opens. A device that answers here
    /// can still fail when a session opens, which only an encode shows.
    ///
    /// # Errors
    /// Returns the reason the device does not encode `codec`.
    pub fn encode_support(&self, codec: D3D12EncodeCodec) -> Result<D3D12EncodeExtent, String> {
        self.encode_support[codec as usize]
            .get_or_init(|| query_encode_support(&self.gpu, codec))
            .clone()
    }

    /// The DXGI adapter the renderer's device is on, as
    /// [`d3d12_adapter_identity`] names it.
    #[must_use]
    pub fn adapter(&self) -> Option<D3D12AdapterIdentity> {
        d3d12_adapter_identity(self.gpu.adapter())
    }

    /// Renderer context backed by the shared D3D12 device.
    #[must_use]
    pub fn gpu(&self) -> GpuContext {
        self.gpu.clone()
    }

    /// `FFmpeg` device reference for one decoder session.
    #[must_use]
    pub fn ffmpeg_device(&self) -> AVHWDeviceContext {
        self.ffmpeg_device.clone()
    }

    /// Whether this device can present 10-bit decoded planes.
    #[must_use]
    pub fn supports_ten_bit(&self) -> bool {
        self.gpu
            .device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_P010)
    }

    /// Raw-device cache identity.
    #[must_use]
    pub const fn identity(&self) -> NativeGpuIdentity {
        self.identity
    }

    /// Frees the copy targets kept idle for later copies and returns their
    /// bytes. Copies still held keep theirs and give them back as before.
    ///
    /// A resident host calls this with the renderer's own release before an
    /// export, so the frame sizes a previous export showed do not stay
    /// allocated through this one.
    #[must_use]
    pub fn release_idle_copies(&self) -> u64 {
        // SAFETY: the context's live pool; the helper locks around what copies share.
        unsafe { alpha2_native_gpu_d3d12_pool_trim(self.copies.0.as_ptr()) }
    }

    /// Copies an NV12 or P010 decode surface on GPU and imports the copy.
    ///
    /// The copy isolates `FFmpeg`'s DPB/resource-state ownership from wgpu. The
    /// returned lease stages a D3D12 fence wait before the sampling submit.
    ///
    /// # Errors
    /// Returns an error for device/format mismatch or copy/import failure.
    #[allow(clippy::too_many_lines)]
    pub fn copy_surface(
        &self,
        frame: Arc<D3D12FrameLease>,
    ) -> Result<ImportedD3D12Frame, BridgeError> {
        if !self.decode_planes {
            return Err(BridgeError::InvalidInput(
                "D3D12 adapter does not expose NV12 plane views",
            ));
        }
        let source = frame.info()?;
        if source.device != self.identity.device {
            return Err(BridgeError::InvalidInput("D3D12 frame device mismatch"));
        }
        let layout = match (source.software_format, source.dxgi_format) {
            (rsmpeg::ffi::AV_PIX_FMT_NV12, DXGI_FORMAT_NV12) => NativePlaneLayout::Nv12,
            (rsmpeg::ffi::AV_PIX_FMT_P010LE, DXGI_FORMAT_P010) => NativePlaneLayout::P010,
            _ => {
                return Err(BridgeError::InvalidInput("D3D12 frame is not NV12 or P010"));
            }
        };
        if layout == NativePlaneLayout::P010
            && !self
                .gpu
                .device()
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_P010)
        {
            return Err(BridgeError::InvalidInput(
                "D3D12 adapter does not expose P010 plane views",
            ));
        }
        // SAFETY: the guard borrows the live D3D12 queue.
        let queue = unsafe { self.gpu.queue().as_hal::<wgpu_hal::api::Dx12>() }
            .ok_or(BridgeError::InvalidInput("renderer queue is not D3D12"))?;
        let mut ticket = std::ptr::null_mut();
        let mut copied = D3D12CopyInfo::default();
        let mut status = 0;
        let source_resource = raw_pointer(source.resource, "invalid D3D12 resource pointer")?;
        let source_fence = raw_pointer(source.fence, "invalid D3D12 fence pointer")?;
        // SAFETY: the frame retains source/fence, the pool its device, the guard the queue, and the
        // outputs are writable for this synchronous setup call.
        let step = unsafe {
            copy(
                self.copies.0.as_ptr(),
                queue.as_raw().as_raw(),
                source_resource,
                source_fence,
                source.fence_value,
                &mut ticket,
                &mut copied,
                &mut status,
            )
        };
        drop(queue);
        if step != 0 {
            if step == COPY_SUBMITTED_UNFENCED {
                // The GPU may be reading the surface for as long as it takes; it must not go back
                // to the decoder.
                std::mem::forget(frame);
            }
            return Err(crate::status::helper_failure(
                alpha2_ffmpeg_bridge::NativeGpuBackend::D3D12,
                "surface copy",
                step,
                status,
            ));
        }
        let ticket = CopyTicket {
            raw: NonNull::new(ticket)
                .ok_or(BridgeError::InvalidInput("D3D12 copy returned no ticket"))?,
            source: ManuallyDrop::new(frame),
        };
        let raw_resource = raw_pointer(copied.resource, "invalid copied resource pointer")?;
        let raw_fence = raw_pointer(copied.fence, "invalid copy fence pointer")?;
        // SAFETY: the ticket owns both COM interfaces; clone adds independent references.
        let resource = unsafe { ID3D12Resource::from_raw_borrowed(&raw_resource) }
            .ok_or(BridgeError::InvalidInput("D3D12 copy returned no resource"))?
            .clone();
        // SAFETY: same ownership argument as `resource`.
        let fence = unsafe { ID3D12Fence::from_raw_borrowed(&raw_fence) }
            .ok_or(BridgeError::InvalidInput("D3D12 copy returned no fence"))?
            .clone();
        let size = wgpu::Extent3d {
            width: copied.width,
            height: copied.height,
            depth_or_array_layers: 1,
        };
        let (texture_format, luma_format, chroma_format) = match layout {
            NativePlaneLayout::P010 => (
                wgpu::TextureFormat::P010,
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
            ),
            _ => (
                wgpu::TextureFormat::NV12,
                wgpu::TextureFormat::R8Unorm,
                wgpu::TextureFormat::Rg8Unorm,
            ),
        };
        // SAFETY: this is a single-mip bi-planar texture created on the renderer device.
        let hal_texture = unsafe {
            wgpu_hal::dx12::Device::texture_from_raw(
                resource,
                texture_format,
                wgpu::TextureDimension::D2,
                size,
                1,
                1,
            )
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some("alpha2 D3D12VA decode copy"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: texture_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: the copy returns the resource to COMMON; D3D12 implicitly promotes it for the
        // first shader read, matching RESOURCE tracking.
        let texture = unsafe {
            self.gpu
                .device()
                .create_texture_from_hal::<wgpu_hal::api::Dx12>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::RESOURCE,
                )
        };
        let y = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(luma_format),
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let uv = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(chroma_format),
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });
        // The decode surface and its copy are both resident, so this path charges twice one frame's
        // bytes.
        let bytes = u64::from(copied.width)
            .saturating_mul(u64::from(copied.height))
            .saturating_mul(layout.bytes_per_pixel_numerator());
        let lease = Arc::new(D3D12CopyLease {
            _ticket: ticket,
            _texture: texture,
            gpu: self.gpu.clone(),
            fence,
            fence_value: copied.fence_value,
            staged: AtomicBool::new(false),
            submitted: AtomicBool::new(false),
            retained_bytes: bytes,
        });
        Ok(ImportedD3D12Frame {
            y,
            uv,
            layout,
            width: copied.width,
            height: copied.height,
            lease,
        })
    }
}

/// The error for a D3D12 helper's `operation` that failed at `step` with the
/// `HRESULT` `status`.
fn helper_error(operation: &'static str, step: i32, status: i32) -> BridgeError {
    crate::status::helper_failure(
        alpha2_ffmpeg_bridge::NativeGpuBackend::D3D12,
        operation,
        step,
        status,
    )
}

/// Which DXGI adapter a `wgpu` D3D12 adapter is, as another process can find
/// it again.
///
/// The LUID is the one name an adapter keeps across processes and
/// enumerations until the machine restarts: an enumeration position moves
/// when an adapter is added, and two adapters can share a name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct D3D12AdapterIdentity {
    /// `DXGI_ADAPTER_DESC1::AdapterLuid`, the high part in the upper 32 bits.
    pub luid: u64,
    /// Whether DXGI flags it `DXGI_ADAPTER_FLAG_SOFTWARE`: WARP, the
    /// Microsoft Basic Render Driver.
    pub software: bool,
}

/// The DXGI identity of `adapter`, or `None` for an adapter on another
/// backend than D3D12, or one DXGI does not describe.
#[must_use]
pub fn d3d12_adapter_identity(adapter: &wgpu::Adapter) -> Option<D3D12AdapterIdentity> {
    // SAFETY: the guard only borrows the live adapter for the description.
    let hal = unsafe { adapter.as_hal::<wgpu_hal::api::Dx12>() }?;
    // SAFETY: `GetDesc1` only fills a description of the adapter.
    let description = unsafe { hal.raw_adapter().GetDesc1() }.ok()?;
    let luid = (u64::from(description.AdapterLuid.HighPart.cast_unsigned()) << 32)
        | u64::from(description.AdapterLuid.LowPart);
    let software = description.Flags
        & windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE
            .0
            .cast_unsigned()
        != 0;
    Some(D3D12AdapterIdentity { luid, software })
}

/// A codec D3D12 Video Encode is asked about, at the profile `FFmpeg`'s
/// D3D12VA encoder opens for 8-bit 4:2:0 input.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum D3D12EncodeCodec {
    /// H.264 Main, as `h264_d3d12va` opens it.
    H264,
    /// HEVC Main, as `hevc_d3d12va` opens it.
    Hevc,
    /// AV1 Main, as `av1_d3d12va` opens it.
    Av1,
}

impl D3D12EncodeCodec {
    /// The codec's name, as the host's export settings spell it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    /// The profile the limits are for.
    #[must_use]
    pub const fn profile(self) -> &'static str {
        match self {
            // H.264 too: `FFmpeg` 8.1.3's `h264_d3d12va` writes a High stream no decoder reads.
            Self::H264 | Self::Hevc | Self::Av1 => "main",
        }
    }

    const fn abi(self) -> u32 {
        match self {
            Self::H264 => crate::interop::ALPHA2_D3D12_ENCODE_H264_MAIN,
            Self::Hevc => crate::interop::ALPHA2_D3D12_ENCODE_HEVC_MAIN,
            Self::Av1 => crate::interop::ALPHA2_D3D12_ENCODE_AV1_MAIN,
        }
    }
}

/// Coded extents and coding limits one D3D12 Video Encode codec has on a
/// device: the D3D12 counterpart of the Vulkan `VideoEncodeExtent`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct D3D12EncodeExtent {
    /// The codec these are the limits of.
    pub codec: D3D12EncodeCodec,
    /// Its profile: `main` for every codec.
    pub profile: &'static str,
    /// Smallest accepted width and height, in samples
    /// (`D3D12_FEATURE_VIDEO_ENCODER_OUTPUT_RESOLUTION`'s
    /// `MinResolutionSupported`).
    pub minimum: (u32, u32),
    /// Largest accepted width and height, in samples.
    pub maximum: (u32, u32),
    /// `Resolution{Width,Height}MultipleRequirement`: the multiple the coded
    /// extent is aligned up to before the limits apply.
    pub granularity: (u32, u32),
    /// How many references a P picture may use.
    pub max_p_picture_l0_references: u32,
    /// The forward references a B picture may use. Zero, together with
    /// `max_l1_references`, means the device encodes no B pictures; AV1
    /// reports its references all as P references.
    pub max_b_picture_l0_references: u32,
    /// The backward references a B picture may use.
    pub max_l1_references: u32,
    /// The rate-control modes the device encodes with, in Vulkan's spelling
    /// where it has one: `disabled` (constant QP), `cbr`, `vbr`, and `qvbr`.
    /// A mode is here only when the device accepts the whole configuration
    /// `FFmpeg` opens with it, not just the mode on its own.
    pub rate_control_modes: Vec<&'static str>,
    /// The modes the device lists but refuses in that whole configuration,
    /// each with the `HRESULT` it refused with (0 for a plain "not
    /// supported"). An export must not ask for one of these.
    pub refused_rate_control_modes: Vec<(&'static str, i32)>,
}

impl D3D12EncodeExtent {
    /// Whether the device can encode B pictures at all.
    #[must_use]
    pub const fn supports_b_pictures(&self) -> bool {
        self.max_b_picture_l0_references > 0 || self.max_l1_references > 0
    }

    /// Whether an output of this size reaches the encoder's coded limits.
    #[must_use]
    pub const fn accepts(&self, width: u32, height: u32) -> bool {
        let coded = self.coded(width, height);
        coded.0 >= self.minimum.0
            && coded.1 >= self.minimum.1
            && coded.0 <= self.maximum.0
            && coded.1 <= self.maximum.1
    }

    /// The coded extent an output of this size is encoded at.
    #[must_use]
    pub const fn coded(&self, width: u32, height: u32) -> (u32, u32) {
        (
            align_up(width, self.granularity.0),
            align_up(height, self.granularity.1),
        )
    }
}

const fn align_up(value: u32, granularity: u32) -> u32 {
    if granularity <= 1 {
        return value;
    }
    value.div_ceil(granularity).saturating_mul(granularity)
}

/// What the check `alpha2_native_gpu_d3d12_encode_caps` stopped at is
/// called in a reason.
const fn encode_check_name(check: u32) -> &'static str {
    use crate::interop::{
        ALPHA2_D3D12_ENCODE_CHECK_CODEC, ALPHA2_D3D12_ENCODE_CHECK_CONFIGURATION,
        ALPHA2_D3D12_ENCODE_CHECK_INPUT_FORMAT, ALPHA2_D3D12_ENCODE_CHECK_PICTURE_CONTROL,
        ALPHA2_D3D12_ENCODE_CHECK_PROFILE_LEVEL, ALPHA2_D3D12_ENCODE_CHECK_RATE_CONTROL,
        ALPHA2_D3D12_ENCODE_CHECK_RESOLUTION, ALPHA2_D3D12_ENCODE_CHECK_SUPPORT,
        ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE,
    };
    match check {
        ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE => "ID3D12VideoDevice3",
        ALPHA2_D3D12_ENCODE_CHECK_CODEC => "codec",
        ALPHA2_D3D12_ENCODE_CHECK_PROFILE_LEVEL => "profile and level",
        ALPHA2_D3D12_ENCODE_CHECK_INPUT_FORMAT => "NV12 input",
        ALPHA2_D3D12_ENCODE_CHECK_RATE_CONTROL => "rate-control mode",
        ALPHA2_D3D12_ENCODE_CHECK_RESOLUTION => "output resolution",
        ALPHA2_D3D12_ENCODE_CHECK_PICTURE_CONTROL => "picture control",
        ALPHA2_D3D12_ENCODE_CHECK_CONFIGURATION => "codec configuration",
        ALPHA2_D3D12_ENCODE_CHECK_SUPPORT => "D3D12_FEATURE_VIDEO_ENCODER_SUPPORT",
        _ => "an unknown",
    }
}

/// The rate-control modes of `Alpha2D3D12EncodeCaps`, in the order of its
/// `rate_control_status`, with the names the extent reports them by.
const RATE_CONTROL_MODES: [(u32, &str); 4] = [
    (crate::interop::ALPHA2_D3D12_RATE_CONTROL_CQP, "disabled"),
    (crate::interop::ALPHA2_D3D12_RATE_CONTROL_CBR, "cbr"),
    (crate::interop::ALPHA2_D3D12_RATE_CONTROL_VBR, "vbr"),
    (crate::interop::ALPHA2_D3D12_RATE_CONTROL_QVBR, "qvbr"),
];

/// What a status from the capability query says, for a reason.
fn encode_status(status: i32) -> String {
    if status == 0 {
        "not supported".to_owned()
    } else {
        format!("HRESULT {status:#010X}")
    }
}

/// The reason `adapter` gives for not encoding `codec`, from the check its
/// D3D12 Video Encode refused at.
///
/// These are the user-facing reasons of the machine classes the encode path
/// refuses on: no `ID3D12VideoDevice3` (an OS or driver without D3D12 Video
/// Encode), no encoder for the codec, and a device that takes every part of
/// the configuration `FFmpeg` opens with but not the whole, for any of its
/// rate-control modes.
fn encode_refusal(adapter: &str, codec: D3D12EncodeCodec, caps: &D3D12EncodeCaps) -> String {
    let (name, profile) = (codec.name(), codec.profile());
    let check = caps.refused_check;
    let status = caps.refused_status;
    if check == crate::interop::ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE {
        return format!(
            "{adapter}: ID3D12VideoDevice3 unavailable ({})",
            encode_status(status)
        );
    }
    if check == crate::interop::ALPHA2_D3D12_ENCODE_CHECK_CODEC && status == 0 {
        return format!("{adapter} reports no D3D12 video encoder for {name}");
    }
    let detail = if check == crate::interop::ALPHA2_D3D12_ENCODE_CHECK_SUPPORT {
        // Each mode the device listed was refused; say how.
        let modes: Vec<String> = RATE_CONTROL_MODES
            .iter()
            .zip(caps.rate_control_status)
            .filter(|((bit, _), _)| caps.refused_rate_control_modes & bit != 0)
            .map(|(&(_, mode), status)| format!("{mode}: {}", encode_status(status)))
            .collect();
        let flags = caps.validation_flags;
        let flags = if flags == 0 {
            String::new()
        } else {
            format!("; validation flags {flags:#X}")
        };
        format!("{}{flags}", modes.join(", "))
    } else {
        encode_status(status)
    };
    format!(
        "{adapter}: D3D12 Video Encode refuses {name} {profile} at the {} check ({detail})",
        encode_check_name(check)
    )
}

/// Asks `gpu`'s D3D12 device what its D3D12 Video Encode accepts for `codec`.
fn query_encode_support(
    gpu: &GpuContext,
    codec: D3D12EncodeCodec,
) -> Result<D3D12EncodeExtent, String> {
    let adapter = gpu.capabilities().adapter_name.clone();
    // SAFETY: the guard only borrows the renderer's live D3D12 device for the query.
    let hal = unsafe { gpu.device().as_hal::<wgpu_hal::api::Dx12>() }
        .ok_or_else(|| format!("{adapter}: the renderer is not using D3D12"))?;
    let mut caps = D3D12EncodeCaps::default();
    let mut status = 0;
    // SAFETY: the device is live for the call and `caps` is writable; the helper only queries.
    let step = unsafe {
        alpha2_native_gpu_d3d12_encode_caps(
            hal.raw_device().as_raw(),
            codec.abi(),
            &raw mut caps,
            &raw mut status,
        )
    };
    drop(hal);
    if step != 0 {
        return Err(format!(
            "{adapter}: {}",
            helper_error("encode capability query", step, status)
        ));
    }
    if caps.supported == 0 {
        return Err(encode_refusal(&adapter, codec, &caps));
    }
    let modes = RATE_CONTROL_MODES;
    Ok(D3D12EncodeExtent {
        codec,
        profile: codec.profile(),
        minimum: (caps.min_width, caps.min_height),
        maximum: (caps.max_width, caps.max_height),
        granularity: (caps.width_multiple.max(1), caps.height_multiple.max(1)),
        max_p_picture_l0_references: caps.max_p_l0_references,
        max_b_picture_l0_references: caps.max_b_l0_references,
        max_l1_references: caps.max_l1_references,
        rate_control_modes: modes
            .iter()
            .filter(|(bit, _)| caps.rate_control_modes & bit != 0)
            .map(|&(_, name)| name)
            .collect(),
        refused_rate_control_modes: modes
            .iter()
            .zip(caps.rate_control_status)
            .filter(|((bit, _), _)| caps.refused_rate_control_modes & bit != 0)
            .map(|(&(_, name), status)| (name, status))
            .collect(),
    })
}

/// Reusable NV12 handoff from the renderer's plane textures to D3D12VA
/// encoder frames: the D3D12 counterpart of the Vulkan `VulkanEncodeCopier`.
///
/// Owns a ring of command allocators, lists and fences (`c/d3d12_encode.c`),
/// so a frame costs one `ExecuteCommandLists` and no CPU wait. One per export.
///
/// The copy is ordered after the render that packed the planes by being
/// submitted after it on the same queue, not by waiting for a fence that
/// render's submission signals: a submission wgpu rejects signals nothing, and
/// a copy waiting for it would never run. It waits on the GPU only for the
/// frame's own fence (the encoder's last read of the texture), and signals it
/// once the texture holds the copy, which `FFmpeg`'s encode queue waits for.
/// `ID3D12CommandQueue` is free-threaded, so unlike Vulkan's this takes no
/// queue lock.
pub struct D3D12EncodeCopier {
    copier: *mut c_void,
    retained: Option<Box<RetainedEncodeWork>>,
    close_failed: bool,
}

/// Raw D3D12 submissions are invisible to wgpu's resource tracking. Keep
/// both ends of every copy and their device alive until its fence passes.
struct RetainedEncodeWork {
    copies: Vec<Option<EncodeCopyInputs>>,
    context: SharedD3D12Context,
    #[cfg(test)]
    lifetime_probe: Option<Arc<()>>,
}

struct EncodeCopyInputs {
    _luma: wgpu::Texture,
    _chroma: wgpu::Texture,
    _frame: rsmpeg::avutil::AVFrame,
}

// SAFETY: the C object is only reached through `&mut self`, and the raw pointer is owned solely by
// this value, which destroys it once.
unsafe impl Send for D3D12EncodeCopier {}

impl fmt::Debug for D3D12EncodeCopier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("D3D12EncodeCopier")
            .finish_non_exhaustive()
    }
}

impl Drop for D3D12EncodeCopier {
    fn drop(&mut self) {
        if !self.copier.is_null() && !self.close_failed {
            let _ = self.try_close();
        }
        if !self.copier.is_null() {
            // The C helper did not finish: leaking only its handle would let Rust free what its
            // copies still use, so keep the entire ownership graph, without waiting twice.
            std::mem::forget(self.retained.take());
        }
    }
}

impl D3D12EncodeCopier {
    /// Creates the handoff for one export on `context`'s device.
    ///
    /// # Errors
    /// Returns an error when the command allocators, lists or fences cannot
    /// be created.
    pub fn new(context: &SharedD3D12Context) -> Result<Self, BridgeError> {
        let gpu = context.gpu();
        // SAFETY: this context is always D3D12; the guard only borrows the device for the call.
        let hal = unsafe { gpu.device().as_hal::<wgpu_hal::api::Dx12>() }
            .ok_or(BridgeError::InvalidInput("renderer is not D3D12"))?;
        let mut copier = std::ptr::null_mut();
        let mut status = 0;
        // SAFETY: the device is this renderer's live `ID3D12Device`, which the copier takes its own
        // reference to, and the outputs are valid locals.
        let step = unsafe {
            alpha2_native_gpu_d3d12_encode_copier_create(
                hal.raw_device().as_raw(),
                &raw mut copier,
                &raw mut status,
            )
        };
        drop(hal);
        if step != 0 {
            return Err(helper_error("encode copier creation", step, status));
        }
        Ok(Self {
            copier,
            retained: Some(Box::new(RetainedEncodeWork {
                copies: (0..Self::slots()).map(|_| None).collect(),
                context: context.clone(),
                #[cfg(test)]
                lifetime_probe: None,
            })),
            close_failed: false,
        })
    }

    /// Plane-texture pairs a caller must cycle through to feed this copier.
    ///
    /// A caller that hands the encode to another thread runs ahead of the
    /// copies, so the pairs have to be a ring as deep as the slot ring this
    /// copier keeps.
    #[must_use]
    pub fn slots() -> usize {
        // SAFETY: a constant read out of the helper; it touches no state.
        unsafe { alpha2_native_gpu_d3d12_encode_copier_slots() as usize }
    }

    /// Waits until the slot the next copy will use is free, and names it.
    ///
    /// Call before recording the render that writes the plane textures, and
    /// render into the pair this returns. The wait is on the fence of the copy
    /// four submissions ago, so in a pipeline that is keeping up it is free.
    /// What that copy kept alive is released here.
    ///
    /// # Errors
    /// Returns an error when the fence wait times out or fails, or a reset
    /// fails.
    pub fn await_slot(&mut self) -> Result<usize, BridgeError> {
        let (mut slot, mut status) = (0_u32, 0);
        // SAFETY: the copier is live and the output pointers are valid locals.
        let step = unsafe {
            alpha2_native_gpu_d3d12_encode_copier_await_slot(
                self.copier,
                &raw mut slot,
                &raw mut status,
            )
        };
        if step != 0 {
            return Err(helper_error("encode copy slot", step, status));
        }
        if let Some(retained) = self.retained.as_mut() {
            retained.copies[slot as usize] = None;
        }
        Ok(slot as usize)
    }

    /// Copies a renderer-packed NV12 pair into a D3D12VA encoder frame's
    /// texture, and records on the frame the fence value the encoder must wait
    /// for.
    ///
    /// The submission that wrote these textures must already be on the
    /// renderer's queue: the copy is ordered after it by queue order alone.
    /// When that submission was rejected the copy still runs, over whatever
    /// the planes held, and the frame is the caller's to fail.
    ///
    /// # Errors
    /// Returns an error when the frame is not one NV12 texture on this device,
    /// the textures are not D3D12 planes of the size, or the submission fails.
    /// A failure after the copy reached the GPU (the step
    /// `ALPHA2_GPU_STEP_QUEUE_SIGNAL`) keeps the frame and the planes for the
    /// rest of the process: nothing then says when the copy ends, and neither
    /// may go back to its pool.
    pub fn submit(
        &mut self,
        frame: &mut alpha2_ffmpeg_bridge::D3D12EncodeFrameAccess<'_>,
        luma: &wgpu::Texture,
        chroma: &wgpu::Texture,
        size: (u32, u32),
    ) -> Result<(), BridgeError> {
        let slot = self.await_slot()?;
        let target = *frame.info();
        let retained = self
            .retained
            .as_mut()
            .ok_or(BridgeError::InvalidInput("invalid encode copier slot"))?;
        if target.device != retained.context.identity.device {
            return Err(BridgeError::InvalidInput("D3D12 frame device mismatch"));
        }
        let inputs = EncodeCopyInputs {
            _luma: luma.clone(),
            _chroma: chroma.clone(),
            _frame: frame.retain_frame()?,
        };
        // SAFETY: both textures belong to this D3D12 device and the pointers are only read; wgpu
        // keeps owning and destroying them, and `inputs` keeps them alive past the copy.
        let luma_resource = unsafe { luma.as_hal::<wgpu_hal::api::Dx12>() }
            .map(|texture| unsafe { texture.raw_resource() }.as_raw().addr() as u64)
            .ok_or(BridgeError::InvalidInput("luma plane is not D3D12"))?;
        // SAFETY: as above.
        let chroma_resource = unsafe { chroma.as_hal::<wgpu_hal::api::Dx12>() }
            .map(|texture| unsafe { texture.raw_resource() }.as_raw().addr() as u64)
            .ok_or(BridgeError::InvalidInput("chroma plane is not D3D12"))?;
        let copy = D3D12EncodeCopy {
            luma: luma_resource,
            chroma: chroma_resource,
            destination: target.resource,
            frame_fence: target.fence,
            frame_value: target.fence_value,
            width: size.0,
            height: size.1,
        };
        let gpu = retained.context.gpu();
        // SAFETY: the guard borrows the renderer's live D3D12 queue.
        let queue = unsafe { gpu.queue().as_hal::<wgpu_hal::api::Dx12>() }
            .ok_or(BridgeError::InvalidInput("renderer queue is not D3D12"))?;
        let (mut signalled, mut status) = (0_u64, 0);
        // SAFETY: `inputs` holds the frame's texture and fence and both planes, the guard the
        // queue, and the helper checks every resource it is given before it records anything.
        let step = unsafe {
            encode_copy_submit(
                self.copier,
                queue.as_raw().as_raw(),
                &copy,
                &mut signalled,
                &mut status,
            )
        };
        drop(queue);
        if step != 0 {
            if step == COPY_SUBMITTED_UNFENCED {
                // The GPU may be reading the planes and writing the frame for as long as it takes,
                // and the encoder would wait for a value nothing signals: neither may be used
                // again.
                std::mem::forget(inputs);
            }
            return Err(helper_error("encode copy", step, status));
        }
        retained.copies[slot] = Some(inputs);
        frame.commit(signalled)
    }

    /// Waits for outstanding copies and releases their resources.
    ///
    /// # Errors
    /// On timeout or device loss, retains the pending GPU resources and device
    /// rather than destroying textures and frames that raw D3D12 work may
    /// still use.
    pub fn finish(mut self) -> Result<(), BridgeError> {
        self.try_close()
    }

    fn try_close(&mut self) -> Result<(), BridgeError> {
        let mut status = 0;
        // SAFETY: created by the matching constructor and destroyed once; the C side waits for
        // every in-flight copy before freeing anything.
        let step =
            unsafe { alpha2_native_gpu_d3d12_encode_copier_destroy(self.copier, &raw mut status) };
        if step != 0 {
            self.close_failed = true;
            return Err(helper_error("encode copier teardown", step, status));
        }
        self.copier = std::ptr::null_mut();
        Ok(())
    }
}

#[cfg(test)]
impl SharedD3D12Context {
    /// The copy pool's targets and command slots made so far, and those idle
    /// in it now: `[targets made, slots made, idle targets, idle slots]`.
    fn copy_pool_counts(&self) -> [u32; 4] {
        let mut counts = [0; 4];
        let [made, slots, idle, idle_slots] = &mut counts;
        // SAFETY: the context's live pool, and four writable integers.
        unsafe {
            alpha2_native_gpu_d3d12_pool_counts(
                self.copies.0.as_ptr(),
                made,
                slots,
                idle,
                idle_slots,
            );
        }
        counts
    }
}

fn raw_pointer(value: u64, message: &'static str) -> Result<*mut c_void, BridgeError> {
    Ok(usize::try_from(value).map_err(|_| BridgeError::InvalidInput(message))? as *mut c_void)
}

#[cfg(test)]
#[path = "d3d12_tests.rs"]
mod tests;
