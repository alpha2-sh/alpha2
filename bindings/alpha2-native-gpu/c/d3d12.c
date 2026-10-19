#define COBJMACROS
#include <initguid.h>
#include <d3d12.h>
#include <stdint.h>
#include <stdlib.h>
#include "alpha2_native_gpu_d3d12.h"
#include "alpha2_native_gpu_status.h"

/* A bi-planar texture decode surfaces are copied into, reused for one format and size. */
typedef struct Alpha2D3D12Target {
    ID3D12Resource *resource;
    DXGI_FORMAT format;
    uint64_t width;
    uint32_t height;
    struct Alpha2D3D12Target *next;
} Alpha2D3D12Target;

/*
 * What one copy is recorded and fenced with, kept between copies once its last copy is known to
 * be over. The fence is the slot's own, not one the pool's copies share: the renderer stages a
 * wait on it and takes the wait off by the fence alone, which on a shared fence would take off
 * the waits of other copies too.
 */
typedef struct Alpha2D3D12Slot {
    ID3D12CommandAllocator *allocator;
    ID3D12GraphicsCommandList *commands;
    ID3D12Fence *fence;
    /* The last value a copy signalled; values only grow. */
    uint64_t fence_value;
    struct Alpha2D3D12Slot *next;
} Alpha2D3D12Slot;

/*
 * The targets and slots of one renderer device. It is freed once its owner has destroyed it and
 * the last copy made from it has given back what it took, or been leaked.
 */
typedef struct Alpha2D3D12Pool {
    SRWLOCK lock;
    /* One for the owner and one for each copy not yet freed. */
    volatile LONG references;
    ID3D12Device *device;
    Alpha2D3D12Target *targets;
    uint32_t idle_targets;
    Alpha2D3D12Slot *slots;
    uint32_t idle_slots;
    /* 0 once the device refused D3D12_HEAP_FLAG_CREATE_NOT_ZEROED (before Windows 10 2004). */
    int not_zeroed;
    uint32_t targets_made;
    uint32_t slots_made;
} Alpha2D3D12Pool;

typedef struct Alpha2D3D12Copy {
    Alpha2D3D12Pool *pool;
    Alpha2D3D12Target *target;
    Alpha2D3D12Slot *slot;
    uint64_t fence_value;
} Alpha2D3D12Copy;

/*
 * How many idle targets and slots a pool keeps; each idle target is a whole frame of video memory.
 * A full pool frees its least recently given back target, so the sizes no longer shown go first.
 */
#define ALPHA2_D3D12_IDLE_TARGETS 4
#define ALPHA2_D3D12_IDLE_SLOTS 8

/* Bounds every wait here, so a hung or lost device cannot hang the process. */
static volatile LONG alpha2_wait_timeout_ms = 30000;

void alpha2_native_gpu_d3d12_set_wait_timeout(uint32_t milliseconds) {
    /* INFINITE would make the wait unbounded again; no caller may store it. */
    if (milliseconds == INFINITE) milliseconds = INFINITE - 1;
    InterlockedExchange(&alpha2_wait_timeout_ms, (LONG)milliseconds);
}

/* The timeout `alpha2_native_gpu_d3d12_set_wait_timeout` stored. */
uint32_t alpha2_native_gpu_d3d12_wait_timeout(void) {
    return (uint32_t)InterlockedCompareExchange(&alpha2_wait_timeout_ms, 0, 0);
}

static void alpha2_d3d12_target_free(Alpha2D3D12Target *target) {
    if (target == NULL) return;
    if (target->resource != NULL) ID3D12Resource_Release(target->resource);
    free(target);
}

static void alpha2_d3d12_slot_free(Alpha2D3D12Slot *slot) {
    if (slot == NULL) return;
    if (slot->commands != NULL) ID3D12GraphicsCommandList_Release(slot->commands);
    if (slot->allocator != NULL) ID3D12CommandAllocator_Release(slot->allocator);
    if (slot->fence != NULL) ID3D12Fence_Release(slot->fence);
    free(slot);
}

/* Drops one reference to `pool`, freeing it and everything idle in it with the last. */
static void alpha2_d3d12_pool_release(Alpha2D3D12Pool *pool) {
    if (InterlockedDecrement(&pool->references) != 0) return;
    while (pool->targets != NULL) {
        Alpha2D3D12Target *target = pool->targets;
        pool->targets = target->next;
        alpha2_d3d12_target_free(target);
    }
    while (pool->slots != NULL) {
        Alpha2D3D12Slot *slot = pool->slots;
        pool->slots = slot->next;
        alpha2_d3d12_slot_free(slot);
    }
    ID3D12Device_Release(pool->device);
    free(pool);
}

