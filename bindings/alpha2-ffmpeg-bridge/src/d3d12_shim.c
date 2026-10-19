#define COBJMACROS
#include <windows.h>
#include "alpha2_ffmpeg_version.h"
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d12va.h>
#include "alpha2_d3d12_interop.h"

/*
 * Releases the device reference `alpha2_d3d12_device_init` took. FFmpeg 8.0.3 and 8.1.3 release
 * `device` only in the free callback `d3d12va_device_create` installs, so a context made over a
 * caller's device needs one of its own.
 */
static void alpha2_d3d12_device_free(AVHWDeviceContext *context) {
    AVD3D12VADeviceContext *d3d12 = context != NULL ? context->hwctx : NULL;
    if (d3d12 == NULL || d3d12->device == NULL) return;
    ID3D12Device_Release(d3d12->device);
    d3d12->device = NULL;
}

/*
 * Points an allocated D3D12VA device context at the renderer's `ID3D12Device`, taking a COM
 * reference that `alpha2_d3d12_device_free` releases. Returns AVERROR(EINVAL) for null arguments
 * and AVERROR_EXTERNAL when d3d12.dll cannot be pinned.
 */
int alpha2_d3d12_device_init(AVHWDeviceContext *context, void *device) {
    if (context == NULL || context->hwctx == NULL || device == NULL)
        return AVERROR(EINVAL);
    /* The device comes from wgpu, which loads d3d12.dll itself and unloads it
       when its last device is gone. This context's reference to the device,
       and the frames and fences made from it, are released later, through
       that DLL's code: pinned, it stays for the life of the process. */
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
 * The `get_format` callback of a D3D12VA decoder: AV_PIX_FMT_D3D12 when offered, with a frames
 * context of the stream's coded size, otherwise AV_PIX_FMT_NONE, which fails the open so the
 * caller can fall back.
 *
 * FFmpeg 8.1.3 sizes D3D12VA surfaces at the visible size, so a cropped stream (1080p H.264 is
 * coded at 1088 rows) decodes into surfaces smaller than its pictures, after which an Intel Iris Xe
 * on driver 32.0.101.7092 hangs every later video call on the device. So the frames context is
 * made here at the coded size, as FFmpeg master does; the decoder takes a context it is given.
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

/*
 * Copies a D3D12 frame's texture, decode fence and device into `output`; the pointers are borrowed
 * from the frame. Returns AVERROR(EINVAL) for anything that is not a complete D3D12 frame.
 */
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
    /* `GetDesc` goes through the vtable; alpha2-native-gpu's d3d12.c says why. */
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

/* The frame's D3D12VA descriptor when `frame` is a complete D3D12 frame, otherwise NULL. */
static AVD3D12VAFrame *alpha2_d3d12_frame_descriptor(const AVFrame *frame) {
    if (frame == NULL || frame->format != AV_PIX_FMT_D3D12 || frame->data[0] == NULL)
        return NULL;
    AVD3D12VAFrame *descriptor = (AVD3D12VAFrame *)frame->data[0];
    return descriptor->texture != NULL && descriptor->sync_ctx.fence != NULL ? descriptor : NULL;
}

/*
 * Writes `AVD3D12VAFrame.sync_ctx.fence_value`, the value FFmpeg's encoder waits for on the
 * frame's fence before it reads the texture. FFmpeg 8.1 moved the sync context within the frame
 * (`subresource_index` came before it), so this goes through the headers this is compiled
 * against, never an offset kept elsewhere. Returns AVERROR(EINVAL) for anything that is not a
 * complete D3D12 frame.
 */
int alpha2_d3d12_frame_set_fence_value(AVFrame *frame, uint64_t value) {
    AVD3D12VAFrame *descriptor = alpha2_d3d12_frame_descriptor(frame);
    if (descriptor == NULL) return AVERROR(EINVAL);
    descriptor->sync_ctx.fence_value = value;
    return 0;
}

/* Reads `AVD3D12VAFrame.sync_ctx.fence_value` into `output`, as the setter above writes it.
   Returns AVERROR(EINVAL) for anything that is not a complete D3D12 frame. */
int alpha2_d3d12_frame_fence_value(const AVFrame *frame, uint64_t *output) {
    AVD3D12VAFrame *descriptor = alpha2_d3d12_frame_descriptor(frame);
    if (descriptor == NULL || output == NULL) return AVERROR(EINVAL);
    *output = descriptor->sync_ctx.fence_value;
    return 0;
}

/*
 * Describes a D3D12VA frames context's textures before `av_hwframe_ctx_init` allocates any:
 * `dxgi_format`, `resource_flags`, no heap flags, and one texture per frame. FFmpeg 8.1 renamed
 * the resource flags (`flags` in 8.0 is `resource_flags` now) and added `heap_flags` and a `flags`
 * of its own whose texture-array mode an encoder input must not have, so this goes through the
 * headers this is compiled against, never an offset kept elsewhere. Returns AVERROR(EINVAL) for a
 * context that is not D3D12VA's.
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
