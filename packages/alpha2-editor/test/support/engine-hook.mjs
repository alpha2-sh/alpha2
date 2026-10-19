// Module hook for preview tests: the bridge's and the preview worker's renderer module resolves to
// a recording stub (no GPU in Node); the audio module is the real one, its mixers recorded.
const stub = new URL("./stub-engine.mjs", import.meta.url).href;
const audio = new URL("./stub-audio.mjs", import.meta.url).href;

export async function resolve(specifier, context, next) {
  const resolved = await next(specifier, context);
  if (
    resolved.url.endsWith("/dist/browser/runtime/engine.js") &&
    (context.parentURL?.endsWith("/dist/browser/bridge.js") ||
      context.parentURL?.endsWith("/dist/browser/worker.js"))
  )
    return { ...resolved, url: stub, shortCircuit: true };
  if (
    resolved.url.endsWith("/dist/browser/runtime/audio.js") &&
    context.parentURL?.endsWith("/dist/browser/bridge.js")
  )
    return { ...resolved, url: audio, shortCircuit: true };
  return resolved;
}