/* A pool for copies on `raw_device`, which it holds a reference to; NULL when out of memory. */
void *alpha2_native_gpu_d3d12_pool_create(void *raw_device) {
    if (raw_device == NULL) return NULL;
    Alpha2D3D12Pool *pool = calloc(1, sizeof(*pool));
    if (pool == NULL) return NULL;
    InitializeSRWLock(&pool->lock);
    pool->references = 1;
    pool->device = raw_device;
    ID3D12Device_AddRef(pool->device);
    pool->not_zeroed = 1;
    return pool;
}

/*
 * Gives up the owner's reference to a pool. What is idle in it is freed now, or with the last copy
 * still out.
 */
void alpha2_native_gpu_d3d12_pool_destroy(void *pool) {
    if (pool != NULL) alpha2_d3d12_pool_release(pool);
}

/* How many targets and slots `raw_pool` has made, and how many are idle in it now. */
void alpha2_native_gpu_d3d12_pool_counts(
    void *raw_pool, uint32_t *targets_made, uint32_t *slots_made,
    uint32_t *idle_targets, uint32_t *idle_slots
) {
    Alpha2D3D12Pool *pool = raw_pool;
    AcquireSRWLockExclusive(&pool->lock);
    *targets_made = pool->targets_made;
    *slots_made = pool->slots_made;
    *idle_targets = pool->idle_targets;
    *idle_slots = pool->idle_slots;
    ReleaseSRWLockExclusive(&pool->lock);
}

/* The bytes of a bi-planar 4:2:0 target: a luma plane and a half-height chroma plane, at one byte a
   sample for NV12 and two for P010. */
static uint64_t alpha2_d3d12_target_bytes(const Alpha2D3D12Target *target) {
    uint64_t sample = target->format == DXGI_FORMAT_P010 ? 2 : 1;
    uint64_t width = (target->width + 1) & ~(uint64_t)1;
    uint64_t chroma_rows = ((uint64_t)target->height + 1) / 2;
    return sample * width * ((uint64_t)target->height + chroma_rows);
}

/*
 * Frees every idle target of `raw_pool` and returns their bytes. Copies still out keep theirs, and
 * give them back as before; idle slots, which hold no frame, stay. Made counts are kept.
 */
uint64_t alpha2_native_gpu_d3d12_pool_trim(void *raw_pool) {
    Alpha2D3D12Pool *pool = raw_pool;
    if (pool == NULL) return 0;
    AcquireSRWLockExclusive(&pool->lock);
    Alpha2D3D12Target *idle = pool->targets;
    pool->targets = NULL;
    pool->idle_targets = 0;
    ReleaseSRWLockExclusive(&pool->lock);
    uint64_t bytes = 0;
    while (idle != NULL) {
        Alpha2D3D12Target *target = idle;
        idle = target->next;
        bytes += alpha2_d3d12_target_bytes(target);
        alpha2_d3d12_target_free(target);
    }
    return bytes;
}

