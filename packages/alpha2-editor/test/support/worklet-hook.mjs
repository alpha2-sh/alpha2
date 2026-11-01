// Module hook: the audio module reaches stub-worklet.mjs, since Node has no AudioWorklet.
const stub = new URL("./stub-worklet.mjs", import.meta.url).href;

export async function resolve(specifier, context, next) {
  if (
    specifier === "alpha2/audio-worklet" &&
    context.parentURL?.endsWith("/dist/browser/runtime/audio.js")
  )
    return { url: stub, shortCircuit: true };
  return next(specifier, context);
}
