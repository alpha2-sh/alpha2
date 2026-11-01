/* pthread recursive mutexes are POSIX 2008, which -std=c11 hides without a feature macro. */
#define _XOPEN_SOURCE 700
#include <stdint.h>
#include <stddef.h>
#include <pthread.h>
#include "alpha2_ffmpeg_version.h"
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vulkan.h>
#include <libavutil/log.h>
#include <libavutil/mem.h>
#include "alpha2_vulkan_interop.h"
#include "alpha2_vulkan_loader.h"

_Static_assert(ALPHA2_MAX_VULKAN_PLANES == AV_NUM_DATA_POINTERS,
               "interop ABI must cover every AVFrame data slot");

static void alpha2_free_extension_list(const char *const *list, int count) {
    if (list == NULL) return;
    for (int index = 0; index < count; ++index) av_free((void *)list[index]);
    av_free((void *)list);
}

static int alpha2_copy_extension_list(
    const char *const *source, uint32_t count, const char *const **destination
) {
    if (count == 0) { *destination = NULL; return 0; }
    char **copy = av_calloc(count, sizeof(*copy));
    if (copy == NULL) return AVERROR(ENOMEM);
    for (uint32_t index = 0; index < count; ++index) {
        copy[index] = av_strdup(source[index]);
        if (copy[index] == NULL) {
            alpha2_free_extension_list((const char *const *)copy, (int)count);
            return AVERROR(ENOMEM);
        }
    }
    *destination = (const char *const *)copy;
    return 0;
}

/*
 * Queue locks owned by alpha2 so FFmpeg, wgpu and the import path serialize on the shared queue.
 * Recursive because a completion callback can release a decoder surface while a submission holds
 * the lock.
 */
#define ALPHA2_MAX_QUEUE_FAMILIES 8

typedef struct Alpha2VulkanDeviceState {
    VkPhysicalDeviceTimelineSemaphoreFeatures timeline;
    VkPhysicalDeviceSynchronization2Features synchronization2;
    VkPhysicalDeviceSamplerYcbcrConversionFeatures ycbcr;
    VkPhysicalDeviceVideoMaintenance1FeaturesKHR video_maintenance1;
    VkPhysicalDeviceVideoDecodeVP9FeaturesKHR video_decode_vp9;
    pthread_mutex_t queue_mutex[ALPHA2_MAX_QUEUE_FAMILIES];
    uint32_t initialized;
    uint32_t unlockable_reported;
} Alpha2VulkanDeviceState;

static void alpha2_free_vulkan_device(AVHWDeviceContext *context);

/* Checks `free` too because other device types keep private data in `user_opaque`. */
static Alpha2VulkanDeviceState *alpha2_device_state(const AVHWDeviceContext *context) {
    if (context == NULL || context->type != AV_HWDEVICE_TYPE_VULKAN ||
        context->free != alpha2_free_vulkan_device) return NULL;
    Alpha2VulkanDeviceState *state = context->user_opaque;
    return state != NULL && state->initialized ? state : NULL;
}

/* Checked before handing out a queue lock so that lock never has to fail open. */
int alpha2_vulkan_queue_lockable(const AVHWDeviceContext *context, uint32_t queue_family) {
    return alpha2_device_state(context) != NULL && queue_family < ALPHA2_MAX_QUEUE_FAMILIES;
}

// An unlockable family leaves FFmpeg and wgpu submissions unserialized. A void callback cannot
// refuse, so log it once per family.
static void alpha2_report_unlockable_queue(
    AVHWDeviceContext *context, Alpha2VulkanDeviceState *state, uint32_t queue_family
) {
    if (state != NULL) {
        if (queue_family < ALPHA2_MAX_QUEUE_FAMILIES) return;
        if (state->unlockable_reported & (1u << (queue_family % 32))) return;
        state->unlockable_reported |= 1u << (queue_family % 32);
    }
    av_log(context, AV_LOG_ERROR,
           "alpha2: queue family %u cannot be locked (%s); FFmpeg and wgpu "
           "submissions to it are unsynchronized\n",
           queue_family,
           state == NULL ? "no device state" : "above ALPHA2_MAX_QUEUE_FAMILIES");
}

