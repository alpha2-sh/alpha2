#define COBJMACROS
#include <initguid.h>
#include <d3d12.h>
#include <d3d12video.h>
#include <stdint.h>
#include <stdlib.h>
#include "alpha2_native_gpu_d3d12.h"
#include "alpha2_native_gpu_status.h"

/*
 * What a device's D3D12 Video Encode accepts, asked of the device: FFmpeg answers only by failing
 * to open an encoder. These are the queries FFmpeg's D3D12VA encoder makes as it opens
 * (`d3d12va_encode.c` and its codec files), in its order, so a device refusing here refuses there.
 */

/* The size the final support check asks about, clamped into the device's limits: an export's
   common size, so that a device that encodes only small pictures is not mistaken for none. */
#define ALPHA2_D3D12_ENCODE_PROBE_WIDTH 1920u
#define ALPHA2_D3D12_ENCODE_PROBE_HEIGHT 1080u
/* FFmpeg's default GOP for these encoders (`g=120`), with no B pictures, as the backend opens
   them. */
#define ALPHA2_D3D12_ENCODE_PROBE_GOP 120u
/* A quantizer and a bit rate every codec accepts, for the mode the support check asks about. */
#define ALPHA2_D3D12_ENCODE_PROBE_QP 26u
#define ALPHA2_D3D12_ENCODE_PROBE_BIT_RATE 8000000u

typedef union Alpha2D3D12EncodeProfile {
    D3D12_VIDEO_ENCODER_PROFILE_H264 h264;
    D3D12_VIDEO_ENCODER_PROFILE_HEVC hevc;
    D3D12_VIDEO_ENCODER_AV1_PROFILE av1;
} Alpha2D3D12EncodeProfile;

typedef union Alpha2D3D12EncodeLevel {
    D3D12_VIDEO_ENCODER_LEVELS_H264 h264;
    D3D12_VIDEO_ENCODER_LEVEL_TIER_CONSTRAINTS_HEVC hevc;
    D3D12_VIDEO_ENCODER_AV1_LEVEL_TIER_CONSTRAINTS av1;
} Alpha2D3D12EncodeLevel;

static D3D12_VIDEO_ENCODER_PROFILE_DESC alpha2_d3d12_encode_profile(
    D3D12_VIDEO_ENCODER_CODEC codec, Alpha2D3D12EncodeProfile *storage
) {
    D3D12_VIDEO_ENCODER_PROFILE_DESC profile = {0};
    switch (codec) {
    case D3D12_VIDEO_ENCODER_CODEC_H264:
        storage->h264 = D3D12_VIDEO_ENCODER_PROFILE_H264_MAIN;
        profile.DataSize = sizeof(storage->h264);
        profile.pH264Profile = &storage->h264;
        break;
    case D3D12_VIDEO_ENCODER_CODEC_HEVC:
        storage->hevc = D3D12_VIDEO_ENCODER_PROFILE_HEVC_MAIN;
        profile.DataSize = sizeof(storage->hevc);
        profile.pHEVCProfile = &storage->hevc;
        break;
    default:
        storage->av1 = D3D12_VIDEO_ENCODER_AV1_PROFILE_MAIN;
        profile.DataSize = sizeof(storage->av1);
        profile.pAV1Profile = &storage->av1;
        break;
    }
    return profile;
}

static D3D12_VIDEO_ENCODER_LEVEL_SETTING alpha2_d3d12_encode_level(
    D3D12_VIDEO_ENCODER_CODEC codec, Alpha2D3D12EncodeLevel *storage
) {
    D3D12_VIDEO_ENCODER_LEVEL_SETTING level = {0};
    switch (codec) {
    case D3D12_VIDEO_ENCODER_CODEC_H264:
        level.DataSize = sizeof(storage->h264);
        level.pH264LevelSetting = &storage->h264;
        break;
    case D3D12_VIDEO_ENCODER_CODEC_HEVC:
        level.DataSize = sizeof(storage->hevc);
        level.pHEVCLevelSetting = &storage->hevc;
        break;
    default:
        level.DataSize = sizeof(storage->av1);
        level.pAV1LevelSetting = &storage->av1;
        break;
    }
    return level;
}

