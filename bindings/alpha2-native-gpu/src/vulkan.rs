//! Vulkan interop: the device the renderer shares with `FFmpeg`'s decoder and
//! encoder, the import of decoded frames as wgpu plane views with their layout
//! transitions and timeline synchronization, the copy of rendered planes into
//! encoder frames, and the device capability probes.

use std::{
    collections::VecDeque,
    error::Error,
    ffi::{CStr, c_void},
    fmt,
    ptr::NonNull,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use alpha2_ffmpeg_bridge::{
    BridgeError, VulkanDeviceInfo, VulkanFrameAccess, VulkanFrameAccessGuard, VulkanFrameLease,
    VulkanQueueLock, create_vulkan_device_context,
};
use alpha2_gpu::{ExternalQueueLock, GpuContext};
use ash::vk::{self, Handle};
use rsmpeg::avutil::AVHWDeviceContext;
use wgpu_hal::Instance as _;

use crate::{NativeGpuIdentity, NativePlaneLayout};

// Each helper that can fail returns 0 or the step that failed, and writes the platform status of
// that step to `status` (`include/alpha2_native_gpu_status.h`).
unsafe extern "C" {
    fn alpha2_native_gpu_vulkan_probe(access: *mut VulkanFrameAccess, status: *mut i32) -> i32;
    fn alpha2_native_gpu_vulkan_acquire_check(
        access: *const VulkanFrameAccess,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_ticket_create(
        device: u64,
        allocator: u64,
        queue_family: u32,
        output: *mut *mut c_void,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_ticket_poll(ticket: *const c_void, status: *mut i32) -> i32;
    fn alpha2_native_gpu_vulkan_ticket_reset(ticket: *mut c_void, status: *mut i32) -> i32;
    fn alpha2_native_gpu_vulkan_acquire_submit(
        access: *mut VulkanFrameAccess,
        ticket: *mut c_void,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_release(
        access: *mut VulkanFrameAccess,
        ticket: *mut c_void,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_signal(
        access: *mut VulkanFrameAccess,
        semaphore: u64,
        wait_value: u64,
        signal_value: u64,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_import_destroy(ticket: *mut c_void);
    fn alpha2_native_gpu_vulkan_import_retire(ticket: *mut c_void, timeout_ns: u64) -> i32;
    #[cfg(test)]
    fn alpha2_native_gpu_vulkan_retained_helpers() -> u64;
    fn alpha2_native_gpu_vulkan_encode_copier_create(
        device: u64,
        queue_family: u32,
        allocator: u64,
        output: *mut *mut c_void,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_encode_copier_destroy(copier: *mut c_void, status: *mut i32)
    -> i32;
    fn alpha2_native_gpu_vulkan_encode_copier_slots() -> u32;
    fn alpha2_native_gpu_vulkan_encode_copier_await_slot(
        copier: *mut c_void,
        slot: *mut u32,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_encode_copier_submit(
        copier: *mut c_void,
        access: *mut VulkanFrameAccess,
        copy: *const VulkanEncodeCopy,
        status: *mut i32,
    ) -> i32;
    fn alpha2_native_gpu_vulkan_set_wait_timeout(nanoseconds: u64);
    fn alpha2_native_gpu_vulkan_wait_timeout() -> u64;
}

/// Sets how long the C helpers wait on a fence before giving up.
pub(crate) fn set_wait_timeout(timeout: std::time::Duration) {
    let nanoseconds = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
    // SAFETY: stores one integer the helpers read on each wait.
    unsafe { alpha2_native_gpu_vulkan_set_wait_timeout(nanoseconds) };
}

/// How long the C helpers wait on a fence before giving up.
fn wait_timeout() -> std::time::Duration {
    // SAFETY: reads one integer.
    std::time::Duration::from_nanos(unsafe { alpha2_native_gpu_vulkan_wait_timeout() })
}

/// The error for a Vulkan helper's `operation` that failed at `step` with
/// the `VkResult` `status`.
fn helper_error(operation: &'static str, step: i32, status: i32) -> BridgeError {
    crate::status::helper_failure(
        alpha2_ffmpeg_bridge::NativeGpuBackend::Vulkan,
        operation,
        step,
        status,
    )
}

/// Fails the Nth call of an import helper on this thread, as only a device
/// fault would, so tests reach the branches that handle those failures.
#[cfg(test)]
pub(crate) mod inject {
    use std::cell::Cell;

    /// An import helper a test can fail.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Hook {
        AcquireSubmit,
        TicketPoll,
        TicketReset,
        Release,
    }

    thread_local! {
        static COUNTDOWNS: [Cell<u32>; 4] =
            const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
        static STATUSES: [Cell<i32>; 4] =
            const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
    }

    /// Fails the `nth` (1-based) next call of `hook` on this thread with a
    /// lost device.
    pub(crate) fn fail_nth(hook: Hook, nth: u32) {
        fail_nth_with(hook, nth, super::vk::Result::ERROR_DEVICE_LOST.as_raw());
    }

    /// Fails the `nth` (1-based) next call of `hook` on this thread with the
    /// `VkResult` `status`.
    pub(crate) fn fail_nth_with(hook: Hook, nth: u32, status: i32) {
        COUNTDOWNS.with(|countdowns| countdowns[hook as usize].set(nth));
        STATUSES.with(|statuses| statuses[hook as usize].set(status));
    }

    /// Whether this call of `hook` is the one to fail; a failure returns the
    /// status the test chose at the step that call would have reached.
    pub(super) fn fails(hook: Hook, step: i32, status: &mut i32) -> Option<i32> {
        let due = COUNTDOWNS.with(|countdowns| {
            let countdown = &countdowns[hook as usize];
            match countdown.get() {
                0 => false,
                1 => {
                    countdown.set(0);
                    true
                }
                left => {
                    countdown.set(left - 1);
                    false
                }
            }
        });
        due.then(|| {
            *status = STATUSES.with(|statuses| statuses[hook as usize].get());
            step
        })
    }
}

/// An import's frame, locked, with the ticket whose commands move it.
///
/// A ticket's command pool, command buffers and fences are Vulkan objects the
/// application must synchronize. Its lease uses them only through this, which
/// holds the frame's lock (and with it the lock of the queue `FFmpeg` and the
/// renderer share), so the lock of the one frame a ticket serves orders every
/// use of it. The pool touches a ticket only before a lease takes it and after
/// the lease's drop gives it back.
struct LeaseAccess<'a> {
    frame: VulkanFrameAccessGuard<'a>,
    ticket: NonNull<c_void>,
}

/// Calls `alpha2_native_gpu_vulkan_acquire_submit`, or in tests fails first
/// where [`inject`] asks.
///
/// # Safety
/// As for the helper: `lease.ticket` was created for the frame's device and
/// queue family, and `lease.frame` may record what the acquire submits.
unsafe fn acquire_submit(lease: &mut LeaseAccess<'_>, status: &mut i32) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::fails(
        inject::Hook::AcquireSubmit,
        crate::interop::ALPHA2_GPU_STEP_QUEUE_SUBMIT,
        status,
    ) {
        return step;
    }
    // SAFETY: the caller's contract; the guard holds the frame's and the queue's locks.
    unsafe {
        alpha2_native_gpu_vulkan_acquire_submit(
            lease.frame.access_mut(),
            lease.ticket.as_ptr(),
            status,
        )
    }
}

/// Calls `alpha2_native_gpu_vulkan_release`, or in tests fails first where
/// [`inject`] asks.
///
/// # Safety
/// As for the helper: `lease.ticket` is the one the frame's acquire was
/// submitted with, and `lease.frame` may record what the release submits.
unsafe fn release(lease: &mut LeaseAccess<'_>, status: &mut i32) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::fails(
        inject::Hook::Release,
        crate::interop::ALPHA2_GPU_STEP_QUEUE_SUBMIT,
        status,
    ) {
        return step;
    }
    // SAFETY: the caller's contract; the guard holds the frame's and the queue's locks.
    unsafe {
        alpha2_native_gpu_vulkan_release(lease.frame.access_mut(), lease.ticket.as_ptr(), status)
    }
}

/// Calls `alpha2_native_gpu_vulkan_ticket_poll`, or in tests fails first
/// where [`inject`] asks.
///
/// # Safety
/// `ticket` is a live ticket the caller owns.
unsafe fn ticket_poll(ticket: *const c_void, status: &mut i32) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::fails(
        inject::Hook::TicketPoll,
        crate::interop::ALPHA2_GPU_STEP_FENCE_STATUS,
        status,
    ) {
        return step;
    }
    // SAFETY: the caller's contract.
    unsafe { alpha2_native_gpu_vulkan_ticket_poll(ticket, status) }
}

/// Calls `alpha2_native_gpu_vulkan_ticket_reset`, or in tests fails first
/// where [`inject`] asks.
///
/// # Safety
/// `ticket` is a live ticket the caller owns, and nothing it submitted runs.
unsafe fn ticket_reset(ticket: *mut c_void, status: &mut i32) -> i32 {
    #[cfg(test)]
    if let Some(step) = inject::fails(
        inject::Hook::TicketReset,
        crate::interop::ALPHA2_GPU_STEP_RESET_FENCE,
        status,
    ) {
        return step;
    }
    // SAFETY: the caller's contract.
    unsafe { alpha2_native_gpu_vulkan_ticket_reset(ticket, status) }
}

use crate::interop::Alpha2VulkanEncodeCopy as VulkanEncodeCopy;

/// Reusable NV12 handoff from the renderer's plane textures to encoder frames.
///
/// Owns the command pool and the command-buffer ring, so a frame costs one
/// queue submission and no CPU wait. One per export.
///
/// The copy is ordered after the render that packed the planes by being
/// submitted after it on the same queue, not by waiting for a signal that
/// render's submission raises: a submission `wgpu` rejects raises nothing,
/// and a copy waiting for it would block the shared queue.
pub struct VulkanEncodeCopier {
    copier: *mut c_void,
    /// The same lock `FFmpeg`'s own submissions and `GpuContext::submit` take.
    /// This copy goes to the graphics queue wgpu uses, so it has to hold it.
    queue_lock: alpha2_ffmpeg_bridge::VulkanQueueLock,
    retained: Option<Box<RetainedEncodeWork>>,
    close_failed: bool,
}

/// Raw Vulkan submissions are invisible to wgpu's resource tracking. Keep
/// both ends of every copy and their device alive until its fence completes.
struct RetainedEncodeWork {
    copies: Vec<Option<EncodeCopyInputs>>,
    _context: SharedVulkanContext,
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
unsafe impl Send for VulkanEncodeCopier {}

impl fmt::Debug for VulkanEncodeCopier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanEncodeCopier")
            .finish_non_exhaustive()
    }
}

impl Drop for VulkanEncodeCopier {
    fn drop(&mut self) {
        if !self.copier.is_null() && !self.close_failed {
            let _ = self.try_close();
        }
        if !self.copier.is_null() {
            // The C helper did not finish: leaking only its handle would let Rust free what its
            // commands still use, so keep the entire ownership graph, without waiting twice.
            std::mem::forget(self.retained.take());
        }
    }
}

impl VulkanEncodeCopier {
    /// Waits for outstanding copies and releases their resources.
    ///
    /// # Errors
    /// On timeout or device loss, retains the pending GPU resources and device
    /// rather than destroying images that raw Vulkan work may still use.
    pub fn finish(mut self) -> Result<(), BridgeError> {
        self.try_close()
    }

    fn try_close(&mut self) -> Result<(), BridgeError> {
        let mut status = 0;
        // SAFETY: created by the matching constructor and destroyed once; the C side waits for
        // every in-flight copy before freeing anything.
        let step =
            unsafe { alpha2_native_gpu_vulkan_encode_copier_destroy(self.copier, &raw mut status) };
        if step != 0 {
            self.close_failed = true;
            return Err(helper_error("encode copier teardown", step, status));
        }
        self.copier = std::ptr::null_mut();
        // Keep the device until the queue and FFmpeg queue-lock references have dropped too;
        // `retained` is deliberately the last owning field.
        Ok(())
    }
}

impl VulkanEncodeCopier {
    /// Creates the handoff for one export on `context`'s device.
    ///
    /// # Errors
    /// Returns an error when the command pool, command buffers or fences
    /// cannot be created, or the device has no queue lock for its graphics
    /// family.
    pub fn new(context: &SharedVulkanContext) -> Result<Self, BridgeError> {
        let gpu = context.gpu();
        // SAFETY: this context is always Vulkan; the handle is only read.
        let device = unsafe { gpu.device().as_hal::<wgpu_hal::api::Vulkan>() }
            .map(|device| device.raw_device().handle().as_raw())
            .ok_or(BridgeError::InvalidInput("renderer is not Vulkan"))?;
        let mut copier = std::ptr::null_mut();
        let mut status = 0;
        // SAFETY: `device` is this renderer's live logical device and the output pointers are valid
        // locals.
        let step = unsafe {
            alpha2_native_gpu_vulkan_encode_copier_create(
                device,
                0,
                0,
                &raw mut copier,
                &raw mut status,
            )
        };
        if step != 0 {
            return Err(helper_error("encode copier creation", step, status));
        }
        Ok(Self {
            copier,
            queue_lock: alpha2_ffmpeg_bridge::VulkanQueueLock::new(context.ffmpeg_device(), 0)?,
            retained: Some(Box::new(RetainedEncodeWork {
                copies: (0..Self::slots()).map(|_| None).collect(),
                _context: context.clone(),
                #[cfg(test)]
                lifetime_probe: None,
            })),
            close_failed: false,
        })
    }

    /// Plane-texture pairs a caller must cycle through to feed this copier.
    ///
    /// One pair is only safe while the copy and the next render are ordered on
    /// one queue by a caller that never runs ahead. A caller that hands the
    /// encode to another thread does run ahead, and then the pairs have to be
    /// a ring as deep as the command-buffer ring this copier already keeps.
    #[must_use]
    pub fn slots() -> usize {
        // SAFETY: a constant read out of the shim; it touches no state.
        unsafe { alpha2_native_gpu_vulkan_encode_copier_slots() as usize }
    }

    /// Waits until the slot the next copy will use is free, and names it.
    ///
    /// Call before recording the render that writes the plane textures, and
    /// render into the pair this returns. The wait is on the fence of the copy
    /// four submissions ago, so in a pipeline that is keeping up it is free.
    ///
    /// # Errors
    /// Returns an error when the fence wait fails.
    pub fn await_slot(&mut self) -> Result<usize, BridgeError> {
        let (mut slot, mut status) = (0_u32, 0);
        // SAFETY: the copier is live and the output pointers are valid locals.
        let step = unsafe {
            alpha2_native_gpu_vulkan_encode_copier_await_slot(
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

    /// Copies a renderer-packed NV12 pair into an encoder frame's image.
    ///
    /// The submission that wrote these textures must already be on the
    /// renderer's queue: the copy is ordered after it by queue order alone.
    /// When that submission was rejected the copy still runs, over whatever
    /// the planes held, and the frame is the caller's to fail.
    ///
    /// # Errors
    /// Returns an error when the frame is not one multi-planar image on this
    /// device, the textures are not Vulkan, or the submission fails.
    pub fn submit(
        &mut self,
        frame: &mut alpha2_ffmpeg_bridge::VulkanEncodeFrameAccess<'_>,
        luma: &wgpu::Texture,
        chroma: &wgpu::Texture,
        size: (u32, u32),
    ) -> Result<(), BridgeError> {
        let slot = self.await_slot()?;
        let inputs = EncodeCopyInputs {
            _luma: luma.clone(),
            _chroma: chroma.clone(),
            _frame: frame.retain_frame()?,
        };
        let kept = self
            .retained
            .as_mut()
            .and_then(|work| work.copies.get_mut(slot))
            .ok_or(BridgeError::InvalidInput("invalid encode copier slot"))?;
        // SAFETY: both textures belong to this Vulkan device and the handles are only read; wgpu
        // keeps owning and destroying them.
        let luma_image = unsafe { luma.as_hal::<wgpu_hal::api::Vulkan>() }
            .map(|texture| unsafe { texture.raw_handle() }.as_raw())
            .ok_or(BridgeError::InvalidInput("luma plane is not Vulkan"))?;
        // SAFETY: as above.
        let chroma_image = unsafe { chroma.as_hal::<wgpu_hal::api::Vulkan>() }
            .map(|texture| unsafe { texture.raw_handle() }.as_raw())
            .ok_or(BridgeError::InvalidInput("chroma plane is not Vulkan"))?;
        let copy = VulkanEncodeCopy {
            luma_image,
            chroma_image,
            width: size.0,
            height: size.1,
        };
        // The copy submits to the graphics queue wgpu and FFmpeg share, and every other submission
        // to it holds this lock. The plane textures it reads are protected by the caller's slot
        // ring rather than by queue order, because a caller whose encode runs on another thread can
        // be several frames ahead by the time this executes.
        let queue = self.queue_lock.lock();
        let mut status = 0;
        // SAFETY: the guard holds FFmpeg's frame lock for the whole call, and the shim validates
        // every handle it reads out of the descriptor.
        let step = unsafe {
            alpha2_native_gpu_vulkan_encode_copier_submit(
                self.copier,
                frame.access_mut(),
                &raw const copy,
                &raw mut status,
            )
        };
        drop(queue);
        if step != 0 {
            return Err(helper_error("encode copy", step, status));
        }
        *kept = Some(inputs);
        frame.commit();
        Ok(())
    }
}

/// Checks that a decoded frame can be imported: moves its planes to
/// `SHADER_READ_ONLY` and back in one submission on its queue, through views
/// like an import's, and waits for it.
///
/// # Errors
/// Returns an error when image-view creation, synchronization, or submission fails.
pub fn probe_vulkan_submission(lease: &VulkanFrameLease) -> Result<(), BridgeError> {
    let mut guard = lease.lock_access()?;
    let mut status = 0;
    // SAFETY: the bridge guard holds the FFmpeg frame and queue locks, and the C helper only uses
    // the descriptor for this synchronous call. It changes only the timeline values, to the ones
    // its submission signals, and only once it has submitted; the layouts it returns to are the
    // frame's.
    let step = unsafe { alpha2_native_gpu_vulkan_probe(guard.access_mut(), &raw mut status) };
    // Written back on failure too: a submission whose wait timed out still signals when it runs,
    // and the frame's next user must signal above it.
    guard.commit();
    if step != 0 {
        return Err(helper_error("decode probe", step, status));
    }
    Ok(())
}

/// Coded extents and coding limits one video-encode profile has on a device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoEncodeExtent {
    /// The H.264 profile these are the limits of: the one `h264_vulkan`
    /// opens its session with on this device (`constrained_baseline`,
    /// `main`, or `high`).
    pub profile: &'static str,
    /// Smallest accepted width and height, in samples.
    pub minimum: (u32, u32),
    /// Largest accepted width and height, in samples.
    pub maximum: (u32, u32),
    /// Granularity the coded extent is aligned up to before the limits apply:
    /// `VkVideoEncodeCapabilitiesKHR::encodeInputPictureGranularity`, which
    /// `FFmpeg` aligns its coded size to.
    ///
    /// This is why a 158x62 output is accepted where 64x48 is not: both are
    /// below the minimum as written, but the first aligns up to the minimum
    /// and the second does not. Comparing an output size against `minimum`
    /// without aligning it first gets that case wrong in the strict direction.
    pub granularity: (u32, u32),
    /// `VkVideoEncodeH264CapabilitiesKHR::maxPPictureL0ReferenceCount`: how
    /// many references a P picture may use.
    pub max_p_picture_l0_references: u32,
    /// `VkVideoEncodeH264CapabilitiesKHR::maxBPictureL0ReferenceCount`: the
    /// forward references a B picture may use. Zero, together with
    /// `max_l1_references`, means the device encodes no B pictures.
    pub max_b_picture_l0_references: u32,
    /// `VkVideoEncodeH264CapabilitiesKHR::maxL1ReferenceCount`: the backward
    /// references a B picture may use.
    pub max_l1_references: u32,
    /// `VkVideoEncodeCapabilitiesKHR::rateControlModes`, as the mode names
    /// the device reports: `cbr`, `vbr`, `disabled` (constant QP).
    pub rate_control_modes: Vec<&'static str>,
}

impl VideoEncodeExtent {
    /// Whether the device can encode B pictures at all.
    #[must_use]
    pub const fn supports_b_pictures(&self) -> bool {
        self.max_b_picture_l0_references > 0 || self.max_l1_references > 0
    }
}

impl VideoEncodeExtent {
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

/// The H.264 profiles `FFmpeg`'s `h264_vulkan` tries for 8-bit 4:2:0 input,
/// in its order. With no profile asked for, which nothing here does, it
/// opens the session with the last one the device supports
/// (`vulkan_encode_h264.c`, `init_profile`): `high` on every device that
/// has it, not `main`.
const H264_ENCODE_PROFILES: [(vk::native::StdVideoH264ProfileIdc, &str); 3] = [
    (
        vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_BASELINE,
        "constrained_baseline",
    ),
    (
        vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN,
        "main",
    ),
    (
        vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
        "high",
    ),
];

/// What `query` answers for the last of `H264_ENCODE_PROFILES` it answers
/// at all: the profile `h264_vulkan` would open, and its answer.
fn last_supported_profile<T>(
    mut query: impl FnMut(vk::native::StdVideoH264ProfileIdc) -> Option<T>,
) -> Option<(&'static str, T)> {
    H264_ENCODE_PROFILES
        .iter()
        .filter_map(|&(idc, name)| query(idc).map(|answer| (name, answer)))
        .last()
}

/// Queries the coded extents this device's 8-bit 4:2:0 H.264 encoder accepts,
/// for the profile `h264_vulkan` opens on it.
///
/// Asked of the physical device rather than of `FFmpeg`, because `FFmpeg` only
/// answers by failing to open a session at a size, after the fact, and a
/// caller needs the answer before it exports.
///
/// Only call this for a device that exposes `VK_KHR_video_queue`,
/// `VK_KHR_video_encode_queue`, and `VK_KHR_video_encode_h264`.
fn h264_encode_extent(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> Option<VideoEncodeExtent> {
    let video = ash::khr::video_queue::Instance::new(entry, instance);
    let (profile, mut extent) = last_supported_profile(|idc| {
        let mut h264 = vk::VideoEncodeH264ProfileInfoKHR::default().std_profile_idc(idc);
        let profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .push_next(&mut h264);
        let mut h264_capabilities = vk::VideoEncodeH264CapabilitiesKHR::default();
        let mut encode = vk::VideoEncodeCapabilitiesKHR::default();
        let mut capabilities = vk::VideoCapabilitiesKHR::default()
            .push_next(&mut encode)
            .push_next(&mut h264_capabilities);
        // SAFETY: the profile chain lives for the call and the device is this instance's. The
        // caller has checked that the device exposes `VK_KHR_video_queue`,
        // `VK_KHR_video_encode_queue`, and `VK_KHR_video_encode_h264`; without them the loader
        // dispatches to a null driver entry point. With them, a device that lacks this profile
        // answers with an error rather than writing anything.
        let result = unsafe {
            (video.fp().get_physical_device_video_capabilities_khr)(
                physical,
                &raw const profile,
                &raw mut capabilities,
            )
        };
        if result != vk::Result::SUCCESS {
            return None;
        }
        let coded = (capabilities.min_coded_extent, capabilities.max_coded_extent);
        Some(encode_extent(coded, &encode, &h264_capabilities))
    })?;
    extent.profile = profile;
    Some(extent)
}

/// The limits one profile's capabilities describe: `coded`, its minimum and
/// maximum coded extents, and its encode and H.264 capabilities. `profile`
/// is left for the caller to name.
fn encode_extent(
    (minimum, maximum): (vk::Extent2D, vk::Extent2D),
    encode: &vk::VideoEncodeCapabilitiesKHR<'_>,
    h264: &vk::VideoEncodeH264CapabilitiesKHR<'_>,
) -> VideoEncodeExtent {
    VideoEncodeExtent {
        profile: "",
        minimum: (minimum.width, minimum.height),
        maximum: (maximum.width, maximum.height),
        // What FFmpeg aligns the coded size to before it checks the limits (`vulkan_encode.c`), not
        // `pictureAccessGranularity`: on Mesa ANV the two are 32 and 16.
        granularity: (
            encode.encode_input_picture_granularity.width.max(1),
            encode.encode_input_picture_granularity.height.max(1),
        ),
        max_p_picture_l0_references: h264.max_p_picture_l0_reference_count,
        max_b_picture_l0_references: h264.max_b_picture_l0_reference_count,
        max_l1_references: h264.max_l1_reference_count,
        rate_control_modes: [
            (vk::VideoEncodeRateControlModeFlagsKHR::CBR, "cbr"),
            (vk::VideoEncodeRateControlModeFlagsKHR::VBR, "vbr"),
            (vk::VideoEncodeRateControlModeFlagsKHR::DISABLED, "disabled"),
        ]
        .into_iter()
        .filter(|(flag, _)| encode.rate_control_modes.contains(*flag))
        .map(|(_, name)| name)
        .collect(),
    }
}

/// A Vulkan layer's specification version.
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VulkanLayerVersion {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version (the SDK release).
    pub patch: u32,
}

impl VulkanLayerVersion {
    /// The first Khronos validation layer `FFmpeg`'s video sessions survive.
    ///
    /// Older ones crash or hang inside the session setup, on the `pNext`
    /// chain `FFmpeg` builds for it: a decode session as an encode one.
    pub const VIDEO_SESSIONS: Self = Self {
        major: 1,
        minor: 4,
        patch: 0,
    };

    /// Unpacks a `VK_MAKE_API_VERSION` value.
    #[must_use]
    pub const fn from_packed(version: u32) -> Self {
        Self {
            major: vk::api_version_major(version),
            minor: vk::api_version_minor(version),
            patch: vk::api_version_patch(version),
        }
    }
}

impl fmt::Display for VulkanLayerVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

const KHRONOS_VALIDATION: &str = "VK_LAYER_KHRONOS_validation";

/// The version of the Khronos validation layer the loader activated on
/// `instance`, or `None` when it activated none.
///
/// Asked of the loader rather than worked out from the environment.
/// `vkEnumerateDeviceLayerProperties` is deprecated for device layers, and
/// what every loader returns from it instead is the instance's activated
/// layer list: the layers the application enabled (wgpu enables this one
/// when its validation flag is set, as in every debug build), the ones
/// `VK_INSTANCE_LAYERS` and `VK_LOADER_LAYERS_ENABLE` add, and none that
/// `VK_LOADER_LAYERS_DISABLE` takes away, each by that loader's own rules.
/// Reimplementing those rules is easy to get wrong both ways: the loader's
/// globs ignore case and have four forms only, `~explicit~` enables nothing,
/// and a loader before 1.3.234 reads neither filter variable.
fn activated_validation_layer(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> Option<VulkanLayerVersion> {
    // SAFETY: `physical` was enumerated from `instance`, which is alive for the call.
    let layers = unsafe { instance.enumerate_device_layer_properties(physical) }.ok()?;
    layers
        .iter()
        .find(|layer| {
            layer
                .layer_name_as_c_str()
                .is_ok_and(|name| name.to_bytes() == KHRONOS_VALIDATION.as_bytes())
        })
        .map(|layer| VulkanLayerVersion::from_packed(layer.spec_version))
}

/// The Vulkan-only HAL instance the shared device is made on.
///
/// It is made through `wgpu-hal` directly, outside alpha2-gpu's instance builder: it has no DX12
/// compiler to share, and its flags are its own.
fn hal_instance() -> Result<wgpu_hal::vulkan::Instance, SharedVulkanError> {
    let hal_desc = wgpu_hal::InstanceDescriptor {
        name: "alpha2 shared Vulkan decode",
        flags: wgpu::InstanceFlags::from_env_or_default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::from_env_or_default(),
        telemetry: None,
        display: None,
    };
    // SAFETY: the returned HAL instance owns its loader and Vulkan instance.
    unsafe {
        wgpu_hal::vulkan::Instance::init(&hal_desc)
            .map_err(|error| SharedVulkanError(format!("Vulkan instance: {error}")))
    }
}

/// Holds the shared graphics-queue lock for the renderer's submissions.
///
/// Private to this module and only ever handed to the [`GpuContext`] it is
/// made for, which takes it around each queue call and releases it on the
/// same thread, innermost hold first, before that call returns (its
/// `QueueGuard` and `QueueLockSlot`). That pairing is the contract of the raw
/// lock below; no other code can reach this value to break it.
#[derive(Debug)]
struct SharedQueueLock(VulkanQueueLock);

impl ExternalQueueLock for SharedQueueLock {
    fn lock(&self) {
        // SAFETY: the context this is handed to releases each hold with one `unlock` on the same
        // thread, in reverse order (see above).
        unsafe { self.0.raw_lock() };
    }

    fn unlock(&self) {
        // SAFETY: the context calls this only to release its own innermost hold, on the thread that
        // took it (see above).
        unsafe { self.0.raw_unlock() };
    }
}

/// Failure while creating a logical Vulkan device shared by wgpu and `FFmpeg`.
#[derive(Debug)]
pub struct SharedVulkanError(String);

impl fmt::Display for SharedVulkanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for SharedVulkanError {}

/// An imported bi-planar 4:2:0 image and its two wgpu plane views.
#[derive(Debug)]
pub struct ImportedVulkanFrame {
    /// Plane-zero luma view.
    pub y: wgpu::TextureView,
    /// Plane-one interleaved chroma view.
    pub uv: wgpu::TextureView,
    /// Sample layout of the imported planes.
    pub layout: NativePlaneLayout,
    /// Allocated image extent, which the visible rectangle is placed inside.
    ///
    /// This is the codec's padded coded size, not the display size, and it is
    /// the extent presentation must normalize sample coordinates against.
    pub coded_size: [u32; 2],
    /// Submission/lifetime lease for the decoder surface.
    pub lease: Arc<VulkanImportLease>,
}

/// Allocation counters of a context's import tickets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportTicketStats {
    /// Tickets created: a command pool, two command buffers, and a fence each.
    pub created: u64,
    /// Imports served by a finished, reset ticket.
    pub reused: u64,
    /// Tickets reset and ready for an import.
    pub idle: usize,
    /// Tickets whose release may still be running.
    pub pending: usize,
    /// Tickets given up on because their device could not report them finished.
    pub abandoned: u64,
}

/// One import ticket and the device and queue family it was made for.
struct PooledTicket {
    ticket: NonNull<c_void>,
    device: u64,
    queue_family: u32,
}

/// A returned ticket waits in `pending` until the fences of its acquire and release, whichever it
/// submitted, are seen signalled, polled without blocking at the next import, and only then is
/// reset and reused. A ticket whose device cannot say, or that is still running when too many wait,
/// is never reused: it is destroyed after a bounded wait or, failing that, leaked, which is the
/// safe loss. Those waits happen outside the pool's lock, and the tickets retired together (the
/// pool's drop retires every pending one) share one wait timeout, so a device that is hung but not
/// lost costs the timeout once, not once per ticket.
#[derive(Default)]
struct ImportTicketPool {
    state: Mutex<ImportTicketPoolState>,
}

#[derive(Default)]
struct ImportTicketPoolState {
    idle: Vec<PooledTicket>,
    pending: VecDeque<PooledTicket>,
    created: u64,
    reused: u64,
    abandoned: u64,
}

// Frames in flight bound how many tickets an export uses at once; these bound what an unusual burst
// leaves behind.
const MAX_IDLE_IMPORT_TICKETS: usize = 8;
const MAX_PENDING_IMPORT_TICKETS: usize = 32;

// SAFETY: a ticket is only touched by the pool under its mutex, or by the one lease that took it;
// Vulkan's own objects are externally synchronized by that single ownership.
unsafe impl Send for ImportTicketPool {}
// SAFETY: as for `Send`: every access goes through the mutex.
unsafe impl Sync for ImportTicketPool {}

impl ImportTicketPool {
    fn take(
        &self,
        device: u64,
        allocator: u64,
        queue_family: u32,
    ) -> Result<PooledTicket, BridgeError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reclaim();
        if let Some(index) = state
            .idle
            .iter()
            .position(|idle| idle.device == device && idle.queue_family == queue_family)
        {
            state.reused = state.reused.saturating_add(1);
            return Ok(state.idle.swap_remove(index));
        }
        state.created = state.created.saturating_add(1);
        drop(state);
        let (mut raw, mut status) = (std::ptr::null_mut(), 0);
        // SAFETY: `device` and `allocator` are the frame's own, read under its lock; the output
        // ticket is uniquely owned by the caller.
        let step = unsafe {
            alpha2_native_gpu_vulkan_ticket_create(
                device,
                allocator,
                queue_family,
                &raw mut raw,
                &raw mut status,
            )
        };
        if step != 0 {
            return Err(helper_error("import ticket creation", step, status));
        }
        NonNull::new(raw)
            .map(|ticket| PooledTicket {
                ticket,
                device,
                queue_family,
            })
            .ok_or(BridgeError::InvalidInput(
                "Vulkan import ticket creation returned no ticket",
            ))
    }

    /// Takes back a ticket, whatever it has submitted: each submission carries
    /// a fence the pool polls before the ticket is reused.
    fn give_back(&self, ticket: PooledTicket) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.push_back(ticket);
        if state.pending.len() > MAX_PENDING_IMPORT_TICKETS {
            state.reclaim();
        }
        let excess = state
            .pending
            .len()
            .saturating_sub(MAX_PENDING_IMPORT_TICKETS);
        let oldest = state.pending.drain(..excess).collect::<Vec<_>>();
        state.abandoned = state.abandoned.saturating_add(oldest.len() as u64);
        // Imports of the device go on while these wait.
        drop(state);
        retire(oldest);
    }

    fn stats(&self) -> ImportTicketStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ImportTicketStats {
            created: state.created,
            reused: state.reused,
            idle: state.idle.len(),
            pending: state.pending.len(),
            abandoned: state.abandoned,
        }
    }
}

impl ImportTicketPoolState {
    /// Resets every pending ticket whose release has finished and makes it
    /// idle, without waiting on any.
    fn reclaim(&mut self) {
        let mut index = 0;
        while index < self.pending.len() {
            let ticket = self.pending[index].ticket;
            let mut status = 0;
            // SAFETY: the pool owns every pending ticket.
            match unsafe { ticket_poll(ticket.as_ptr(), &mut status) } {
                0 => index += 1,
                1 => {
                    let Some(finished) = self.pending.remove(index) else {
                        break;
                    };
                    // SAFETY: nothing the ticket submitted is still running.
                    let reset = unsafe { ticket_reset(ticket.as_ptr(), &mut status) };
                    if reset != 0 {
                        self.abandoned = self.abandoned.saturating_add(1);
                    } else if self.idle.len() < MAX_IDLE_IMPORT_TICKETS {
                        self.idle.push(finished);
                    } else {
                        // SAFETY: finished and reset, so nothing uses it.
                        unsafe { alpha2_native_gpu_vulkan_import_destroy(ticket.as_ptr()) };
                    }
                }
                _ => {
                    // The device cannot say whether it finished: never reuse it, and leak it rather
                    // than free it running.
                    self.pending.remove(index);
                    self.abandoned = self.abandoned.saturating_add(1);
                }
            }
        }
    }
}

impl Drop for ImportTicketPool {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for idle in state.idle.drain(..) {
            // SAFETY: idle tickets are reset and unused.
            unsafe { alpha2_native_gpu_vulkan_import_destroy(idle.ticket.as_ptr()) };
        }
        retire(state.pending.drain(..));
    }
}

/// Destroys tickets the pool owns once what they submitted has finished, all within one wait
/// timeout from now; a ticket still unfinished when it runs out is leaked, not freed under the GPU.
fn retire(tickets: impl IntoIterator<Item = PooledTicket>) {
    let deadline = std::time::Instant::now().checked_add(wait_timeout());
    for pooled in tickets {
        let left = deadline.map_or(std::time::Duration::MAX, |deadline| {
            deadline.saturating_duration_since(std::time::Instant::now())
        });
        let nanoseconds = u64::try_from(left.as_nanos()).unwrap_or(u64::MAX);
        // SAFETY: the pool owns the ticket and gives it up here; the helper frees it only once its
        // fences have signalled.
        unsafe { alpha2_native_gpu_vulkan_import_retire(pooled.ticket.as_ptr(), nanoseconds) };
    }
}

/// Owns the imported texture, `FFmpeg` frame, and native transition commands.
pub struct VulkanImportLease {
    frame: Arc<VulkanFrameLease>,
    _texture: wgpu::Texture,
    gpu: GpuContext,
    batch: Arc<VulkanSubmissionBatch>,
    tickets: Arc<ImportTicketPool>,
    /// Taken only by the drop, which gives it back to `tickets`; used only
    /// through [`Self::lock`].
    ticket: Option<PooledTicket>,
    staged_signals: Mutex<Vec<StagedSignal>>,
    queued: AtomicBool,
    acquired: AtomicBool,
    released: AtomicBool,
    retained_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct StagedSignal {
    semaphore: u64,
    acquired_value: u64,
    signal_value: u64,
}

#[derive(Default)]
struct VulkanSubmissionBatch {
    pending: Mutex<Vec<Weak<VulkanImportLease>>>,
}

impl VulkanSubmissionBatch {
    fn enqueue(&self, lease: &Arc<VulkanImportLease>) {
        if !lease.queued.swap(true, Ordering::AcqRel) {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::downgrade(lease));
        }
    }

    fn flush(&self) -> Result<(), BridgeError> {
        let leases = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .filter_map(|lease| lease.upgrade())
            .filter(|lease| !lease.acquired.load(Ordering::Acquire))
            .collect::<Vec<_>>();
        if leases.is_empty() {
            return Ok(());
        }
        // Draining cleared the queue, so a lease that is still unacquired when this call fails has
        // to be re-enqueueable or it would never acquire.
        let result = flush_leases(&leases);
        if result.is_err() {
            for lease in &leases {
                if !lease.acquired.load(Ordering::Acquire) {
                    lease.queued.store(false, Ordering::Release);
                }
            }
        }
        result
    }
}

/// Acquires every queued import for the renderer's next submission.
///
/// Each frame is acquired under its own lock, from the state it is in at that
/// moment: the barrier starts from its current layout, and the timeline wait
/// is on the value `FFmpeg` last signalled. Recording the barriers at import
/// and submitting them together under the first frame's lock would let a
/// decode in between (into a frame used as a reference) move a frame on, so
/// the batch would wait on a stale value and signal one `FFmpeg` had already
/// passed.
///
/// The signal the sampling submission will raise is staged under that same
/// lock, before `FFmpeg` can see the frame again. Staged after the lock is
/// released, it would leave a window in which a decode that uses the frame as
/// a reference reads the acquire's value and layout: it would move the image
/// to its DPB layout while the renderer was still to sample it
/// (VUID-vkCmdDraw-None-09600), and signal the next value on the frame's
/// timeline from the decode queue while the renderer's submission signals a
/// later one from its own. With two queues raising one timeline out of order,
/// a wait on it can stay unsatisfied, and `FFmpeg`, freeing a picture, waits
/// forever. Staged first, the frame's recorded value is the renderer's, so a
/// decode that uses it waits for the sampling to finish.
///
/// Only one frame lock is ever held, so this cannot deadlock against a decode
/// that locks several frames in its own order; the cost is one queue
/// submission per imported frame instead of one per batch.
fn flush_leases(leases: &[Arc<VulkanImportLease>]) -> Result<(), BridgeError> {
    for lease in leases {
        let mut access = lease.lock()?;
        // Decided under the frame's lock, so a lease two flushes reach is acquired once.
        if lease.acquired.load(Ordering::Acquire) {
            continue;
        }
        let mut status = 0;
        // SAFETY: the ticket was created for this retained frame at import, and the frame and queue
        // stay locked for the submission.
        let step = unsafe { acquire_submit(&mut access, &mut status) };
        if step != 0 {
            return Err(helper_error("import acquire", step, status));
        }
        lease.acquired.store(true, Ordering::Release);
        // The acquire is submitted whatever staging does next, so its state is committed either
        // way.
        access.frame.commit();
        // SAFETY: staging records, as the frame's timeline values, the signals it adds to the
        // renderer's next queue submission, which samples this frame and is made before anything
        // waits for them; the layouts stay the ones the acquire just recorded.
        lease.stage_submission_signal(unsafe { access.frame.access_mut() })?;
    }
    Ok(())
}

// SAFETY: what a lease shares across threads is behind locks. The ticket's Vulkan objects are used
// only through `LeaseAccess`, under the lock of the frame it serves, which also holds the queue
// `FFmpeg` and the renderer share; the drop, which gives the ticket back, has the lease to itself.
// The staged signals are behind their mutex, the flags are atomics, and the frame and wgpu objects
// synchronize themselves.
unsafe impl Send for VulkanImportLease {}
// SAFETY: as for `Send`: `&self` reaches the ticket only through `Self::lock`.
unsafe impl Sync for VulkanImportLease {}

impl fmt::Debug for VulkanImportLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanImportLease")
            .field("released", &self.released.load(Ordering::Acquire))
            .field("retained_bytes", &self.retained_bytes)
            .finish_non_exhaustive()
    }
}