/* An idle target of `descriptor`'s format and size, or a new one; NULL with `result` on failure. */
static Alpha2D3D12Target *alpha2_d3d12_take_target(
    Alpha2D3D12Pool *pool, const D3D12_RESOURCE_DESC *descriptor, HRESULT *result
) {
    AcquireSRWLockExclusive(&pool->lock);
    for (Alpha2D3D12Target **link = &pool->targets; *link != NULL; link = &(*link)->next) {
        Alpha2D3D12Target *target = *link;
        if (target->format == descriptor->Format && target->width == descriptor->Width &&
            target->height == descriptor->Height) {
            *link = target->next;
            pool->idle_targets--;
            ReleaseSRWLockExclusive(&pool->lock);
            target->next = NULL;
            return target;
        }
    }
    int not_zeroed = pool->not_zeroed;
    ReleaseSRWLockExclusive(&pool->lock);
    Alpha2D3D12Target *target = calloc(1, sizeof(*target));
    if (target == NULL) {
        *result = E_OUTOFMEMORY;
        return NULL;
    }
    target->format = descriptor->Format;
    target->width = descriptor->Width;
    target->height = descriptor->Height;
    D3D12_HEAP_PROPERTIES heap = {
        .Type = D3D12_HEAP_TYPE_DEFAULT,
        .CreationNodeMask = 1,
        .VisibleNodeMask = 1,
    };
    D3D12_RESOURCE_DESC wanted = *descriptor;
    wanted.Flags = D3D12_RESOURCE_FLAG_NONE;
    /* Each copy overwrites the whole target, so the zero fill a committed resource gets by
       default would be wasted. */
    *result = ID3D12Device_CreateCommittedResource(
        pool->device, &heap,
        not_zeroed ? D3D12_HEAP_FLAG_CREATE_NOT_ZEROED : D3D12_HEAP_FLAG_NONE,
        &wanted, D3D12_RESOURCE_STATE_COMMON, NULL,
        &IID_ID3D12Resource, (void **)&target->resource);
    if (*result == E_INVALIDARG && not_zeroed) {
        /* A runtime before Windows 10 2004 does not know the flag. */
        *result = ID3D12Device_CreateCommittedResource(
            pool->device, &heap, D3D12_HEAP_FLAG_NONE, &wanted, D3D12_RESOURCE_STATE_COMMON,
            NULL, &IID_ID3D12Resource, (void **)&target->resource);
        if (SUCCEEDED(*result)) {
            AcquireSRWLockExclusive(&pool->lock);
            pool->not_zeroed = 0;
            ReleaseSRWLockExclusive(&pool->lock);
        }
    }
    if (FAILED(*result)) {
        alpha2_d3d12_target_free(target);
        return NULL;
    }
    AcquireSRWLockExclusive(&pool->lock);
    pool->targets_made++;
    ReleaseSRWLockExclusive(&pool->lock);
    return target;
}

/*
 * An idle slot, reset and recording, or a new one; NULL with the failing step and `result`. An
 * idle slot's last copy is over: a copy gives its slot back only once its fence says so.
 */
static Alpha2D3D12Slot *alpha2_d3d12_take_slot(
    Alpha2D3D12Pool *pool, int32_t *step, HRESULT *result
) {
    AcquireSRWLockExclusive(&pool->lock);
    Alpha2D3D12Slot *slot = pool->slots;
    if (slot != NULL) {
        pool->slots = slot->next;
        pool->idle_slots--;
    }
    ReleaseSRWLockExclusive(&pool->lock);
    if (slot != NULL) {
        slot->next = NULL;
        *step = ALPHA2_GPU_STEP_RESET_COMMAND_POOL;
        *result = ID3D12CommandAllocator_Reset(slot->allocator);
        if (FAILED(*result)) goto fail;
        *step = ALPHA2_GPU_STEP_RESET_COMMAND_BUFFER;
        *result = ID3D12GraphicsCommandList_Reset(slot->commands, slot->allocator, NULL);
        if (FAILED(*result)) goto fail;
        return slot;
    }
    slot = calloc(1, sizeof(*slot));
    if (slot == NULL) {
        *step = ALPHA2_GPU_STEP_HOST_ALLOCATION;
        *result = 0;
        return NULL;
    }
    *step = ALPHA2_GPU_STEP_CREATE_COMMAND_ALLOCATOR;
    *result = ID3D12Device_CreateCommandAllocator(
        pool->device, D3D12_COMMAND_LIST_TYPE_DIRECT,
        &IID_ID3D12CommandAllocator, (void **)&slot->allocator);
    if (FAILED(*result)) goto fail;
    *step = ALPHA2_GPU_STEP_CREATE_COMMAND_LIST;
    *result = ID3D12Device_CreateCommandList(
        pool->device, 0, D3D12_COMMAND_LIST_TYPE_DIRECT, slot->allocator, NULL,
        &IID_ID3D12GraphicsCommandList, (void **)&slot->commands);
    if (FAILED(*result)) goto fail;
    *step = ALPHA2_GPU_STEP_CREATE_FENCE;
    *result = ID3D12Device_CreateFence(
        pool->device, 0, D3D12_FENCE_FLAG_NONE, &IID_ID3D12Fence, (void **)&slot->fence);
    if (FAILED(*result)) goto fail;
    AcquireSRWLockExclusive(&pool->lock);
    pool->slots_made++;
    ReleaseSRWLockExclusive(&pool->lock);
    return slot;
fail:
    alpha2_d3d12_slot_free(slot);
    return NULL;
}

