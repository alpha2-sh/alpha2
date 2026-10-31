//! A native GPU helper's failure as a typed error.
//!
//! Every helper returns the step that failed and writes the platform status
//! that step returned (`include/alpha2_native_gpu_status.h`). This reads what
//! that status means from its backend's own codes, so a lost device, exhausted
//! memory, or a timeout reaches the host as a [`GpuFault`], not as words in a
//! message. Every backend's table is compiled on every target, so the D3D12
//! and Metal codes are tested on Linux too.

use alpha2_ffmpeg_bridge::{BridgeError, GpuFault, NativeGpuBackend, NativeGpuFailure};

use crate::interop::{
    ALPHA2_GPU_STEP_ALLOCATE_COMMAND_BUFFERS, ALPHA2_GPU_STEP_ARGUMENTS,
    ALPHA2_GPU_STEP_BEGIN_COMMAND_BUFFER, ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST,
    ALPHA2_GPU_STEP_CREATE_COMMAND_ALLOCATOR, ALPHA2_GPU_STEP_CREATE_COMMAND_LIST,
    ALPHA2_GPU_STEP_CREATE_COMMAND_POOL, ALPHA2_GPU_STEP_CREATE_FENCE,
    ALPHA2_GPU_STEP_CREATE_IMAGE_VIEW, ALPHA2_GPU_STEP_CREATE_RESOURCE,
    ALPHA2_GPU_STEP_CREATE_SEMAPHORE, ALPHA2_GPU_STEP_CREATE_TEXTURE_FROM_IMAGE,
    ALPHA2_GPU_STEP_END_COMMAND_BUFFER, ALPHA2_GPU_STEP_FENCE_STATUS,
    ALPHA2_GPU_STEP_HOST_ALLOCATION, ALPHA2_GPU_STEP_LOADER, ALPHA2_GPU_STEP_NO_OUTPUT,
    ALPHA2_GPU_STEP_QUEUE, ALPHA2_GPU_STEP_QUEUE_SIGNAL, ALPHA2_GPU_STEP_QUEUE_SUBMIT,
    ALPHA2_GPU_STEP_QUEUE_WAIT, ALPHA2_GPU_STEP_RESET_COMMAND_BUFFER,
    ALPHA2_GPU_STEP_RESET_COMMAND_POOL, ALPHA2_GPU_STEP_RESET_FENCE, ALPHA2_GPU_STEP_UNSUPPORTED,
    ALPHA2_GPU_STEP_WAIT_FENCE, ALPHA2_GPU_STEP_WAIT_SEMAPHORE,
};

/// What the helper's step is called in a message.
const fn step_name(step: i32) -> &'static str {
    match step {
        ALPHA2_GPU_STEP_ARGUMENTS => "argument check",
        ALPHA2_GPU_STEP_UNSUPPORTED => "frame check",
        ALPHA2_GPU_STEP_HOST_ALLOCATION => "host allocation",
        ALPHA2_GPU_STEP_LOADER => "command loading",
        ALPHA2_GPU_STEP_QUEUE => "queue lookup",
        ALPHA2_GPU_STEP_NO_OUTPUT => "result check",
        ALPHA2_GPU_STEP_CREATE_COMMAND_POOL => "command pool creation",
        ALPHA2_GPU_STEP_ALLOCATE_COMMAND_BUFFERS => "command buffer allocation",
        ALPHA2_GPU_STEP_CREATE_FENCE => "fence creation",
        ALPHA2_GPU_STEP_CREATE_SEMAPHORE => "semaphore creation",
        ALPHA2_GPU_STEP_CREATE_IMAGE_VIEW => "image view creation",
        ALPHA2_GPU_STEP_BEGIN_COMMAND_BUFFER => "command buffer begin",
        ALPHA2_GPU_STEP_END_COMMAND_BUFFER => "command buffer end",
        ALPHA2_GPU_STEP_QUEUE_SUBMIT => "queue submit",
        ALPHA2_GPU_STEP_WAIT_FENCE => "fence wait",
        ALPHA2_GPU_STEP_WAIT_SEMAPHORE => "semaphore wait",
        ALPHA2_GPU_STEP_RESET_FENCE => "fence reset",
        ALPHA2_GPU_STEP_RESET_COMMAND_BUFFER => "command buffer reset",
        ALPHA2_GPU_STEP_RESET_COMMAND_POOL => "command pool reset",
        ALPHA2_GPU_STEP_FENCE_STATUS => "fence status",
        ALPHA2_GPU_STEP_CREATE_RESOURCE => "resource creation",
        ALPHA2_GPU_STEP_CREATE_COMMAND_ALLOCATOR => "command allocator creation",
        ALPHA2_GPU_STEP_CREATE_COMMAND_LIST => "command list creation",
        ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST => "command list close",
        ALPHA2_GPU_STEP_QUEUE_WAIT => "queue wait",
        ALPHA2_GPU_STEP_QUEUE_SIGNAL => "queue signal",
        ALPHA2_GPU_STEP_CREATE_TEXTURE_FROM_IMAGE => "texture creation",
        _ => "an unknown step",
    }
}

