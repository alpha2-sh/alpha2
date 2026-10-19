#include <stdint.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdlib.h>
#include <vulkan/vulkan.h>
#include "alpha2_vulkan_interop.h"
#include "alpha2_vulkan_loader.h"
#include "alpha2_native_gpu_status.h"

/* Vulkan commands used here, resolved from the loader at run time (`alpha2_vulkan_loader.h`). */
#define ALPHA2_VK_COMMANDS(X) \
    X(AllocateCommandBuffers) \
    X(BeginCommandBuffer) \
    X(CmdCopyImage) \
    X(CmdPipelineBarrier) \
    X(CreateCommandPool) \
    X(CreateFence) \
    X(CreateImageView) \
    X(CreateSemaphore) \
    X(DestroyCommandPool) \
    X(DestroyFence) \
    X(DestroyImageView) \
    X(DestroySemaphore) \
    X(EndCommandBuffer) \
    X(GetDeviceQueue) \
    X(GetFenceStatus) \
    X(QueueSubmit) \
    X(ResetCommandBuffer) \
    X(ResetCommandPool) \
    X(ResetFences) \
    X(WaitForFences) \
    X(WaitSemaphores)

#define ALPHA2_VK_MEMBER(name) PFN_vk##name name;
static struct { ALPHA2_VK_COMMANDS(ALPHA2_VK_MEMBER) } alpha2_vk;
static int alpha2_vk_loaded;
static pthread_once_t alpha2_vk_once = PTHREAD_ONCE_INIT;

static void alpha2_vk_load(void) {
    int loaded = 1;
#define ALPHA2_VK_LOAD(name) \
    alpha2_vk.name = (PFN_vk##name)alpha2_vulkan_symbol("vk" #name); \
    loaded = loaded && alpha2_vk.name != NULL;
    ALPHA2_VK_COMMANDS(ALPHA2_VK_LOAD)
#undef ALPHA2_VK_LOAD
    alpha2_vk_loaded = loaded;
}

static int alpha2_vk_ready(void) {
    pthread_once(&alpha2_vk_once, alpha2_vk_load);
    return alpha2_vk_loaded;
}

#define vkAllocateCommandBuffers alpha2_vk.AllocateCommandBuffers
#define vkBeginCommandBuffer alpha2_vk.BeginCommandBuffer
#define vkCmdCopyImage alpha2_vk.CmdCopyImage
#define vkCmdPipelineBarrier alpha2_vk.CmdPipelineBarrier
#define vkCreateCommandPool alpha2_vk.CreateCommandPool
#define vkCreateFence alpha2_vk.CreateFence
#define vkCreateImageView alpha2_vk.CreateImageView
#define vkCreateSemaphore alpha2_vk.CreateSemaphore
#define vkDestroyCommandPool alpha2_vk.DestroyCommandPool
#define vkDestroyFence alpha2_vk.DestroyFence
#define vkDestroyImageView alpha2_vk.DestroyImageView
#define vkDestroySemaphore alpha2_vk.DestroySemaphore
#define vkEndCommandBuffer alpha2_vk.EndCommandBuffer
#define vkGetDeviceQueue alpha2_vk.GetDeviceQueue
#define vkGetFenceStatus alpha2_vk.GetFenceStatus
#define vkQueueSubmit alpha2_vk.QueueSubmit
#define vkResetCommandBuffer alpha2_vk.ResetCommandBuffer
#define vkResetCommandPool alpha2_vk.ResetCommandPool
#define vkResetFences alpha2_vk.ResetFences
#define vkWaitForFences alpha2_vk.WaitForFences
#define vkWaitSemaphores alpha2_vk.WaitSemaphores

/* Bounds every wait here, so a hung or lost device cannot hang a resident host. */
static _Atomic uint64_t alpha2_wait_timeout_ns = 30000000000ull;

/* Sets the timeout every later wait here uses; safe to call from any thread at any time. */
void alpha2_native_gpu_vulkan_set_wait_timeout(uint64_t nanoseconds) {
    atomic_store_explicit(&alpha2_wait_timeout_ns, nanoseconds, memory_order_relaxed);
}

static uint64_t alpha2_vulkan_wait_timeout(void) {
    return atomic_load_explicit(&alpha2_wait_timeout_ns, memory_order_relaxed);
}

/* The timeout `alpha2_native_gpu_vulkan_set_wait_timeout` stored. */
uint64_t alpha2_native_gpu_vulkan_wait_timeout(void) {
    return alpha2_vulkan_wait_timeout();
}

/* Helpers whose GPU work did not finish in time, left alive rather than freed under the GPU. */
static _Atomic uint64_t alpha2_retained_helpers;

static void alpha2_retain_unfinished(void) {
    atomic_fetch_add_explicit(&alpha2_retained_helpers, 1, memory_order_relaxed);
}

/* How many helpers this process has leaked so far because their GPU work did not finish in time. */
uint64_t alpha2_native_gpu_vulkan_retained_helpers(void) {
    return atomic_load_explicit(&alpha2_retained_helpers, memory_order_relaxed);
}

typedef struct Alpha2VulkanImportTicket {
    VkDevice device;
    const VkAllocationCallbacks *allocator;
    uint32_t queue_family;
    VkCommandPool pool;
    VkCommandBuffer acquire;
    VkCommandBuffer release;
    VkFence acquire_fence;
    VkFence release_fence;
    /*
     * Whether an acquire or a release was submitted with its fence since the ticket was created or
     * last reset. A ticket is finished only when every fence it submitted has signalled: an acquire
     * may still be waiting on the decoder's timeline when its lease is dropped.
     */
    int acquire_submitted;
    int release_submitted;
    /* The layout and access FFmpeg had each plane in at acquire, which release restores. */
    VkImageLayout restore_layouts[ALPHA2_MAX_VULKAN_PLANES];
    VkAccessFlags restore_access[ALPHA2_MAX_VULKAN_PLANES];
} Alpha2VulkanImportTicket;

static int alpha2_submit_transition(
    Alpha2VulkanFrameAccess *access, VkCommandBuffer command,
    const VkImageMemoryBarrier *barriers, uint32_t barrier_count,
    VkPipelineStageFlags source_stage, VkPipelineStageFlags destination_stage,
    VkFence fence, int32_t *status
) {
    VkDevice device = (VkDevice)(uintptr_t)access->frame.device;
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, access->queue_family, 0, &queue);
    if (queue == VK_NULL_HANDLE) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE, 0);
    VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    VkResult result = vkBeginCommandBuffer(command, &begin);
    if (result != VK_SUCCESS)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_BEGIN_COMMAND_BUFFER, result);
    vkCmdPipelineBarrier(command, source_stage, destination_stage, 0,
        0, NULL, 0, NULL, barrier_count, barriers);
    result = vkEndCommandBuffer(command);
    if (result != VK_SUCCESS)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_END_COMMAND_BUFFER, result);
    VkSemaphore semaphores[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint64_t wait_values[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint64_t signal_values[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint32_t semaphore_count = 0;
    for (size_t image = 0; image < ALPHA2_MAX_VULKAN_PLANES; ++image) {
        if (access->frame.images[image] == 0) continue;
        VkSemaphore semaphore = (VkSemaphore)(uintptr_t)access->frame.semaphores[image];
        if (semaphore == VK_NULL_HANDLE) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
        uint32_t slot = 0;
        while (slot < semaphore_count && semaphores[slot] != semaphore) ++slot;
        if (slot == semaphore_count) { semaphores[slot] = semaphore; ++semaphore_count; }
        if (wait_values[slot] <= access->frame.semaphore_values[image]) {
            wait_values[slot] = access->frame.semaphore_values[image];
            signal_values[slot] = wait_values[slot] + 1;
        }
    }
    VkPipelineStageFlags stages[ALPHA2_MAX_VULKAN_PLANES];
    for (uint32_t index = 0; index < semaphore_count; ++index)
        stages[index] = destination_stage;
    VkTimelineSemaphoreSubmitInfo timeline = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
        .waitSemaphoreValueCount = semaphore_count, .pWaitSemaphoreValues = wait_values,
        .signalSemaphoreValueCount = semaphore_count, .pSignalSemaphoreValues = signal_values};
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &timeline,
        .waitSemaphoreCount = semaphore_count, .pWaitSemaphores = semaphores,
        .pWaitDstStageMask = stages, .commandBufferCount = 1, .pCommandBuffers = &command,
        .signalSemaphoreCount = semaphore_count, .pSignalSemaphores = semaphores};
    result = vkQueueSubmit(queue, 1, &submit, fence);
    if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SUBMIT, result);
    for (size_t image = 0; image < ALPHA2_MAX_VULKAN_PLANES; ++image)
        for (uint32_t slot = 0; slot < semaphore_count; ++slot)
            if ((VkSemaphore)(uintptr_t)access->frame.semaphores[image] == semaphores[slot])
                access->frame.semaphore_values[image] = signal_values[slot];
    return 0;
}

