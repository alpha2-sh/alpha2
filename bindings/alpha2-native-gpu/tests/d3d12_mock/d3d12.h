/*
 * The few Win32 and D3D12 declarations `c/d3d12.c` and `c/d3d12_encode.c` use, so that the copy
 * pool and the encode copier compile and run on Linux against stand-in COM objects
 * (`../d3d12_pool.c`). Every COM call the helper makes is a macro here, as COBJMACROS makes it in
 * the Windows SDK, but it calls a mock function the harness defines; `ID3D12Resource` alone has a
 * vtable, for the `GetDesc` call the helper spells out.
 * Names, members and constants follow the Windows SDK; values are the SDK's where the helper
 * compares them, and arbitrary where it only passes them on.
 */
#ifndef ALPHA2_D3D12_MOCK_H
#define ALPHA2_D3D12_MOCK_H

#include <stdint.h>

typedef int32_t LONG;
typedef uint32_t UINT;
typedef uint64_t UINT64;
typedef uint32_t DWORD;
typedef int32_t HRESULT;
typedef void *HANDLE;

#define FALSE 0
#define TRUE 1
#define S_OK ((HRESULT)0)
#define E_INVALIDARG ((HRESULT)0x80070057u)
#define E_OUTOFMEMORY ((HRESULT)0x8007000Eu)
#define SUCCEEDED(result) ((HRESULT)(result) >= 0)
#define FAILED(result) ((HRESULT)(result) < 0)
#define INFINITE 0xFFFFFFFFu

/* The harness runs on one thread; the lock records only whether it is held. */
typedef struct SRWLOCK {
    int held;
} SRWLOCK;
void InitializeSRWLock(SRWLOCK *lock);
void AcquireSRWLockExclusive(SRWLOCK *lock);
void ReleaseSRWLockExclusive(SRWLOCK *lock);

#define InterlockedIncrement(target) __atomic_add_fetch((target), 1, __ATOMIC_SEQ_CST)
#define InterlockedDecrement(target) __atomic_sub_fetch((target), 1, __ATOMIC_SEQ_CST)
#define InterlockedExchange(target, value) \
    __atomic_exchange_n((target), (value), __ATOMIC_SEQ_CST)
static inline LONG mock_compare_exchange(volatile LONG *target, LONG value, LONG comparand) {
    __atomic_compare_exchange_n(target, &comparand, value, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    return comparand;
}
#define InterlockedCompareExchange(target, value, comparand) \
    mock_compare_exchange((target), (value), (comparand))

HANDLE CreateEventW(void *attributes, int manual_reset, int initial_state, const void *name);
int CloseHandle(HANDLE handle);
DWORD WaitForSingleObject(HANDLE handle, DWORD milliseconds);

typedef enum DXGI_FORMAT {
    DXGI_FORMAT_UNKNOWN = 0,
    DXGI_FORMAT_R8G8B8A8_UNORM = 28,
    DXGI_FORMAT_R8G8_UNORM = 49,
    DXGI_FORMAT_R8_UNORM = 61,
    DXGI_FORMAT_NV12 = 103,
    DXGI_FORMAT_P010 = 104,
} DXGI_FORMAT;

typedef enum D3D12_RESOURCE_DIMENSION {
    D3D12_RESOURCE_DIMENSION_UNKNOWN = 0,
    D3D12_RESOURCE_DIMENSION_BUFFER = 1,
    D3D12_RESOURCE_DIMENSION_TEXTURE2D = 3,
} D3D12_RESOURCE_DIMENSION;

typedef enum D3D12_RESOURCE_FLAGS {
    D3D12_RESOURCE_FLAG_NONE = 0,
} D3D12_RESOURCE_FLAGS;

typedef struct DXGI_SAMPLE_DESC {
    UINT Count;
    UINT Quality;
} DXGI_SAMPLE_DESC;

typedef struct D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DIMENSION Dimension;
    UINT64 Alignment;
    UINT64 Width;
    UINT Height;
    uint16_t DepthOrArraySize;
    uint16_t MipLevels;
    DXGI_FORMAT Format;
    DXGI_SAMPLE_DESC SampleDesc;
    int Layout;
    D3D12_RESOURCE_FLAGS Flags;
} D3D12_RESOURCE_DESC;

typedef enum D3D12_HEAP_TYPE {
    D3D12_HEAP_TYPE_DEFAULT = 1,
} D3D12_HEAP_TYPE;

