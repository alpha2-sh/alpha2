#ifndef ALPHA2_D3D12_INTEROP_H
#define ALPHA2_D3D12_INTEROP_H

#include <stdint.h>

/*
 * One D3D12VA frame as `alpha2_d3d12_frame_info` reads it. The pointers are borrowed from the frame
 * and stay valid only while it is held. `width` and `height` are the frame's (the picture with its
 * right and bottom crop applied, its left and top crop still on the frame); `surface_width` and
 * `surface_height` are those of the texture it was decoded into.
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