/* Records that `check` refused, with its HRESULT or 0, and returns 0: a refusal is an answer. */
static int32_t alpha2_d3d12_encode_refuse(
    Alpha2D3D12EncodeCaps *caps, uint32_t check, HRESULT result
) {
    caps->supported = 0;
    caps->refused_check = check;
    caps->refused_status = (int32_t)result;
    return 0;
}

/* FFmpeg's HEVC configurations, in its order (`hevc_config_support_sets`, d3d12va_encode_hevc.c):
   its encoder opens with the first one the device takes. */
static const D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC alpha2_d3d12_hevc_configurations[] = {
    {D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_NONE,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_8x8,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_32x32,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_4x4,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_32x32, 3, 3},
    {D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_NONE,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_8x8,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_32x32,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_4x4,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_32x32, 0, 0},
    {D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_NONE,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_8x8,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_32x32,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_4x4,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_32x32, 2, 2},
    {D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_NONE,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_8x8,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_64x64,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_4x4,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_32x32, 2, 2},
    {D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_NONE,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_8x8,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_CUSIZE_64x64,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_4x4,
     D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_TUSIZE_32x32, 4, 4},
};

typedef struct Alpha2D3D12EncodeConfiguration {
    D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264 h264;
    D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC hevc;
    D3D12_VIDEO_ENCODER_AV1_CODEC_CONFIGURATION av1;
    D3D12_VIDEO_ENCODER_SEQUENCE_GOP_STRUCTURE_H264 h264_gop;
    D3D12_VIDEO_ENCODER_SEQUENCE_GOP_STRUCTURE_HEVC hevc_gop;
    D3D12_VIDEO_ENCODER_AV1_SEQUENCE_STRUCTURE av1_sequence;
} Alpha2D3D12EncodeConfiguration;

/*
 * Asks for `codec`'s codec configuration and fills the one FFmpeg would open with into `chosen`,
 * with `configuration` and `gop` pointing into it. Returns the HRESULT, and `supported` 0 when the
 * device refused every configuration.
 */
