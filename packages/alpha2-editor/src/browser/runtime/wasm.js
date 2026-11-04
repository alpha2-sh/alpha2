import * as binding from "alpha2/wasm";
import { WasmModule } from "alpha2/wasm-module";

const engine = new WasmModule(() => import("alpha2/wasm"));

/**
 * Instantiates `alpha2/wasm` once per realm and resolves to its exports. Every caller goes through
 * here, since two concurrent wasm-bindgen inits would replace the module's memory under objects
 * made from the first; a failed load is retried by the next call.
 * @param {string | URL | undefined} wasmUrl
 */
export function loadEngineWasm(wasmUrl) {
  return engine.load(wasmUrl).then(() => binding);
}