static void alpha2_lock_queue(
    AVHWDeviceContext *context, uint32_t queue_family, uint32_t index
) {
    (void)index;
    Alpha2VulkanDeviceState *state = alpha2_device_state(context);
    if (state != NULL && queue_family < ALPHA2_MAX_QUEUE_FAMILIES) {
        pthread_mutex_lock(&state->queue_mutex[queue_family]);
        return;
    }
    alpha2_report_unlockable_queue(context, state, queue_family);
}

static void alpha2_unlock_queue(
    AVHWDeviceContext *context, uint32_t queue_family, uint32_t index
) {
    (void)index;
    Alpha2VulkanDeviceState *state = alpha2_device_state(context);
    if (state != NULL && queue_family < ALPHA2_MAX_QUEUE_FAMILIES) {
        pthread_mutex_unlock(&state->queue_mutex[queue_family]);
        return;
    }
    alpha2_report_unlockable_queue(context, state, queue_family);
}

void alpha2_vulkan_queue_lock(AVHWDeviceContext *context, uint32_t queue_family) {
    alpha2_lock_queue(context, queue_family, 0);
}

void alpha2_vulkan_queue_unlock(AVHWDeviceContext *context, uint32_t queue_family) {
    alpha2_unlock_queue(context, queue_family, 0);
}

static void alpha2_free_vulkan_device(AVHWDeviceContext *context) {
    Alpha2VulkanDeviceState *state = alpha2_device_state(context);
    if (state != NULL) {
        state->initialized = 0;
        for (size_t index = 0; index < ALPHA2_MAX_QUEUE_FAMILIES; ++index)
            pthread_mutex_destroy(&state->queue_mutex[index]);
    }
    AVVulkanDeviceContext *vulkan = context != NULL ? context->hwctx : NULL;
    if (vulkan == NULL) return;
    alpha2_free_extension_list(vulkan->enabled_inst_extensions,
                               vulkan->nb_enabled_inst_extensions);
    alpha2_free_extension_list(vulkan->enabled_dev_extensions,
                               vulkan->nb_enabled_dev_extensions);
    vulkan->enabled_inst_extensions = NULL;
    vulkan->enabled_dev_extensions = NULL;
    av_freep(&context->user_opaque);
}

/*
 * Fills a Vulkan device context from the renderer's device and installs this shim's queue locks.
 * Returns AVERROR(EINVAL) for a missing handle or queue, AVERROR(ENOSYS) without a Vulkan loader,
 * AVERROR(ENOMEM) when allocation fails.
 */