/*
 * Keeps `target` and `slot`, which no GPU work uses, for the next copy. A full pool frees its least
 * recently given back target to keep `target`, and frees `slot` itself (slots are all alike).
 * Either may be NULL; a slot's command list must be closed.
 */
static void alpha2_d3d12_give_back(
    Alpha2D3D12Pool *pool, Alpha2D3D12Target *target, Alpha2D3D12Slot *slot
) {
    Alpha2D3D12Target *evicted = NULL;
    AcquireSRWLockExclusive(&pool->lock);
    if (target != NULL) {
        if (pool->idle_targets >= ALPHA2_D3D12_IDLE_TARGETS) {
            /* The list runs from the most recently given back to the least. */
            Alpha2D3D12Target **last = &pool->targets;
            while ((*last)->next != NULL) last = &(*last)->next;
            evicted = *last;
            *last = NULL;
            pool->idle_targets--;
        }
        target->next = pool->targets;
        pool->targets = target;
        pool->idle_targets++;
        target = NULL;
    }
    if (slot != NULL && pool->idle_slots < ALPHA2_D3D12_IDLE_SLOTS) {
        slot->next = pool->slots;
        pool->slots = slot;
        pool->idle_slots++;
        slot = NULL;
    }
    ReleaseSRWLockExclusive(&pool->lock);
    alpha2_d3d12_target_free(evicted);
    alpha2_d3d12_slot_free(slot);
}

/* Gives `copy`'s target and slot back to its pool once its GPU work is known to be done, and
   returns 1; returns 0, giving back nothing, when that cannot be established in time. The GPU may
   still be using what is left, and so may it be using the decode surface the copy reads: a caller
   that sees 0 must not give that surface back either. */
static int alpha2_d3d12_copy_free(Alpha2D3D12Copy *copy) {
    if (copy == NULL) return 1;
    ID3D12Fence *fence = copy->slot->fence;
    if (ID3D12Fence_GetCompletedValue(fence) < copy->fence_value) {
        HANDLE event = CreateEventW(NULL, FALSE, FALSE, NULL);
        if (event != NULL && SUCCEEDED(ID3D12Fence_SetEventOnCompletion(
                fence, copy->fence_value, event)))
            WaitForSingleObject(event,
                (DWORD)InterlockedCompareExchange(&alpha2_wait_timeout_ms, 0, 0));
        if (event != NULL) CloseHandle(event);
        /* A timeout, or a wait that could not be set up: only the fence says the copy is over.
           Its target and slot stay out of the pool, and the pool stays with them. */
        if (ID3D12Fence_GetCompletedValue(fence) < copy->fence_value) return 0;
    }
    alpha2_d3d12_give_back(copy->pool, copy->target, copy->slot);
    alpha2_d3d12_pool_release(copy->pool);
    free(copy);
    return 1;
}

/*
 * Copies a decoded NV12 or P010 surface into a target from `raw_pool` on `raw_queue`, after the
 * decoder's fence reaches `decode_value`, and returns the copy's ticket and description. Returns
 * 0, or the step that failed with its HRESULT in `status` (`alpha2_native_gpu_status.h`).
 * ALPHA2_GPU_STEP_QUEUE_SIGNAL means the copy is on the GPU unfenced.
 */
