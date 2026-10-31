#define COBJMACROS
#include <windows.h>
#include "alpha2_ffmpeg_version.h"
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d12va.h>
#include "alpha2_d3d12_interop.h"

/*
 * FFmpeg releases `device` only in the free callback `d3d12va_device_create` installs, so a context
 * over a caller's device needs its own.
 */
static void alpha2_d3d12_device_free(AVHWDeviceContext *context) {
    AVD3D12VADeviceContext *d3d12 = context != NULL ? context->hwctx : NULL;
    if (d3d12 == NULL || d3d12->device == NULL) return;
    ID3D12Device_Release(d3d12->device);
    d3d12->device = NULL;
}

/*
 * Points a D3D12VA device context at the renderer's `ID3D12Device`, taking a COM reference.
 * Returns AVERROR(EINVAL) for null arguments and AVERROR_EXTERNAL when d3d12.dll cannot be pinned.
 */
int alpha2_d3d12_device_init(AVHWDeviceContext *context, void *device) {
    if (context == NULL || context->hwctx == NULL || device == NULL)
        return AVERROR(EINVAL);
    /* wgpu unloads d3d12.dll after its last device, but this context releases its references
       through that DLL later, so pin it for the life of the process. */
    HMODULE runtime = NULL;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, L"d3d12.dll", &runtime))
        return AVERROR_EXTERNAL;
    AVD3D12VADeviceContext *d3d12 = context->hwctx;
    d3d12->device = (ID3D12Device *)device;
    ID3D12Device_AddRef(d3d12->device);
    context->free = alpha2_d3d12_device_free;
    return 0;
}

/*
 * Picks AV_PIX_FMT_D3D12 when offered, otherwise AV_PIX_FMT_NONE so the open fails and the caller
 * can fall back.
 *
 * FFmpeg 8.1 sizes surfaces at the visible size, so a cropped stream (1080p H.264 is coded at 1088
 * rows) decodes into undersized surfaces, which hangs some integrated GPU drivers. The frames
 * context is made here at the coded size instead, as FFmpeg master does.
 */
enum AVPixelFormat alpha2_d3d12_get_format(
    AVCodecContext *context, const enum AVPixelFormat *formats
) {
    if (formats == NULL) return AV_PIX_FMT_NONE;
    for (const enum AVPixelFormat *format = formats;
         *format != AV_PIX_FMT_NONE; ++format) {
        if (*format != AV_PIX_FMT_D3D12) continue;
        if (context == NULL || context->hw_device_ctx == NULL) return *format;
        AVBufferRef *frames = NULL;
        if (avcodec_get_hw_frames_parameters(
                context, context->hw_device_ctx, AV_PIX_FMT_D3D12, &frames) < 0)
            return AV_PIX_FMT_NONE;
        AVHWFramesContext *described = (AVHWFramesContext *)frames->data;
        described->width = context->coded_width;
        described->height = context->coded_height;
        if (av_hwframe_ctx_init(frames) < 0) {
            av_buffer_unref(&frames);
            return AV_PIX_FMT_NONE;
        }
        /* A stream that changes size asks again; the earlier pool goes with its frames. */
        av_buffer_unref(&context->hw_frames_ctx);
        context->hw_frames_ctx = frames;
        return *format;
    }
    return AV_PIX_FMT_NONE;
}