impl VulkanImportLease {
    /// Locks the frame for a use of the ticket: the only way `&self` reaches
    /// it (see [`LeaseAccess`]).
    fn lock(&self) -> Result<LeaseAccess<'_>, BridgeError> {
        let ticket = self
            .ticket
            .as_ref()
            .map(|pooled| pooled.ticket)
            .ok_or(BridgeError::InvalidInput("missing Vulkan import ticket"))?;
        Ok(LeaseAccess {
            frame: self.frame.lock_access()?,
            ticket,
        })
    }

    /// Approximate decoded-surface bytes retained by this import.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// Timeline signals this lease has already staged on the wgpu queue.
    ///
    /// Zero until the frame that samples this import stages its submission,
    /// and zero again once that submission completes and the lease is
    /// released. A nonzero count when the next frame begins staging would mean
    /// a signal from the previous frame is about to ride on this one.
    #[must_use]
    pub fn staged_signals(&self) -> usize {
        self.staged_signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Queues this import for the next [`Self::flush_submission`], which
    /// acquires the frame and adds its timeline signals to the wgpu
    /// submission that samples it. Queuing an import twice is harmless.
    pub fn prepare_submission(self: &Arc<Self>) {
        self.batch.enqueue(self);
    }

    /// Acquires every queued import of this context, one native queue
    /// submission per frame, each under that frame's lock, and stages the
    /// signals of the wgpu submission that samples them.
    ///
    /// # Errors
    /// Returns an error if a frame cannot be locked, its acquire fails, or its
    /// signals cannot be staged. Imports not yet acquired can then be queued
    /// again for a retry.
    pub fn flush_submission(&self) -> Result<(), BridgeError> {
        self.batch.flush()
    }

    /// Stages the sampling submission's timeline signals and records them as
    /// the frame's values. `access` is the frame's lock, held since the acquire.
    fn stage_submission_signal(&self, access: &mut VulkanFrameAccess) -> Result<(), BridgeError> {
        let mut staged = self
            .staged_signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !staged.is_empty() || self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        // SAFETY: the queue was created from this context's Vulkan OpenDevice.
        let queue = unsafe { self.gpu.queue().as_hal::<wgpu_hal::api::Vulkan>() }
            .ok_or(BridgeError::InvalidInput("renderer queue is not Vulkan"))?;
        let descriptor = access.frame_mut();
        for index in 0..descriptor.images.len() {
            if descriptor.images[index] == 0 {
                continue;
            }
            let semaphore = descriptor.semaphores[index];
            let acquired_value = descriptor.semaphore_values[index];
            if semaphore == 0 {
                return Err(BridgeError::InvalidInput("frame has no timeline semaphore"));
            }
            if let Some(existing) = staged
                .iter_mut()
                .find(|existing| existing.semaphore == semaphore)
            {
                existing.acquired_value = existing.acquired_value.max(acquired_value);
                existing.signal_value = existing.acquired_value.saturating_add(1);
            } else {
                staged.push(StagedSignal {
                    semaphore,
                    acquired_value,
                    signal_value: acquired_value.saturating_add(1),
                });
            }
        }
        for signal in &*staged {
            queue.add_signal_semaphore(
                vk::Semaphore::from_raw(signal.semaphore),
                Some(signal.signal_value),
            );
            for index in 0..descriptor.images.len() {
                if descriptor.semaphores[index] == signal.semaphore {
                    descriptor.semaphore_values[index] = signal.signal_value;
                }
            }
        }
        Ok(())
    }

    /// Records that the wgpu submission sampling this import was made.
    ///
    /// When the frame's signals were staged on that submission, the frame
    /// already records its value and layout, and nothing more is submitted.
    /// Otherwise the planes are returned to the layout `FFmpeg` had them in,
    /// in a submission of their own.
    ///
    /// # Errors
    /// Returns a bridge error if the frame can no longer be locked or submitted.
    pub fn submitted(&self) -> Result<(), BridgeError> {
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        if !self
            .staged_signals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            self.released.store(true, Ordering::Release);
            return Ok(());
        }
        let mut access = self.lock()?;
        // Decided under the frame's lock, so a lease two callers reach is released once.
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut status = 0;
        // SAFETY: ticket was created for this retained frame and remains owned by this lease until
        // queue completion drops it.
        let step = unsafe { release(&mut access, &mut status) };
        if step != 0 {
            return Err(helper_error("import release", step, status));
        }
        access.frame.commit();
        self.released.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for VulkanImportLease {
    /// Leaves the frame as `FFmpeg` can go on from, and gives the ticket back.
    ///
    /// An acquired lease that the renderer's sampling submission never
    /// released (its frame failed, or was dropped, after the acquire) still
    /// owes the frame what it promised. When its signal was staged, the frame
    /// records the value that submission was to raise, and a decode that used
    /// the frame since has queued a wait on it: taking the signal back off
    /// the queue alone would leave that wait, and every later batch on the
    /// decode queue, waiting forever. So under the frame's lock, which `FFmpeg`
    /// holds from reading the value to submitting its wait, a signal wgpu has
    /// not submitted yet is taken off its queue and raised by a submission of
    /// its own, after the acquire. A lease with nothing staged releases the
    /// frame from where it stands instead.
    ///
    /// Either way the ticket goes back to the pool, which reuses it once the
    /// fences of everything it submitted have signalled, the acquire's
    /// included: an acquire may still be waiting on the decoder when its
    /// lease is dropped, and freeing its command pool then would free commands
    /// the GPU has yet to run.
    fn drop(&mut self) {
        let Some(pooled) = self.ticket.take() else {
            return;
        };
        if self.acquired.load(Ordering::Acquire) && !self.released.load(Ordering::Acquire) {
            let staged = std::mem::take(
                self.staged_signals
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if let Ok(frame) = self.frame.lock_access() {
                // The drop has the lease, and so its ticket, to itself.
                let mut lease = LeaseAccess {
                    frame,
                    ticket: pooled.ticket,
                };
                if staged.is_empty() {
                    let mut status = 0;
                    // SAFETY: the ticket is this frame's and the guard holds its lock.
                    if unsafe { release(&mut lease, &mut status) } == 0 {
                        lease.frame.commit();
                    }
                } else {
                    // SAFETY: the queue belongs to the shared Vulkan device.
                    let queue = unsafe { self.gpu.queue().as_hal::<wgpu_hal::api::Vulkan>() };
                    for signal in &staged {
                        let unsubmitted = queue.as_ref().is_some_and(|queue| {
                            queue.remove_signal_semaphore(vk::Semaphore::from_raw(signal.semaphore))
                        });
                        if unsubmitted {
                            let mut status = 0;
                            // SAFETY: the guard holds the frame's and the queue's locks; the
                            // semaphore is the frame's timeline, which reaches `acquired_value`
                            // with the acquire.
                            let _ = unsafe {
                                alpha2_native_gpu_vulkan_signal(
                                    lease.frame.access_mut(),
                                    signal.semaphore,
                                    signal.acquired_value,
                                    signal.signal_value,
                                    &raw mut status,
                                )
                            };
                        }
                    }
                }
            }
        }
        self.tickets.give_back(pooled);
    }
}

/// Device features `FFmpeg`'s Vulkan code uses that wgpu does not enable.
///
/// `FFmpeg` enables these itself on a device it creates. On this one, which
/// wgpu creates, `FFmpeg` uses them whether or not they are on: every decode
/// and encode submission is `vkQueueSubmit2`, which needs `synchronization2`,
/// its YCbCr conversion needs `samplerYcbcrConversion`, and its
/// profile-independent images need `videoMaintenance1`.
#[derive(Clone, Copy, Debug, Default)]
struct FfmpegDeviceFeatures {
    synchronization2: bool,
    sampler_ycbcr_conversion: bool,
    video_maintenance1: bool,
}

/// The [`FfmpegDeviceFeatures`] `physical` supports; `video_maintenance1` is
/// only asked for when its extension is being enabled.
fn ffmpeg_device_features(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
    video_maintenance1: bool,
) -> FfmpegDeviceFeatures {
    let mut synchronization2 = vk::PhysicalDeviceSynchronization2Features::default();
    let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default();
    let mut maintenance = vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default();
    let mut features = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut synchronization2)
        .push_next(&mut ycbcr);
    if video_maintenance1 {
        features = features.push_next(&mut maintenance);
    }
    // SAFETY: `physical` was enumerated from `instance`, and every structure in the chain outlives
    // the call.
    unsafe { instance.get_physical_device_features2(physical, &mut features) };
    FfmpegDeviceFeatures {
        synchronization2: synchronization2.synchronization2 == vk::TRUE,
        sampler_ycbcr_conversion: ycbcr.sampler_ycbcr_conversion == vk::TRUE,
        video_maintenance1: video_maintenance1 && maintenance.video_maintenance1 == vk::TRUE,
    }
}

impl FfmpegDeviceFeatures {
    /// Adds the supported features to the device's creation chain.
    ///
    /// wgpu builds its own feature structures after this callback and uses
    /// none of these three, so they are appended rather than merged. They
    /// live for the process: the chain is read after the callback returns.
    fn enable(self, create_info: &mut vk::DeviceCreateInfo<'_>) {
        let mut info = std::mem::take(create_info);
        if self.synchronization2 {
            info = info.push_next(Box::leak(Box::new(
                vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true),
            )));
        }
        if self.sampler_ycbcr_conversion {
            info = info.push_next(Box::leak(Box::new(
                vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default()
                    .sampler_ycbcr_conversion(true),
            )));
        }
        if self.video_maintenance1 {
            info = info.push_next(Box::leak(Box::new(
                vk::PhysicalDeviceVideoMaintenance1FeaturesKHR::default().video_maintenance1(true),
            )));
        }
        *create_info = info;
    }
}