static HRESULT alpha2_d3d12_encode_configure(
    ID3D12VideoDevice3 *video, D3D12_VIDEO_ENCODER_CODEC codec,
    D3D12_VIDEO_ENCODER_PROFILE_DESC profile, Alpha2D3D12EncodeConfiguration *chosen,
    D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION *configuration,
    D3D12_VIDEO_ENCODER_SEQUENCE_GOP_STRUCTURE *gop, int *supported
) {
    D3D12_FEATURE_DATA_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT query = {
        .NodeIndex = 0,
        .Codec = codec,
        .Profile = profile,
    };
    HRESULT result = S_OK;
    *supported = 0;
    if (codec == D3D12_VIDEO_ENCODER_CODEC_HEVC) {
        D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC limits = alpha2_d3d12_hevc_configurations[0];
        size_t count = sizeof(alpha2_d3d12_hevc_configurations) /
                       sizeof(alpha2_d3d12_hevc_configurations[0]);
        for (size_t index = 0; index < count && !*supported; ++index) {
            limits = alpha2_d3d12_hevc_configurations[index];
            query.CodecSupportLimits.DataSize = sizeof(limits);
            query.CodecSupportLimits.pHEVCSupport = &limits;
            result = ID3D12VideoDevice3_CheckFeatureSupport(
                video, D3D12_FEATURE_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT, &query,
                sizeof(query));
            *supported = SUCCEEDED(result) && query.IsSupported;
        }
        if (!*supported) return result;
        /* FFmpeg's flags for what the device offers (`d3d12va_encode_hevc_get_encoder_caps`). */
        D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_FLAGS flags =
            D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_FLAG_NONE;
        if (limits.SupportFlags &
            (D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_ASYMETRIC_MOTION_PARTITION_SUPPORT |
             D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_ASYMETRIC_MOTION_PARTITION_REQUIRED))
            flags |= D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_FLAG_USE_ASYMETRIC_MOTION_PARTITION;
        if (limits.SupportFlags & D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_SAO_FILTER_SUPPORT)
            flags |= D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_FLAG_ENABLE_SAO_FILTER;
        if (limits.SupportFlags &
            D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_DISABLING_LOOP_FILTER_ACROSS_SLICES_SUPPORT)
            flags |= D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_FLAG_DISABLE_LOOP_FILTER_ACROSS_SLICES;
        if (limits.SupportFlags & D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_HEVC_FLAG_TRANSFORM_SKIP_SUPPORT)
            flags |= D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC_FLAG_ENABLE_TRANSFORM_SKIPPING;
        chosen->hevc = (D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_HEVC){
            .ConfigurationFlags = flags,
            .MinLumaCodingUnitSize = limits.MinLumaCodingUnitSize,
            .MaxLumaCodingUnitSize = limits.MaxLumaCodingUnitSize,
            .MinLumaTransformUnitSize = limits.MinLumaTransformUnitSize,
            .MaxLumaTransformUnitSize = limits.MaxLumaTransformUnitSize,
            .max_transform_hierarchy_depth_inter = limits.max_transform_hierarchy_depth_inter,
            .max_transform_hierarchy_depth_intra = limits.max_transform_hierarchy_depth_intra,
        };
        configuration->DataSize = sizeof(chosen->hevc);
        configuration->pHEVCConfig = &chosen->hevc;
        /* log2_max_pic_order_cnt_lsb_minus4 as FFmpeg derives it for a GOP of 120. */
        chosen->hevc_gop = (D3D12_VIDEO_ENCODER_SEQUENCE_GOP_STRUCTURE_HEVC){
            .GOPLength = ALPHA2_D3D12_ENCODE_PROBE_GOP,
            .PPicturePeriod = 1,
            .log2_max_pic_order_cnt_lsb_minus4 = 3,
        };
        gop->DataSize = sizeof(chosen->hevc_gop);
        gop->pHEVCGroupOfPictures = &chosen->hevc_gop;
        return result;
    }
    if (codec == D3D12_VIDEO_ENCODER_CODEC_H264) {
        D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_H264 limits = {0};
        query.CodecSupportLimits.DataSize = sizeof(limits);
        query.CodecSupportLimits.pH264Support = &limits;
        result = ID3D12VideoDevice3_CheckFeatureSupport(
            video, D3D12_FEATURE_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT, &query, sizeof(query));
        *supported = SUCCEEDED(result) && query.IsSupported;
        if (!*supported) return result;
        /* FFmpeg's defaults: CABAC where offered, and the deblocking filter on every edge. */
        D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264_FLAGS flags =
            D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264_FLAG_NONE;
        if (limits.SupportFlags & D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT_H264_FLAG_CABAC_ENCODING_SUPPORT)
            flags |= D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264_FLAG_ENABLE_CABAC_ENCODING;
        chosen->h264 = (D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264){
            .ConfigurationFlags = flags,
            .DirectModeConfig = D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264_DIRECT_MODES_DISABLED,
            .DisableDeblockingFilterConfig =
                D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION_H264_SLICES_DEBLOCKING_MODE_0_ALL_LUMA_CHROMA_SLICE_BLOCK_EDGES_ALWAYS_FILTERED,
        };
        configuration->DataSize = sizeof(chosen->h264);
        configuration->pH264Config = &chosen->h264;
        /* log2_max_frame_num_minus4: ceil(log2(120)) - 4. */
        chosen->h264_gop = (D3D12_VIDEO_ENCODER_SEQUENCE_GOP_STRUCTURE_H264){
            .GOPLength = ALPHA2_D3D12_ENCODE_PROBE_GOP,
            .PPicturePeriod = 1,
            .log2_max_frame_num_minus4 = 3,
        };
        gop->DataSize = sizeof(chosen->h264_gop);
        gop->pH264GroupOfPictures = &chosen->h264_gop;
        return result;
    }
    D3D12_VIDEO_ENCODER_AV1_CODEC_CONFIGURATION_SUPPORT limits = {0};
    query.CodecSupportLimits.DataSize = sizeof(limits);
    query.CodecSupportLimits.pAV1Support = &limits;
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_CODEC_CONFIGURATION_SUPPORT, &query, sizeof(query));
    *supported = SUCCEEDED(result) && query.IsSupported;
    if (!*supported) return result;
    /* The features the device requires and no more; 8 order-hint bits, the AV1 default. */
    chosen->av1 = (D3D12_VIDEO_ENCODER_AV1_CODEC_CONFIGURATION){
        .FeatureFlags = limits.RequiredFeatureFlags,
        .OrderHintBitsMinus1 = 7,
    };
    configuration->DataSize = sizeof(chosen->av1);
    configuration->pAV1Config = &chosen->av1;
    chosen->av1_sequence = (D3D12_VIDEO_ENCODER_AV1_SEQUENCE_STRUCTURE){
        .IntraDistance = ALPHA2_D3D12_ENCODE_PROBE_GOP,
        .InterFramePeriod = 1,
    };
    gop->DataSize = sizeof(chosen->av1_sequence);
    gop->pAV1SequenceStructure = &chosen->av1_sequence;
    return result;
}