/*
 * Checks that a frame can be imported: one sampled layer whose planes live on the renderer's queue
 * family and carry timeline semaphores.
 */
int alpha2_native_gpu_vulkan_acquire_check(const Alpha2VulkanFrameAccess *access, int32_t *status) {
    if (access == NULL || access->frame.sampled == 0 || access->layer_count != 1)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
        if (access->frame.images[index] == 0) continue;
        if (access->frame.queue_families[index] != VK_QUEUE_FAMILY_IGNORED &&
            access->frame.queue_families[index] != access->queue_family)
            return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
        if (access->frame.semaphores[index] == 0)
            return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    }
    return 0;
}

/*
 * Creates the command resources one import uses: a pool, its acquire and release command buffers,
 * and a fence for each. Nothing is recorded yet: the frame's layout and timeline value are only
 * read when it is acquired, under its lock, because FFmpeg may decode into it (as a reference) in
 * between. A ticket is reusable once its release has finished
 * (`alpha2_native_gpu_vulkan_ticket_poll`) and it has been reset.
 */
int alpha2_native_gpu_vulkan_ticket_create(
    uint64_t device, uint64_t allocator, uint32_t queue_family, void **output, int32_t *status
) {
    if (output == NULL || device == 0) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    if (!alpha2_vk_ready()) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_LOADER, 0);
    Alpha2VulkanImportTicket *ticket = calloc(1, sizeof(*ticket));
    if (ticket == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_HOST_ALLOCATION, 0);
    ticket->device = (VkDevice)(uintptr_t)device;
    ticket->allocator = (const VkAllocationCallbacks *)(uintptr_t)allocator;
    ticket->queue_family = queue_family;
    int step = ALPHA2_GPU_STEP_CREATE_COMMAND_POOL;
    /*
     * Each command buffer may be begun again after a submission of it failed, which leaves it
     * executable; only a resettable one may be (VUID-vkBeginCommandBuffer-commandBuffer-00050).
     */
    VkCommandPoolCreateInfo pool_info = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT | VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        .queueFamilyIndex = queue_family};
    VkResult result = vkCreateCommandPool(ticket->device, &pool_info, ticket->allocator, &ticket->pool);
    if (result != VK_SUCCESS) goto fail;
    step = ALPHA2_GPU_STEP_ALLOCATE_COMMAND_BUFFERS;
    VkCommandBufferAllocateInfo allocate = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = ticket->pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 2};
    VkCommandBuffer commands[2];
    result = vkAllocateCommandBuffers(ticket->device, &allocate, commands);
    if (result != VK_SUCCESS) goto fail;
    ticket->acquire = commands[0]; ticket->release = commands[1];
    step = ALPHA2_GPU_STEP_CREATE_FENCE;
    VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    result = vkCreateFence(ticket->device, &fence_info, ticket->allocator, &ticket->acquire_fence);
    if (result != VK_SUCCESS) goto fail;
    result = vkCreateFence(ticket->device, &fence_info, ticket->allocator, &ticket->release_fence);
    if (result != VK_SUCCESS) goto fail;
    *output = ticket;
    return 0;
