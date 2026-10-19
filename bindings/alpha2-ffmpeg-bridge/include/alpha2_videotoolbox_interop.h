#ifndef ALPHA2_VIDEOTOOLBOX_INTEROP_H
#define ALPHA2_VIDEOTOOLBOX_INTEROP_H

#include <stdint.h>

/*
 * One VideoToolbox frame as `alpha2_videotoolbox_frame_info` reads it. `pixel_buffer` is borrowed
 * from the frame.
 */
typedef struct Alpha2VideoToolboxFrameInfo {
    uint64_t pixel_buffer;
    int32_t width;
    int32_t height;
    int32_t hardware_format;
    /* Sample layout of the CoreVideo buffer, from the frames context. */
    int32_t software_format;
} Alpha2VideoToolboxFrameInfo;

#endif
