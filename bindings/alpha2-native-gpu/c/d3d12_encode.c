#define COBJMACROS
#include <d3d12.h>
#include <stdint.h>
#include <stdlib.h>
#include "alpha2_native_gpu_d3d12.h"
#include "alpha2_native_gpu_status.h"

/* `c/d3d12.c`: the timeout every wait of these helpers uses. */
uint32_t alpha2_native_gpu_d3d12_wait_timeout(void);

#define ALPHA2_D3D12_ENCODE_COPY_SLOTS 4

/* The status a wait that ran out of time reports: WAIT_TIMEOUT, which no failing HRESULT is. */
#define ALPHA2_D3D12_WAIT_TIMED_OUT ((HRESULT)0x102)

/* One encode copy's allocator, list and fence, in a fixed ring: an export's copies go in order. */
typedef struct Alpha2D3D12EncodeSlot {
    ID3D12CommandAllocator *allocator;
    /* NULL once a close failed: such a list may not be reset, so the next use makes it again. */
    ID3D12GraphicsCommandList *commands;
    ID3D12Fence *fence;
    /* The last value a copy from this slot signalled, or was to signal; values only grow. */
    uint64_t fence_value;
    /* 1 while the list is reset and open with nothing recorded, ready for the next copy. */
    int recording;
} Alpha2D3D12EncodeSlot;

/*
 * Owns everything the NV12 handoff into D3D12VA encoder frames reuses across frames: four slots,
 * waiting only on the fence of the slot about to be reused, rather than idling the queue a frame.
 *
 * Nothing on the GPU waits for the renderer: the packing render was submitted earlier on the same
 * queue, and the copy's transitions out of RENDER_TARGET order it after that work. Waiting instead
 * on a fence the render was to signal would never run when wgpu rejected that submission.
 */
typedef struct Alpha2D3D12EncodeCopier {
    ID3D12Device *device;
    Alpha2D3D12EncodeSlot slots[ALPHA2_D3D12_ENCODE_COPY_SLOTS];
    uint32_t slot;
    /*
     * 1 once a copy went to the GPU and its slot could not be signalled. Nothing then says when
     * that copy ends, so no slot is reused and nothing is freed: every later call fails at
     * ALPHA2_GPU_STEP_QUEUE_SIGNAL, and the caller keeps what the copier reads.
     */
    int unfenced;
} Alpha2D3D12EncodeCopier;

static void alpha2_d3d12_encode_slot_release(Alpha2D3D12EncodeSlot *slot) {
    if (slot->commands != NULL) ID3D12GraphicsCommandList_Release(slot->commands);
    if (slot->allocator != NULL) ID3D12CommandAllocator_Release(slot->allocator);
    if (slot->fence != NULL) ID3D12Fence_Release(slot->fence);
    slot->commands = NULL;
    slot->allocator = NULL;
    slot->fence = NULL;
}

/*
 * Makes `slot`'s allocator and command list, the list closed, as a reset expects it. Returns 0, or
 * the step that failed with its HRESULT in `result`.
 */
static int32_t alpha2_d3d12_encode_slot_make_list(
    ID3D12Device *device, Alpha2D3D12EncodeSlot *slot, HRESULT *result
) {
    if (slot->allocator == NULL) {
        *result = ID3D12Device_CreateCommandAllocator(
            device, D3D12_COMMAND_LIST_TYPE_DIRECT,
            &IID_ID3D12CommandAllocator, (void **)&slot->allocator);
        if (FAILED(*result)) return ALPHA2_GPU_STEP_CREATE_COMMAND_ALLOCATOR;
    }
    *result = ID3D12Device_CreateCommandList(
        device, 0, D3D12_COMMAND_LIST_TYPE_DIRECT, slot->allocator, NULL,
        &IID_ID3D12GraphicsCommandList, (void **)&slot->commands);
    if (FAILED(*result)) {
        slot->commands = NULL;
        return ALPHA2_GPU_STEP_CREATE_COMMAND_LIST;
    }
    /* A list is made open; the slot's own reset opens it for each copy. */
    *result = ID3D12GraphicsCommandList_Close(slot->commands);
    if (FAILED(*result)) {
        ID3D12GraphicsCommandList_Release(slot->commands);
        slot->commands = NULL;
        return ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST;
    }
    return 0;
}

/*
 * Waits, up to the shared timeout, until the last copy from `slot` is over. Returns 0, or the step
 * with WAIT_TIMEOUT or the failing HRESULT in `result`.
 */
