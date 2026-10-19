#ifndef ALPHA2_VULKAN_LOADER_H
#define ALPHA2_VULKAN_LOADER_H

/*
 * The Vulkan loader, opened at run time as FFmpeg and wgpu open it. Nothing links libvulkan.so.1,
 * so a machine without a loader still starts the host, which then runs on a backend that needs
 * none; these helpers only run once a Vulkan device exists, which means the loader is there.
 */

#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>
#include <vulkan/vulkan.h>

static void *alpha2_vulkan_handle;
static pthread_once_t alpha2_vulkan_opened = PTHREAD_ONCE_INIT;

static void alpha2_vulkan_open(void) {
    alpha2_vulkan_handle = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
}

/* The loader's entry point `name`, or NULL when there is no loader or it lacks the entry point. */
static inline PFN_vkVoidFunction alpha2_vulkan_symbol(const char *name) {
    pthread_once(&alpha2_vulkan_opened, alpha2_vulkan_open);
    if (alpha2_vulkan_handle == NULL) return NULL;
    return (PFN_vkVoidFunction)dlsym(alpha2_vulkan_handle, name);
}

#endif