fail:
    if (ticket->acquire_fence != VK_NULL_HANDLE) vkDestroyFence(ticket->device, ticket->acquire_fence, ticket->allocator);
    if (ticket->release_fence != VK_NULL_HANDLE) vkDestroyFence(ticket->device, ticket->release_fence, ticket->allocator);
    if (ticket->pool != VK_NULL_HANDLE) vkDestroyCommandPool(ticket->device, ticket->pool, ticket->allocator);
    free(ticket);
    return ALPHA2_GPU_FAIL(status, step, result);
}

/*
 * Without waiting: 1 when nothing the ticket submitted can still be running, 0 while its acquire or
 * release runs, and a negative step (with `status`) when the device can no longer say. Each
 * submission carries its own fence, so this asks the GPU rather than inferring it: an acquire whose
 * lease was dropped without the sampling submission it fed may still be waiting on the decoder.
 */
static int alpha2_fence_finished(const Alpha2VulkanImportTicket *ticket, int submitted, VkFence fence, int32_t *status) {
    if (!submitted) return 1;
    VkResult result = vkGetFenceStatus(ticket->device, fence);
    if (result == VK_SUCCESS) return 1;
    if (result == VK_NOT_READY) return 0;
    return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_FENCE_STATUS, result);
}

/* Whether a ticket's acquire and release have both finished. */
int alpha2_native_gpu_vulkan_ticket_poll(const void *opaque, int32_t *status) {
    const Alpha2VulkanImportTicket *ticket = opaque;
    if (ticket == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    int acquired = alpha2_fence_finished(ticket, ticket->acquire_submitted, ticket->acquire_fence, status);
    if (acquired != 1) return acquired;
    return alpha2_fence_finished(ticket, ticket->release_submitted, ticket->release_fence, status);
}

/*
 * Makes a ticket that `alpha2_native_gpu_vulkan_ticket_poll` reported finished ready for another
 * import: its fences unsignalled, its command buffers back to the initial state.
 */
int alpha2_native_gpu_vulkan_ticket_reset(void *opaque, int32_t *status) {
    Alpha2VulkanImportTicket *ticket = opaque;
    if (ticket == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    VkResult result;
    if (ticket->acquire_submitted) {
        result = vkResetFences(ticket->device, 1, &ticket->acquire_fence);
        if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_RESET_FENCE, result);
        ticket->acquire_submitted = 0;
    }
    if (ticket->release_submitted) {
        result = vkResetFences(ticket->device, 1, &ticket->release_fence);
        if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_RESET_FENCE, result);
        ticket->release_submitted = 0;
    }
    result = vkResetCommandPool(ticket->device, ticket->pool, 0);
    if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_RESET_COMMAND_POOL, result);
    return 0;
}

/*
 * Moves every plane to SHADER_READ_ONLY for wgpu and submits that now.
 *
 * The caller holds the frame's lock and has just read its state, so the barrier starts from the
 * layout the image is really in and the wait is on the timeline value FFmpeg last signalled.
 * Recording this at import instead, and submitting it later under another frame's lock, would wait
 * on a stale value and signal one FFmpeg may already have passed: a timeline that goes backwards
 * (VUID-VkSubmitInfo-pSignalSemaphores-03242). On success `access` holds the new layout, access
 * mask, and timeline value for the commit.
 */
int alpha2_native_gpu_vulkan_acquire_submit(Alpha2VulkanFrameAccess *access, void *opaque, int32_t *status) {
    Alpha2VulkanImportTicket *ticket = opaque;
    if (access == NULL || ticket == NULL ||
        (VkDevice)(uintptr_t)access->frame.device != ticket->device ||
        access->queue_family != ticket->queue_family)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    VkImageMemoryBarrier barriers[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint32_t barrier_count = 0;
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
        if (access->frame.images[index] == 0) continue;
        ticket->restore_layouts[index] = (VkImageLayout)access->frame.layouts[index];
        ticket->restore_access[index] = (VkAccessFlags)access->frame.access[index];
        barriers[barrier_count++] = (VkImageMemoryBarrier){
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = access->frame.access[index], .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
            .oldLayout = (VkImageLayout)access->frame.layouts[index],
            .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = (VkImage)(uintptr_t)access->frame.images[index],
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    }
    int step = alpha2_submit_transition(access, ticket->acquire, barriers, barrier_count,
        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
        ticket->acquire_fence, status);
    if (step != 0) return step;
    ticket->acquire_submitted = 1;
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
        if (access->frame.images[index] == 0) continue;
        access->frame.access[index] = VK_ACCESS_SHADER_READ_BIT;
        access->frame.layouts[index] = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    }
    return 0;
}