/// One Vulkan logical device adopted by both the renderer and `FFmpeg`.
#[derive(Clone)]
pub struct SharedVulkanContext {
    // Fields drop in order: the idle import tickets are Vulkan objects of the device, so they go
    // while it still exists.
    import_tickets: Arc<ImportTicketPool>,
    // Drop FFmpeg references before wgpu destroys the raw device.
    ffmpeg_device: AVHWDeviceContext,
    gpu: GpuContext,
    submission_batch: Arc<VulkanSubmissionBatch>,
    identity: NativeGpuIdentity,
    decode_codecs: Vec<&'static str>,
    encode_codecs: Vec<&'static str>,
    encode_extent: Option<VideoEncodeExtent>,
    validation_layer: Option<VulkanLayerVersion>,
}

impl fmt::Debug for SharedVulkanContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedVulkanContext")
            .field("identity", &self.identity)
            .field("decode_codecs", &self.decode_codecs)
            .field("encode_codecs", &self.encode_codecs)
            .finish_non_exhaustive()
    }
}

impl SharedVulkanContext {
    /// Creates a Vulkan 1.3 device with wgpu, video-decode, and NV12 support.
    ///
    /// # Errors
    /// Returns a capability or initialization failure suitable for auto-mode fallback.
    #[allow(clippy::too_many_lines)]
    pub fn create() -> Result<Self, SharedVulkanError> {
        let _turn = alpha2_gpu::serialize_acquisition();
        let hal_instance = hal_instance()?;
        // SAFETY: adapter enumeration does not require a surface.
        let mut adapters = unsafe { hal_instance.enumerate_adapters(None) };
        let position = choose_adapter(
            &mut adapters,
            std::env::var("ALPHA2_VULKAN_DEVICE").ok().as_deref(),
        )?;
        let exposed = adapters.swap_remove(position);
        let physical = exposed.adapter.raw_physical_device();
        let shared_instance = exposed.adapter.shared_instance();
        let raw_instance = shared_instance.raw_instance().handle();
        let validation_layer = activated_validation_layer(shared_instance.raw_instance(), physical);
        // SAFETY: `physical` was enumerated from this instance.
        let queue_properties = unsafe {
            shared_instance
                .raw_instance()
                .get_physical_device_queue_family_properties(physical)
        };
        let graphics_family = u32::try_from(
            queue_properties
                .iter()
                .position(|family| {
                    family.queue_flags.contains(vk::QueueFlags::GRAPHICS) && family.queue_count > 0
                })
                .ok_or_else(|| SharedVulkanError("Vulkan adapter has no graphics queue".into()))?,
        )
        .map_err(|_| SharedVulkanError("graphics queue index is out of range".into()))?;
        if graphics_family != 0 {
            return Err(SharedVulkanError(
                "wgpu-hal shared-device path requires graphics queue family 0".into(),
            ));
        }
        let graphics_flags = queue_properties[0].queue_flags;
        if !graphics_flags.contains(vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER) {
            return Err(SharedVulkanError(
                "graphics queue family lacks FFmpeg compute/transfer capabilities".into(),
            ));
        }
        let decode_family = u32::try_from(
            queue_properties
                .iter()
                .position(|family| {
                    family
                        .queue_flags
                        .contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                        && family.queue_count > 0
                })
                .ok_or_else(|| {
                    SharedVulkanError("Vulkan adapter has no video-decode queue".into())
                })?,
        )
        .map_err(|_| SharedVulkanError("video-decode queue index is out of range".into()))?;
        if decode_family == graphics_family {
            return Err(SharedVulkanError(
                "shared Vulkan device requires a decode queue separate from wgpu graphics".into(),
            ));
        }
        // The encode family is optional: a device that decodes but cannot encode still runs every
        // other path.
        let encode_family = queue_properties
            .iter()
            .position(|family| {
                family
                    .queue_flags
                    .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                    && family.queue_count > 0
            })
            .and_then(|index| u32::try_from(index).ok());
        // SAFETY: `physical` was enumerated from this instance.
        let extension_properties = unsafe {
            shared_instance
                .raw_instance()
                .enumerate_device_extension_properties(physical)
        }
        .map_err(|error| SharedVulkanError(format!("Vulkan extensions: {error}")))?;
        let supports = |name: &CStr| {
            extension_properties.iter().any(|property| {
                property
                    .extension_name_as_c_str()
                    .is_ok_and(|candidate| candidate == name)
            })
        };
        let mut video_extensions = vec![
            ash::khr::video_queue::NAME,
            ash::khr::video_decode_queue::NAME,
        ];
        for extension in [
            ash::khr::video_decode_h264::NAME,
            ash::khr::video_decode_h265::NAME,
            ash::khr::video_decode_av1::NAME,
        ] {
            if supports(extension) {
                video_extensions.push(extension);
            }
        }
        if !video_extensions.iter().all(|extension| supports(extension))
            || !video_extensions.contains(&ash::khr::video_decode_h264::NAME)
        {
            return Err(SharedVulkanError(
                "Vulkan H.264 video decode is unavailable".into(),
            ));
        }
        // FFmpeg's Vulkan encoder refuses to open without both of these, and wgpu asks for neither,
        // so they are added to the device it creates. `video_maintenance1` is what lets the encoder
        // use a video session without pre-allocating its own DPB images.
        let encode_extensions = [
            ash::khr::video_encode_queue::NAME,
            ash::khr::video_maintenance1::NAME,
            ash::khr::video_encode_h264::NAME,
        ];
        let encode_family = encode_family.filter(|_| {
            encode_extensions
                .iter()
                .all(|extension| supports(extension))
        });
        // Asked only now that the device is known to expose the video queue and H.264 encode
        // extensions: the query is an extension entry point, and a driver without them has no
        // function behind the loader's trampoline. Asked here because `shared_instance` borrows the
        // adapter, which is moved into wgpu once the device exists.
        let h264_extent = encode_family.and_then(|_| {
            h264_encode_extent(
                shared_instance.entry(),
                shared_instance.raw_instance(),
                physical,
            )
        });
        if let Some(family) = encode_family {
            video_extensions.extend_from_slice(&encode_extensions);
            let _ = family;
        }
        let mut decode_caps = vk::VideoCodecOperationFlagsKHR::empty();
        let mut decode_codecs = Vec::new();
        for (extension, cap, label) in [
            (
                ash::khr::video_decode_h264::NAME,
                vk::VideoCodecOperationFlagsKHR::DECODE_H264,
                "h264",
            ),
            (
                ash::khr::video_decode_h265::NAME,
                vk::VideoCodecOperationFlagsKHR::DECODE_H265,
                "hevc",
            ),
            (
                ash::khr::video_decode_av1::NAME,
                vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
                "av1",
            ),
        ] {
            if video_extensions.contains(&extension) {
                decode_caps |= cap;
                decode_codecs.push(label);
            }
        }
        // P010 is optional: an adapter without it still decodes 8-bit streams, and 10-bit sources
        // fall back to software on that device.
        let required_features = (exposed.features
            & (wgpu::Features::TIMESTAMP_QUERY
                | wgpu::Features::TEXTURE_FORMAT_P010
                | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM))
            | wgpu::Features::TEXTURE_FORMAT_NV12;
        let required_limits = wgpu::Limits::downlevel_webgl2_defaults()
            .using_resolution(exposed.capabilities.limits.clone());
        let enabled_extensions = exposed
            .adapter
            .required_device_extensions(required_features);
        let core = exposed
            .adapter
            .physical_device_features(&enabled_extensions, required_features)
            .get_core();
        let ffmpeg_features = ffmpeg_device_features(
            shared_instance.raw_instance(),
            physical,
            video_extensions.contains(&ash::khr::video_maintenance1::NAME),
        );
        let extra_family = decode_family;
        let extra_encode_family =
            encode_family.filter(|family| *family != graphics_family && *family != decode_family);
        let extra_extensions = video_extensions.clone();
        // SAFETY: all added extensions and queues were queried from this adapter.
        let open = unsafe {
            exposed.adapter.open_with_callback(
                required_features,
                &required_limits,
                &wgpu::MemoryHints::Performance,
                Some(Box::new(move |args| {
                    ffmpeg_features.enable(args.create_info);
                    for extension in extra_extensions {
                        if !args.extensions.contains(&extension) {
                            args.extensions.push(extension);
                        }
                    }
                    if extra_family != 0 {
                        args.queue_create_infos.push(
                            vk::DeviceQueueCreateInfo::default()
                                .queue_family_index(extra_family)
                                .queue_priorities(&[1.0]),
                        );
                    }
                    if let Some(family) = extra_encode_family {
                        args.queue_create_infos.push(
                            vk::DeviceQueueCreateInfo::default()
                                .queue_family_index(family)
                                .queue_priorities(&[1.0]),
                        );
                    }
                })),
            )
        }
        .map_err(|error| SharedVulkanError(format!("shared Vulkan device: {error}")))?;
        let raw_device = open.device.raw_device().handle();
        let device_extensions = open
            .device
            .enabled_device_extensions()
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let instance_extensions = shared_instance
            .extensions()
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        // SAFETY: the instance, physical device, and device are the ones wgpu just opened with
        // these queues and extensions, and `Self` drops the `FFmpeg` context before wgpu destroys
        // the device.
        let ffmpeg_device = unsafe {
            create_vulkan_device_context(&VulkanDeviceInfo {
                instance: raw_instance.as_raw(),
                physical_device: physical.as_raw(),
                device: raw_device.as_raw(),
                graphics_queue_family: 0,
                graphics_queue_count: 1,
                decode_queue_family: decode_family,
                decode_queue_count: 1,
                decode_video_caps: decode_caps.as_raw(),
                encode_queue_family: encode_family.unwrap_or(0),
                encode_queue_count: u32::from(encode_family.is_some()),
                encode_video_caps: encode_family
                    .map_or(0, |_| vk::VideoCodecOperationFlagsKHR::ENCODE_H264.as_raw()),
                core_features: [
                    core.shader_image_gather_extended,
                    core.fragment_stores_and_atomics,
                    core.shader_int64,
                    core.vertex_pipeline_stores_and_atomics,
                ],
                synchronization2: u32::from(ffmpeg_features.synchronization2),
                sampler_ycbcr_conversion: u32::from(ffmpeg_features.sampler_ycbcr_conversion),
                video_maintenance1: u32::from(ffmpeg_features.video_maintenance1),
                instance_extensions,
                device_extensions,
            })
        }
        .map_err(|error| SharedVulkanError(format!("FFmpeg Vulkan device: {error}")))?;
        // SAFETY: the adapter and instance originate from the same HAL object.
        let wgpu_instance =
            unsafe { wgpu::Instance::from_hal::<wgpu_hal::api::Vulkan>(hal_instance) };
        // SAFETY: `exposed` was enumerated from the HAL instance `wgpu_instance` now wraps.
        let adapter = unsafe { wgpu_instance.create_adapter_from_hal(exposed) };
        let descriptor = wgpu::DeviceDescriptor {
            label: Some("alpha2 shared Vulkan decode"),
            required_features,
            required_limits,
            ..Default::default()
        };
        // SAFETY: OpenDevice has the descriptor's exact features and limits.
        let (device, queue) = unsafe { adapter.create_device_from_hal(open, &descriptor) }
            .map_err(|error| SharedVulkanError(format!("wgpu adoption: {error}")))?;
        // wgpu and FFmpeg submit to the same graphics queue, which Vulkan requires be externally
        // synchronized, so the renderer takes the same lock FFmpeg takes around its own
        // submissions.
        let queue_lock: Arc<dyn ExternalQueueLock> = Arc::new(SharedQueueLock(
            VulkanQueueLock::new(ffmpeg_device.clone(), 0)
                .map_err(|error| SharedVulkanError(format!("FFmpeg Vulkan queue lock: {error}")))?,
        ));
        let gpu = GpuContext::from_parts_with_queue_lock(adapter, device, queue, Some(queue_lock));
        Ok(Self {
            ffmpeg_device,
            gpu,
            submission_batch: Arc::new(VulkanSubmissionBatch::default()),
            import_tickets: Arc::new(ImportTicketPool::default()),
            identity: NativeGpuIdentity {
                physical_device: physical.as_raw(),
                device: raw_device.as_raw(),
                epoch: 1,
            },
            decode_codecs,
            encode_extent: h264_extent,
            encode_codecs: if encode_family.is_some() {
                vec!["h264"]
            } else {
                Vec::new()
            },
            validation_layer,
        })
    }

