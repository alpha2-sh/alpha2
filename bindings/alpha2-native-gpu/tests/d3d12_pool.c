/*
 * The D3D12 copy pool of `c/d3d12.c` and the encode copier of `c/d3d12_encode.c`, run on Linux
 * against stand-in COM objects (`d3d12_mock/d3d12.h`). The stand-ins count what the helpers make
 * and release; a fence completes as soon as it is signalled, unless a check holds the queue's
 * signals to stand for GPU work not yet run. `d3d12_pool.rs` builds and runs this; no GPU is
 * needed.
 * What only a D3D12 device shows (the copies' pixels, the debug layer) is in
 * `src/d3d12_tests.rs`, which runs on Windows.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../c/d3d12.c"
#include "../c/d3d12_encode.c"

enum MockKind {
    MOCK_DEVICE,
    MOCK_RESOURCE,
    MOCK_ALLOCATOR,
    MOCK_LIST,
    MOCK_FENCE,
    MOCK_QUEUE,
    MOCK_KINDS,
};

/* Every stand-in starts with a resource vtable, so that any of them can be cast to
   `ID3D12Resource`; only resources use it. */
typedef struct MockObject {
    const ID3D12ResourceVtbl *lpVtbl;
    enum MockKind kind;
    long references;
    D3D12_RESOURCE_DESC descriptor;
    uint64_t completed;
    /* A command list's: 1 while it is open for recording. */
    int open;
} MockObject;

struct ID3D12Device { MockObject object; };
struct ID3D12CommandAllocator { MockObject object; };
struct ID3D12GraphicsCommandList { MockObject object; };
struct ID3D12CommandList { MockObject object; };
struct ID3D12Fence { MockObject object; };
struct ID3D12CommandQueue { MockObject object; };

const IID IID_ID3D12Resource = {1};
const IID IID_ID3D12CommandAllocator = {2};
const IID IID_ID3D12GraphicsCommandList = {3};
const IID IID_ID3D12Fence = {4};
const IID IID_ID3D12VideoDevice = {5};

static long live[MOCK_KINDS];
/* `CreateCommittedResource` calls that made a resource: the pool's targets. */
static long committed;
static int failures;
/* Calls the encode copier's checks count, and the faults a check sets up for it: a queue that
   holds its signals (a fence that never completes), and the next close, wait or nth signal
   failing. */
static long allocator_resets, list_resets, executions, region_copies;
static int hold_signals;
static HRESULT next_close_result, next_wait_result;
static int signal_fails_at;

#define CHECK(condition, ...) do { \
    if (!(condition)) { \
        failures++; \
        fprintf(stderr, "FAILED %s:%d: %s: ", __FILE__, __LINE__, #condition); \
        fprintf(stderr, __VA_ARGS__); \
        fputc('\n', stderr); \
    } \
} while (0)

void InitializeSRWLock(SRWLOCK *lock) { lock->held = 0; }

void AcquireSRWLockExclusive(SRWLOCK *lock) {
    CHECK(!lock->held, "the pool lock is taken twice");
    lock->held = 1;
}

void ReleaseSRWLockExclusive(SRWLOCK *lock) {
    CHECK(lock->held, "the pool lock is released unheld");
    lock->held = 0;
}

static int event_object;

HANDLE CreateEventW(void *attributes, int manual_reset, int initial_state, const void *name) {
    (void)attributes; (void)manual_reset; (void)initial_state; (void)name;
    return &event_object;
}

int CloseHandle(HANDLE handle) { (void)handle; return 1; }

DWORD WaitForSingleObject(HANDLE handle, DWORD milliseconds) {
    (void)handle; (void)milliseconds;
    return 0;
}

static void mock_get_desc(ID3D12Resource *self, D3D12_RESOURCE_DESC *output) {
    *output = ((MockObject *)self)->descriptor;
}

static const ID3D12ResourceVtbl resource_vtable = {mock_get_desc};

static MockObject *mock_new(enum MockKind kind) {
    MockObject *object = calloc(1, sizeof(*object));
    if (object == NULL) abort();
    object->lpVtbl = &resource_vtable;
    object->kind = kind;
    object->references = 1;
    live[kind]++;
    return object;
}