/*
 * Returns every plane to the layout it was acquired from, after wgpu's sampling, and signals the
 * timeline when that is done. Like the acquire it starts from the frame's current state, which the
 * caller has just read under its lock.
 */
int alpha2_native_gpu_vulkan_release(Alpha2VulkanFrameAccess *access, void *opaque, int32_t *status) {
    Alpha2VulkanImportTicket *ticket = opaque;
    if (access == NULL || ticket == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    VkImageMemoryBarrier barriers[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint32_t barrier_count = 0;
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
        if (access->frame.images[index] == 0) continue;
        barriers[barrier_count++] = (VkImageMemoryBarrier){
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = access->frame.access[index],
            .dstAccessMask = ticket->restore_access[index],
            .oldLayout = (VkImageLayout)access->frame.layouts[index],
            .newLayout = ticket->restore_layouts[index],
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = (VkImage)(uintptr_t)access->frame.images[index],
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    }
    int step = alpha2_submit_transition(access, ticket->release, barriers, barrier_count,
        VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
        ticket->release_fence, status);
    if (step == 0) {
        ticket->release_submitted = 1;
        for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
            if (access->frame.images[index] == 0) continue;
            access->frame.access[index] = (uint32_t)ticket->restore_access[index];
            access->frame.layouts[index] = (uint32_t)ticket->restore_layouts[index];
        }
    }
    return step;
}

/*
 * Raises `signal_value` on a frame's timeline once it reaches `wait_value`, from the graphics
 * queue, with nothing else in the batch.
 *
 * Keeps a promise the import made to FFmpeg: the frame records the value the renderer's sampling
 * submission was to signal, and a decode that used the frame since has queued a wait on it. When
 * that submission will never come, this submission signals the value instead, after the acquire it
 * follows. The caller holds the frame's lock, which holds the queue's.
 */
int alpha2_native_gpu_vulkan_signal(
    Alpha2VulkanFrameAccess *access, uint64_t semaphore, uint64_t wait_value, uint64_t signal_value,
    int32_t *status
) {
    if (access == NULL || semaphore == 0 || signal_value <= wait_value)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    if (!alpha2_vk_ready()) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_LOADER, 0);
    VkDevice device = (VkDevice)(uintptr_t)access->frame.device;
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, access->queue_family, 0, &queue);
    if (queue == VK_NULL_HANDLE) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE, 0);
    VkSemaphore timeline_semaphore = (VkSemaphore)(uintptr_t)semaphore;
    VkPipelineStageFlags stage = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
    VkTimelineSemaphoreSubmitInfo timeline = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
        .waitSemaphoreValueCount = 1, .pWaitSemaphoreValues = &wait_value,
        .signalSemaphoreValueCount = 1, .pSignalSemaphoreValues = &signal_value};
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &timeline,
        .waitSemaphoreCount = 1, .pWaitSemaphores = &timeline_semaphore, .pWaitDstStageMask = &stage,
        .signalSemaphoreCount = 1, .pSignalSemaphores = &timeline_semaphore};
    VkResult result = vkQueueSubmit(queue, 1, &submit, VK_NULL_HANDLE);
    if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SUBMIT, result);
    return 0;
}

/* Frees a ticket; the caller guarantees nothing it submitted can still run. */
void alpha2_native_gpu_vulkan_import_destroy(void *opaque) {
    Alpha2VulkanImportTicket *ticket = opaque;
    if (ticket == NULL) return;
    vkDestroyFence(ticket->device, ticket->acquire_fence, ticket->allocator);
    vkDestroyFence(ticket->device, ticket->release_fence, ticket->allocator);
    vkDestroyCommandPool(ticket->device, ticket->pool, ticket->allocator);
    free(ticket);
}

/*
 * Frees a ticket once every fence it submitted has signalled, waiting at most `timeout_ns` for
 * them all together (0 only looks), and leaks it instead when one has not: the GPU may still be
 * running its commands. Returns 0 when it was freed, 1 when it was leaked.
 */
int alpha2_native_gpu_vulkan_import_retire(void *opaque, uint64_t timeout_ns) {
    Alpha2VulkanImportTicket *ticket = opaque;
    if (ticket == NULL) return 0;
    /* Only a submitted fence can signal; waiting on another would run out the whole timeout. */
    VkFence fences[2];
    uint32_t count = 0;
    if (ticket->acquire_submitted) fences[count++] = ticket->acquire_fence;
    if (ticket->release_submitted) fences[count++] = ticket->release_fence;
    if (count != 0 &&
        vkWaitForFences(ticket->device, count, fences, VK_TRUE, timeout_ns) != VK_SUCCESS) {
        /* The GPU may still be using these; leaking them is the safe loss. */
        alpha2_retain_unfinished();
        return 1;
    }
    alpha2_native_gpu_vulkan_import_destroy(ticket);
    return 0;
}

/*
 * The formats an import views the planes of a `format` image in, as `ImportedVulkanFrame` does, and
 * how many: none for an image it does not import.
 */
