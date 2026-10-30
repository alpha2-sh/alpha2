#include <CoreVideo/CoreVideo.h>
#include <IOSurface/IOSurface.h>
#include <Metal/Metal.h>
#include <VideoToolbox/VideoToolbox.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include "alpha2_native_gpu_metal.h"
#include "alpha2_native_gpu_status.h"

/*
 * Creates a CoreVideo Metal texture cache on `device`. Returns 0, -1 for a null argument or a
 * failure without a status, or the CVReturn that failed.
 */
int32_t alpha2_native_gpu_metal_cache_create(void *device, void **output) {
    if (device == NULL || output == NULL) return -1;
    CVMetalTextureCacheRef cache = NULL;
    CVReturn result = CVMetalTextureCacheCreate(kCFAllocatorDefault, NULL,
        (__bridge id<MTLDevice>)device, NULL, &cache);
    if (result != kCVReturnSuccess || cache == NULL) return result == 0 ? -1 : result;
    *output = cache;
    return 0;
}

/* Flushes and releases a cache `alpha2_native_gpu_metal_cache_create` made. */
void alpha2_native_gpu_metal_cache_destroy(void *cache) {
    if (cache == NULL) return;
    CVMetalTextureCacheFlush((CVMetalTextureCacheRef)cache, 0);
    CFRelease(cache);
}

/*
 * Describes a bi-planar 8- or 10-bit 4:2:0 pixel buffer: its size, plane sizes, clean aperture (the
 * whole buffer when the aperture is unusable) and IOSurface. Returns -1 for a null argument, -2 for
 * another pixel format, -3 for a plane count other than 2, -4 without an IOSurface.
 */
int32_t alpha2_native_gpu_metal_frame_info(void *pixel_buffer,
                                           Alpha2MetalFrameInfo *output) {
    if (pixel_buffer == NULL || output == NULL) return -1;
    CVPixelBufferRef buffer = (CVPixelBufferRef)pixel_buffer;
    OSType format = CVPixelBufferGetPixelFormatType(buffer);
    if (format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange &&
        format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange &&
        format != kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange &&
        format != kCVPixelFormatType_420YpCbCr10BiPlanarFullRange) return -2;
    if (CVPixelBufferGetPlaneCount(buffer) != 2) return -3;
    IOSurfaceRef surface = CVPixelBufferGetIOSurface(buffer);
    if (surface == NULL) return -4;
    output->width = (uint32_t)CVPixelBufferGetWidth(buffer);
    output->height = (uint32_t)CVPixelBufferGetHeight(buffer);
    output->plane_width[0] = (uint32_t)CVPixelBufferGetWidthOfPlane(buffer, 0);
    output->plane_height[0] = (uint32_t)CVPixelBufferGetHeightOfPlane(buffer, 0);
    output->plane_width[1] = (uint32_t)CVPixelBufferGetWidthOfPlane(buffer, 1);
    output->plane_height[1] = (uint32_t)CVPixelBufferGetHeightOfPlane(buffer, 1);
    CGRect clean = CVImageBufferGetCleanRect(buffer);
    if (clean.origin.x < 0 || clean.origin.y < 0 || clean.size.width <= 0 ||
        clean.size.height <= 0 || CGRectGetMaxX(clean) > output->width ||
        CGRectGetMaxY(clean) > output->height) {
        clean = CGRectMake(0, 0, output->width, output->height);
    }
    /*
     * Each edge rounds on its own, so the rectangle stays inside the buffer: rounding the origin
     * and the size apart would move a fractional aperture's right edge one pixel past it (an origin
     * of 0.5 and a width of 63.5 in a 64-wide buffer).
     */
    uint32_t left = (uint32_t)floor(CGRectGetMinX(clean) + 0.5);
    uint32_t top = (uint32_t)floor(CGRectGetMinY(clean) + 0.5);
    uint32_t right = (uint32_t)floor(CGRectGetMaxX(clean) + 0.5);
    uint32_t bottom = (uint32_t)floor(CGRectGetMaxY(clean) + 0.5);
    if (right <= left || bottom <= top) {
        left = 0;
        top = 0;
        right = output->width;
        bottom = output->height;
    }
    output->clean_x = left;
    output->clean_y = top;
    output->clean_width = right - left;
    output->clean_height = bottom - top;
    output->pixel_format = format;
    output->io_surface = (uint64_t)(uintptr_t)surface;
    return 0;
}

/*
 * Returns 0, or the step that failed with its CVReturn in `status` (`alpha2_native_gpu_status.h`).
 */
