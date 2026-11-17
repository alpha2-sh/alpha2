#ifndef ALPHA2_FFMPEG_VERSION_H
#define ALPHA2_FFMPEG_VERSION_H

/*
 * Include first in every shim. FFmpeg layouts can change at any major version, so headers of a
 * major other than the pinned SDK's stop the build instead of reading wrong offsets.
 */

#include <libavcodec/version_major.h>
#include <libavutil/version.h>

#if !defined(ALPHA2_FFMPEG_AVUTIL_MAJOR) || !defined(ALPHA2_FFMPEG_AVCODEC_MAJOR)
#error "compile FFmpeg shims with alpha2_build_support::compile_ffmpeg_shim, which defines the pinned majors"
#endif

#if LIBAVUTIL_VERSION_MAJOR != ALPHA2_FFMPEG_AVUTIL_MAJOR
#error "these libavutil headers are not the major version alpha2's FFmpeg shims are written for; build against the pinned SDK (scripts/fetch-ffmpeg-sdk.sh)"
#endif

/* The D3D12VA frame layouts the shims read first appear in libavutil 60.13.100. */
#if LIBAVUTIL_VERSION_INT < AV_VERSION_INT(60, 13, 100)
#error "these libavutil headers are older than 60.13.100, the first with the layouts alpha2's FFmpeg shims read; build against the pinned SDK (scripts/fetch-ffmpeg-sdk.sh)"
#endif

#if LIBAVCODEC_VERSION_MAJOR != ALPHA2_FFMPEG_AVCODEC_MAJOR
#error "these libavcodec headers are not the major version alpha2's FFmpeg shims are written for; build against the pinned SDK (scripts/fetch-ffmpeg-sdk.sh)"
#endif

#endif