static uint32_t alpha2_import_plane_formats(VkFormat format, VkFormat planes[2]) {
    switch (format) {
    case VK_FORMAT_G8_B8R8_2PLANE_420_UNORM:
        planes[0] = VK_FORMAT_R8_UNORM;
        planes[1] = VK_FORMAT_R8G8_UNORM;
        return 2;
    case VK_FORMAT_G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16:
        planes[0] = VK_FORMAT_R16_UNORM;
        planes[1] = VK_FORMAT_R16G16_UNORM;
        return 2;
    default:
        return 0;
    }
}

/*
 * Moves a decoded frame to SHADER_READ_ONLY and back in one fenced submission, and waits for it.
 * Once that is submitted, the frame's descriptor records the timeline values it signals, whether
 * the wait then succeeds or not.
 *
 * A wait that does not succeed leaves the submission pending, so its fence and command pool are
 * left alive rather than destroyed under it (VUID-vkDestroyFence-fence-01120,
 * VUID-vkDestroyCommandPool-commandPool-00041), as every other helper here does.
 */
int alpha2_native_gpu_vulkan_probe(Alpha2VulkanFrameAccess *access, int32_t *status) {
    if (access == NULL || access->frame.sampled == 0 || access->layer_count == 0)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    if (!alpha2_vk_ready()) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_LOADER, 0);
    VkDevice device = (VkDevice)(uintptr_t)access->frame.device;
    const VkAllocationCallbacks *allocator =
        (const VkAllocationCallbacks *)(uintptr_t)access->allocator;
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, access->queue_family, 0, &queue);
    if (queue == VK_NULL_HANDLE) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE, 0);

    VkCommandPool pool = VK_NULL_HANDLE;
    VkCommandBuffer command = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    VkResult result;
    /* A submission is on the GPU not seen to finish. Set before any jump to the cleanup. */
    int pending = 0;
    int step = ALPHA2_GPU_STEP_CREATE_COMMAND_POOL;
    VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT,
        .queueFamilyIndex = access->queue_family,
    };
    result = vkCreateCommandPool(device, &pool_info, allocator, &pool);
    if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, step, result);
    step = ALPHA2_GPU_STEP_ALLOCATE_COMMAND_BUFFERS;
    VkCommandBufferAllocateInfo allocate_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    result = vkAllocateCommandBuffers(device, &allocate_info, &command);
    if (result != VK_SUCCESS) goto cleanup;
    step = ALPHA2_GPU_STEP_CREATE_FENCE;
    VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    result = vkCreateFence(device, &fence_info, allocator, &fence);
    if (result != VK_SUCCESS) goto cleanup;

    VkImageMemoryBarrier acquire[ALPHA2_MAX_VULKAN_PLANES] = {0};
    VkImageMemoryBarrier release[ALPHA2_MAX_VULKAN_PLANES] = {0};
    VkSemaphore semaphores[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint64_t wait_values[ALPHA2_MAX_VULKAN_PLANES] = {0};
    uint64_t signal_values[ALPHA2_MAX_VULKAN_PLANES] = {0};
    VkImageView views[ALPHA2_MAX_VULKAN_PLANES][2] = {{0}};
    uint32_t barrier_count = 0, semaphore_count = 0;
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
        if (access->frame.images[index] == 0) continue;
        if (access->frame.semaphores[index] == 0 || access->frame.formats[index] == 0 ||
            (access->frame.queue_families[index] != VK_QUEUE_FAMILY_IGNORED &&
             access->frame.queue_families[index] != access->queue_family)) {
            step = ALPHA2_GPU_STEP_UNSUPPORTED;
            result = VK_SUCCESS;
            goto cleanup_views;
        }
        VkImage image = (VkImage)(uintptr_t)access->frame.images[index];
        /*
         * The views an import makes: each plane of a single NV12 or P010 image in its own format,
         * sampled only. A view of the whole multi-planar image would need a sampler Y'CbCr
         * conversion (VUID-VkImageViewCreateInfo-format-06415), and one inheriting the image's
         * STORAGE usage a format that supports it (-usage-02275); import makes neither, and nothing
         * else is importable to view.
         */
        VkFormat plane_formats[2];
        uint32_t plane_count = alpha2_import_plane_formats(
            (VkFormat)access->frame.formats[index], plane_formats);
        for (uint32_t plane = 0; plane < plane_count; ++plane) {
            VkImageViewUsageCreateInfo usage = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_USAGE_CREATE_INFO,
                .usage = VK_IMAGE_USAGE_SAMPLED_BIT,
            };
            VkImageViewCreateInfo view_info = {
                .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
                .pNext = &usage,
                .image = image,
                .viewType = access->layer_count > 1 ? VK_IMAGE_VIEW_TYPE_2D_ARRAY : VK_IMAGE_VIEW_TYPE_2D,
                .format = plane_formats[plane],
                .components = {VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY,
                               VK_COMPONENT_SWIZZLE_IDENTITY, VK_COMPONENT_SWIZZLE_IDENTITY},
                .subresourceRange = {plane == 0 ? VK_IMAGE_ASPECT_PLANE_0_BIT : VK_IMAGE_ASPECT_PLANE_1_BIT,
                                     0, 1, 0, access->layer_count},
            };
            step = ALPHA2_GPU_STEP_CREATE_IMAGE_VIEW;
            result = vkCreateImageView(device, &view_info, allocator, &views[index][plane]);
            if (result != VK_SUCCESS) goto cleanup_views;
        }
        acquire[barrier_count] = (VkImageMemoryBarrier){
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = access->frame.access[index],
            .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
            .oldLayout = (VkImageLayout)access->frame.layouts[index],
            .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = image,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, access->layer_count},
        };
        release[barrier_count] = acquire[barrier_count];
        release[barrier_count].srcAccessMask = VK_ACCESS_SHADER_READ_BIT;
        release[barrier_count].dstAccessMask = access->frame.access[index];
        release[barrier_count].oldLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
        release[barrier_count].newLayout = (VkImageLayout)access->frame.layouts[index];
        ++barrier_count;
        VkSemaphore semaphore = (VkSemaphore)(uintptr_t)access->frame.semaphores[index];
        uint32_t slot = 0;
        while (slot < semaphore_count && semaphores[slot] != semaphore) ++slot;
        if (slot == semaphore_count) { semaphores[slot] = semaphore; ++semaphore_count; }
        if (wait_values[slot] <= access->frame.semaphore_values[index]) {
            wait_values[slot] = access->frame.semaphore_values[index];
            signal_values[slot] = wait_values[slot] + 1;
        }
    }
    VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    step = ALPHA2_GPU_STEP_BEGIN_COMMAND_BUFFER;
    result = vkBeginCommandBuffer(command, &begin);
    if (result != VK_SUCCESS) goto cleanup_views;
    vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
        VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL, 0, NULL, barrier_count, acquire);
    vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
        VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, 0, 0, NULL, 0, NULL, barrier_count, release);
    step = ALPHA2_GPU_STEP_END_COMMAND_BUFFER;
    result = vkEndCommandBuffer(command);
    if (result != VK_SUCCESS) goto cleanup_views;
    VkPipelineStageFlags stages[ALPHA2_MAX_VULKAN_PLANES];
    for (uint32_t index = 0; index < semaphore_count; ++index)
        stages[index] = VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT;
    VkTimelineSemaphoreSubmitInfo timeline = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
        .waitSemaphoreValueCount = semaphore_count, .pWaitSemaphoreValues = wait_values,
        .signalSemaphoreValueCount = semaphore_count, .pSignalSemaphoreValues = signal_values};
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &timeline,
        .waitSemaphoreCount = semaphore_count, .pWaitSemaphores = semaphores,
        .pWaitDstStageMask = stages, .commandBufferCount = 1, .pCommandBuffers = &command,
        .signalSemaphoreCount = semaphore_count, .pSignalSemaphores = semaphores};
    step = ALPHA2_GPU_STEP_QUEUE_SUBMIT;
    result = vkQueueSubmit(queue, 1, &submit, fence);
    if (result == VK_SUCCESS) {
        /*
         * Submitted, so the batch signals these values whenever it runs, finished in time or not:
         * the frame records them, or the next submission on it (FFmpeg's) would signal the value
         * this one does, or one below it, and move the timeline backwards
         * (VUID-VkSubmitInfo-pSignalSemaphores-03242). The caller writes them back either way.
         */
        for (size_t image = 0; image < ALPHA2_MAX_VULKAN_PLANES; ++image)
            for (uint32_t slot = 0; slot < semaphore_count; ++slot)
                if ((VkSemaphore)(uintptr_t)access->frame.semaphores[image] == semaphores[slot])
                    access->frame.semaphore_values[image] = signal_values[slot];
        step = ALPHA2_GPU_STEP_WAIT_FENCE;
        result = vkWaitForFences(device, 1, &fence, VK_TRUE, alpha2_vulkan_wait_timeout());
        /* Submitted, not seen to finish: the batch may still use the fence and command buffer. */
        pending = result != VK_SUCCESS;
    }