int alpha2_vulkan_device_init(AVHWDeviceContext *context,
                              const Alpha2VulkanDeviceInfo *info) {
    if (context == NULL || context->hwctx == NULL || info == NULL ||
        info->instance == 0 || info->physical_device == 0 || info->device == 0 ||
        info->graphics_queue_count == 0 || info->decode_queue_count == 0) return AVERROR(EINVAL);
    AVVulkanDeviceContext *vulkan = context->hwctx;
    /* FFmpeg needs the resolver of the loader the renderer's instance came from. */
    PFN_vkGetInstanceProcAddr resolver =
        (PFN_vkGetInstanceProcAddr)alpha2_vulkan_symbol("vkGetInstanceProcAddr");
    if (resolver == NULL) return AVERROR(ENOSYS);
    vulkan->get_proc_addr = resolver;
    vulkan->inst = (VkInstance)(uintptr_t)info->instance;
    vulkan->phys_dev = (VkPhysicalDevice)(uintptr_t)info->physical_device;
    vulkan->act_dev = (VkDevice)(uintptr_t)info->device;
    Alpha2VulkanDeviceState *state = av_mallocz(sizeof(*state));
    if (state == NULL) return AVERROR(ENOMEM);
    /* FFmpeg reads enabled features from this chain, so it must match the device exactly. */
    state->video_decode_vp9 = (VkPhysicalDeviceVideoDecodeVP9FeaturesKHR){
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_DECODE_VP9_FEATURES_KHR,
        .videoDecodeVP9 = info->video_decode_vp9 != 0,
    };
    state->video_maintenance1 = (VkPhysicalDeviceVideoMaintenance1FeaturesKHR){
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VIDEO_MAINTENANCE_1_FEATURES_KHR,
        .pNext = &state->video_decode_vp9,
        .videoMaintenance1 = info->video_maintenance1 != 0,
    };
    state->ycbcr = (VkPhysicalDeviceSamplerYcbcrConversionFeatures){
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES,
        .pNext = &state->video_maintenance1,
        .samplerYcbcrConversion = info->sampler_ycbcr_conversion != 0,
    };
    state->synchronization2 = (VkPhysicalDeviceSynchronization2Features){
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SYNCHRONIZATION_2_FEATURES,
        .pNext = &state->ycbcr,
        .synchronization2 = info->synchronization2 != 0,
    };
    state->timeline = (VkPhysicalDeviceTimelineSemaphoreFeatures){
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
        .pNext = &state->synchronization2,
        .timelineSemaphore = VK_TRUE,
    };
    pthread_mutexattr_t attributes;
    if (pthread_mutexattr_init(&attributes) != 0) { av_free(state); return AVERROR(ENOMEM); }
    pthread_mutexattr_settype(&attributes, PTHREAD_MUTEX_RECURSIVE);
    for (size_t index = 0; index < ALPHA2_MAX_QUEUE_FAMILIES; ++index) {
        if (pthread_mutex_init(&state->queue_mutex[index], &attributes) != 0) {
            while (index-- > 0) pthread_mutex_destroy(&state->queue_mutex[index]);
            pthread_mutexattr_destroy(&attributes);
            av_free(state);
            return AVERROR(ENOMEM);
        }
    }
    pthread_mutexattr_destroy(&attributes);
    state->initialized = 1;
    VkPhysicalDeviceTimelineSemaphoreFeatures *timeline = &state->timeline;
    context->user_opaque = state;
    context->free = alpha2_free_vulkan_device;
    // Deprecated in FFmpeg 8.1 but still called; see alpha2_vulkan_frame_access_begin.
    #pragma GCC diagnostic push
    #pragma GCC diagnostic ignored "-Wdeprecated-declarations"
    vulkan->lock_queue = alpha2_lock_queue;
    vulkan->unlock_queue = alpha2_unlock_queue;
    #pragma GCC diagnostic pop
    vulkan->device_features = (VkPhysicalDeviceFeatures2){
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
        .pNext = timeline,
        .features = {
            .shaderImageGatherExtended = info->shader_image_gather_extended,
            .fragmentStoresAndAtomics = info->fragment_stores_and_atomics,
            .shaderInt64 = info->shader_int64,
            .vertexPipelineStoresAndAtomics = info->vertex_pipeline_stores_and_atomics,
        },
    };
    int result = alpha2_copy_extension_list(info->instance_extensions,
        info->instance_extension_count, &vulkan->enabled_inst_extensions);
    if (result < 0) return result;
    vulkan->nb_enabled_inst_extensions = (int)info->instance_extension_count;
    result = alpha2_copy_extension_list(info->device_extensions,
        info->device_extension_count, &vulkan->enabled_dev_extensions);
    if (result < 0) {
        alpha2_free_extension_list(vulkan->enabled_inst_extensions,
                                   vulkan->nb_enabled_inst_extensions);
        vulkan->enabled_inst_extensions = NULL;
        vulkan->nb_enabled_inst_extensions = 0;
        return result;
    }
    vulkan->nb_enabled_dev_extensions = (int)info->device_extension_count;
    #pragma GCC diagnostic push
    #pragma GCC diagnostic ignored "-Wdeprecated-declarations"
    vulkan->queue_family_index = (int)info->graphics_queue_family;
    vulkan->nb_graphics_queues = (int)info->graphics_queue_count;
    vulkan->queue_family_tx_index = (int)info->graphics_queue_family;
    vulkan->nb_tx_queues = (int)info->graphics_queue_count;
    vulkan->queue_family_comp_index = (int)info->graphics_queue_family;
    vulkan->nb_comp_queues = (int)info->graphics_queue_count;
    vulkan->queue_family_decode_index = (int)info->decode_queue_family;
    vulkan->nb_decode_queues = (int)info->decode_queue_count;
    #pragma GCC diagnostic pop
    vulkan->qf[0] = (AVVulkanDeviceQueueFamily){
        .idx = (int)info->graphics_queue_family,
        .num = (int)info->graphics_queue_count,
        .flags = VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT,
    };
    vulkan->nb_qf = 1;
    if (info->decode_queue_family == info->graphics_queue_family) {
        vulkan->qf[0].flags |= VK_QUEUE_VIDEO_DECODE_BIT_KHR;
        vulkan->qf[0].video_caps = info->decode_video_caps;
    } else {
        vulkan->qf[vulkan->nb_qf++] = (AVVulkanDeviceQueueFamily){
            .idx = (int)info->decode_queue_family,
            .num = (int)info->decode_queue_count,
            .flags = VK_QUEUE_VIDEO_DECODE_BIT_KHR,
            .video_caps = info->decode_video_caps,
        };
    }
    /* Advertise an encode family only if the device created a queue from it. */
    if (info->encode_queue_count > 0) {
        if (info->encode_queue_family == info->graphics_queue_family) {
            vulkan->qf[0].flags |= VK_QUEUE_VIDEO_ENCODE_BIT_KHR;
            vulkan->qf[0].video_caps |= info->encode_video_caps;
        } else if (info->encode_queue_family == info->decode_queue_family &&
                   vulkan->nb_qf > 1) {
            vulkan->qf[1].flags |= VK_QUEUE_VIDEO_ENCODE_BIT_KHR;
            vulkan->qf[1].video_caps |= info->encode_video_caps;
        } else {
            vulkan->qf[vulkan->nb_qf++] = (AVVulkanDeviceQueueFamily){
                .idx = (int)info->encode_queue_family,
                .num = (int)info->encode_queue_count,
                .flags = VK_QUEUE_VIDEO_ENCODE_BIT_KHR,
                .video_caps = info->encode_video_caps,
            };
        }
    }
    return 0;
}