void mock_add_ref(void *object) { ((MockObject *)object)->references++; }

void mock_release(void *raw) {
    MockObject *object = raw;
    CHECK(object->references > 0, "a released object is released again");
    if (--object->references == 0) {
        live[object->kind]--;
        free(object);
    }
}

/* The harness's device has no D3D12 Video interface and is never removed. */
HRESULT mock_query_interface(ID3D12Device *device, const IID *iid, void **output) {
    (void)device;
    (void)iid;
    *output = NULL;
    return E_NOINTERFACE;
}

HRESULT mock_device_removed_reason(ID3D12Device *device) {
    (void)device;
    return S_OK;
}

HRESULT mock_create_committed_resource(
    ID3D12Device *device, const D3D12_HEAP_PROPERTIES *heap, D3D12_HEAP_FLAGS flags,
    const D3D12_RESOURCE_DESC *descriptor, D3D12_RESOURCE_STATES state, const void *clear,
    const IID *iid, void **output
) {
    (void)device; (void)clear;
    CHECK(heap->Type == D3D12_HEAP_TYPE_DEFAULT, "a target outside the default heap");
    CHECK(state == D3D12_RESOURCE_STATE_COMMON, "a target made in state %d", (int)state);
    CHECK(iid == &IID_ID3D12Resource, "a resource asked for another interface");
    CHECK(!(flags & D3D12_HEAP_FLAG_CREATE_NOT_ZEROED), "a target made in not-zeroed memory");
    MockObject *object = mock_new(MOCK_RESOURCE);
    object->descriptor = *descriptor;
    committed++;
    *output = object;
    return S_OK;
}

HRESULT mock_create_command_allocator(
    ID3D12Device *device, D3D12_COMMAND_LIST_TYPE type, const IID *iid, void **output
) {
    (void)device; (void)type; (void)iid;
    *output = mock_new(MOCK_ALLOCATOR);
    return S_OK;
}

HRESULT mock_create_command_list(
    ID3D12Device *device, UINT mask, D3D12_COMMAND_LIST_TYPE type,
    ID3D12CommandAllocator *allocator, void *state, const IID *iid, void **output
) {
    (void)device; (void)mask; (void)type; (void)allocator; (void)state; (void)iid;
    MockObject *list = mock_new(MOCK_LIST);
    list->open = 1;
    *output = list;
    return S_OK;
}

HRESULT mock_create_fence(
    ID3D12Device *device, UINT64 value, D3D12_FENCE_FLAGS flags, const IID *iid, void **output
) {
    (void)device; (void)flags; (void)iid;
    MockObject *fence = mock_new(MOCK_FENCE);
    fence->completed = value;
    *output = fence;
    return S_OK;
}

HRESULT mock_allocator_reset(ID3D12CommandAllocator *allocator) {
    (void)allocator;
    allocator_resets++;
    return S_OK;
}

HRESULT mock_list_reset(
    ID3D12GraphicsCommandList *commands, ID3D12CommandAllocator *allocator, void *state
) {
    (void)allocator; (void)state;
    MockObject *list = (MockObject *)commands;
    CHECK(!list->open, "an open command list is reset");
    list->open = 1;
    list_resets++;
    return S_OK;
}

void mock_list_barrier(
    ID3D12GraphicsCommandList *commands, UINT count, const D3D12_RESOURCE_BARRIER *barriers
) {
    CHECK(((MockObject *)commands)->open, "a barrier recorded into a closed command list");
    if (count != 3) return;
    /* An encode copy's: both planes between RENDER_TARGET and COPY_SOURCE, the frame between
       COMMON and COPY_DEST, each pair back where it started on the way out. */
    int entering = barriers[0].Transition.StateBefore == D3D12_RESOURCE_STATE_RENDER_TARGET;
    D3D12_RESOURCE_STATES plane[2] = {
        D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE};
    D3D12_RESOURCE_STATES frame[2] = {D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST};
    for (UINT index = 0; index < 3; index++) {
        const D3D12_RESOURCE_STATES *states = index < 2 ? plane : frame;
        CHECK(barriers[index].Transition.StateBefore == states[!entering] &&
              barriers[index].Transition.StateAfter == states[entering] &&
              barriers[index].Transition.Subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
              "encode barrier %u goes from %d to %d", index,
              (int)barriers[index].Transition.StateBefore,
              (int)barriers[index].Transition.StateAfter);
    }
}