cleanup_views:
    /* The barriers name the images, not these views, so a pending batch does not use them. */
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index)
        for (size_t plane = 0; plane < 2; ++plane)
            if (views[index][plane] != VK_NULL_HANDLE)
                vkDestroyImageView(device, views[index][plane], allocator);
cleanup:
    if (pending) {
        /* The GPU may still be using these; leaking them is the safe loss. */
        alpha2_retain_unfinished();
    } else {
        if (fence != VK_NULL_HANDLE) vkDestroyFence(device, fence, allocator);
        vkDestroyCommandPool(device, pool, allocator);
    }
    if (step == ALPHA2_GPU_STEP_UNSUPPORTED) return ALPHA2_GPU_FAIL(status, step, 0);
    return result == VK_SUCCESS ? 0 : ALPHA2_GPU_FAIL(status, step, result);
}

#define ALPHA2_ENCODE_COPY_SLOTS 4

/*
 * Owns everything the NV12 handoff reuses across frames: one transient pool and a ring of command
 * buffers, waiting only on the fence of the slot it reuses (a pool per frame ended by
 * `vkQueueWaitIdle` would serialize the renderer against the encoder).
 *
 * Nothing on the GPU waits for the renderer: the packing render is submitted earlier on the same
 * queue, and the copy's first barrier has the colour-attachment stage and writes in its source
 * scope, which orders the copy after it. Waiting instead on a semaphore the render was to signal
 * would block the queue the renderer and FFmpeg share whenever that submission never happened.
 */
typedef struct Alpha2VulkanEncodeCopier {
    VkDevice device;
    const VkAllocationCallbacks *allocator;
    uint32_t queue_family;
    VkCommandPool pool;
    VkCommandBuffer commands[ALPHA2_ENCODE_COPY_SLOTS];
    VkFence fences[ALPHA2_ENCODE_COPY_SLOTS];
    uint32_t pending[ALPHA2_ENCODE_COPY_SLOTS];
    uint32_t slot;
} Alpha2VulkanEncodeCopier;