/*
 * Copies up to `capacity` queue families. Returns the context's family count, or -1 for a context
 * this shim did not initialize.
 */
int alpha2_vulkan_device_queue_families(
    const AVHWDeviceContext *context, Alpha2VulkanQueueFamily *output, uint32_t capacity
) {
    if (alpha2_device_state(context) == NULL) return -1;
    const AVVulkanDeviceContext *vulkan = context->hwctx;
    for (int index = 0; index < vulkan->nb_qf && (uint32_t)index < capacity; ++index) {
        output[index] = (Alpha2VulkanQueueFamily){
            .index = vulkan->qf[index].idx,
            .count = vulkan->qf[index].num,
            .flags = (uint32_t)vulkan->qf[index].flags,
            .video_caps = (uint32_t)vulkan->qf[index].video_caps,
        };
    }
    return vulkan->nb_qf;
}

static int alpha2_is_vulkan_frames(const AVHWFramesContext *frames) {
    return frames->hwctx != NULL && frames->device_ctx != NULL &&
           frames->device_ctx->type == AV_HWDEVICE_TYPE_VULKAN;
}

/*
 * Must run before `av_hwframe_ctx_init`. A nonzero usage replaces hwcontext_vulkan's default, and
 * the format must support every flag. Returns AVERROR(EINVAL) for a non-Vulkan context.
 */
int alpha2_vulkan_frames_add_usage(AVBufferRef *frames_ref, uint32_t usage) {
    if (frames_ref == NULL || frames_ref->data == NULL) return AVERROR(EINVAL);
    AVHWFramesContext *frames = (AVHWFramesContext *)frames_ref->data;
    if (!alpha2_is_vulkan_frames(frames)) return AVERROR(EINVAL);
    AVVulkanFramesContext *vulkan = frames->hwctx;
    vulkan->usage |= (VkImageUsageFlags)usage;
    return 0;
}

/* The usage the pool was created with, including hwcontext_vulkan's default. */
uint32_t alpha2_vulkan_frames_usage(const AVBufferRef *frames_ref) {
    if (frames_ref == NULL || frames_ref->data == NULL) return 0;
    const AVHWFramesContext *frames = (const AVHWFramesContext *)frames_ref->data;
    if (!alpha2_is_vulkan_frames(frames)) return 0;
    const AVVulkanFramesContext *vulkan = frames->hwctx;
    return (uint32_t)vulkan->usage;
}

/* Returns AV_PIX_FMT_NONE when Vulkan is not offered so the caller can fall back. */
enum AVPixelFormat alpha2_vulkan_get_format(
    AVCodecContext *context, const enum AVPixelFormat *formats
) {
    (void)context;
    if (formats == NULL) return AV_PIX_FMT_NONE;
    for (const enum AVPixelFormat *format = formats;
         *format != AV_PIX_FMT_NONE; ++format) {
        if (*format == AV_PIX_FMT_VULKAN) return *format;
    }
    return AV_PIX_FMT_NONE;
}