void mock_list_copy(
    ID3D12GraphicsCommandList *commands, ID3D12Resource *target, ID3D12Resource *source
) {
    (void)commands;
    const D3D12_RESOURCE_DESC *to = &((MockObject *)target)->descriptor;
    const D3D12_RESOURCE_DESC *from = &((MockObject *)source)->descriptor;
    CHECK(to->Format == from->Format && to->Width == from->Width && to->Height == from->Height,
          "a %llux%u surface copied into a %llux%u target",
          (unsigned long long)from->Width, from->Height,
          (unsigned long long)to->Width, to->Height);
}

void mock_list_copy_region(
    ID3D12GraphicsCommandList *commands, const D3D12_TEXTURE_COPY_LOCATION *target, UINT x,
    UINT y, UINT z, const D3D12_TEXTURE_COPY_LOCATION *source, const D3D12_BOX *box
) {
    CHECK(((MockObject *)commands)->open, "a copy recorded into a closed command list");
    CHECK(x == 0 && y == 0 && z == 0, "a plane copied to (%u, %u, %u)", x, y, z);
    CHECK(target->Type == D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX &&
          source->Type == D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX &&
          source->SubresourceIndex == 0, "a copy that is not plane to plane");
    const D3D12_RESOURCE_DESC *to = &((MockObject *)target->pResource)->descriptor;
    const D3D12_RESOURCE_DESC *from = &((MockObject *)source->pResource)->descriptor;
    /* Plane 0 of the NV12 frame takes the R8 luma, plane 1 the RG8 chroma at half size. */
    DXGI_FORMAT plane = target->SubresourceIndex == 0 ? DXGI_FORMAT_R8_UNORM : DXGI_FORMAT_R8G8_UNORM;
    UINT scale = target->SubresourceIndex == 0 ? 1 : 2;
    CHECK(to->Format == DXGI_FORMAT_NV12 && target->SubresourceIndex < 2 && from->Format == plane,
          "plane %u copied from format %d into format %d", target->SubresourceIndex,
          (int)from->Format, (int)to->Format);
    CHECK(box->left == 0 && box->top == 0 && box->front == 0 && box->back == 1 &&
          box->right <= from->Width && box->bottom <= from->Height &&
          box->right * scale <= to->Width && box->bottom * scale <= to->Height,
          "a %ux%u box out of a %llux%u plane into a %llux%u frame", box->right, box->bottom,
          (unsigned long long)from->Width, from->Height, (unsigned long long)to->Width,
          to->Height);
    region_copies++;
}

HRESULT mock_list_close(ID3D12GraphicsCommandList *commands) {
    MockObject *list = (MockObject *)commands;
    CHECK(list->open, "a closed command list is closed again");
    list->open = 0;
    HRESULT result = next_close_result;
    next_close_result = S_OK;
    return result;
}

UINT64 mock_fence_completed(ID3D12Fence *fence) { return ((MockObject *)fence)->completed; }

HRESULT mock_fence_set_event(ID3D12Fence *fence, UINT64 value, HANDLE event) {
    (void)fence; (void)value; (void)event;
    return S_OK;
}

HRESULT mock_queue_wait(ID3D12CommandQueue *queue, ID3D12Fence *fence, UINT64 value) {
    (void)queue; (void)fence; (void)value;
    HRESULT result = next_wait_result;
    next_wait_result = S_OK;
    return result;
}

void mock_queue_execute(ID3D12CommandQueue *queue, UINT count, ID3D12CommandList *const *lists) {
    (void)queue;
    for (UINT index = 0; index < count; index++)
        CHECK(!((MockObject *)lists[index])->open, "an open command list is executed");
    executions++;
}

HRESULT mock_queue_signal(ID3D12CommandQueue *queue, ID3D12Fence *fence, UINT64 value) {
    (void)queue;
    if (signal_fails_at > 0 && --signal_fails_at == 0) return (HRESULT)0x887A0005u;
    if (!hold_signals) ((MockObject *)fence)->completed = value;
    return S_OK;
}