/*
 * Creates a copier for `queue_family` of the device: its pool, ring of command buffers and their
 * fences. Returns 0, or the step that failed with its VkResult in `status`.
 */
int alpha2_native_gpu_vulkan_encode_copier_create(
    uint64_t device_handle, uint32_t queue_family, uint64_t allocator_handle, void **output,
    int32_t *status
) {
    if (device_handle == 0 || output == NULL)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    if (!alpha2_vk_ready()) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_LOADER, 0);
    Alpha2VulkanEncodeCopier *copier = calloc(1, sizeof(*copier));
    if (copier == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_HOST_ALLOCATION, 0);
    copier->device = (VkDevice)(uintptr_t)device_handle;
    copier->allocator = (const VkAllocationCallbacks *)(uintptr_t)allocator_handle;
    copier->queue_family = queue_family;
    VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT |
                 VK_COMMAND_POOL_CREATE_TRANSIENT_BIT,
        .queueFamilyIndex = queue_family,
    };
    VkResult result =
        vkCreateCommandPool(copier->device, &pool_info, copier->allocator, &copier->pool);
    if (result != VK_SUCCESS) {
        free(copier);
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_CREATE_COMMAND_POOL, result);
    }
    VkCommandBufferAllocateInfo allocate = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = copier->pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = ALPHA2_ENCODE_COPY_SLOTS,
    };
    result = vkAllocateCommandBuffers(copier->device, &allocate, copier->commands);
    if (result != VK_SUCCESS) {
        vkDestroyCommandPool(copier->device, copier->pool, copier->allocator);
        free(copier);
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ALLOCATE_COMMAND_BUFFERS, result);
    }
    for (uint32_t index = 0; index < ALPHA2_ENCODE_COPY_SLOTS; ++index) {
        VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
        result = vkCreateFence(copier->device, &fence_info, copier->allocator,
                               &copier->fences[index]);
        if (result != VK_SUCCESS) {
            while (index-- > 0)
                vkDestroyFence(copier->device, copier->fences[index], copier->allocator);
            vkDestroyCommandPool(copier->device, copier->pool, copier->allocator);
            free(copier);
            return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_CREATE_FENCE, result);
        }
    }
    *output = copier;
    return 0;
}

/* Waits for every copy still in flight, a bounded time each, before freeing what they use. */
int alpha2_native_gpu_vulkan_encode_copier_destroy(void *handle, int32_t *status) {
    Alpha2VulkanEncodeCopier *copier = handle;
    if (copier == NULL) return 0;
    for (uint32_t index = 0; index < ALPHA2_ENCODE_COPY_SLOTS; ++index) {
        if (copier->pending[index]) {
            VkResult result = vkWaitForFences(copier->device, 1, &copier->fences[index],
                                              VK_TRUE, alpha2_vulkan_wait_timeout());
            /*
             * The caller must retain the images and device as well as this helper when a copy has
             * not finished.
             */
            if (result != VK_SUCCESS)
                return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_WAIT_FENCE, result);
            copier->pending[index] = 0;
        }
    }
    for (uint32_t index = 0; index < ALPHA2_ENCODE_COPY_SLOTS; ++index)
        vkDestroyFence(copier->device, copier->fences[index], copier->allocator);
    vkDestroyCommandPool(copier->device, copier->pool, copier->allocator);
    free(copier);
    return 0;
}

/*
 * Slots in the command-buffer ring, which is also the number of plane-texture pairs a caller needs
 * to keep the renderer from overwriting a pair a copy is still reading.
 */
uint32_t alpha2_native_gpu_vulkan_encode_copier_slots(void) {
    return ALPHA2_ENCODE_COPY_SLOTS;
}

/*
 * Waits for the slot the next submit will use and reports which one it is.
 *
 * The submit path waits for the same fence, but it does so after the caller has already recorded
 * the render that writes the plane textures, too late to protect them. Calling this first is what
 * makes the pairs a ring: the render into pair N is not even submitted until the copy that last
 * read pair N has completed.
 */
int alpha2_native_gpu_vulkan_encode_copier_await_slot(
    void *handle, uint32_t *slot, int32_t *status
) {
    Alpha2VulkanEncodeCopier *copier = handle;
    if (copier == NULL || slot == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    uint32_t index = copier->slot;
    if (copier->pending[index]) {
        VkResult waited = vkWaitForFences(copier->device, 1, &copier->fences[index], VK_TRUE,
                                          alpha2_vulkan_wait_timeout());
        if (waited != VK_SUCCESS)
            return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_WAIT_FENCE, waited);
        copier->pending[index] = 0;
    }
    *slot = index;
    return 0;
}

/*
 * Copies a renderer-packed NV12 pair into an encoder frame's image.
 *
 * FFmpeg hands out one `VK_FORMAT_G8_B8R8_2PLANE_420_UNORM` image per frame, not one per plane, and
 * wgpu 30 allows only `TEXTURE_BINDING` on its `NV12` format, so wgpu can neither render into that
 * image nor copy into it, and this has to be raw. The two sources are wgpu textures the renderer
 * just wrote as colour attachments; they are restored to that layout afterwards so wgpu's own state
 * tracking stays true.
 */
