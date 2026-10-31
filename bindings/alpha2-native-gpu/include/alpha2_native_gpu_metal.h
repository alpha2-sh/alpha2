#ifndef ALPHA2_NATIVE_GPU_METAL_H
#define ALPHA2_NATIVE_GPU_METAL_H

#include <stdint.h>

/* Fixed-width ABI shared only by c/metal.m and src/metal.rs. */
typedef struct Alpha2MetalFrameInfo {
    uint32_t width;
    uint32_t height;
    uint32_t plane_width[2];
    uint32_t plane_height[2];
    uint32_t clean_x;
    uint32_t clean_y;
    uint32_t clean_width;
    uint32_t clean_height;
    uint32_t pixel_format;
    uint32_t plane_count;
    uint64_t io_surface;
} Alpha2MetalFrameInfo;

#endif
