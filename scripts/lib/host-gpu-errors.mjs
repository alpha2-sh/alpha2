// What a host run prints when its GPU work went wrong, on any backend: Vulkan `VUID-*` and
// `VK_ERROR_*`, plus alpha2-gpu's own device events, which read the same on Metal and D3D12.

const PATTERN =
  /VK_ERROR_[A-Z_]+|VUID-[\w-]+|validation layer|did not finish within|uncaptured GPU [\w-]+ error|GPU device lost|GPU work did not complete within/i;

/** The first GPU error `text` reports, or null. */
export function hostGpuError(text) {
  return text.match(PATTERN)?.[0] ?? null;
}