static uint32_t alpha2_d3d12_encode_fit(
    uint32_t value, uint32_t multiple, uint32_t minimum, uint32_t maximum
) {
    if (multiple > 1) value = (value + multiple - 1) / multiple * multiple;
    if (value < minimum) value = minimum;
    if (value > maximum) value = maximum;
    return value;
}

typedef union Alpha2D3D12EncodeRate {
    D3D12_VIDEO_ENCODER_RATE_CONTROL_CQP cqp;
    D3D12_VIDEO_ENCODER_RATE_CONTROL_CBR cbr;
    D3D12_VIDEO_ENCODER_RATE_CONTROL_VBR vbr;
    D3D12_VIDEO_ENCODER_RATE_CONTROL_QVBR qvbr;
} Alpha2D3D12EncodeRate;

/*
 * `mode` as FFmpeg's `d3d12va_encode_init_rate_control` sets it up for a bit rate or a quantizer
 * alone, with its parameters written to `storage`: CBR and VBR with the VBV sizes it always asks
 * for (a buffer of one second, three quarters full), VBR and QVBR peaking at twice the target.
 */
static D3D12_VIDEO_ENCODER_RATE_CONTROL alpha2_d3d12_encode_rate(
    D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE mode, Alpha2D3D12EncodeRate *storage
) {
    const uint64_t rate = ALPHA2_D3D12_ENCODE_PROBE_BIT_RATE;
    D3D12_VIDEO_ENCODER_RATE_CONTROL control = {
        .Mode = mode,
        .Flags = D3D12_VIDEO_ENCODER_RATE_CONTROL_FLAG_NONE,
        .TargetFrameRate = {30, 1},
    };
    *storage = (Alpha2D3D12EncodeRate){0};
    switch (mode) {
    case D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_CBR:
        storage->cbr.TargetBitRate = rate;
        storage->cbr.VBVCapacity = rate;
        storage->cbr.InitialVBVFullness = rate * 3 / 4;
        control.Flags = D3D12_VIDEO_ENCODER_RATE_CONTROL_FLAG_ENABLE_VBV_SIZES;
        control.ConfigParams.DataSize = sizeof(storage->cbr);
        control.ConfigParams.pConfiguration_CBR = &storage->cbr;
        break;
    case D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_VBR:
        storage->vbr.TargetAvgBitRate = rate;
        storage->vbr.PeakBitRate = rate * 2;
        storage->vbr.VBVCapacity = rate * 2;
        storage->vbr.InitialVBVFullness = rate * 2 * 3 / 4;
        control.Flags = D3D12_VIDEO_ENCODER_RATE_CONTROL_FLAG_ENABLE_VBV_SIZES;
        control.ConfigParams.DataSize = sizeof(storage->vbr);
        control.ConfigParams.pConfiguration_VBR = &storage->vbr;
        break;
    case D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_QVBR:
        storage->qvbr.TargetAvgBitRate = rate;
        storage->qvbr.PeakBitRate = rate * 2;
        storage->qvbr.ConstantQualityTarget = ALPHA2_D3D12_ENCODE_PROBE_QP;
        control.ConfigParams.DataSize = sizeof(storage->qvbr);
        control.ConfigParams.pConfiguration_QVBR = &storage->qvbr;
        break;
    default:
        storage->cqp.ConstantQP_FullIntracodedFrame = ALPHA2_D3D12_ENCODE_PROBE_QP;
        storage->cqp.ConstantQP_InterPredictedFrame_PrevRefOnly = ALPHA2_D3D12_ENCODE_PROBE_QP;
        storage->cqp.ConstantQP_InterPredictedFrame_BiDirectionalRef = ALPHA2_D3D12_ENCODE_PROBE_QP;
        control.ConfigParams.DataSize = sizeof(storage->cqp);
        control.ConfigParams.pConfiguration_CQP = &storage->cqp;
        break;
    }
    return control;
}