/* Returns -1 when `frame` is not a Vulkan frame, -2 when a context is missing. */
static int alpha2_contexts(
    const AVFrame *frame, AVHWFramesContext **frames,
    AVVulkanFramesContext **vulkan_frames,
    AVVulkanDeviceContext **vulkan_device, AVVkFrame **vulkan_frame
) {
    if (frame == NULL || frame->format != AV_PIX_FMT_VULKAN ||
        frame->hw_frames_ctx == NULL || frame->data[0] == NULL) return -1;
    *frames = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    if (*frames == NULL || (*frames)->hwctx == NULL ||
        (*frames)->device_ctx == NULL || (*frames)->device_ctx->hwctx == NULL) return -2;
    *vulkan_frames = (AVVulkanFramesContext *)(*frames)->hwctx;
    *vulkan_device = (AVVulkanDeviceContext *)(*frames)->device_ctx->hwctx;
    *vulkan_frame = (AVVkFrame *)frame->data[0];
    return 0;
}

static void alpha2_copy_info(
    const AVFrame *frame, const AVHWFramesContext *frames,
    const AVVulkanFramesContext *vulkan_frames,
    const AVVulkanDeviceContext *vulkan_device,
    const AVVkFrame *vulkan_frame, Alpha2VulkanFrameInfo *output
) {
    *output = (Alpha2VulkanFrameInfo){0};
    output->hardware_format = frame->format;
    output->software_format = frames->sw_format;
    output->width = frame->width;
    output->height = frame->height;
    /* FFmpeg sizes decode images from the frames context (hwcontext_vulkan.c create_frame()). */
    output->coded_width = frames->width;
    output->coded_height = frames->height;
    output->sampled = (vulkan_frames->usage & VK_IMAGE_USAGE_SAMPLED_BIT) != 0;
    output->instance = (uint64_t)(uintptr_t)vulkan_device->inst;
    output->physical_device = (uint64_t)(uintptr_t)vulkan_device->phys_dev;
    output->device = (uint64_t)(uintptr_t)vulkan_device->act_dev;
    for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
        if (vulkan_frame->img[index] == VK_NULL_HANDLE) continue;
        ++output->plane_count;
        output->images[index] = (uint64_t)(uintptr_t)vulkan_frame->img[index];
        output->formats[index] = (uint32_t)vulkan_frames->format[index];
        output->layouts[index] = (uint32_t)vulkan_frame->layout[index];
        output->access[index] = (uint32_t)vulkan_frame->access[index];
        output->semaphores[index] = (uint64_t)(uintptr_t)vulkan_frame->sem[index];
        output->semaphore_values[index] = vulkan_frame->sem_value[index];
        output->queue_families[index] = vulkan_frame->queue_family[index];
    }
}

/* Reads under the frame lock. Returns `alpha2_contexts` codes, or -3 for a frame without images. */
int alpha2_vulkan_frame_info(const AVFrame *frame, Alpha2VulkanFrameInfo *output) {
    AVHWFramesContext *frames;
    AVVulkanFramesContext *vulkan_frames;
    AVVulkanDeviceContext *vulkan_device;
    AVVkFrame *vulkan_frame;
    int result = alpha2_contexts(frame, &frames, &vulkan_frames, &vulkan_device, &vulkan_frame);
    if (result != 0 || output == NULL) return result != 0 ? result : -1;
    if (vulkan_frames->lock_frame != NULL) vulkan_frames->lock_frame(frames, vulkan_frame);
    alpha2_copy_info(frame, frames, vulkan_frames, vulkan_device, vulkan_frame, output);
    if (vulkan_frames->unlock_frame != NULL) vulkan_frames->unlock_frame(frames, vulkan_frame);
    return output->plane_count == 0 ? -3 : 0;
}

/*
 * FFmpeg 8.1 deprecates lock_queue and unlock_queue but still calls them in ff_vk_exec_submit, so
 * they still serialize its submissions with wgpu's.
 */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wdeprecated-declarations"

/*
 * Takes the frame lock, then the first graphics or compute queue lock. On 0 both stay held until
 * `alpha2_vulkan_frame_access_end`; on failure none is held. Returns -4 without frame or queue
 * locks or layers, -5 without a graphics or compute queue, else `alpha2_vulkan_frame_info` codes.
 */