typedef struct D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_TYPE Type;
    int CPUPageProperty;
    int MemoryPoolPreference;
    UINT CreationNodeMask;
    UINT VisibleNodeMask;
} D3D12_HEAP_PROPERTIES;

typedef enum D3D12_HEAP_FLAGS {
    D3D12_HEAP_FLAG_NONE = 0,
    D3D12_HEAP_FLAG_CREATE_NOT_ZEROED = 0x1000,
} D3D12_HEAP_FLAGS;

typedef enum D3D12_RESOURCE_STATES {
    D3D12_RESOURCE_STATE_COMMON = 0,
    D3D12_RESOURCE_STATE_RENDER_TARGET = 0x4,
    D3D12_RESOURCE_STATE_COPY_DEST = 0x400,
    D3D12_RESOURCE_STATE_COPY_SOURCE = 0x800,
} D3D12_RESOURCE_STATES;

typedef enum D3D12_COMMAND_LIST_TYPE {
    D3D12_COMMAND_LIST_TYPE_DIRECT = 0,
} D3D12_COMMAND_LIST_TYPE;

typedef enum D3D12_FENCE_FLAGS {
    D3D12_FENCE_FLAG_NONE = 0,
} D3D12_FENCE_FLAGS;

typedef struct ID3D12Device ID3D12Device;
typedef struct ID3D12Resource ID3D12Resource;
typedef struct ID3D12CommandAllocator ID3D12CommandAllocator;
typedef struct ID3D12GraphicsCommandList ID3D12GraphicsCommandList;
typedef struct ID3D12CommandList ID3D12CommandList;
typedef struct ID3D12Fence ID3D12Fence;
typedef struct ID3D12CommandQueue ID3D12CommandQueue;

typedef struct ID3D12ResourceVtbl {
    void (*GetDesc)(ID3D12Resource *self, D3D12_RESOURCE_DESC *output);
} ID3D12ResourceVtbl;

struct ID3D12Resource {
    const ID3D12ResourceVtbl *lpVtbl;
};

typedef enum D3D12_RESOURCE_BARRIER_TYPE {
    D3D12_RESOURCE_BARRIER_TYPE_TRANSITION = 0,
} D3D12_RESOURCE_BARRIER_TYPE;

typedef enum D3D12_RESOURCE_BARRIER_FLAGS {
    D3D12_RESOURCE_BARRIER_FLAG_NONE = 0,
} D3D12_RESOURCE_BARRIER_FLAGS;

#define D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES 0xffffffffu

typedef struct D3D12_RESOURCE_TRANSITION_BARRIER {
    ID3D12Resource *pResource;
    UINT Subresource;
    D3D12_RESOURCE_STATES StateBefore;
    D3D12_RESOURCE_STATES StateAfter;
} D3D12_RESOURCE_TRANSITION_BARRIER;

typedef struct D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER_TYPE Type;
    D3D12_RESOURCE_BARRIER_FLAGS Flags;
    union {
        D3D12_RESOURCE_TRANSITION_BARRIER Transition;
    };
} D3D12_RESOURCE_BARRIER;

typedef enum D3D12_TEXTURE_COPY_TYPE {
    D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX = 0,
    D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT = 1,
} D3D12_TEXTURE_COPY_TYPE;

typedef struct D3D12_TEXTURE_COPY_LOCATION {
    ID3D12Resource *pResource;
    D3D12_TEXTURE_COPY_TYPE Type;
    union {
        /* Stands in for D3D12_PLACED_SUBRESOURCE_FOOTPRINT, which the helpers never use. */
        UINT64 PlacedFootprint[4];
        UINT SubresourceIndex;
    };
} D3D12_TEXTURE_COPY_LOCATION;

typedef struct D3D12_BOX {
    UINT left;
    UINT top;
    UINT front;
    UINT right;
    UINT bottom;
    UINT back;
} D3D12_BOX;

typedef struct IID {
    int id;
} IID;
extern const IID IID_ID3D12Resource;
extern const IID IID_ID3D12CommandAllocator;
extern const IID IID_ID3D12GraphicsCommandList;
extern const IID IID_ID3D12Fence;

/* The mock objects' calls, defined by the harness. */
void mock_add_ref(void *object);
void mock_release(void *object);
HRESULT mock_create_committed_resource(
    ID3D12Device *device, const D3D12_HEAP_PROPERTIES *heap, D3D12_HEAP_FLAGS flags,
    const D3D12_RESOURCE_DESC *descriptor, D3D12_RESOURCE_STATES state, const void *clear,
    const IID *iid, void **output);