/*
 * Asks `raw_device`'s D3D12 Video Encode what it accepts for `codec` (ALPHA2_D3D12_ENCODE_*) with
 * NV12 input, and writes the answer to `caps`: the limits, or the check that refused and its
 * HRESULT. Returns 0 with either answer, or the step that kept it from asking, with its status in
 * `status`.
 *
 * The checks: the video device (ID3D12VideoDevice3, which FFmpeg's encoder needs), the codec, the
 * profile, NV12 input, the rate-control modes, the output resolutions, the picture control
 * (references), the codec configuration, and last D3D12_FEATURE_VIDEO_ENCODER_SUPPORT for the
 * configuration FFmpeg opens with, which a device can refuse after accepting every part of it.
 */
int32_t alpha2_native_gpu_d3d12_encode_caps(
    void *raw_device, uint32_t codec_id, Alpha2D3D12EncodeCaps *caps, int32_t *status
) {
    if (raw_device == NULL || caps == NULL)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    D3D12_VIDEO_ENCODER_CODEC codec;
    switch (codec_id) {
    case ALPHA2_D3D12_ENCODE_H264_MAIN: codec = D3D12_VIDEO_ENCODER_CODEC_H264; break;
    case ALPHA2_D3D12_ENCODE_HEVC_MAIN: codec = D3D12_VIDEO_ENCODER_CODEC_HEVC; break;
    case ALPHA2_D3D12_ENCODE_AV1_MAIN: codec = D3D12_VIDEO_ENCODER_CODEC_AV1; break;
    default: return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    }
    *caps = (Alpha2D3D12EncodeCaps){0};
    ID3D12Device *device = raw_device;
    ID3D12VideoDevice3 *video = NULL;
    HRESULT result = ID3D12Device_QueryInterface(device, &IID_ID3D12VideoDevice3, (void **)&video);
    if (FAILED(result) || video == NULL)
        return alpha2_d3d12_encode_refuse(caps, ALPHA2_D3D12_ENCODE_CHECK_VIDEO_DEVICE, result);
    int32_t step = 0;
    D3D12_VIDEO_ENCODER_PICTURE_RESOLUTION_RATIO_DESC *ratios = NULL;

    D3D12_FEATURE_DATA_VIDEO_ENCODER_CODEC codec_support = {.NodeIndex = 0, .Codec = codec};
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_CODEC, &codec_support, sizeof(codec_support));
    if (FAILED(result) || !codec_support.IsSupported) {
        alpha2_d3d12_encode_refuse(caps, ALPHA2_D3D12_ENCODE_CHECK_CODEC, FAILED(result) ? result : 0);
        goto done;
    }

    Alpha2D3D12EncodeProfile profile_storage;
    D3D12_VIDEO_ENCODER_PROFILE_DESC profile = alpha2_d3d12_encode_profile(codec, &profile_storage);
    Alpha2D3D12EncodeLevel lowest, highest;
    D3D12_FEATURE_DATA_VIDEO_ENCODER_PROFILE_LEVEL levels = {
        .NodeIndex = 0,
        .Codec = codec,
        .Profile = profile,
        .MinSupportedLevel = alpha2_d3d12_encode_level(codec, &lowest),
        .MaxSupportedLevel = alpha2_d3d12_encode_level(codec, &highest),
    };
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_PROFILE_LEVEL, &levels, sizeof(levels));
    if (FAILED(result) || !levels.IsSupported) {
        alpha2_d3d12_encode_refuse(
            caps, ALPHA2_D3D12_ENCODE_CHECK_PROFILE_LEVEL, FAILED(result) ? result : 0);
        goto done;
    }

    D3D12_FEATURE_DATA_VIDEO_ENCODER_INPUT_FORMAT input = {
        .NodeIndex = 0,
        .Codec = codec,
        .Profile = profile,
        .Format = DXGI_FORMAT_NV12,
    };
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_INPUT_FORMAT, &input, sizeof(input));
    if (FAILED(result) || !input.IsSupported) {
        alpha2_d3d12_encode_refuse(
            caps, ALPHA2_D3D12_ENCODE_CHECK_INPUT_FORMAT, FAILED(result) ? result : 0);
        goto done;
    }

    static const struct {
        D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE mode;
        uint32_t bit;
    } modes[] = {
        {D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_CQP, ALPHA2_D3D12_RATE_CONTROL_CQP},
        {D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_CBR, ALPHA2_D3D12_RATE_CONTROL_CBR},
        {D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_VBR, ALPHA2_D3D12_RATE_CONTROL_VBR},
        {D3D12_VIDEO_ENCODER_RATE_CONTROL_MODE_QVBR, ALPHA2_D3D12_RATE_CONTROL_QVBR},
    };
    HRESULT mode_result = S_OK;
    for (size_t index = 0; index < sizeof(modes) / sizeof(modes[0]); ++index) {
        D3D12_FEATURE_DATA_VIDEO_ENCODER_RATE_CONTROL_MODE mode = {
            .NodeIndex = 0,
            .Codec = codec,
            .RateControlMode = modes[index].mode,
        };
        result = ID3D12VideoDevice3_CheckFeatureSupport(
            video, D3D12_FEATURE_VIDEO_ENCODER_RATE_CONTROL_MODE, &mode, sizeof(mode));
        if (FAILED(result)) mode_result = result;
        else if (mode.IsSupported) caps->rate_control_modes |= modes[index].bit;
    }
    if (caps->rate_control_modes == 0) {
        alpha2_d3d12_encode_refuse(caps, ALPHA2_D3D12_ENCODE_CHECK_RATE_CONTROL, mode_result);
        goto done;
    }

    D3D12_FEATURE_DATA_VIDEO_ENCODER_OUTPUT_RESOLUTION_RATIOS_COUNT ratio_count = {
        .NodeIndex = 0,
        .Codec = codec,
    };
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_OUTPUT_RESOLUTION_RATIOS_COUNT, &ratio_count,
        sizeof(ratio_count));
    if (FAILED(result)) {
        alpha2_d3d12_encode_refuse(caps, ALPHA2_D3D12_ENCODE_CHECK_RESOLUTION, result);
        goto done;
    }
    if (ratio_count.ResolutionRatiosCount > 0) {
        ratios = calloc(ratio_count.ResolutionRatiosCount, sizeof(*ratios));
        if (ratios == NULL) {
            step = ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_HOST_ALLOCATION, 0);
            goto done;
        }
    }
    D3D12_FEATURE_DATA_VIDEO_ENCODER_OUTPUT_RESOLUTION resolution = {
        .NodeIndex = 0,
        .Codec = codec,
        .ResolutionRatiosCount = ratio_count.ResolutionRatiosCount,
        .pResolutionRatios = ratios,
    };
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_OUTPUT_RESOLUTION, &resolution, sizeof(resolution));
    if (FAILED(result) || !resolution.IsSupported) {
        alpha2_d3d12_encode_refuse(
            caps, ALPHA2_D3D12_ENCODE_CHECK_RESOLUTION, FAILED(result) ? result : 0);
        goto done;
    }
    caps->min_width = resolution.MinResolutionSupported.Width;
    caps->min_height = resolution.MinResolutionSupported.Height;
    caps->max_width = resolution.MaxResolutionSupported.Width;
    caps->max_height = resolution.MaxResolutionSupported.Height;
    caps->width_multiple = resolution.ResolutionWidthMultipleRequirement;
    caps->height_multiple = resolution.ResolutionHeightMultipleRequirement;

    D3D12_VIDEO_ENCODER_CODEC_PICTURE_CONTROL_SUPPORT_H264 h264_pictures = {0};
    D3D12_VIDEO_ENCODER_CODEC_PICTURE_CONTROL_SUPPORT_HEVC hevc_pictures = {0};
    /* AV1 is asked about the single-reference prediction FFmpeg's encoder uses without B frames. */
    D3D12_VIDEO_ENCODER_CODEC_AV1_PICTURE_CONTROL_SUPPORT av1_pictures = {
        .PredictionMode = D3D12_VIDEO_ENCODER_AV1_COMP_PREDICTION_TYPE_SINGLE_REFERENCE,
    };
    D3D12_FEATURE_DATA_VIDEO_ENCODER_CODEC_PICTURE_CONTROL_SUPPORT pictures = {
        .NodeIndex = 0,
        .Codec = codec,
        .Profile = profile,
    };
    switch (codec) {
    case D3D12_VIDEO_ENCODER_CODEC_H264:
        pictures.PictureSupport.DataSize = sizeof(h264_pictures);
        pictures.PictureSupport.pH264Support = &h264_pictures;
        break;
    case D3D12_VIDEO_ENCODER_CODEC_HEVC:
        pictures.PictureSupport.DataSize = sizeof(hevc_pictures);
        pictures.PictureSupport.pHEVCSupport = &hevc_pictures;
        break;
    default:
        pictures.PictureSupport.DataSize = sizeof(av1_pictures);
        pictures.PictureSupport.pAV1Support = &av1_pictures;
        break;
    }
    result = ID3D12VideoDevice3_CheckFeatureSupport(
        video, D3D12_FEATURE_VIDEO_ENCODER_CODEC_PICTURE_CONTROL_SUPPORT, &pictures,
        sizeof(pictures));
    if (FAILED(result) || !pictures.IsSupported) {
        alpha2_d3d12_encode_refuse(
            caps, ALPHA2_D3D12_ENCODE_CHECK_PICTURE_CONTROL, FAILED(result) ? result : 0);
        goto done;
    }
    switch (codec) {
    case D3D12_VIDEO_ENCODER_CODEC_H264:
        caps->max_p_l0_references = h264_pictures.MaxL0ReferencesForP;
        caps->max_b_l0_references = h264_pictures.MaxL0ReferencesForB;
        caps->max_l1_references = h264_pictures.MaxL1ReferencesForB;
        break;
    case D3D12_VIDEO_ENCODER_CODEC_HEVC:
        caps->max_p_l0_references = hevc_pictures.MaxL0ReferencesForP;
        caps->max_b_l0_references = hevc_pictures.MaxL0ReferencesForB;
        caps->max_l1_references = hevc_pictures.MaxL1ReferencesForB;
        break;
    default:
        /* AV1 has no B pictures as H.26x has them: its references are all forward here. */
        caps->max_p_l0_references = av1_pictures.MaxUniqueReferencesPerFrame;
        break;
    }

    Alpha2D3D12EncodeConfiguration chosen = {0};
    D3D12_VIDEO_ENCODER_CODEC_CONFIGURATION configuration = {0};
    D3D12_VIDEO_ENCODER_SEQUENCE_GOP_STRUCTURE gop = {0};
    int configured = 0;
    result = alpha2_d3d12_encode_configure(
        video, codec, profile, &chosen, &configuration, &gop, &configured);
    if (!configured) {
        alpha2_d3d12_encode_refuse(
            caps, ALPHA2_D3D12_ENCODE_CHECK_CONFIGURATION, FAILED(result) ? result : 0);
        goto done;
    }

    D3D12_VIDEO_ENCODER_PICTURE_RESOLUTION_DESC size = {
        alpha2_d3d12_encode_fit(ALPHA2_D3D12_ENCODE_PROBE_WIDTH, caps->width_multiple,
                                caps->min_width, caps->max_width),
        alpha2_d3d12_encode_fit(ALPHA2_D3D12_ENCODE_PROBE_HEIGHT, caps->height_multiple,
                                caps->min_height, caps->max_height),
    };
    /*
     * Each mode the device lists, asked about whole: a device can list a mode and still refuse
     * the session FFmpeg opens with it (an Intel Iris Xe on driver 30.0.101.2079 fails VBR with
     * its VBV sizes at DXGI_ERROR_DRIVER_INTERNAL_ERROR). Only the modes that pass stay listed.
     */
    uint32_t listed = caps->rate_control_modes;
    caps->rate_control_modes = 0;
    HRESULT first_refusal = S_OK;
    uint32_t first_flags = 0;
    for (size_t index = 0; index < sizeof(modes) / sizeof(modes[0]); ++index) {
        if (!(listed & modes[index].bit)) continue;
        Alpha2D3D12EncodeRate storage;
        Alpha2D3D12EncodeProfile suggested_profile;
        Alpha2D3D12EncodeLevel suggested_level;
        D3D12_FEATURE_DATA_VIDEO_ENCODER_RESOLUTION_SUPPORT_LIMITS limits = {0};
        D3D12_FEATURE_DATA_VIDEO_ENCODER_SUPPORT support = {
            .NodeIndex = 0,
            .Codec = codec,
            .InputFormat = DXGI_FORMAT_NV12,
            .CodecConfiguration = configuration,
            .CodecGopSequence = gop,
            .RateControl = alpha2_d3d12_encode_rate(modes[index].mode, &storage),
            .IntraRefresh = D3D12_VIDEO_ENCODER_INTRA_REFRESH_MODE_NONE,
            .SubregionFrameEncoding = D3D12_VIDEO_ENCODER_FRAME_SUBREGION_LAYOUT_MODE_FULL_FRAME,
            .ResolutionsListCount = 1,
            .pResolutionList = &size,
            /* FFmpeg's MAX_DPB_SIZE - 1. */
            .MaxReferenceFramesInDPB = 15,
            .SuggestedProfile = alpha2_d3d12_encode_profile(codec, &suggested_profile),
            .SuggestedLevel = alpha2_d3d12_encode_level(codec, &suggested_level),
            .pResolutionDependentSupport = &limits,
        };
        result = ID3D12VideoDevice3_CheckFeatureSupport(
            video, D3D12_FEATURE_VIDEO_ENCODER_SUPPORT, &support, sizeof(support));
        int passed = SUCCEEDED(result) &&
            (support.SupportFlags & D3D12_VIDEO_ENCODER_SUPPORT_FLAG_GENERAL_SUPPORT_OK);
        if (passed) {
            caps->rate_control_modes |= modes[index].bit;
            continue;
        }
        caps->refused_rate_control_modes |= modes[index].bit;
        caps->rate_control_status[index] = FAILED(result) ? (int32_t)result : 0;
        if (first_refusal == S_OK && first_flags == 0) {
            first_refusal = FAILED(result) ? result : S_OK;
            first_flags = SUCCEEDED(result) ? (uint32_t)support.ValidationFlags : 0;
        }
    }
    if (caps->rate_control_modes == 0) {
        alpha2_d3d12_encode_refuse(caps, ALPHA2_D3D12_ENCODE_CHECK_SUPPORT, first_refusal);
        caps->validation_flags = first_flags;
        goto done;
    }
    caps->supported = 1;
done:
    free(ratios);
    ID3D12VideoDevice3_Release(video);
    return step;
}