static int32_t alpha2_d3d12_encode_slot_wait(Alpha2D3D12EncodeSlot *slot, HRESULT *result) {
    if (ID3D12Fence_GetCompletedValue(slot->fence) >= slot->fence_value) return 0;
    HANDLE event = CreateEventW(NULL, FALSE, FALSE, NULL);
    if (event == NULL) {
        *result = 0;
        return ALPHA2_GPU_STEP_HOST_ALLOCATION;
    }
    *result = ID3D12Fence_SetEventOnCompletion(slot->fence, slot->fence_value, event);
    if (SUCCEEDED(*result))
        WaitForSingleObject(event, (DWORD)alpha2_native_gpu_d3d12_wait_timeout());
    CloseHandle(event);
    if (FAILED(*result)) return ALPHA2_GPU_STEP_WAIT_FENCE;
    /* Only the fence says the copy is over; a wake for any other reason is a timeout. */
    if (ID3D12Fence_GetCompletedValue(slot->fence) < slot->fence_value) {
        *result = ALPHA2_D3D12_WAIT_TIMED_OUT;
        return ALPHA2_GPU_STEP_WAIT_FENCE;
    }
    return 0;
}

/*
 * Readies `slot` for a copy: waits for its last copy, then resets its allocator and list, or makes
 * a list again after a failed close. A slot already readied is left as it is, so a copy that
 * failed before it recorded anything takes the same slot again. Returns 0, or the step that failed
 * with its HRESULT in `result`.
 */
static int32_t alpha2_d3d12_encode_slot_ready(
    Alpha2D3D12EncodeCopier *copier, Alpha2D3D12EncodeSlot *slot, HRESULT *result
) {
    if (slot->recording) return 0;
    int32_t step = alpha2_d3d12_encode_slot_wait(slot, result);
    if (step != 0) return step;
    /* The allocator's commands are over, so it may be reset whether or not its list survived. */
    *result = ID3D12CommandAllocator_Reset(slot->allocator);
    if (FAILED(*result)) return ALPHA2_GPU_STEP_RESET_COMMAND_POOL;
    if (slot->commands == NULL) {
        step = alpha2_d3d12_encode_slot_make_list(copier->device, slot, result);
        if (step != 0) return step;
    }
    *result = ID3D12GraphicsCommandList_Reset(slot->commands, slot->allocator, NULL);
    if (FAILED(*result)) return ALPHA2_GPU_STEP_RESET_COMMAND_BUFFER;
    slot->recording = 1;
    return 0;
}

/*
 * Creates a copier on `raw_device`, which it holds a reference to: its ring of allocators, closed
 * command lists and fences. Returns 0, or the step that failed with its HRESULT in `status`.
 */
int32_t alpha2_native_gpu_d3d12_encode_copier_create(
    void *raw_device, void **output, int32_t *status
) {
    if (raw_device == NULL || output == NULL)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    Alpha2D3D12EncodeCopier *copier = calloc(1, sizeof(*copier));
    if (copier == NULL) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_HOST_ALLOCATION, 0);
    copier->device = raw_device;
    ID3D12Device_AddRef(copier->device);
    HRESULT result = S_OK;
    int32_t step = 0;
    for (uint32_t index = 0; index < ALPHA2_D3D12_ENCODE_COPY_SLOTS; ++index) {
        Alpha2D3D12EncodeSlot *slot = &copier->slots[index];
        step = alpha2_d3d12_encode_slot_make_list(copier->device, slot, &result);
        if (step != 0) break;
        result = ID3D12Device_CreateFence(
            copier->device, 0, D3D12_FENCE_FLAG_NONE, &IID_ID3D12Fence, (void **)&slot->fence);
        if (FAILED(result)) {
            slot->fence = NULL;
            step = ALPHA2_GPU_STEP_CREATE_FENCE;
            break;
        }
    }
    if (step != 0) {
        for (uint32_t index = 0; index < ALPHA2_D3D12_ENCODE_COPY_SLOTS; ++index)
            alpha2_d3d12_encode_slot_release(&copier->slots[index]);
        ID3D12Device_Release(copier->device);
        free(copier);
        return ALPHA2_GPU_FAIL(status, step, result);
    }
    *output = copier;
    return 0;
}

/*
 * Waits for every copy still in flight, a bounded time each, before freeing what they use, and
 * returns 0. When a wait times out or fails, or a copy could not be fenced at all, it returns that
 * step and frees nothing: the caller must then keep the planes, the frames and the device as well
 * as this copier.
 */
int32_t alpha2_native_gpu_d3d12_encode_copier_destroy(void *handle, int32_t *status) {
    Alpha2D3D12EncodeCopier *copier = handle;
    if (copier == NULL) return 0;
    if (copier->unfenced) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SIGNAL, 0);
    for (uint32_t index = 0; index < ALPHA2_D3D12_ENCODE_COPY_SLOTS; ++index) {
        HRESULT result = S_OK;
        int32_t step = alpha2_d3d12_encode_slot_wait(&copier->slots[index], &result);
        if (step != 0) return ALPHA2_GPU_FAIL(status, step, result);
    }
    for (uint32_t index = 0; index < ALPHA2_D3D12_ENCODE_COPY_SLOTS; ++index)
        alpha2_d3d12_encode_slot_release(&copier->slots[index]);
    ID3D12Device_Release(copier->device);
    free(copier);
    return 0;
}

