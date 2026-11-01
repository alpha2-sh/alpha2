#ifndef ALPHA2_VULKAN_LOADER_H
#define ALPHA2_VULKAN_LOADER_H

/*
 * The Vulkan loader is opened at run time, not linked, so the host still starts on a machine
 * without one. These helpers only run once a Vulkan device exists.
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

static inline PFN_vkVoidFunction alpha2_vulkan_symbol(const char *name) {
    pthread_once(&alpha2_vulkan_opened, alpha2_vulkan_open);
    if (alpha2_vulkan_handle == NULL) return NULL;
    return (PFN_vkVoidFunction)dlsym(alpha2_vulkan_handle, name);
}

#endif
