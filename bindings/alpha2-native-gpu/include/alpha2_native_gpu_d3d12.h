#ifndef ALPHA2_NATIVE_GPU_D3D12_H
#define ALPHA2_NATIVE_GPU_D3D12_H

#include <stdint.h>

/* Fixed-width ABI shared only by c/d3d12*.c and src/d3d12.rs. */

/* Failures are reported as in alpha2_native_gpu_status.h; a copy that was
   submitted but could not be fenced fails at ALPHA2_GPU_STEP_QUEUE_SIGNAL. */
typedef struct Alpha2D3D12CopyInfo {
    uint64_t resource;
    uint64_t fence;
    uint64_t fence_value;
    uint32_t width;
    uint32_t height;
    uint32_t dxgi_format;
} Alpha2D3D12CopyInfo;

/* One renderer-packed NV12 pair on its way into a D3D12VA encoder frame. */
typedef struct Alpha2D3D12EncodeCopy {
    uint64_t luma;        /* ID3D12Resource*, R8_UNORM, in RENDER_TARGET */
    uint64_t chroma;      /* ID3D12Resource*, R8G8_UNORM, in RENDER_TARGET */
    uint64_t destination; /* ID3D12Resource*, NV12, in COMMON (AVD3D12VAFrame.texture) */
    uint64_t frame_fence; /* AVD3D12VAFrame.sync_ctx.fence */
    uint64_t frame_value; /* its sync_ctx.fence_value now; waited on before writing */
    uint32_t width;
    uint32_t height;
} Alpha2D3D12EncodeCopy;

/* The codecs `alpha2_native_gpu_d3d12_encode_caps` answers for, each at the
   profile FFmpeg's D3D12VA encoder opens for 8-bit 4:2:0 input. */
#define ALPHA2_D3D12_ENCODE_H264_MAIN 1u
#define ALPHA2_D3D12_ENCODE_HEVC_MAIN 2u
#define ALPHA2_D3D12_ENCODE_AV1_MAIN 3u

/* `Alpha2D3D12EncodeCaps.rate_control_modes` bits. */
#define ALPHA2_D3D12_RATE_CONTROL_CQP (1u << 0)
#define ALPHA2_D3D12_RATE_CONTROL_CBR (1u << 1)
#define ALPHA2_D3D12_RATE_CONTROL_VBR (1u << 2)
#define ALPHA2_D3D12_RATE_CONTROL_QVBR (1u << 3)

/* The check an encode-capability query stopped at, in the order it makes
   them. 0 means none refused. */
#define ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE 1u      /* QueryInterface(ID3D12VideoDevice3) */
#define ALPHA2_D3D12_ENCODE_CHECK_CODEC 2u             /* D3D12_FEATURE_VIDEO_ENCODER_CODEC */
#define ALPHA2_D3D12_ENCODE_CHECK_PROFILE_LEVEL 3u     /* ..._PROFILE_LEVEL */
#define ALPHA2_D3D12_ENCODE_CHECK_INPUT_FORMAT 4u      /* ..._INPUT_FORMAT, NV12 */
#define ALPHA2_D3D12_ENCODE_CHECK_RATE_CONTROL 5u      /* ..._RATE_CONTROL_MODE, any of four */
#define ALPHA2_D3D12_ENCODE_CHECK_RESOLUTION 6u        /* ..._OUTPUT_RESOLUTION(_RATIOS_COUNT) */
#define ALPHA2_D3D12_ENCODE_CHECK_PICTURE_CONTROL 7u   /* ..._CODEC_PICTURE_CONTROL_SUPPORT */
#define ALPHA2_D3D12_ENCODE_CHECK_CONFIGURATION 8u     /* ..._CODEC_CONFIGURATION_SUPPORT */
#define ALPHA2_D3D12_ENCODE_CHECK_SUPPORT 9u           /* D3D12_FEATURE_VIDEO_ENCODER_SUPPORT */

/*
 * What a device's D3D12 Video Encode accepts for one codec. `supported` is 1
 * when every check passed, and the limits are then filled in. Otherwise
 * `refused_check` names the check that refused, `refused_status` its HRESULT
 * (0 when the call succeeded and answered "not supported"), and, for the
 * final support check, `validation_flags` the device's
 * D3D12_VIDEO_ENCODER_VALIDATION_FLAGS. The final check runs once per
 * rate-control mode the device lists; it refuses only when every mode does,
 * with the first refusal's status.
 */
typedef struct Alpha2D3D12EncodeCaps {
    uint32_t supported;
    uint32_t refused_check;
    int32_t refused_status;
    uint32_t validation_flags;
    uint32_t min_width;
    uint32_t min_height;
    uint32_t max_width;
    uint32_t max_height;
    /* Resolution{Width,Height}MultipleRequirement. */
    uint32_t width_multiple;
    uint32_t height_multiple;
    uint32_t max_p_l0_references;
    uint32_t max_b_l0_references;
    uint32_t max_l1_references;
    /* ALPHA2_D3D12_RATE_CONTROL_*: the modes the final support check passed. */
    uint32_t rate_control_modes;
    /* The modes the device lists that the final support check refused, and
       each one's HRESULT (0 for a plain "not supported"), indexed CQP, CBR,
       VBR, QVBR. */
    uint32_t refused_rate_control_modes;
    int32_t rate_control_status[4];
} Alpha2D3D12EncodeCaps;

#endif
