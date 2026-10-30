/*
 * The D3D12 Video declarations `c/d3d12.c` uses to ask a device for its video interface and why
 * it was removed, mocked like `d3d12.h`: the harness's device has no video interface and is
 * never removed.
 */
#ifndef ALPHA2_D3D12VIDEO_MOCK_H
#define ALPHA2_D3D12VIDEO_MOCK_H

#include "d3d12.h"

#define E_NOINTERFACE ((HRESULT)0x80004002u)

typedef struct ID3D12VideoDevice ID3D12VideoDevice;

extern const IID IID_ID3D12VideoDevice;

HRESULT mock_query_interface(ID3D12Device *device, const IID *iid, void **output);
HRESULT mock_device_removed_reason(ID3D12Device *device);

#define ID3D12Device_QueryInterface(self, iid, output) mock_query_interface(self, iid, output)
#define ID3D12Device_GetDeviceRemovedReason(self) mock_device_removed_reason(self)
#define ID3D12VideoDevice_Release(self) mock_release(self)

#endif