HRESULT mock_create_command_allocator(
    ID3D12Device *device, D3D12_COMMAND_LIST_TYPE type, const IID *iid, void **output);
HRESULT mock_create_command_list(
    ID3D12Device *device, UINT mask, D3D12_COMMAND_LIST_TYPE type,
    ID3D12CommandAllocator *allocator, void *state, const IID *iid, void **output);
HRESULT mock_create_fence(
    ID3D12Device *device, UINT64 value, D3D12_FENCE_FLAGS flags, const IID *iid, void **output);
HRESULT mock_allocator_reset(ID3D12CommandAllocator *allocator);
HRESULT mock_list_reset(
    ID3D12GraphicsCommandList *commands, ID3D12CommandAllocator *allocator, void *state);
void mock_list_barrier(
    ID3D12GraphicsCommandList *commands, UINT count, const D3D12_RESOURCE_BARRIER *barriers);
void mock_list_copy(
    ID3D12GraphicsCommandList *commands, ID3D12Resource *target, ID3D12Resource *source);
void mock_list_copy_region(
    ID3D12GraphicsCommandList *commands, const D3D12_TEXTURE_COPY_LOCATION *target, UINT x,
    UINT y, UINT z, const D3D12_TEXTURE_COPY_LOCATION *source, const D3D12_BOX *box);
HRESULT mock_list_close(ID3D12GraphicsCommandList *commands);
UINT64 mock_fence_completed(ID3D12Fence *fence);
HRESULT mock_fence_set_event(ID3D12Fence *fence, UINT64 value, HANDLE event);
HRESULT mock_queue_wait(ID3D12CommandQueue *queue, ID3D12Fence *fence, UINT64 value);
void mock_queue_execute(ID3D12CommandQueue *queue, UINT count, ID3D12CommandList *const *lists);
HRESULT mock_queue_signal(ID3D12CommandQueue *queue, ID3D12Fence *fence, UINT64 value);

#define ID3D12Device_AddRef(self) mock_add_ref(self)
#define ID3D12Device_Release(self) mock_release(self)
#define ID3D12Device_CreateCommittedResource(self, heap, flags, descriptor, state, clear, iid, output) \
    mock_create_committed_resource(self, heap, flags, descriptor, state, clear, iid, output)
#define ID3D12Device_CreateCommandAllocator(self, type, iid, output) \
    mock_create_command_allocator(self, type, iid, output)
#define ID3D12Device_CreateCommandList(self, mask, type, allocator, state, iid, output) \
    mock_create_command_list(self, mask, type, allocator, state, iid, output)
#define ID3D12Device_CreateFence(self, value, flags, iid, output) \
    mock_create_fence(self, value, flags, iid, output)
#define ID3D12Resource_Release(self) mock_release(self)
#define ID3D12CommandAllocator_Release(self) mock_release(self)
#define ID3D12CommandAllocator_Reset(self) mock_allocator_reset(self)
#define ID3D12GraphicsCommandList_Release(self) mock_release(self)
#define ID3D12GraphicsCommandList_Reset(self, allocator, state) \
    mock_list_reset(self, allocator, state)
#define ID3D12GraphicsCommandList_ResourceBarrier(self, count, barriers) \
    mock_list_barrier(self, count, barriers)
#define ID3D12GraphicsCommandList_CopyResource(self, target, source) \
    mock_list_copy(self, target, source)
#define ID3D12GraphicsCommandList_CopyTextureRegion(self, target, x, y, z, source, box) \
    mock_list_copy_region(self, target, x, y, z, source, box)
#define ID3D12GraphicsCommandList_Close(self) mock_list_close(self)
#define ID3D12Fence_Release(self) mock_release(self)
#define ID3D12Fence_GetCompletedValue(self) mock_fence_completed(self)
#define ID3D12Fence_SetEventOnCompletion(self, value, event) mock_fence_set_event(self, value, event)
#define ID3D12CommandQueue_Wait(self, fence, value) mock_queue_wait(self, fence, value)
#define ID3D12CommandQueue_ExecuteCommandLists(self, count, lists) \
    mock_queue_execute(self, count, lists)
#define ID3D12CommandQueue_Signal(self, fence, value) mock_queue_signal(self, fence, value)

#endif