/// What a `VkResult` says about the device.
const fn vulkan_fault(status: i32) -> Option<GpuFault> {
    match status {
        // VK_TIMEOUT
        2 => Some(GpuFault::Timeout),
        // VK_ERROR_OUT_OF_HOST_MEMORY
        -1 => Some(GpuFault::HostMemory),
        // VK_ERROR_OUT_OF_DEVICE_MEMORY
        -2 => Some(GpuFault::DeviceMemory),
        // VK_ERROR_DEVICE_LOST
        -4 => Some(GpuFault::DeviceLost),
        _ => None,
    }
}

/// What an `HRESULT` from a D3D12 call says about the device.
#[allow(clippy::cast_possible_wrap)] // HRESULTs are the bits of these u32 codes.
const fn d3d12_fault(status: i32) -> Option<GpuFault> {
    const E_OUTOFMEMORY: i32 = 0x8007_000E_u32 as i32;
    const DXGI_ERROR_DEVICE_REMOVED: i32 = 0x887A_0005_u32 as i32;
    const DXGI_ERROR_DEVICE_HUNG: i32 = 0x887A_0006_u32 as i32;
    const DXGI_ERROR_DEVICE_RESET: i32 = 0x887A_0007_u32 as i32;
    const DXGI_ERROR_DRIVER_INTERNAL_ERROR: i32 = 0x887A_0020_u32 as i32;
    // What a fence wait that ran out of time reports (`c/d3d12_encode.c`): `WaitForSingleObject`'s
    // WAIT_TIMEOUT, which no failing HRESULT is.
    const WAIT_TIMEOUT: i32 = 0x102;
    match status {
        WAIT_TIMEOUT => Some(GpuFault::Timeout),
        // What D3D12 returns when a resource, allocator, or list does not fit: video memory on a
        // discrete GPU.
        E_OUTOFMEMORY => Some(GpuFault::DeviceMemory),
        // Windows reset the device because a command ran past its timeout detection.
        DXGI_ERROR_DEVICE_HUNG => Some(GpuFault::DeviceHung),
        DXGI_ERROR_DEVICE_REMOVED | DXGI_ERROR_DEVICE_RESET | DXGI_ERROR_DRIVER_INTERNAL_ERROR => {
            Some(GpuFault::DeviceLost)
        }
        _ => None,
    }
}

/// What a `CVReturn` from `CoreVideo`'s Metal texture cache says about the device.
const fn metal_fault(status: i32) -> Option<GpuFault> {
    match status {
        // kCVReturnAllocationFailed: the texture's memory, which on the unified-memory Macs this
        // path accepts is the GPU's.
        -6662 => Some(GpuFault::DeviceMemory),
        _ => None,
    }
}

