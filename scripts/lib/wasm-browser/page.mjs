// The page every case runs in: how a case reports, how it says it cannot run here (`skip`), and the
// helpers several cases share.

/**
 * The luma of one pixel of a `VideoFrame`, as an async function's source (so a Node test can run
 * it on fake frames). 4:2:0, 4:2:2 and 4:4:4 frames give their first plane's sample; RGB frames
 * give BT.709 limited-range luma, rounded, which is what a decoder handing out YUV would have.
 */
export const LUMA_AT = `async function lumaAt(frame, x, y) {
  const bytes = new Uint8Array(frame.allocationSize());
  const [first] = await frame.copyTo(bytes);
  const at = first.offset + y * first.stride;
  if (/^(I4|NV12)/.test(frame.format)) return bytes[at + x];
  const rgb = { RGBA: [0, 1, 2], RGBX: [0, 1, 2], BGRA: [2, 1, 0], BGRX: [2, 1, 0] }[frame.format];
  if (rgb === undefined) throw new Error("no luma for a " + frame.format + " frame");
  const [r, g, b] = rgb.map((channel) => bytes[at + 4 * x + channel]);
  return Math.round(16 + (65.481 * r + 128.553 * g + 24.966 * b) / 255);
}`;

// The import map also resolves the "alpha2/..." imports of the editor package's built runtime
// (packages/alpha2-editor/dist), which a case may load.
export function page(body, videoImport = false) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>pending</title>
<script type="importmap">{"imports":{"alpha2/extensions-wasm":"/packages/alpha2/wasm/alpha2_extensions.js","mediabunny":"/node_modules/mediabunny/dist/bundles/mediabunny.mjs","alpha2/assets":"/packages/alpha2/dist/assets.js","alpha2/edit":"/packages/alpha2/dist/edit.js","alpha2/extensions":"/packages/alpha2/dist/extensions.js","alpha2/frame-supply":"/packages/alpha2/dist/frame-supply.js","alpha2/hdr-frames":"/packages/alpha2/dist/hdr-frames.js","alpha2/mediabunny":"/packages/alpha2/dist/mediabunny.js","alpha2/project":"/packages/alpha2/dist/project.js","alpha2/text":"/packages/alpha2/dist/text.js","alpha2/worker-error":"/packages/alpha2/dist/worker-error.js","alpha2/abort":"/packages/alpha2/dist/abort.js","alpha2/active-clock":"/packages/alpha2/dist/active-clock.js","alpha2/wasm":"/packages/alpha2/wasm/alpha2_wasm.js"}}</script>
</head><body>
<script type="module">
const VIDEO_IMPORT = ${videoImport};
const report = (value) => {
  document.title = encodeURIComponent(JSON.stringify(value));
  const result = document.createElement("pre");
  result.textContent = JSON.stringify(value, null, 2);
  document.body.append(result);
};
// What a case throws when it observed that this browser or machine lacks a capability: the run
// records the skip under the capability's name, and fails it when ALPHA2_REQUIRE names it.
class CaseSkip extends Error {
  constructor(capability, reason, details) {
    super(reason);
    this.capability = capability;
    this.details = details;
  }
}
const skip = (capability, reason, details = {}) => {
  throw new CaseSkip(capability, reason, details);
};
// Chrome's stack starts with the message; Firefox's and Safari's are the frames alone.
const described = (error) => {
  const message = String(error?.message ?? error);
  const stack = String(error?.stack ?? "");
  return stack.includes(message) ? stack : stack ? message + "\\n" + stack : message;
};
// A device or instance lost while a VideoFrame is imported into WebGPU: headless Chrome and
// SwiftShader's WebGPU do this. A loss the case saw before it imported anything is the engine's.
const importUnavailable = (error) =>
  /external Instance reference no longer exists|device was lost|Device was destroyed/i.test(String(error?.message ?? error));
${LUMA_AT}
// The engine on the browser's WebGPU device, for cases whose subject is not the GPU. Without an
// adapter the case cannot run here.
const gpuEngine = async (Registry) => {
  if (!(await navigator.gpu?.requestAdapter())) skip("webgpu", "no WebGPU adapter");
  return Registry.create();
};
// The adapter an engine runs on; a case about WebGPU cannot run on another backend.
const webgpuAdapter = (engine) => {
  const { name = "", backend = "" } = JSON.parse(engine.diagnostics()).adapter ?? {};
  if (backend !== "BrowserWebGpu")
    skip("webgpu", "the engine runs on " + (backend || "no backend") + ", not WebGPU", { adapter: { name, backend } });
  return { name, backend };
};
try {
${body}
} catch (error) {
  if (error instanceof CaseSkip)
    report({ skipped: error.message, capability: error.capability, ...error.details });
  else if (VIDEO_IMPORT && !error?.beforeImport && importUnavailable(error))
    report({
      skipped: "this browser lost WebGPU importing a VideoFrame (" + String(error?.message ?? error) + ")",
      capability: "webgpu-video-import",
    });
  else report({ error: described(error) });
}
</script></body></html>`;
}
