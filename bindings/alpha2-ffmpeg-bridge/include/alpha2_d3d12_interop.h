#ifndef ALPHA2_D3D12_INTEROP_H
#define ALPHA2_D3D12_INTEROP_H

#include <stdint.h>

/*
 * Pointers are borrowed from the frame and valid only while it is held. `width` and `height` have
 * the right and bottom crop applied but not the left and top; `surface_*` is the texture size.
 */
typedef struct Alpha2D3D12FrameInfo {
    uint64_t resource;
    uint64_t fence;
    uint64_t fence_value;
    uint64_t device;
    uint32_t dxgi_format;
    int32_t width;
    int32_t height;
    int32_t hardware_format;
    int32_t software_format;
    uint32_t surface_width;
    uint32_t surface_height;
} Alpha2D3D12FrameInfo;

#endif
