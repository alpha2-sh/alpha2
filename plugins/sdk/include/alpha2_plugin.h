/*
 * alpha2-plugin/1: the ABI of alpha2 extractor plugins. docs/plugin-abi.md specifies it.
 *
 * A native plugin exports alpha2_plugin_v1. A wasm plugin exports the alpha2_* functions below
 * and imports the host functions from module "alpha2"; there, pointers and lengths are i32
 * offsets into the module's exported memory.
 */
#ifndef ALPHA2_PLUGIN_H
#define ALPHA2_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define ALPHA2_PLUGIN_ABI "alpha2-plugin/1"
#define ALPHA2_PLUGIN_ABI_MAJOR 1u

/* What a plugin call returns. */
#define ALPHA2_OK 0
/* The extractor failed; it called fail with the reason first. */
#define ALPHA2_FAILED 1
/* A panic or exception was caught at the boundary. */
#define ALPHA2_TRAPPED 2

/* Four bytes a pixel, straight alpha, display-encoded sRGB. */
#define ALPHA2_VIDEO_RGBA8 0u
/* One byte a pixel, the BT.709 luma of the display-encoded picture. */
#define ALPHA2_VIDEO_LUMA8 1u

#define ALPHA2_FORWARD 0u
#define ALPHA2_BACKWARD 1u

#define ALPHA2_LOG_DEBUG 0u
#define ALPHA2_LOG_INFO 1u
#define ALPHA2_LOG_WARN 2u

/* Emit record tags; a record is a header (u8 tag, u8 0, u16 output index, u32 body length)
 * and its body, little-endian and unaligned. */
#define ALPHA2_RECORD_SAMPLE 1u
#define ALPHA2_RECORD_WORD 2u
#define ALPHA2_RECORD_INTERVAL 3u
/* The length of an absent speaker or label. */
#define ALPHA2_RECORD_ABSENT 0xFFFFFFFFu

/* What a host gives a native plugin instance; valid until destroy. Pointers passed to these
 * functions are borrowed for the call alone. */
typedef struct Alpha2HostV1 {
    uint32_t abi;
    void *context;
    /* A batch of emit records. ALPHA2_OK, or ALPHA2_FAILED once the session has refused an
     * emit; the plugin then returns ALPHA2_FAILED from the call it is in. */
    int32_t (*emit)(void *context, const uint8_t *records, size_t len);
    /* The resource name (UTF-8) the manifest declares, valid until destroy; NULL when it
     * declares none by that name. */
    const uint8_t *(*resource)(void *context, const uint8_t *name, size_t name_len,
                               size_t *len);
    /* Why the call about to return ALPHA2_FAILED failed (UTF-8). */
    void (*fail)(void *context, const uint8_t *message, size_t len);
    void (*log)(void *context, uint32_t level, const uint8_t *message, size_t len);
} Alpha2HostV1;

/* What alpha2_plugin_v1 returns: a static table. Pointers passed to the plugin are borrowed
 * for the call alone. The host calls an instance from one thread at a time. */
typedef struct Alpha2PluginV1 {
    uint32_t abi;
    /* The descriptor, UTF-8 JSON; static. */
    const uint8_t *(*describe)(size_t *len);
    /* A new instance, or NULL. */
    void *(*create)(const Alpha2HostV1 *host);
    void (*destroy)(void *instance);
    /* The run's context, UTF-8 JSON. */
    int32_t (*begin)(void *instance, const uint8_t *context, size_t len);
    int32_t (*pass)(void *instance, uint32_t pass, uint32_t direction);
    /* Rows top first, tightly packed. */
    int32_t (*video)(void *instance, int64_t time, uint32_t width, uint32_t height,
                     uint32_t format, const uint8_t *data, size_t len);
    /* count = frames * channels floats, planar, channel after channel. */
    int32_t (*audio)(void *instance, int64_t start, int64_t start_sample, uint32_t frames,
                     uint32_t rate, uint32_t channels, const float *samples, size_t count);
    /* Whole-range work emits here. */
    int32_t (*end)(void *instance);
} Alpha2PluginV1;

#if !defined(__wasm__)

/* The one symbol a native plugin exports. */
const Alpha2PluginV1 *alpha2_plugin_v1(void);

#else

/* wasm exports. alpha2_describe returns the descriptor's offset in the low 32 bits and its
 * length in the high 32. */
int32_t alpha2_abi(void);
int32_t alpha2_alloc(int32_t size, int32_t align);
void alpha2_free(int32_t ptr, int32_t size, int32_t align);
int64_t alpha2_describe(void);
int32_t alpha2_begin(int32_t ptr, int32_t len);
int32_t alpha2_pass(int32_t pass, int32_t direction);
int32_t alpha2_video(int64_t time, int32_t width, int32_t height, int32_t format, int32_t ptr,
                     int32_t len);
int32_t alpha2_audio(int64_t start, int64_t start_sample, int32_t frames, int32_t rate,
                     int32_t channels, int32_t ptr, int32_t count);
int32_t alpha2_end(void);

/* wasm imports, alpha2.emit and so on. resource_size is -1 for an undeclared name;
 * resource_read is 0 when it copied the whole resource. */
#define ALPHA2_IMPORT(name) __attribute__((import_module("alpha2"), import_name(name)))
ALPHA2_IMPORT("emit") int32_t alpha2_host_emit(const uint8_t *records, size_t len);
ALPHA2_IMPORT("resource_size") int64_t alpha2_host_resource_size(const uint8_t *name,
                                                                 size_t name_len);
ALPHA2_IMPORT("resource_read") int32_t alpha2_host_resource_read(const uint8_t *name,
                                                                 size_t name_len, uint8_t *dst,
                                                                 size_t dst_len);
ALPHA2_IMPORT("fail") void alpha2_host_fail(const uint8_t *message, size_t len);
ALPHA2_IMPORT("log") void alpha2_host_log(uint32_t level, const uint8_t *message, size_t len);
#undef ALPHA2_IMPORT

#endif

#ifdef __cplusplus
}
#endif

#endif