static ID3D12Device *device;
static ID3D12CommandQueue *queue;
static ID3D12Fence *decode_fence;

static ID3D12Resource *surface(DXGI_FORMAT format, uint64_t width, uint32_t height) {
    MockObject *object = mock_new(MOCK_RESOURCE);
    object->descriptor = (D3D12_RESOURCE_DESC){
        .Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        .Width = width,
        .Height = height,
        .DepthOrArraySize = 1,
        .MipLevels = 1,
        .Format = format,
        .SampleDesc = {1, 0},
    };
    return (ID3D12Resource *)object;
}

static void *copy(void *pool, DXGI_FORMAT format, uint64_t width, uint32_t height) {
    ID3D12Resource *source = surface(format, width, height);
    void *ticket = NULL;
    Alpha2D3D12CopyInfo info;
    int32_t status = 0;
    int32_t step = alpha2_native_gpu_d3d12_copy(
        pool, queue, source, decode_fence, 1, &ticket, &info, &status);
    CHECK(step == 0, "a %llux%u copy failed at step %d", (unsigned long long)width, height, step);
    CHECK(info.width == width && info.height == height && info.dxgi_format == (uint32_t)format,
          "the copy describes a %ux%u target of format %u", info.width, info.height,
          info.dxgi_format);
    mock_release(source);
    return ticket;
}

static void give_back(void *ticket) {
    CHECK(alpha2_native_gpu_d3d12_copy_destroy(ticket) == 1, "a finished copy was kept out");
}

/* `[targets made, slots made, idle targets, idle slots]` of `pool`. */
typedef struct Counts {
    uint32_t made, slots, idle, idle_slots;
} Counts;

static Counts counts(void *pool) {
    Counts c;
    alpha2_native_gpu_d3d12_pool_counts(pool, &c.made, &c.slots, &c.idle, &c.idle_slots);
    return c;
}

static uint64_t nv12_bytes(uint64_t width, uint64_t height) { return width * height * 3 / 2; }

/*
 * A timeline that holds four 4K copies at once (two clips crossfading, each with a frame in
 * flight) fills the pool with 4K targets; the 1080p clip after it must not make a target for each
 * of its frames.
 */
static void a_full_pool_makes_room_for_the_size_shown_now(void) {
    void *pool = alpha2_native_gpu_d3d12_pool_create(device);
    void *held[4];
    for (int index = 0; index < 4; index++)
        held[index] = copy(pool, DXGI_FORMAT_NV12, 3840, 2160);
    for (int index = 0; index < 4; index++) give_back(held[index]);
    Counts before = counts(pool);
    CHECK(before.made == 4 && before.idle == 4, "made %u, idle %u", before.made, before.idle);
    long before_frames = committed;
    for (int frame = 0; frame < 100; frame++) give_back(copy(pool, DXGI_FORMAT_NV12, 1920, 1080));
    long during = committed - before_frames;
    printf("CreateCommittedResource during the 100 1080p frames, one copy out: %ld\n", during);
    CHECK(during == 1, "%ld targets made for 100 1080p frames", during);
    /* Two copies out at once, as a frame in flight keeps one: two targets, then none. */
    void *previous = copy(pool, DXGI_FORMAT_NV12, 1280, 720);
    before_frames = committed;
    for (int frame = 0; frame < 100; frame++) {
        void *next = copy(pool, DXGI_FORMAT_NV12, 1280, 720);
        give_back(previous);
        previous = next;
    }
    give_back(previous);
    during = committed - before_frames;
    printf("CreateCommittedResource during the 100 720p frames, two copies out: %ld\n", during);
    CHECK(during == 1, "%ld more targets made for 100 720p frames", during);
    /* Each eviction took the least recently given back target: left idle are the two 720p
       targets, the 1080p one and the newest 4K one. */
    Counts after = counts(pool);
    CHECK(after.made == 7 && after.idle == 4, "made %u, idle %u", after.made, after.idle);
    uint64_t trimmed = alpha2_native_gpu_d3d12_pool_trim(pool);
    uint64_t expected =
        nv12_bytes(3840, 2160) + nv12_bytes(1920, 1080) + 2 * nv12_bytes(1280, 720);
    CHECK(trimmed == expected, "trimmed %llu bytes, expected %llu",
          (unsigned long long)trimmed, (unsigned long long)expected);
    alpha2_native_gpu_d3d12_pool_destroy(pool);
}