    /// Renderer context backed by the shared logical device.
    #[must_use]
    pub fn gpu(&self) -> GpuContext {
        self.gpu.clone()
    }

    /// `FFmpeg` device reference for one decoder session.
    #[must_use]
    pub fn ffmpeg_device(&self) -> AVHWDeviceContext {
        self.ffmpeg_device.clone()
    }

    /// Raw-device cache identity.
    #[must_use]
    pub const fn identity(&self) -> NativeGpuIdentity {
        self.identity
    }

    /// Whether this device can present 10-bit decoded planes.
    #[must_use]
    pub fn supports_ten_bit(&self) -> bool {
        self.gpu
            .device()
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_P010)
    }

    /// Decode codecs (`h264`, `hevc`, `av1`) whose extensions the device was
    /// created with.
    #[must_use]
    pub fn decode_codecs(&self) -> &[&'static str] {
        &self.decode_codecs
    }

    /// The Khronos validation layer loaded on this instance, and its version.
    ///
    /// Loaded means the loader activated it: wgpu enabled it (its validation
    /// flag, on in every debug build) or the environment injected it.
    #[must_use]
    pub const fn validation_layer(&self) -> Option<VulkanLayerVersion> {
        self.validation_layer
    }

    /// Why `FFmpeg` must not open a video session on this device, or `None`
    /// when it may.
    ///
    /// A validation layer older than [`VulkanLayerVersion::VIDEO_SESSIONS`]
    /// crashes or hangs inside the session setup, and a decode session is
    /// set up as an encode one is: hardware decode and the GPU encoder both
    /// ask this, so neither opens one the other is kept from.
    #[must_use]
    pub fn video_session_hazard(&self) -> Option<String> {
        self.validation_layer
            .filter(|layer| *layer < VulkanLayerVersion::VIDEO_SESSIONS)
            .map(|layer| {
                format!(
                    "the loaded Vulkan validation layer is {layer}, and layers older than {} \
                     crash in FFmpeg's video session setup",
                    VulkanLayerVersion::VIDEO_SESSIONS
                )
            })
    }

    /// Video-encode codecs this device created a queue and extensions for.
    ///
    /// Empty on an adapter with no encode queue, which every other path still
    /// runs on. `FFmpeg`'s Vulkan encoder refuses to open without the queue,
    /// so this is what decides whether that encoder is even a candidate.
    #[must_use]
    pub fn encode_codecs(&self) -> &[&'static str] {
        &self.encode_codecs
    }

    /// Output sizes this device's H.264 encoder accepts.
    ///
    /// `h264_vulkan` refuses anything outside it, and an export that asks for
    /// one silently takes the readback path instead. The limit is the
    /// device's, not the encoder list's: it comes from
    /// `vkGetPhysicalDeviceVideoCapabilitiesKHR`, and on the adapter this was
    /// developed against the minimum is 160x64.
    ///
    /// `None` when the device has no encode queue, or when the query itself
    /// failed, in which case no size can be promised either way.
    #[must_use]
    pub const fn encode_extent(&self) -> Option<&VideoEncodeExtent> {
        self.encode_extent.as_ref()
    }

    /// Allocation counters of the import tickets this device reuses.
    #[must_use]
    pub fn import_ticket_stats(&self) -> ImportTicketStats {
        self.import_tickets.stats()
    }

    /// Imports one same-device `FFmpeg` NV12 or P010 surface as plane views.
    ///
    /// The acquire transition waits on `FFmpeg`'s latest timeline value without
    /// a CPU fence wait. The returned lease must receive `submitted()` directly
    /// after the wgpu submission that samples the views.
    ///
    /// # Errors
    /// Returns an error for a different device, unsupported image layout, or a
    /// failed Vulkan transition submission.
    #[allow(clippy::too_many_lines)]
    pub fn import_surface(
        &self,
        frame: Arc<VulkanFrameLease>,
    ) -> Result<ImportedVulkanFrame, BridgeError> {
        let info = frame.info()?;
        if info.device != self.identity.device
            || info.physical_device != self.identity.physical_device
        {
            return Err(BridgeError::InvalidInput(
                "decoded frame belongs to another Vulkan device",
            ));
        }
        let layout = match (info.software_format, info.formats[0]) {
            (rsmpeg::ffi::AV_PIX_FMT_NV12, format)
                if format
                    == vk::Format::G8_B8R8_2PLANE_420_UNORM
                        .as_raw()
                        .cast_unsigned() =>
            {
                NativePlaneLayout::Nv12
            }
            (rsmpeg::ffi::AV_PIX_FMT_P010LE, format)
                if format
                    == vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
                        .as_raw()
                        .cast_unsigned() =>
            {
                NativePlaneLayout::P010
            }
            _ => {
                return Err(BridgeError::InvalidInput(
                    "only sampled single-image NV12 or P010 is importable",
                ));
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
                "shared Vulkan device does not expose P010 plane views",
            ));
        }
        if info.plane_count != 1 || !info.sampled || info.width <= 0 || info.height <= 0 {
            return Err(BridgeError::InvalidInput(
                "only sampled single-image NV12 or P010 is importable",
            ));
        }
        // The image comes from the frames context, so its extent is the padded coded size;
        // importing it as the display size would rescale every sample.
        if info.coded_width < info.width || info.coded_height < info.height {
            return Err(BridgeError::InvalidInput(
                "decoded frame is larger than its allocated image",
            ));
        }
        let coded_width = u32::try_from(info.coded_width)
            .map_err(|_| BridgeError::InvalidInput("invalid coded width"))?;
        let coded_height = u32::try_from(info.coded_height)
            .map_err(|_| BridgeError::InvalidInput("invalid coded height"))?;
        let access = frame.lock_access()?;
        let mut status = 0;
        // SAFETY: the guard holds FFmpeg's frame and graphics queue locks while C reads the frame's
        // state.
        let step =
            unsafe { alpha2_native_gpu_vulkan_acquire_check(access.access(), &raw mut status) };
        if step != 0 {
            return Err(helper_error("import check", step, status));
        }
        let (device, allocator, queue_family) = {
            let access = access.access();
            (
                access.frame().device,
                access.allocator(),
                access.queue_family(),
            )
        };
        drop(access);
        let ticket = self.import_tickets.take(device, allocator, queue_family)?;
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
        let hal_descriptor = wgpu_hal::TextureDescriptor {
            label: Some("alpha2 FFmpeg decode surface"),
            size: wgpu::Extent3d {
                width: coded_width,
                height: coded_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: texture_format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu_hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        // SAFETY: this context is always Vulkan and the frame identity check above established the
        // same logical device.
        let hal_device = unsafe { self.gpu.device().as_hal::<wgpu_hal::api::Vulkan>() }
            .ok_or(BridgeError::InvalidInput("renderer is not Vulkan"))?;
        let drop_owner = Arc::clone(&frame);
        // SAFETY: FFmpeg created the image with the validated descriptor and retains its allocation
        // through `drop_owner`; wgpu must not destroy it.
        let hal_texture = unsafe {
            hal_device.texture_from_raw(
                vk::Image::from_raw(info.images[0]),
                &hal_descriptor,
                Some(Box::new(move || drop(drop_owner))),
                wgpu_hal::vulkan::TextureMemory::External,
            )
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some("alpha2 FFmpeg decode surface"),
            size: hal_descriptor.size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: texture_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // SAFETY: the batched acquire transitions the whole image to RESOURCE's shader-read layout
        // before its first wgpu submission, and the HAL texture came from this device.
        let texture = unsafe {
            self.gpu
                .device()
                .create_texture_from_hal::<wgpu_hal::api::Vulkan>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::RESOURCE,
                )
        };
        let y = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("alpha2 FFmpeg decode Y"),
            format: Some(luma_format),
            aspect: wgpu::TextureAspect::Plane0,
            ..Default::default()
        });
        let uv = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("alpha2 FFmpeg decode UV"),
            format: Some(chroma_format),
            aspect: wgpu::TextureAspect::Plane1,
            ..Default::default()
        });
        let pixels = u64::from(coded_width).saturating_mul(u64::from(coded_height));
        let lease = Arc::new(VulkanImportLease {
            frame,
            _texture: texture,
            gpu: self.gpu.clone(),
            batch: Arc::clone(&self.submission_batch),
            tickets: Arc::clone(&self.import_tickets),
            ticket: Some(ticket),
            staged_signals: Mutex::new(Vec::new()),
            queued: AtomicBool::new(false),
            acquired: AtomicBool::new(false),
            released: AtomicBool::new(false),
            retained_bytes: pixels
                .saturating_mul(layout.bytes_per_pixel_numerator())
                .div_ceil(2),
        });
        Ok(ImportedVulkanFrame {
            y,
            uv,
            layout,
            coded_size: [coded_width, coded_height],
            lease,
        })
    }
}

