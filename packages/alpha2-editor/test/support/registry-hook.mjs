// Module hook for tests of the editor's renderer module (runtime/engine.js): its wasm binding
// resolves to a recording fake (fake-registry.mjs), so the module runs in Node without a GPU.
const fake = new URL("./fake-registry.mjs", import.meta.url).href;

export async function resolve(specifier, context, next) {
  if (
    context.parentURL?.endsWith("/dist/browser/runtime/engine.js") &&
    (specifier === "alpha2/wasm" || specifier === "./wasm.js")
  )
    return { url: fake, shortCircuit: true };
  return next(specifier, context);
}
