// Stands in for the bridge's engine-wasm loader (gated-wasm-hook.mjs). While a test sets
// `globalThis.gatedWasm = { requested(), gate, binding }`, a load waits for `gate`.
export async function loadEngineWasm(url) {
  const gated = globalThis.gatedWasm;
  if (!gated) {
    const real = await import("../../dist/browser/runtime/wasm.js");
    return real.loadEngineWasm(url);
  }
  gated.requested?.();
  await gated.gate;
  return gated.binding;
}