/* Two sizes shown in turn, as a picture-in-picture, both stay once the pool has room for them. */
static void sizes_shown_in_turn_keep_their_targets(void) {
    void *pool = alpha2_native_gpu_d3d12_pool_create(device);
    for (int index = 0; index < 4; index++) give_back(copy(pool, DXGI_FORMAT_NV12, 64 + 2 * index, 32));
    for (int frame = 0; frame < 50; frame++) {
        give_back(copy(pool, DXGI_FORMAT_NV12, 3840, 2160));
        give_back(copy(pool, DXGI_FORMAT_P010, 3840, 2160));
    }
    Counts c = counts(pool);
    CHECK(c.made == 6, "%u targets made for 4 sizes, then two shown in turn", c.made);
    alpha2_native_gpu_d3d12_pool_destroy(pool);
}

/*
 * A trim frees every idle target and reports their bytes; a copy still out keeps its target and
 * gives it back to the pool afterwards. A pool its owner destroyed goes with its last copy.
 */
static void a_trim_frees_the_idle_targets_alone(void) {
    void *pool = alpha2_native_gpu_d3d12_pool_create(device);
    void *out = copy(pool, DXGI_FORMAT_NV12, 1920, 1080);
    give_back(copy(pool, DXGI_FORMAT_NV12, 3840, 2160));
    give_back(copy(pool, DXGI_FORMAT_P010, 3840, 2160));
    long resources = live[MOCK_RESOURCE];
    uint64_t trimmed = alpha2_native_gpu_d3d12_pool_trim(pool);
    uint64_t expected = nv12_bytes(3840, 2160) * 3;
    printf("trimmed %llu bytes of idle 4K NV12 and P010 targets\n", (unsigned long long)trimmed);
    CHECK(trimmed == expected, "trimmed %llu bytes, expected %llu",
          (unsigned long long)trimmed, (unsigned long long)expected);
    CHECK(live[MOCK_RESOURCE] == resources - 2, "%ld of 2 idle targets freed",
          resources - live[MOCK_RESOURCE]);
    Counts c = counts(pool);
    CHECK(c.made == 3 && c.idle == 0 && c.idle_slots == 1,
          "made %u, idle %u, idle slots %u", c.made, c.idle, c.idle_slots);
    CHECK(alpha2_native_gpu_d3d12_pool_trim(pool) == 0, "a second trim found more");
    give_back(out);
    c = counts(pool);
    CHECK(c.idle == 1, "the copy out gave its target back: idle %u", c.idle);
    out = copy(pool, DXGI_FORMAT_NV12, 1920, 1080);
    alpha2_native_gpu_d3d12_pool_destroy(pool);
    CHECK(live[MOCK_RESOURCE] == 1, "the copy out keeps its target: %ld live", live[MOCK_RESOURCE]);
    give_back(out);
}

/* Targets are made in zeroed memory, whatever their size: a copy that skips all-zero blocks then
   leaves zeros, never an earlier frame. */
static void targets_are_made_in_zeroed_memory(void) {
    void *pool = alpha2_native_gpu_d3d12_pool_create(device);
    long before = committed;
    give_back(copy(pool, DXGI_FORMAT_NV12, 64, 32));
    give_back(copy(pool, DXGI_FORMAT_NV12, 128, 64));
    CHECK(committed - before == 2, "made %ld targets", committed - before);
    CHECK(counts(pool).made == 2, "made %u", counts(pool).made);
    alpha2_native_gpu_d3d12_pool_destroy(pool);
}

/* An encode copy's inputs: R8 luma and RG8 chroma planes of a `width` x `height` picture, an NV12
   frame of that size, and the frame's fence. */
typedef struct EncodeInputs {
    ID3D12Resource *luma, *chroma, *frame;
    ID3D12Fence *fence;
} EncodeInputs;

