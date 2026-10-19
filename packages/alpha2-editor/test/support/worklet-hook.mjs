// Module hook for mixer tests: the editor's audio module reaches a stand-in audio worklet
// (stub-worklet.mjs), so its transport runs in Node, which has no AudioWorklet.
const stub = new URL("./stub-worklet.mjs", import.meta.url).href;

export async function resolve(specifier, context, next) {
  if (
    specifier === "alpha2/audio-worklet" &&
    context.parentURL?.endsWith("/dist/browser/runtime/audio.js")
  )
    return { url: stub, shortCircuit: true };
  return next(specifier, context);
}