/// The error for `operation`'s failure at `step` with `status`, on `backend`.
///
/// A refusal by the helper's own checks of what it was given is invalid
/// input; everything else is a [`BridgeError::NativeGpu`] carrying the step,
/// the raw status and, where the status names one, its fault.
pub(crate) fn helper_failure(
    backend: NativeGpuBackend,
    operation: &'static str,
    step: i32,
    status: i32,
) -> BridgeError {
    match step {
        ALPHA2_GPU_STEP_ARGUMENTS => {
            return BridgeError::InvalidInput("native GPU helper arguments do not belong together");
        }
        ALPHA2_GPU_STEP_UNSUPPORTED => {
            return BridgeError::InvalidInput("frame is not one the native GPU helper handles");
        }
        _ => {}
    }
    let fault = if step == ALPHA2_GPU_STEP_HOST_ALLOCATION {
        Some(GpuFault::HostMemory)
    } else {
        match backend {
            NativeGpuBackend::Vulkan => vulkan_fault(status),
            NativeGpuBackend::D3D12 => d3d12_fault(status),
            NativeGpuBackend::Metal => metal_fault(status),
        }
    };
    BridgeError::NativeGpu(NativeGpuFailure {
        backend,
        operation,
        step: step_name(step),
        status,
        fault,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fault_of(error: &BridgeError) -> Option<GpuFault> {
        error.gpu_fault()
    }

    /// A device Windows reset for a command that ran too long is lost to the
    /// work it was given; one removed or reset for another reason is lost.
    #[test]
    fn d3d12_device_hung_is_its_own_fault() {
        #[allow(clippy::cast_possible_wrap)]
        let hresult = |code: u32| code as i32;
        assert_eq!(
            d3d12_fault(hresult(0x887A_0006)),
            Some(GpuFault::DeviceHung)
        );
        for lost in [0x887A_0005, 0x887A_0007, 0x887A_0020] {
            assert_eq!(
                d3d12_fault(hresult(lost)),
                Some(GpuFault::DeviceLost),
                "{lost:#x}"
            );
        }
    }

    #[test]
    fn a_helper_status_names_its_backend_step_and_fault() {
        // A helper status is not an FFmpeg code: it must name the backend, the step and the fault,
        // never read as "FFmpeg error code N".
        let vulkan =
            |step, status| helper_failure(NativeGpuBackend::Vulkan, "encode copy", step, status);
        let cases = [
            (
                vulkan(ALPHA2_GPU_STEP_QUEUE_SUBMIT, -4),
                Some(GpuFault::DeviceLost),
            ),
            (
                vulkan(ALPHA2_GPU_STEP_QUEUE_SUBMIT, -2),
                Some(GpuFault::DeviceMemory),
            ),
            (
                vulkan(ALPHA2_GPU_STEP_CREATE_FENCE, -1),
                Some(GpuFault::HostMemory),
            ),
            (
                vulkan(ALPHA2_GPU_STEP_WAIT_FENCE, 2),
                Some(GpuFault::Timeout),
            ),
            (
                vulkan(ALPHA2_GPU_STEP_HOST_ALLOCATION, 0),
                Some(GpuFault::HostMemory),
            ),
            // VK_ERROR_INITIALIZATION_FAILED names no fault.
            (vulkan(ALPHA2_GPU_STEP_CREATE_COMMAND_POOL, -3), None),
            (vulkan(ALPHA2_GPU_STEP_LOADER, 0), None),
        ];
        for (error, fault) in &cases {
            assert_eq!(fault_of(error), *fault, "{error}");
            let message = error.to_string();
            assert!(!message.contains("FFmpeg"), "{message}");
            assert!(message.starts_with("Vulkan encode copy"), "{message}");
        }
        assert_eq!(
            cases[0].0.to_string(),
            "Vulkan encode copy failed at queue submit (VkResult -4)"
        );
        // The step and the status are in the message; the fault is not, so nothing classifies it by
        // words.
        assert!(!cases[1].0.to_string().contains("memory"));
        assert!(
            cases[3]
                .0
                .to_string()
                .contains("GPU work did not complete within the wait timeout")
        );

        let d3d12 = |status: u32| {
            helper_failure(
                NativeGpuBackend::D3D12,
                "surface copy",
                ALPHA2_GPU_STEP_CREATE_RESOURCE,
                status.cast_signed(),
            )
        };
        let removed = d3d12(0x887A_0005);
        assert_eq!(fault_of(&removed), Some(GpuFault::DeviceLost));
        assert_eq!(
            removed.to_string(),
            "D3D12 surface copy failed at resource creation (HRESULT 0x887A0005)"
        );
        assert_eq!(fault_of(&d3d12(0x887A_0006)), Some(GpuFault::DeviceHung));
        for lost in [0x887A_0007, 0x887A_0020] {
            assert_eq!(fault_of(&d3d12(lost)), Some(GpuFault::DeviceLost));
        }
        assert_eq!(fault_of(&d3d12(0x8007_000E)), Some(GpuFault::DeviceMemory));
        // E_INVALIDARG is a failure, not evidence about the device.
        assert_eq!(fault_of(&d3d12(0x8007_0057)), None);
        // An encode copier's fence wait that ran out of time.
        let timed_out = helper_failure(
            NativeGpuBackend::D3D12,
            "encode copy slot",
            ALPHA2_GPU_STEP_WAIT_FENCE,
            0x102,
        );
        assert_eq!(fault_of(&timed_out), Some(GpuFault::Timeout));

        let metal = |status| {
            helper_failure(
                NativeGpuBackend::Metal,
                "texture import",
                ALPHA2_GPU_STEP_CREATE_TEXTURE_FROM_IMAGE,
                status,
            )
        };
        assert_eq!(fault_of(&metal(-6662)), Some(GpuFault::DeviceMemory));
        assert_eq!(fault_of(&metal(-6683)), None);
        assert_eq!(
            metal(-6683).to_string(),
            "Metal texture import failed at texture creation (CVReturn -6683)"
        );

        // The helper's own refusal of what it was given is invalid input.
        assert!(matches!(
            vulkan(ALPHA2_GPU_STEP_ARGUMENTS, 0),
            BridgeError::InvalidInput(_)
        ));
        assert!(matches!(
            vulkan(ALPHA2_GPU_STEP_UNSUPPORTED, 0),
            BridgeError::InvalidInput(_)
        ));
    }

    #[test]
    fn an_ffmpeg_code_reads_as_ffmpeg_words_for_it() {
        // `FfmpegCode` is for AVERROR codes only, shown as `rsmpeg` shows every other FFmpeg
        // failure.
        let message = BridgeError::FfmpegCode(-28).to_string();
        assert!(message.starts_with("AVERROR(-28)"), "{message}");
        assert!(!message.contains("FFmpeg error code"), "{message}");
    }
}