/* Pointers are borrowed from the frame. Returns AVERROR(EINVAL) for an incomplete D3D12 frame. */
int alpha2_d3d12_frame_info(
    const AVFrame *frame, Alpha2D3D12FrameInfo *output
) {
    if (frame == NULL || output == NULL || frame->format != AV_PIX_FMT_D3D12 ||
        frame->data[0] == NULL || frame->hw_frames_ctx == NULL)
        return AVERROR(EINVAL);
    AVD3D12VAFrame *descriptor = (AVD3D12VAFrame *)frame->data[0];
    AVHWFramesContext *frames = (AVHWFramesContext *)frame->hw_frames_ctx->data;
    if (descriptor->texture == NULL || descriptor->sync_ctx.fence == NULL ||
        frames == NULL || frames->device_ref == NULL)
        return AVERROR(EINVAL);
    AVHWDeviceContext *device_context =
        (AVHWDeviceContext *)frames->device_ref->data;
    AVD3D12VADeviceContext *d3d12 = device_context != NULL
        ? device_context->hwctx : NULL;
    if (d3d12 == NULL || d3d12->device == NULL) return AVERROR(EINVAL);
    /* Call through the vtable: the macro fails on mingw-w64 and has the wrong ABI on old SDKs. */
    D3D12_RESOURCE_DESC resource;
    descriptor->texture->lpVtbl->GetDesc(descriptor->texture, &resource);
    *output = (Alpha2D3D12FrameInfo){
        .resource = (uint64_t)(uintptr_t)descriptor->texture,
        .fence = (uint64_t)(uintptr_t)descriptor->sync_ctx.fence,
        .fence_value = descriptor->sync_ctx.fence_value,
        .device = (uint64_t)(uintptr_t)d3d12->device,
        .dxgi_format = (uint32_t)resource.Format,
        .width = frame->width,
        .height = frame->height,
        .hardware_format = frame->format,
        .software_format = frames->sw_format,
        /* A texture is at most 16384 texels a side (D3D12_REQ_TEXTURE2D_U_OR_V_DIMENSION). */
        .surface_width = (uint32_t)resource.Width,
        .surface_height = resource.Height,
    };
    return 0;
}

static AVD3D12VAFrame *alpha2_d3d12_frame_descriptor(const AVFrame *frame) {
    if (frame == NULL || frame->format != AV_PIX_FMT_D3D12 || frame->data[0] == NULL)
        return NULL;
    AVD3D12VAFrame *descriptor = (AVD3D12VAFrame *)frame->data[0];
    return descriptor->texture != NULL && descriptor->sync_ctx.fence != NULL ? descriptor : NULL;
}

/*
 * Sets the fence value FFmpeg's encoder waits for before reading the texture. The sync context
 * moved in FFmpeg 8.1, so access goes through the compiled headers. Returns AVERROR(EINVAL) unless
 * `frame` is a complete D3D12 frame.
 */
int alpha2_d3d12_frame_set_fence_value(AVFrame *frame, uint64_t value) {
    AVD3D12VAFrame *descriptor = alpha2_d3d12_frame_descriptor(frame);
    if (descriptor == NULL) return AVERROR(EINVAL);
    descriptor->sync_ctx.fence_value = value;
    return 0;
}

/* Returns AVERROR(EINVAL) unless `frame` is a complete D3D12 frame. */
int alpha2_d3d12_frame_fence_value(const AVFrame *frame, uint64_t *output) {
    AVD3D12VAFrame *descriptor = alpha2_d3d12_frame_descriptor(frame);
    if (descriptor == NULL || output == NULL) return AVERROR(EINVAL);
    *output = descriptor->sync_ctx.fence_value;
    return 0;
}

/*
 * Sets the texture format and flags before `av_hwframe_ctx_init`, one texture per frame (an
 * encoder input must not use texture-array mode). These fields changed in FFmpeg 8.1, so access
 * goes through the compiled headers. Returns AVERROR(EINVAL) for a non-D3D12VA context.
 */
int alpha2_d3d12_frames_configure(
    AVBufferRef *frames_ref, uint32_t dxgi_format, uint32_t resource_flags
) {
    if (frames_ref == NULL || frames_ref->data == NULL) return AVERROR(EINVAL);
    AVHWFramesContext *frames = (AVHWFramesContext *)frames_ref->data;
    if (frames->hwctx == NULL || frames->device_ctx == NULL ||
        frames->device_ctx->type != AV_HWDEVICE_TYPE_D3D12VA)
        return AVERROR(EINVAL);
    AVD3D12VAFramesContext *d3d12 = frames->hwctx;
    d3d12->format = (DXGI_FORMAT)dxgi_format;
    d3d12->resource_flags = (D3D12_RESOURCE_FLAGS)resource_flags;
    d3d12->heap_flags = D3D12_HEAP_FLAG_NONE;
    d3d12->flags = AV_D3D12VA_FRAME_FLAG_NONE;
    return 0;
}
