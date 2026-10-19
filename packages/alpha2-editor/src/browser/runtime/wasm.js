import init from "alpha2/wasm";
import * as binding from "alpha2/wasm";

let loading = null;

/**
 * Instantiates `alpha2/wasm` once for this realm and resolves to its exports. wasm-bindgen's own
 * init reuses an instance only after one has finished: two calls in flight instantiate twice, and
 * the later instance replaces the module's memory under objects made from the first, whose next
 * call reads out of bounds. Every caller goes through here. A failed load is retried by the next
 * call.
 * @param {string | URL | undefined} wasmUrl
 */
export function loadEngineWasm(wasmUrl) {
  loading ??= init(wasmUrl ? { module_or_path: wasmUrl } : undefined)
    .then(() => binding)
    .catch((error) => {
      loading = null;
      throw error;
    });
  return loading;
}
