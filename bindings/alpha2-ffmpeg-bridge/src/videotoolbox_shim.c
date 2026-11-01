#include "alpha2_ffmpeg_version.h"
#include <libavcodec/avcodec.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/pixfmt.h>
#include "alpha2_videotoolbox_interop.h"

/* Returns AV_PIX_FMT_NONE when VideoToolbox is not offered so the caller can fall back. */
enum AVPixelFormat alpha2_videotoolbox_get_format(
    AVCodecContext *context, const enum AVPixelFormat *formats
) {
    (void)context;
    if (formats == NULL) return AV_PIX_FMT_NONE;
    for (const enum AVPixelFormat *format = formats;
         *format != AV_PIX_FMT_NONE; ++format) {
        if (*format == AV_PIX_FMT_VIDEOTOOLBOX) return *format;
    }
    return AV_PIX_FMT_NONE;
}

/* Returns -1 unless `frame` is a VideoToolbox frame with a pixel buffer. */
int alpha2_videotoolbox_frame_info(
    const AVFrame *frame, Alpha2VideoToolboxFrameInfo *output
) {
    if (frame == NULL || output == NULL ||
        frame->format != AV_PIX_FMT_VIDEOTOOLBOX || frame->data[3] == NULL)
        return -1;
    /* The frames context records the layout, so CoreVideo need not be linked. */
    enum AVPixelFormat software_format = AV_PIX_FMT_NONE;
    if (frame->hw_frames_ctx != NULL && frame->hw_frames_ctx->data != NULL)
        software_format =
            ((const AVHWFramesContext *)frame->hw_frames_ctx->data)->sw_format;
    *output = (Alpha2VideoToolboxFrameInfo){
        .pixel_buffer = (uint64_t)(uintptr_t)frame->data[3],
        .width = frame->width,
        .height = frame->height,
        .hardware_format = frame->format,
        .software_format = software_format,
    };
    return 0;
}