/// The adapter the shared device opens among `adapters`, which this sorts in
/// the order it chooses in ([`adapter_rank`]): the first that supports NV12
/// plane views and, with a `selector` (`ALPHA2_VULKAN_DEVICE`), whose name
/// contains it.
fn choose_adapter(
    adapters: &mut [wgpu_hal::ExposedAdapter<wgpu_hal::api::Vulkan>],
    selector: Option<&str>,
) -> Result<usize, SharedVulkanError> {
    let preference = wgpu::PowerPreference::from_env();
    adapters.sort_by_key(|adapter| adapter_rank(adapter.info.device_type, preference));
    adapters
        .iter()
        .position(|adapter| {
            selector.is_none_or(|needle| adapter.info.name.contains(needle))
                && adapter
                    .features
                    .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        })
        .ok_or_else(|| {
            SharedVulkanError(selector.map_or_else(
                || "no Vulkan adapter supports NV12 plane views".to_owned(),
                |needle| {
                    format!(
                        "no Vulkan adapter whose name contains ALPHA2_VULKAN_DEVICE={needle:?} \
                         supports NV12 plane views"
                    )
                },
            ))
        })
}

/// Where the device [`SharedVulkanContext::create`] opens is in the
/// loader's physical-device order, for `selector` as `ALPHA2_VULKAN_DEVICE`
/// names it (`None` unset): the index `FFmpeg` takes for a Vulkan device of
/// its own (`av_hwdevice_ctx_create`'s `device`). A probe that opens its own
/// device passes this, so it opens the device an export renders, decodes, and
/// encodes on. `FFmpeg` reads a name that starts with a digit as an index,
/// so passing the variable on would pick another device, or none.
///
/// # Errors
/// Returns why no device is chosen: no Vulkan instance, or no adapter the
/// shared device could open.
pub fn shared_device_index(selector: Option<&str>) -> Result<usize, SharedVulkanError> {
    let _turn = alpha2_gpu::serialize_acquisition();
    let hal_instance = hal_instance()?;
    // SAFETY: adapter enumeration does not require a surface.
    let mut adapters = unsafe { hal_instance.enumerate_adapters(None) };
    let position = choose_adapter(&mut adapters, selector)?;
    let chosen = &adapters[position];
    let physical = chosen.adapter.raw_physical_device();
    // SAFETY: the instance is alive for the call; enumeration has no other precondition.
    let order = unsafe {
        chosen
            .adapter
            .shared_instance()
            .raw_instance()
            .enumerate_physical_devices()
    }
    .map_err(|error| SharedVulkanError(format!("Vulkan device enumeration: {error}")))?;
    order
        .iter()
        .position(|device| *device == physical)
        .ok_or_else(|| SharedVulkanError("the chosen adapter is not in the device list".into()))
}

/// Where an adapter of `device_type` comes in the shared device's choice,
/// lowest first: a discrete GPU before an integrated one, unless
/// `WGPU_POWER_PREF` asks for low power, which puts the integrated one first,
/// as `wgpu` orders the renderer's adapters. The shared device is the
/// renderer's on Linux, so ignoring the variable here would ignore it for
/// every export. `ALPHA2_VULKAN_DEVICE` still picks by name among them.
fn adapter_rank(device_type: wgpu::DeviceType, preference: Option<wgpu::PowerPreference>) -> u8 {
    let low_power = preference == Some(wgpu::PowerPreference::LowPower);
    match device_type {
        wgpu::DeviceType::IntegratedGpu if low_power => 0,
        wgpu::DeviceType::DiscreteGpu if low_power => 1,
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        _ => 2,
    }
}

#[cfg(test)]
#[path = "vulkan_failure_tests.rs"]
mod failure_tests;

#[cfg(test)]
#[path = "vulkan_tests.rs"]
mod tests;