static EncodeInputs encode_inputs(uint32_t width, uint32_t height) {
    MockObject *fence = mock_new(MOCK_FENCE);
    fence->completed = 1;
    return (EncodeInputs){
        .luma = surface(DXGI_FORMAT_R8_UNORM, width, height),
        .chroma = surface(DXGI_FORMAT_R8G8_UNORM, width / 2, height / 2),
        .frame = surface(DXGI_FORMAT_NV12, width, height),
        .fence = (ID3D12Fence *)fence,
    };
}

static void encode_inputs_release(EncodeInputs inputs) {
    mock_release(inputs.luma);
    mock_release(inputs.chroma);
    mock_release(inputs.frame);
    mock_release(inputs.fence);
}

/* Submits one encode copy of `inputs`, waiting for the frame's fence at 1, and returns its step;
   a success signals 2. */
static int32_t encode_copy(void *copier, EncodeInputs inputs, int32_t *status) {
    Alpha2D3D12EncodeCopy copy = {
        .luma = (uint64_t)(uintptr_t)inputs.luma,
        .chroma = (uint64_t)(uintptr_t)inputs.chroma,
        .destination = (uint64_t)(uintptr_t)inputs.frame,
        .frame_fence = (uint64_t)(uintptr_t)inputs.fence,
        .frame_value = 1,
        .width = (uint32_t)((MockObject *)inputs.luma)->descriptor.Width,
        .height = ((MockObject *)inputs.luma)->descriptor.Height,
    };
    uint64_t signalled = 0;
    int32_t step = alpha2_native_gpu_d3d12_encode_copier_submit(
        copier, queue, &copy, &signalled, status);
    CHECK(step != 0 || signalled == 2, "a copy waiting for 1 signalled %llu",
          (unsigned long long)signalled);
    return step;
}

/* Completes every fence of `copier`'s slots, as the GPU would once it ran what is held. */
static void encode_slots_complete(Alpha2D3D12EncodeCopier *copier) {
    for (int index = 0; index < ALPHA2_D3D12_ENCODE_COPY_SLOTS; index++)
        ((MockObject *)copier->slots[index].fence)->completed = copier->slots[index].fence_value;
}

/* Slots go round in order, each copy recording two plane copies, and a slot is reset for its next
   copy only once its fence has passed: never while the GPU may still run its commands. */
static void encode_slots_rotate_and_reset_only_after_their_fence(void) {
    void *handle = NULL;
    int32_t status = 0;
    CHECK(alpha2_native_gpu_d3d12_encode_copier_create(device, &handle, &status) == 0, "create");
    Alpha2D3D12EncodeCopier *copier = handle;
    CHECK(alpha2_native_gpu_d3d12_encode_copier_slots() == 4, "slots");
    EncodeInputs inputs = encode_inputs(1920, 1080);
    long copies = region_copies;
    for (uint32_t index = 0; index < 6; index++) {
        uint32_t slot = 99;
        CHECK(alpha2_native_gpu_d3d12_encode_copier_await_slot(handle, &slot, &status) == 0 &&
              slot == index % 4, "copy %u awaited slot %u", index, slot);
        CHECK(encode_copy(handle, inputs, &status) == 0, "copy %u failed: %d", index, status);
    }
    CHECK(region_copies - copies == 12, "%ld plane copies for 6 frames", region_copies - copies);
    /* Four copies the GPU has not run yet: the slot the next one takes is still in use. */
    hold_signals = 1;
    for (int index = 0; index < 4; index++)
        CHECK(encode_copy(handle, inputs, &status) == 0, "held copy %d", index);
    long resets = allocator_resets, reopened = list_resets;
    uint32_t slot = 99;
    int32_t step = alpha2_native_gpu_d3d12_encode_copier_await_slot(handle, &slot, &status);
    CHECK(step == ALPHA2_GPU_STEP_WAIT_FENCE && status == ALPHA2_D3D12_WAIT_TIMED_OUT,
          "a slot still in use waited out as step %d, status %#x", step, (unsigned)status);
    CHECK(allocator_resets == resets && slot == 99, "a slot was reset under its running copy");
    step = encode_copy(handle, inputs, &status);
    CHECK(step == ALPHA2_GPU_STEP_WAIT_FENCE, "a copy into a busy slot submitted: step %d", step);
    CHECK(allocator_resets == resets, "a copy reset a slot under its running copy");
    hold_signals = 0;
    encode_slots_complete(copier);
    CHECK(alpha2_native_gpu_d3d12_encode_copier_await_slot(handle, &slot, &status) == 0 &&
          slot == 2 && allocator_resets == resets + 1 && list_resets == reopened + 1,
          "the slot once its fence passed: %u", slot);
    /* Awaiting again readies nothing twice: the slot is already open for its copy. */
    CHECK(alpha2_native_gpu_d3d12_encode_copier_await_slot(handle, &slot, &status) == 0 &&
          slot == 2 && allocator_resets == resets + 1, "a ready slot was reset again");
    CHECK(alpha2_native_gpu_d3d12_encode_copier_destroy(handle, &status) == 0, "destroy");
    encode_inputs_release(inputs);
}

