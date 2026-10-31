// Module hook: the renderer module's wasm binding resolves to fake-registry.mjs, so it runs without
// a GPU.
const fake = new URL("./fake-registry.mjs", import.meta.url).href;

export async function resolve(specifier, context, next) {
  if (
    context.parentURL?.endsWith("/dist/browser/runtime/engine.js") &&
    (specifier === "alpha2/wasm" || specifier === "./wasm.js")
  )
    return { url: fake, shortCircuit: true };
  return next(specifier, context);
}
