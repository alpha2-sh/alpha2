#ifndef ALPHA2_FFMPEG_VERSION_H
#define ALPHA2_FFMPEG_VERSION_H

/*
 * Every shim that reads FFmpeg structures includes this first.
 *
 * The shims read structures FFmpeg may lay out differently at any major version (AVVkFrame,
 * AVVulkanFramesContext, the D3D12 and VideoToolbox contexts). The build script defines the majors
 * the shims are written for, the pinned SDK's (alpha2-build-support's FFMPEG_AVUTIL_MAJOR and
 * FFMPEG_AVCODEC_MAJOR); headers of another major stop the build here rather than compile to code
 * that reads the wrong offsets.
 */

#include <libavcodec/version_major.h>
#include <libavutil/version.h>

#if !defined(ALPHA2_FFMPEG_AVUTIL_MAJOR) || !defined(ALPHA2_FFMPEG_AVCODEC_MAJOR)
#error "compile FFmpeg shims with alpha2_build_support::compile_ffmpeg_shim, which defines the pinned majors"
#endif

#if LIBAVUTIL_VERSION_MAJOR != ALPHA2_FFMPEG_AVUTIL_MAJOR
#error "these libavutil headers are not the major version alpha2's FFmpeg shims are written for; build against the pinned SDK (scripts/fetch-ffmpeg-sdk.sh)"
#endif

#if LIBAVCODEC_VERSION_MAJOR != ALPHA2_FFMPEG_AVCODEC_MAJOR
#error "these libavcodec headers are not the major version alpha2's FFmpeg shims are written for; build against the pinned SDK (scripts/fetch-ffmpeg-sdk.sh)"
#endif

#endif