int alpha2_vulkan_frame_access_begin(AVFrame *frame, Alpha2VulkanFrameAccess *output) {
    AVHWFramesContext *frames;
    AVVulkanFramesContext *vulkan_frames;
    AVVulkanDeviceContext *vulkan_device;
    AVVkFrame *vulkan_frame;
    int result = alpha2_contexts(frame, &frames, &vulkan_frames, &vulkan_device, &vulkan_frame);
    if (result != 0 || output == NULL) return result != 0 ? result : -1;
    if (vulkan_frames->lock_frame == NULL || vulkan_frames->unlock_frame == NULL ||
        vulkan_device->lock_queue == NULL || vulkan_device->unlock_queue == NULL ||
        vulkan_frames->nb_layers <= 0) return -4;
    int family = -1;
    for (int index = 0; index < vulkan_device->nb_qf; ++index) {
        if (vulkan_device->qf[index].num > 0 &&
            (vulkan_device->qf[index].flags &
             (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT)) != 0) {
            family = vulkan_device->qf[index].idx;
            break;
        }
    }
    if (family < 0) return -5;
    /* Frame lock before queue lock, as in ff_vk_exec_submit; the reverse deadlocks with decode. */
    vulkan_frames->lock_frame(frames, vulkan_frame);
    vulkan_device->lock_queue(frames->device_ctx, (uint32_t)family, 0);
    *output = (Alpha2VulkanFrameAccess){0};
    alpha2_copy_info(frame, frames, vulkan_frames, vulkan_device, vulkan_frame, &output->frame);
    output->allocator = (uint64_t)(uintptr_t)vulkan_device->alloc;
    output->queue_family = (uint32_t)family;
    output->layer_count = (uint32_t)vulkan_frames->nb_layers;
    output->frames_context = (uint64_t)(uintptr_t)frames;
    output->locked_frame = (uint64_t)(uintptr_t)vulkan_frame;
    if (output->frame.plane_count == 0) {
        vulkan_device->unlock_queue(frames->device_ctx, (uint32_t)family, 0);
        vulkan_frames->unlock_frame(frames, vulkan_frame);
        return -3;
    }
    return 0;
}

/*
 * Releases the locks named by `begun`. With `updated`, first writes back layouts, access masks and
 * timeline values for planes whose image and semaphore are unchanged; a timeline value is never
 * lowered.
 */
void alpha2_vulkan_frame_access_end(
    const Alpha2VulkanFrameAccess *begun, const Alpha2VulkanFrameAccess *updated
) {
    /* Release exactly what begin locked rather than re-resolving the frame's contexts. */
    if (begun == NULL) return;
    AVHWFramesContext *frames = (AVHWFramesContext *)(uintptr_t)begun->frames_context;
    AVVkFrame *vulkan_frame = (AVVkFrame *)(uintptr_t)begun->locked_frame;
    if (frames == NULL || vulkan_frame == NULL || frames->hwctx == NULL ||
        frames->device_ctx == NULL || frames->device_ctx->hwctx == NULL) return;
    AVVulkanFramesContext *vulkan_frames = (AVVulkanFramesContext *)frames->hwctx;
    AVVulkanDeviceContext *vulkan_device =
        (AVVulkanDeviceContext *)frames->device_ctx->hwctx;
    if (updated != NULL) {
        /*
         * FFmpeg expects `layout` and `access` updated after every barrier. A stale layout makes a
         * later decode read a reference picture in the wrong layout, which is undefined where DPB
         * and output alias.
         */
        for (size_t index = 0; index < ALPHA2_MAX_VULKAN_PLANES; ++index) {
            uint64_t image = begun->frame.images[index];
            if (image == 0 || (uint64_t)(uintptr_t)vulkan_frame->img[index] != image ||
                updated->frame.images[index] != image)
                continue;
            uint64_t semaphore = begun->frame.semaphores[index];
            if (semaphore != 0 && (uint64_t)(uintptr_t)vulkan_frame->sem[index] == semaphore &&
                updated->frame.semaphores[index] == semaphore &&
                updated->frame.semaphore_values[index] >= vulkan_frame->sem_value[index])
                vulkan_frame->sem_value[index] = updated->frame.semaphore_values[index];
            vulkan_frame->layout[index] = (VkImageLayout)updated->frame.layouts[index];
            vulkan_frame->access[index] = (VkAccessFlagBits)updated->frame.access[index];
        }
    }
    vulkan_device->unlock_queue(frames->device_ctx, begun->queue_family, 0);
    vulkan_frames->unlock_frame(frames, vulkan_frame);
}
#pragma GCC diagnostic pop
