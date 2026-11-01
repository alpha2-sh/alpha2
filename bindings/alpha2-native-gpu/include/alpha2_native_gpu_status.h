#ifndef ALPHA2_NATIVE_GPU_STATUS_H
#define ALPHA2_NATIVE_GPU_STATUS_H

#include <stdint.h>

/*
 * How a native GPU helper reports a failure. Shared by c/vulkan.c, c/d3d12.c, c/metal.m and
 * src/status.rs.
 *
 * A helper returns 0 on success, or the negative step below that failed, and writes the platform
 * status at that step (VkResult, HRESULT, or CVReturn) to its `status` out-parameter; the
 * helper's own checks write 0. Callers classify the status by backend code, not by message.
 */

/* The helper's own checks. */
#define ALPHA2_GPU_STEP_ARGUMENTS (-1)
#define ALPHA2_GPU_STEP_UNSUPPORTED (-2)
#define ALPHA2_GPU_STEP_HOST_ALLOCATION (-3)
#define ALPHA2_GPU_STEP_LOADER (-4)
#define ALPHA2_GPU_STEP_QUEUE (-5)
#define ALPHA2_GPU_STEP_NO_OUTPUT (-6)

/* Vulkan calls. */
#define ALPHA2_GPU_STEP_CREATE_COMMAND_POOL (-10)
#define ALPHA2_GPU_STEP_ALLOCATE_COMMAND_BUFFERS (-11)
#define ALPHA2_GPU_STEP_CREATE_FENCE (-12)
#define ALPHA2_GPU_STEP_CREATE_SEMAPHORE (-13)
#define ALPHA2_GPU_STEP_CREATE_IMAGE_VIEW (-14)
#define ALPHA2_GPU_STEP_BEGIN_COMMAND_BUFFER (-15)
#define ALPHA2_GPU_STEP_END_COMMAND_BUFFER (-16)
#define ALPHA2_GPU_STEP_QUEUE_SUBMIT (-17)
#define ALPHA2_GPU_STEP_WAIT_FENCE (-18)
#define ALPHA2_GPU_STEP_WAIT_SEMAPHORE (-19)
#define ALPHA2_GPU_STEP_RESET_FENCE (-20)
#define ALPHA2_GPU_STEP_RESET_COMMAND_BUFFER (-21)
#define ALPHA2_GPU_STEP_RESET_COMMAND_POOL (-22)
#define ALPHA2_GPU_STEP_FENCE_STATUS (-23)

/* Direct3D 12 calls. */
#define ALPHA2_GPU_STEP_CREATE_RESOURCE (-30)
#define ALPHA2_GPU_STEP_CREATE_COMMAND_ALLOCATOR (-31)
#define ALPHA2_GPU_STEP_CREATE_COMMAND_LIST (-32)
#define ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST (-33)
#define ALPHA2_GPU_STEP_QUEUE_WAIT (-34)
/* Submitted but not fenced: the GPU may read the source surface indefinitely, so the caller must
   not give it back. */
#define ALPHA2_GPU_STEP_QUEUE_SIGNAL (-35)

/* CoreVideo and Metal calls. */
#define ALPHA2_GPU_STEP_CREATE_TEXTURE_FROM_IMAGE (-40)

/* Writes `result` to `status` when there is one, and returns `step`. */
#define ALPHA2_GPU_FAIL(status, step, result) \
    ((status) != NULL ? (void)(*(status) = (int32_t)(result)) : (void)0, (step))

#endif
