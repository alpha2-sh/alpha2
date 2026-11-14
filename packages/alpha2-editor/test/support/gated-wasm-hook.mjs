// Module hook: the bridge's lazy engine-wasm loader resolves to gated-wasm.mjs.
const stub = new URL("./gated-wasm.mjs", import.meta.url).href;

export async function resolve(specifier, context, next) {
  const resolved = await next(specifier, context);
  if (
    resolved.url.endsWith("/dist/browser/runtime/wasm.js") &&
    context.parentURL?.endsWith("/dist/browser/bridge.js")
  )
    return { ...resolved, url: stub, shortCircuit: true };
  return resolved;
}