/*
 * Slots in the ring, which is also the number of plane-texture pairs a caller needs to keep the
 * renderer from overwriting a pair a copy is still reading.
 */
uint32_t alpha2_native_gpu_d3d12_encode_copier_slots(void) {
    return ALPHA2_D3D12_ENCODE_COPY_SLOTS;
}

/*
 * Waits for the slot the next submit will use, resets its allocator and list, and reports which
 * one it is. Returns 0, or the step that failed with its HRESULT (WAIT_TIMEOUT for a wait that ran
 * out of time) in `status`.
 *
 * The submit readies the same slot, but by then the caller has already submitted the render that
 * writes the plane textures, too late to protect them. Calling this first is what makes the pairs a
 * ring: the render into pair N is not even recorded until the copy that last read pair N is over.
 */
int32_t alpha2_native_gpu_d3d12_encode_copier_await_slot(
    void *handle, uint32_t *slot, int32_t *status
) {
    Alpha2D3D12EncodeCopier *copier = handle;
    if (copier == NULL || slot == NULL)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    if (copier->unfenced) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SIGNAL, 0);
    HRESULT result = S_OK;
    int32_t step = alpha2_d3d12_encode_slot_ready(copier, &copier->slots[copier->slot], &result);
    if (step != 0) return ALPHA2_GPU_FAIL(status, step, result);
    *slot = copier->slot;
    return 0;
}

/* Whether `resource` is a single 2D texture of `format`, at least `width` x `height`. */
static int alpha2_d3d12_encode_holds(
    ID3D12Resource *resource, DXGI_FORMAT format, uint32_t width, uint32_t height
) {
    /* `GetDesc` is spelled out through the vtable for the reason `c/d3d12.c` gives. */
    D3D12_RESOURCE_DESC descriptor;
    resource->lpVtbl->GetDesc(resource, &descriptor);
    return descriptor.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D &&
           descriptor.DepthOrArraySize == 1 && descriptor.Format == format &&
           descriptor.Width >= width && descriptor.Height >= height;
}

static D3D12_RESOURCE_BARRIER alpha2_d3d12_encode_transition(
    ID3D12Resource *resource, D3D12_RESOURCE_STATES before, D3D12_RESOURCE_STATES after
) {
    D3D12_RESOURCE_BARRIER barrier = {
        .Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        .Flags = D3D12_RESOURCE_BARRIER_FLAG_NONE,
    };
    barrier.Transition.pResource = resource;
    barrier.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    barrier.Transition.StateBefore = before;
    barrier.Transition.StateAfter = after;
    return barrier;
}

/* Records a copy of the top-left `width` x `height` of `source` into plane `plane` of `target`. */
static void alpha2_d3d12_encode_copy_plane(
    ID3D12GraphicsCommandList *commands, ID3D12Resource *target, uint32_t plane,
    ID3D12Resource *source, uint32_t width, uint32_t height
) {
    D3D12_TEXTURE_COPY_LOCATION to = {
        .pResource = target,
        .Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        .SubresourceIndex = plane,
    };
    D3D12_TEXTURE_COPY_LOCATION from = {
        .pResource = source,
        .Type = D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        .SubresourceIndex = 0,
    };
    D3D12_BOX box = {.left = 0, .top = 0, .front = 0, .right = width, .bottom = height, .back = 1};
    ID3D12GraphicsCommandList_CopyTextureRegion(commands, &to, 0, 0, 0, &from, &box);
}

/*
 * Copies a renderer-packed NV12 pair into a D3D12VA encoder frame's texture on `raw_queue`, after
 * the frame's fence reaches `copy->frame_value`, and signals `frame_value + 1` on that fence once
 * the texture holds the copy, the value written to `signal_value`. Returns 0, or the step that
 * failed with its HRESULT in `status`. ALPHA2_GPU_STEP_QUEUE_SIGNAL means the copy is on the GPU
 * and its end, or the frame's value, is never signalled: the caller must keep the frame and the
 * planes from any further use.
 *
 * FFmpeg hands out one NV12 texture per frame, and wgpu 30 allows only `TEXTURE_BINDING` on its
 * `NV12` format, so wgpu can neither render into that texture nor copy into it, and this has to be
 * raw. The two sources are wgpu textures the renderer just wrote as render targets; they are put
 * back in RENDER_TARGET afterwards so wgpu's own state tracking stays true. The frame's texture
 * rests in COMMON, as FFmpeg makes it and its encoder leaves it, and goes back there: the encode
 * reads it on a queue of another type.
 */
