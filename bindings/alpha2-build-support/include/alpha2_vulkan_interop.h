#ifndef ALPHA2_VULKAN_INTEROP_H
#define ALPHA2_VULKAN_INTEROP_H

#include <stdint.h>

#define ALPHA2_MAX_VULKAN_PLANES 8

/* Fixed-width ABI shared only by alpha2-ffmpeg-bridge and alpha2-native-gpu. */
typedef struct Alpha2VulkanFrameInfo {
    /* `coded_width`/`coded_height` are the VkImage extent, larger than `width`/`height` when the
     * codec pads to macroblocks. */
    int32_t hardware_format, software_format, width, height;
    int32_t coded_width, coded_height;
    uint32_t plane_count, sampled;
    uint64_t instance, physical_device, device;
    uint64_t images[ALPHA2_MAX_VULKAN_PLANES];
    uint32_t formats[ALPHA2_MAX_VULKAN_PLANES];
    uint32_t layouts[ALPHA2_MAX_VULKAN_PLANES];
    uint32_t access[ALPHA2_MAX_VULKAN_PLANES];
    uint64_t semaphores[ALPHA2_MAX_VULKAN_PLANES];
    uint64_t semaphore_values[ALPHA2_MAX_VULKAN_PLANES];
    uint32_t queue_families[ALPHA2_MAX_VULKAN_PLANES];
} Alpha2VulkanFrameInfo;

typedef struct Alpha2VulkanFrameAccess {
    Alpha2VulkanFrameInfo frame;
    uint64_t allocator;
    uint32_t queue_family, layer_count;
    /* Captured at begin so end can release the lock even if the frame's context is gone. */
    uint64_t frames_context, locked_frame;
} Alpha2VulkanFrameAccess;

/* Renderer-packed NV12 planes (wgpu-owned images) to copy into an encoder frame. */
typedef struct Alpha2VulkanEncodeCopy {
    uint64_t luma_image;
    uint64_t chroma_image;
    uint32_t width;
    uint32_t height;
} Alpha2VulkanEncodeCopy;

typedef struct Alpha2VulkanDeviceInfo {
    uint64_t instance;
    uint64_t physical_device;
    uint64_t device;
    uint32_t graphics_queue_family;
    uint32_t graphics_queue_count;
    uint32_t decode_queue_family;
    uint32_t decode_queue_count;
    uint32_t decode_video_caps;
    /* Zero `encode_queue_count` means no video-encode queue; FFmpeg must not be told of one. */
    uint32_t encode_queue_family;
    uint32_t encode_queue_count;
    uint32_t encode_video_caps;
    uint32_t shader_image_gather_extended;
    uint32_t fragment_stores_and_atomics;
    uint32_t shader_int64;
    uint32_t vertex_pipeline_stores_and_atomics;
    /* Features FFmpeg uses that the application enabled on the device. */
    uint32_t synchronization2;
    uint32_t sampler_ycbcr_conversion;
    uint32_t video_maintenance1;
    uint32_t video_decode_vp9;
    const char *const *instance_extensions;
    uint32_t instance_extension_count;
    const char *const *device_extensions;
    uint32_t device_extension_count;
} Alpha2VulkanDeviceInfo;

/* One entry of the queue-family table given to FFmpeg (`AVVulkanDeviceContext.qf`). */
typedef struct Alpha2VulkanQueueFamily {
    int32_t index;
    int32_t count;
    uint32_t flags;
    uint32_t video_caps;
} Alpha2VulkanQueueFamily;

#endif