int alpha2_native_gpu_vulkan_encode_copier_submit(
    void *handle, Alpha2VulkanFrameAccess *access, const Alpha2VulkanEncodeCopy *copy,
    int32_t *status
) {
    Alpha2VulkanEncodeCopier *copier = handle;
    if (copier == NULL || access == NULL || copy == NULL || copy->luma_image == 0 ||
        copy->chroma_image == 0 || copy->width == 0 || copy->height == 0 ||
        access->layer_count != 1) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    /* One multi-planar destination image is the layout this path is for. */
    if (access->frame.images[0] == 0 || access->frame.images[1] != 0)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    if ((VkDevice)(uintptr_t)access->frame.device != copier->device)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);

    uint32_t slot = copier->slot;
    if (copier->pending[slot]) {
        VkResult waited = vkWaitForFences(copier->device, 1, &copier->fences[slot], VK_TRUE,
                                          alpha2_vulkan_wait_timeout());
        if (waited != VK_SUCCESS)
            return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_WAIT_FENCE, waited);
        copier->pending[slot] = 0;
    }
    VkResult result = vkResetFences(copier->device, 1, &copier->fences[slot]);
    if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_RESET_FENCE, result);
    VkCommandBuffer command = copier->commands[slot];
    result = vkResetCommandBuffer(command, 0);
    if (result != VK_SUCCESS)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_RESET_COMMAND_BUFFER, result);

    VkImage destination = (VkImage)(uintptr_t)access->frame.images[0];
    VkImage luma = (VkImage)(uintptr_t)copy->luma_image;
    VkImage chroma = (VkImage)(uintptr_t)copy->chroma_image;
    VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    result = vkBeginCommandBuffer(command, &begin);
    if (result != VK_SUCCESS)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_BEGIN_COMMAND_BUFFER, result);

    VkImageMemoryBarrier entry[3] = {
        {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
         .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
         .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = luma,
         .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}},
        {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
         .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
         .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = chroma,
         .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}},
        {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
         .srcAccessMask = 0,
         .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
         .oldLayout = (VkImageLayout)access->frame.layouts[0],
         .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = destination,
         .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}},
    };
    /*
     * The source scope orders the copy after the render that packed the planes, an earlier
     * submission on this queue (COLOR_ATTACHMENT_OUTPUT), and after the encoder's last use of the
     * destination, whose frame semaphore the submission waits for at the TRANSFER stage: a barrier
     * chains after a wait only through a shared stage.
     */
    vkCmdPipelineBarrier(command,
        VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT,
        VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 3, entry);

    VkImageCopy regions[2] = {
        {.srcSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
         .dstSubresource = {VK_IMAGE_ASPECT_PLANE_0_BIT, 0, 0, 1},
         .extent = {copy->width, copy->height, 1}},
        {.srcSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
         .dstSubresource = {VK_IMAGE_ASPECT_PLANE_1_BIT, 0, 0, 1},
         .extent = {copy->width / 2, copy->height / 2, 1}},
    };
    vkCmdCopyImage(command, luma, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        destination, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &regions[0]);
    vkCmdCopyImage(command, chroma, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        destination, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &regions[1]);

    VkImageMemoryBarrier exit[3] = {entry[0], entry[1], entry[2]};
    for (int index = 0; index < 2; ++index) {
        exit[index].srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
        exit[index].dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
        exit[index].oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
        exit[index].newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    }
    exit[2].srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    exit[2].dstAccessMask = 0;
    exit[2].oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    exit[2].newLayout = VK_IMAGE_LAYOUT_VIDEO_ENCODE_SRC_KHR;
    vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_TRANSFER_BIT,
        VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
        0, 0, NULL, 0, NULL, 3, exit);
    result = vkEndCommandBuffer(command);
    if (result != VK_SUCCESS)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_END_COMMAND_BUFFER, result);

    VkSemaphore frame_semaphore = (VkSemaphore)(uintptr_t)access->frame.semaphores[0];
    if (frame_semaphore == VK_NULL_HANDLE)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    uint64_t wait_value = access->frame.semaphore_values[0];
    VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_TRANSFER_BIT;
    uint64_t signal_value = wait_value + 1;
    VkTimelineSemaphoreSubmitInfo timeline = {
        .sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
        .waitSemaphoreValueCount = 1,
        .pWaitSemaphoreValues = &wait_value,
        .signalSemaphoreValueCount = 1,
        .pSignalSemaphoreValues = &signal_value,
    };
    VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .pNext = &timeline,
        .waitSemaphoreCount = 1,
        .pWaitSemaphores = &frame_semaphore,
        .pWaitDstStageMask = &wait_stage,
        .commandBufferCount = 1,
        .pCommandBuffers = &command,
        .signalSemaphoreCount = 1,
        .pSignalSemaphores = &frame_semaphore,
    };
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(copier->device, copier->queue_family, 0, &queue);
    if (queue == VK_NULL_HANDLE) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE, 0);
    result = vkQueueSubmit(queue, 1, &submit, copier->fences[slot]);
    if (result != VK_SUCCESS) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SUBMIT, result);
    copier->pending[slot] = 1;
    copier->slot = (slot + 1) % ALPHA2_ENCODE_COPY_SLOTS;
    /*
     * FFmpeg reads these back to build the encoder's own wait, so they have to describe what this
     * submission actually left behind.
     */
    access->frame.semaphore_values[0] = signal_value;
    access->frame.layouts[0] = (uint32_t)VK_IMAGE_LAYOUT_VIDEO_ENCODE_SRC_KHR;
    access->frame.access[0] = 0;
    return 0;
}