int32_t alpha2_native_gpu_metal_import(void *cache, void *pixel_buffer,
                                       uint32_t plane, void **texture,
                                       void **metal_texture, int32_t *status) {
    if (cache == NULL || pixel_buffer == NULL || texture == NULL ||
        metal_texture == NULL || plane > 1)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    CVPixelBufferRef buffer = (CVPixelBufferRef)pixel_buffer;
    size_t width = CVPixelBufferGetWidthOfPlane(buffer, plane);
    size_t height = CVPixelBufferGetHeightOfPlane(buffer, plane);
    OSType pixel_format = CVPixelBufferGetPixelFormatType(buffer);
    /*
     * Ten-bit CoreVideo planes are left-aligned in 16-bit containers, so each plane is wrapped with
     * the matching 16-bit Metal format.
     */
    bool ten_bit =
        pixel_format == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange ||
        pixel_format == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange;
    MTLPixelFormat format;
    if (plane == 0)
        format = ten_bit ? MTLPixelFormatR16Unorm : MTLPixelFormatR8Unorm;
    else
        format = ten_bit ? MTLPixelFormatRG16Unorm : MTLPixelFormatRG8Unorm;
    CVMetalTextureRef imported = NULL;
    CVReturn result = CVMetalTextureCacheCreateTextureFromImage(kCFAllocatorDefault,
        (CVMetalTextureCacheRef)cache, buffer, NULL, format, width, height,
        plane, &imported);
    if (result != kCVReturnSuccess || imported == NULL)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_CREATE_TEXTURE_FROM_IMAGE, result);
    id<MTLTexture> raw = CVMetalTextureGetTexture(imported);
    if (raw == nil) {
        CFRelease(imported);
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_NO_OUTPUT, 0);
    }
    *texture = imported;
    *metal_texture = (__bridge void *)raw;
    return 0;
}

/* Releases a CVMetalTexture `alpha2_native_gpu_metal_import` returned. */
void alpha2_native_gpu_metal_texture_destroy(void *texture) {
    if (texture != NULL) CFRelease(texture);
}

/* Wraps one plane of an encoder-input pixel buffer as a Metal render target.
   The texture aliases the buffer's IOSurface, so what the renderer draws into it
   is what VideoToolbox encodes; no pixel is copied. Only 8-bit video-range NV12
   is accepted, because that is the only layout the renderer's pack writes.
   Returns -1 for a bad argument, -2 for another pixel format, -3 for a plane
   count other than 2, -4 without an IOSurface, -5 when Metal makes no texture,
   and -6 on a GPU outside the Apple family. */
int32_t alpha2_native_gpu_metal_encoder_plane(void *device, void *pixel_buffer,
                                              uint32_t plane, uint32_t *width,
                                              uint32_t *height, void **texture) {
    if (device == NULL || pixel_buffer == NULL || width == NULL || height == NULL ||
        texture == NULL || plane > 1) return -1;
    CVPixelBufferRef buffer = (CVPixelBufferRef)pixel_buffer;
    if (CVPixelBufferGetPixelFormatType(buffer) !=
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) return -2;
    if (CVPixelBufferGetPlaneCount(buffer) != 2) return -3;
    IOSurfaceRef surface = CVPixelBufferGetIOSurface(buffer);
    if (surface == NULL) return -4;
    id<MTLDevice> metal = (__bridge id<MTLDevice>)device;
    size_t plane_width = CVPixelBufferGetWidthOfPlane(buffer, plane);
    size_t plane_height = CVPixelBufferGetHeightOfPlane(buffer, plane);
    MTLTextureDescriptor *descriptor = [MTLTextureDescriptor
        texture2DDescriptorWithPixelFormat:(plane == 0 ? MTLPixelFormatR8Unorm
                                                       : MTLPixelFormatRG8Unorm)
                                     width:plane_width
                                    height:plane_height
                                 mipmapped:NO];
    descriptor.usage = MTLTextureUsageRenderTarget | MTLTextureUsageShaderRead;
    /*
     * Only an Apple-family GPU (Apple silicon) is accepted, the one the path is verified on.
     * Unified memory is not that test: an Intel Mac's integrated GPU has it, and there a
     * shared-storage texture is not available. The storage mode is left to Metal, which takes it
     * from the IOSurface, as Dawn does; forcing Shared fails Metal API validation where it wants
     * Managed.
     */
    if (![metal supportsFamily:MTLGPUFamilyApple1]) return -6;
    id<MTLTexture> raw = [metal newTextureWithDescriptor:descriptor
                                               iosurface:surface
                                                   plane:plane];
    if (raw == nil) return -5;
    *width = (uint32_t)plane_width;
    *height = (uint32_t)plane_height;
    *texture = (__bridge_retained void *)raw;
    return 0;
}

/*
 * Whether VideoToolbox decodes `codec_type` (a CMVideoCodecType) in hardware on this machine.
 * The supplemental decoders (VP9, AV1) are registered first, as FFmpeg does before it opens one.
 */
int32_t alpha2_native_gpu_vt_hardware_decodes(uint32_t codec_type) {
    if (@available(macOS 11.0, *)) {
        VTRegisterSupplementalVideoDecoderIfAvailable((CMVideoCodecType)codec_type);
    }
    return VTIsHardwareDecodeSupported((CMVideoCodecType)codec_type) ? 1 : 0;
}