int32_t alpha2_native_gpu_d3d12_copy(
    void *raw_pool, void *raw_queue, void *raw_source,
    void *raw_decode_fence, uint64_t decode_value,
    void **output_ticket, Alpha2D3D12CopyInfo *output, int32_t *status
) {
    if (raw_pool == NULL || raw_queue == NULL || raw_source == NULL ||
        raw_decode_fence == NULL || output_ticket == NULL || output == NULL)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    Alpha2D3D12Pool *pool = raw_pool;
    ID3D12CommandQueue *queue = raw_queue;
    ID3D12Resource *source = raw_source;
    ID3D12Fence *decode_fence = raw_decode_fence;
    // `GetDesc` returns a struct, which the COM ABI passes back through a hidden out-parameter.
    // Calling through the vtable with that parameter is the same on mingw-w64 and current Windows
    // SDK headers; the `ID3D12Resource_GetDesc` macro is a compile error on mingw-w64 and, on old
    // SDK headers, a by-value call with the wrong ABI.
    D3D12_RESOURCE_DESC descriptor;
    source->lpVtbl->GetDesc(source, &descriptor);
    if (descriptor.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D ||
        descriptor.DepthOrArraySize != 1 || descriptor.MipLevels != 1 ||
        (descriptor.Format != DXGI_FORMAT_NV12 &&
         descriptor.Format != DXGI_FORMAT_P010))
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    Alpha2D3D12Copy *copy = calloc(1, sizeof(*copy));
    if (copy == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_HOST_ALLOCATION, 0);
    HRESULT result = S_OK;
    int32_t step = ALPHA2_GPU_STEP_CREATE_RESOURCE;
    copy->target = alpha2_d3d12_take_target(pool, &descriptor, &result);
    if (copy->target == NULL) goto fail;
    copy->slot = alpha2_d3d12_take_slot(pool, &step, &result);
    if (copy->slot == NULL) goto fail;
    ID3D12GraphicsCommandList *commands = copy->slot->commands;
    ID3D12Resource *target = copy->target->resource;
    /* A target rests in COMMON: made there, and left there by the copy before. */
    D3D12_RESOURCE_BARRIER barriers[] = {
        {
            .Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            .Flags = D3D12_RESOURCE_BARRIER_FLAG_NONE,
            .Transition = {
                .pResource = target,
                .Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                .StateBefore = D3D12_RESOURCE_STATE_COMMON,
                .StateAfter = D3D12_RESOURCE_STATE_COPY_DEST,
            },
        },
        {
            .Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            .Flags = D3D12_RESOURCE_BARRIER_FLAG_NONE,
            .Transition = {
                .pResource = target,
                .Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                .StateBefore = D3D12_RESOURCE_STATE_COPY_DEST,
                .StateAfter = D3D12_RESOURCE_STATE_COMMON,
            },
        },
    };
    ID3D12GraphicsCommandList_ResourceBarrier(commands, 1, &barriers[0]);
    ID3D12GraphicsCommandList_CopyResource(commands, target, source);
    ID3D12GraphicsCommandList_ResourceBarrier(commands, 1, &barriers[1]);
    step = ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST;
    result = ID3D12GraphicsCommandList_Close(commands);
    if (FAILED(result)) goto fail;
    step = ALPHA2_GPU_STEP_QUEUE_WAIT;
    result = ID3D12CommandQueue_Wait(queue, decode_fence, decode_value);
    if (FAILED(result)) goto fail;
    InterlockedIncrement(&pool->references);
    copy->pool = pool;
    ID3D12CommandList *lists[] = {(ID3D12CommandList *)commands};
    ID3D12CommandQueue_ExecuteCommandLists(queue, 1, lists);
    uint64_t value = copy->slot->fence_value + 1;
    result = ID3D12CommandQueue_Signal(queue, copy->slot->fence, value);
    /*
     * The copy is on the GPU now, reading the decode surface, and with no signal nothing says when
     * it ends: it, its target and slot, and the surface are left in use, and the pool with them.
     */
    if (FAILED(result)) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SIGNAL, result);
    copy->slot->fence_value = value;
    copy->fence_value = value;
    *output = (Alpha2D3D12CopyInfo){
        .resource = (uint64_t)(uintptr_t)target,
        .fence = (uint64_t)(uintptr_t)copy->slot->fence,
        .fence_value = value,
        .width = (uint32_t)descriptor.Width,
        .height = descriptor.Height,
        .dxgi_format = (uint32_t)descriptor.Format,
    };
    *output_ticket = copy;
    return 0;
fail:
    /* Nothing reached the GPU, so what was taken goes back; a slot whose command list could not
       be closed is freed instead, as no reset may follow an open list. */
    if (step == ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST) {
        alpha2_d3d12_slot_free(copy->slot);
        copy->slot = NULL;
    }
    alpha2_d3d12_give_back(pool, copy->target, copy->slot);
    free(copy);
    return ALPHA2_GPU_FAIL(status, step, result);
}

/*
 * Frees a ticket `alpha2_native_gpu_d3d12_copy` returned: 1 once its target and slot are back in
 * the pool, 0 when its copy could not be shown finished in time, which leaks the copy and keeps
 * its source surface in use.
 */
int32_t alpha2_native_gpu_d3d12_copy_destroy(void *ticket) {
    return alpha2_d3d12_copy_free(ticket);
}