/* Destroying a copier whose copy has not run reports the wait and frees nothing: the caller keeps
   the planes and frames that copy uses, and the copier itself. */
static void an_encode_copier_with_a_pending_slot_is_not_freed(void) {
    void *handle = NULL;
    int32_t status = 0;
    CHECK(alpha2_native_gpu_d3d12_encode_copier_create(device, &handle, &status) == 0, "create");
    EncodeInputs inputs = encode_inputs(256, 192);
    hold_signals = 1;
    CHECK(encode_copy(handle, inputs, &status) == 0, "copy");
    hold_signals = 0;
    long lists = live[MOCK_LIST], fences = live[MOCK_FENCE];
    int32_t step = alpha2_native_gpu_d3d12_encode_copier_destroy(handle, &status);
    CHECK(step == ALPHA2_GPU_STEP_WAIT_FENCE && status == ALPHA2_D3D12_WAIT_TIMED_OUT,
          "a pending copy's teardown: step %d, status %#x", step, (unsigned)status);
    CHECK(live[MOCK_LIST] == lists && live[MOCK_FENCE] == fences, "a pending copier was freed");
    encode_slots_complete(handle);
    CHECK(alpha2_native_gpu_d3d12_encode_copier_destroy(handle, &status) == 0, "destroy");
    CHECK(live[MOCK_LIST] == lists - 4 && live[MOCK_FENCE] == fences - 4,
          "the copier's lists and fences outlived it");
    encode_inputs_release(inputs);
}

/* A copy that fails before it reaches the GPU leaves the copier as it was, on the same slot; one
   whose list did not close makes that slot a new list. A copy whose slot cannot be signalled is on
   the GPU unfenced, and the copier then refuses everything, its teardown included; one whose
   frame cannot be signalled loses that frame alone. */
static void an_encode_copy_failure_leaves_the_copier_as_its_step_says(void) {
    void *handle = NULL;
    int32_t status = 0;
    CHECK(alpha2_native_gpu_d3d12_encode_copier_create(device, &handle, &status) == 0, "create");
    Alpha2D3D12EncodeCopier *copier = handle;
    EncodeInputs inputs = encode_inputs(158, 62);
    long ran = executions, lists = live[MOCK_LIST];
    next_wait_result = (HRESULT)0x887A0005u;
    CHECK(encode_copy(handle, inputs, &status) == ALPHA2_GPU_STEP_QUEUE_WAIT &&
          status == (int32_t)0x887A0005u, "a failed wait: %d", status);
    next_close_result = E_INVALIDARG;
    CHECK(encode_copy(handle, inputs, &status) == ALPHA2_GPU_STEP_CLOSE_COMMAND_LIST,
          "a failed close");
    CHECK(executions == ran && copier->slot == 0, "a failed copy reached the GPU or moved on");
    CHECK(live[MOCK_LIST] == lists - 1 && copier->slots[0].commands == NULL,
          "the list that did not close was kept");
    CHECK(encode_copy(handle, inputs, &status) == 0 && executions == ran + 1 &&
          live[MOCK_LIST] == lists && copier->slot == 1, "the slot made a new list");
    /* The frame's signal fails: that frame is lost, the copier goes on. */
    signal_fails_at = 2;
    CHECK(encode_copy(handle, inputs, &status) == ALPHA2_GPU_STEP_QUEUE_SIGNAL, "frame signal");
    CHECK(!copier->unfenced && encode_copy(handle, inputs, &status) == 0, "the copier went on");
    /* The slot's signal fails: nothing says when that copy ends. */
    signal_fails_at = 1;
    CHECK(encode_copy(handle, inputs, &status) == ALPHA2_GPU_STEP_QUEUE_SIGNAL, "slot signal");
    uint32_t slot = 0;
    CHECK(alpha2_native_gpu_d3d12_encode_copier_await_slot(handle, &slot, &status) ==
              ALPHA2_GPU_STEP_QUEUE_SIGNAL, "an unfenced copier hands out a slot");
    CHECK(encode_copy(handle, inputs, &status) == ALPHA2_GPU_STEP_QUEUE_SIGNAL,
          "an unfenced copier copies");
    CHECK(alpha2_native_gpu_d3d12_encode_copier_destroy(handle, &status) ==
              ALPHA2_GPU_STEP_QUEUE_SIGNAL && live[MOCK_LIST] == lists,
          "an unfenced copier was freed");
    /* What a caller leaks; freed here only so the harness can count every other object. */
    copier->unfenced = 0;
    encode_slots_complete(copier);
    CHECK(alpha2_native_gpu_d3d12_encode_copier_destroy(handle, &status) == 0, "destroy");
    encode_inputs_release(inputs);
}

