import init from "alpha2/wasm";
import * as binding from "alpha2/wasm";

let loading = null;

/**
 * Instantiates `alpha2/wasm` once per realm and resolves to its exports. wasm-bindgen's init only
 * reuses a finished instance, so two concurrent calls would instantiate twice and the second would
 * replace the module's memory under objects made from the first. Every caller goes through here; a
 * failed load is retried by the next call.
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