int32_t alpha2_native_gpu_d3d12_encode_copier_submit(
    void *handle, void *raw_queue, const Alpha2D3D12EncodeCopy *copy, uint64_t *signal_value,
    int32_t *status
) {
    Alpha2D3D12EncodeCopier *copier = handle;
    if (copier == NULL || raw_queue == NULL || copy == NULL || signal_value == NULL ||
        copy->luma == 0 || copy->chroma == 0 || copy->destination == 0 ||
        copy->frame_fence == 0 || copy->width == 0 || copy->height == 0 ||
        copy->width % 2 != 0 || copy->height % 2 != 0)
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_ARGUMENTS, 0);
    ID3D12CommandQueue *queue = raw_queue;
    ID3D12Resource *luma = (ID3D12Resource *)(uintptr_t)copy->luma;
    ID3D12Resource *chroma = (ID3D12Resource *)(uintptr_t)copy->chroma;
    ID3D12Resource *destination = (ID3D12Resource *)(uintptr_t)copy->destination;
    ID3D12Fence *frame_fence = (ID3D12Fence *)(uintptr_t)copy->frame_fence;
    uint32_t width = copy->width, height = copy->height;
    /* One NV12 texture per frame is the layout this path is for; a texture-array pool is not. */
    if (!alpha2_d3d12_encode_holds(luma, DXGI_FORMAT_R8_UNORM, width, height) ||
        !alpha2_d3d12_encode_holds(chroma, DXGI_FORMAT_R8G8_UNORM, width / 2, height / 2) ||
        !alpha2_d3d12_encode_holds(destination, DXGI_FORMAT_NV12, width, height))
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_UNSUPPORTED, 0);
    if (copier->unfenced) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SIGNAL, 0);

    Alpha2D3D12EncodeSlot *slot = &copier->slots[copier->slot];
    HRESULT result = S_OK;
    int32_t step = alpha2_d3d12_encode_slot_ready(copier, slot, &result);
    if (step != 0) return ALPHA2_GPU_FAIL(status, step, result);
    ID3D12GraphicsCommandList *commands = slot->commands;
    D3D12_RESOURCE_BARRIER enter[3] = {
        alpha2_d3d12_encode_transition(
            luma, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE),
        alpha2_d3d12_encode_transition(
            chroma, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE),
        alpha2_d3d12_encode_transition(
            destination, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST),
    };
    D3D12_RESOURCE_BARRIER leave[3] = {
        alpha2_d3d12_encode_transition(
            luma, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_RENDER_TARGET),
        alpha2_d3d12_encode_transition(
            chroma, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_RENDER_TARGET),
        alpha2_d3d12_encode_transition(
            destination, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON),
    };
    ID3D12GraphicsCommandList_ResourceBarrier(commands, 3, enter);
    alpha2_d3d12_encode_copy_plane(commands, destination, 0, luma, width, height);
    alpha2_d3d12_encode_copy_plane(commands, destination, 1, chroma, width / 2, height / 2);
    ID3D12GraphicsCommandList_ResourceBarrier(commands, 3, leave);
    slot->recording = 0;
    result = ID3D12GraphicsCommandList_Close(commands);
    if (FAILED(result)) {
        /* No reset may follow a list that failed to close; the slot's next use makes another. */
        ID3D12GraphicsCommandList_Release(slot->commands);
        slot->commands = NULL;
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST, result);
    }
    /* The encoder's last read of this texture, which FFmpeg signalled on the frame's fence. */
    result = ID3D12CommandQueue_Wait(queue, frame_fence, copy->frame_value);
    if (FAILED(result)) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_WAIT, result);
    ID3D12CommandList *lists[] = {(ID3D12CommandList *)commands};
    ID3D12CommandQueue_ExecuteCommandLists(queue, 1, lists);
    /*
     * The copy is on the GPU now. Order matters from here: the slot's fence is the copier's own and
     * says when the slot may be reused; the frame's fence is raised last, after the copy that
     * writes its texture, so the encode queue never reads a half-written surface.
     */
    uint64_t value = slot->fence_value + 1;
    slot->fence_value = value;
    result = ID3D12CommandQueue_Signal(queue, slot->fence, value);
    if (FAILED(result)) {
        copier->unfenced = 1;
        return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SIGNAL, result);
    }
    copier->slot = (copier->slot + 1) % ALPHA2_D3D12_ENCODE_COPY_SLOTS;
    /* The slot is fenced, so the copier goes on; only this frame never gets its value. */
    result = ID3D12CommandQueue_Signal(queue, frame_fence, copy->frame_value + 1);
    if (FAILED(result)) return ALPHA2_GPU_FAIL(status, ALPHA2_GPU_STEP_QUEUE_SIGNAL, result);
    *signal_value = copy->frame_value + 1;
    return 0;
}