/* Planes and frames that do not belong together are refused before anything is recorded. */
static void an_encode_copy_of_mismatched_resources_is_refused(void) {
    void *handle = NULL;
    int32_t status = 0;
    CHECK(alpha2_native_gpu_d3d12_encode_copier_create(device, &handle, &status) == 0, "create");
    EncodeInputs inputs = encode_inputs(64, 32);
    EncodeInputs small = encode_inputs(32, 16);
    EncodeInputs swapped = inputs;
    swapped.luma = inputs.chroma;
    EncodeInputs too_small = inputs;
    too_small.frame = small.frame;
    long copies = region_copies;
    CHECK(encode_copy(handle, swapped, &status) == ALPHA2_GPU_STEP_UNSUPPORTED, "swapped planes");
    CHECK(encode_copy(handle, too_small, &status) == ALPHA2_GPU_STEP_UNSUPPORTED, "small frame");
    CHECK(region_copies == copies, "a refused copy recorded a plane");
    CHECK(alpha2_native_gpu_d3d12_encode_copier_submit(handle, queue, NULL, NULL, &status) ==
              ALPHA2_GPU_STEP_ARGUMENTS, "no copy");
    CHECK(encode_copy(handle, inputs, &status) == 0, "the copier went on");
    CHECK(alpha2_native_gpu_d3d12_encode_copier_destroy(handle, &status) == 0, "destroy");
    encode_inputs_release(inputs);
    encode_inputs_release(small);
}

int main(void) {
    device = (ID3D12Device *)mock_new(MOCK_DEVICE);
    queue = (ID3D12CommandQueue *)mock_new(MOCK_QUEUE);
    decode_fence = (ID3D12Fence *)mock_new(MOCK_FENCE);
    decode_fence->object.completed = 1;
    a_full_pool_makes_room_for_the_size_shown_now();
    sizes_shown_in_turn_keep_their_targets();
    a_trim_frees_the_idle_targets_alone();
    targets_are_made_in_zeroed_memory();
    encode_slots_rotate_and_reset_only_after_their_fence();
    an_encode_copier_with_a_pending_slot_is_not_freed();
    an_encode_copy_failure_leaves_the_copier_as_its_step_says();
    an_encode_copy_of_mismatched_resources_is_refused();
    mock_release(decode_fence);
    mock_release(queue);
    CHECK(device->object.references == 1, "the pools kept %ld device references",
          device->object.references - 1);
    mock_release(device);
    for (int kind = 0; kind < MOCK_KINDS; kind++)
        CHECK(live[kind] == 0, "%ld objects of kind %d were never released", live[kind], kind);
    if (failures != 0) {
        fprintf(stderr, "%d checks failed\n", failures);
        return 1;
    }
    printf("D3D12 copy pool and encode copier: every check passed\n");
    return 0;
}
