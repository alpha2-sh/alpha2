/*
 * Two threads set the Vulkan helper's wait timeout at once. Compile with ThreadSanitizer: the
 * public Rust setter can run concurrently with another setter or any native GPU wait. No GPU is
 * needed for this.
 */
#include <pthread.h>
#include <stdint.h>
#include <assert.h>

void alpha2_native_gpu_vulkan_set_wait_timeout(uint64_t nanoseconds);

static pthread_barrier_t ready;

static void *write_timeout(void *argument) {
    uint64_t offset = (uint64_t)(uintptr_t)argument;
    pthread_barrier_wait(&ready);
    for (uint64_t index = 0; index < 100000; ++index)
        alpha2_native_gpu_vulkan_set_wait_timeout(30000000000ull + offset + index);
    return NULL;
}

int main(void) {
    pthread_t first, second;
    assert(pthread_barrier_init(&ready, NULL, 2) == 0);
    assert(pthread_create(&first, NULL, write_timeout, (void *)(uintptr_t)1) == 0);
    assert(pthread_create(&second, NULL, write_timeout, (void *)(uintptr_t)2) == 0);
    assert(pthread_join(first, NULL) == 0);
    assert(pthread_join(second, NULL) == 0);
    assert(pthread_barrier_destroy(&ready) == 0);
    return 0;
}
