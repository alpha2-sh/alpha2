// RGB frames give BT.709 limited-range luma, matching a YUV decoder. Kept as source so a Node test
// can run it.
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

// The skip for the engine's DECODE_UNAVAILABLE when WebCodecs, asked on its own, refuses the
// track's codec too; null otherwise, so a refusal of a codec this browser decodes fails the case.
// A refusal after the browser's decoder threw needs more, since browsers claim layouts they then
// reject: `globalThis.trialDecodes(facts)` must find the case's own source failing a plain
// WebCodecs decode. The sample rate is not in the facts and rarely decides support. Kept as source
// for a Node test.
export const DECODE_REFUSAL_SKIP = `async function decodeRefusalSkip(error) {
  const facts = error?.code === "DECODE_UNAVAILABLE" ? error.facts : undefined;
  if (!facts?.kind || !facts.codec) return null;
  const probe = facts.kind === "audio"
    ? globalThis.AudioDecoder?.isConfigSupported({ codec: facts.codec, sampleRate: 48000, numberOfChannels: facts.channels ?? 2 })
    : globalThis.VideoDecoder?.isConfigSupported({ codec: facts.codec });
  const supported = probe
    ? await probe.then(({ supported }) => supported === true, () => false)
    : false;
  if (supported && (error.cause === undefined || (await globalThis.trialDecodes?.(facts)) !== false))
    return null;
  return {
    skipped: "this browser does not decode the case's source (WebCodecs refuses " + facts.codec + "): "
      + String(error.message ?? error),
    capability: facts.kind === "audio" ? "webcodecs-audio" : "webcodecs-video",
  };
}`;

// Whether a media file the page fetched has a track matching `facts` whose first packets decode
// with plain WebCodecs: true if one does, false if every match fails, undefined with no match.
const TRIAL_DECODES = `const fetchedMedia = new Set();
const pageFetch = globalThis.fetch.bind(globalThis);
globalThis.fetch = (resource, ...rest) => {
  const url = new URL(resource instanceof Request ? resource.url : String(resource), location.href);
  if (/\\.(mp4|m4a|mov|mkv|webm|ogg|opus|mp3|wav|flac)$/i.test(url.pathname)) fetchedMedia.add(url.href);
  return pageFetch(resource, ...rest);
};
const trialDecode = async (track, kind) => {
  const { EncodedPacketSink } = await import("mediabunny");
  const config = await track.getDecoderConfig();
  if (config === null) return false;
  const packets = [];
  for await (const packet of new EncodedPacketSink(track).packets()) {
    packets.push(packet);
    if (packets.length === 4) break;
  }
  let decoded = false;
  let failed = false;
  const Decoder = kind === "audio" ? AudioDecoder : VideoDecoder;
  const decoder = new Decoder({
    output: (output) => { decoded = true; output.close(); },
    error: () => { failed = true; },
  });
  try {
    // @ts-expect-error TypeScript cannot call a union of the WebCodecs decoders' overloaded configure.
    decoder.configure(config);
    for (const packet of packets)
      decoder.decode(kind === "audio" ? packet.toEncodedAudioChunk() : packet.toEncodedVideoChunk());
    await decoder.flush();
  } catch {
    failed = true;
  } finally {
    if (decoder.state !== "closed") decoder.close();
  }
  return decoded && !failed;
};
globalThis.trialDecodes = async (facts) => {
  const { ALL_FORMATS, BlobSource, Input } = await import("mediabunny");
  let answer;
  for (const url of fetchedMedia) {
    const input = new Input({ source: new BlobSource(await (await pageFetch(url)).blob()), formats: ALL_FORMATS });
    try {
      const tracks = facts.kind === "audio" ? await input.getAudioTracks() : await input.getVideoTracks();
      for (const track of tracks) {
        const codecs = [track.codec, await track.getCodecParameterString().catch(() => null)];
        if (!codecs.includes(facts.codec)) continue;
        if (facts.channels !== undefined && !(track.isAudioTrack() && track.numberOfChannels === facts.channels))
          continue;
        if (await trialDecode(track, facts.kind)) return true;
        answer = false;
      }
    } catch {
      // A file mediabunny does not read is not the refused source.
    } finally {
      input.dispose();
    }
  }
  return answer;
};`;

// Workers have no import map, so `/worker-modules/<path>` serves `<path>` with these resolved.
export const IMPORTS = {
  "alpha2/extensions-wasm": "/packages/alpha2/wasm/alpha2_extensions.js",
  "alpha2/extensions-pdf-wasm":
    "/packages/alpha2/wasm/alpha2_extensions_pdf.js",
  mediabunny: "/node_modules/mediabunny/dist/bundles/mediabunny.mjs",
  "alpha2/apply-edit": "/packages/alpha2/dist/apply-edit.js",
  "alpha2/assets": "/packages/alpha2/dist/assets.js",
  "alpha2/edit": "/packages/alpha2/dist/edit.js",
  "alpha2/extract": "/packages/alpha2/dist/extract.js",
  "alpha2/plugins": "/packages/alpha2/dist/plugins.js",
  "alpha2/extensions": "/packages/alpha2/dist/extensions.js",
  "alpha2/frame-supply": "/packages/alpha2/dist/frame-supply.js",
  "alpha2/hdr-frames": "/packages/alpha2/dist/hdr-frames.js",
  "alpha2/mediabunny": "/packages/alpha2/dist/mediabunny.js",
  "alpha2/project": "/packages/alpha2/dist/project.js",
  "alpha2/text": "/packages/alpha2/dist/text.js",
  "alpha2/worker-error": "/packages/alpha2/dist/worker-error.js",
  "alpha2/abort": "/packages/alpha2/dist/abort.js",
  "alpha2/active-clock": "/packages/alpha2/dist/active-clock.js",
  "alpha2/wasm": "/packages/alpha2/wasm/alpha2_wasm.js",
  alpha2: "/packages/alpha2/dist/index.js",
  "alpha2/audio-timeline": "/packages/alpha2/dist/audio-timeline.js",
  "alpha2/audio-worklet": "/packages/alpha2/dist/audio-worklet.js",
  "alpha2/captions": "/packages/alpha2/dist/captions.js",
  "alpha2/containers": "/packages/alpha2/dist/containers.js",
  "alpha2/effects": "/packages/alpha2/dist/effects.js",
  "alpha2/export-destination": "/packages/alpha2/dist/export-destination.js",
  "alpha2/export-failure": "/packages/alpha2/dist/export-failure.js",
  "alpha2/export-metrics": "/packages/alpha2/dist/export-metrics.js",
  "alpha2/export-retry": "/packages/alpha2/dist/export-retry.js",
  "alpha2/gpu-recovery": "/packages/alpha2/dist/gpu-recovery.js",
  "alpha2/layout": "/packages/alpha2/dist/layout.js",
  "alpha2/rate-control": "/packages/alpha2/dist/rate-control.js",
  "alpha2/renderer-choice": "/packages/alpha2/dist/renderer-choice.js",
  "alpha2/video-frames": "/packages/alpha2/dist/video-frames.js",
  "alpha2/wasm-module": "/packages/alpha2/dist/wasm-module.js",
};

export function page(body, videoImport = false) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>pending</title>
<script type="importmap">${JSON.stringify({ imports: IMPORTS })}</script>
</head><body>
<script type="module">
const VIDEO_IMPORT = ${videoImport};
// A GPU process restarting at page start gives no adapter, so retry once after 500 ms.
let adapterRetries = 0;
if (globalThis.GPU?.prototype.requestAdapter) {
  const requestAdapter = GPU.prototype.requestAdapter;
  GPU.prototype.requestAdapter = async function (...args) {
    const adapter = await requestAdapter.apply(this, args);
    if (adapter !== null) return adapter;
    adapterRetries += 1;
    console.warn("no WebGPU adapter; asking once more");
    await new Promise((resolve) => setTimeout(resolve, 500));
    return requestAdapter.apply(this, args);
  };
}
let simulationCheck = (value) => value;
const simulate = async (names, check) => {
  (await import("/scripts/lib/wasm-browser/safari-sim.mjs")).installSimulations(names);
  if (check) simulationCheck = check;
};
const SIMULATIONS = new URL(location.href).searchParams.get("sim");
if (SIMULATIONS) await simulate(SIMULATIONS.split(","));
const report = (value) => {
  if (adapterRetries > 0) value = { ...value, adapterRetries };
  if (value.ok === true) value = simulationCheck(value);
  document.title = encodeURIComponent(JSON.stringify(value));
  const result = document.createElement("pre");
  result.textContent = JSON.stringify(value, null, 2);
  document.body.append(result);
};
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
// Losses during VideoFrame import (headless Chrome, SwiftShader) or refused copies (Windows WARP)
// are the browser's; a loss before any import is the engine's.
const importUnavailable = (error) =>
  /external Instance reference no longer exists|device was lost|Device was destroyed|Failed to copy content from video element/i.test(String(error?.message ?? error));
${LUMA_AT}
// A positive control: a canvas cleared to one colour without the engine and captured the same way.
// A blank control skips the case (presentation-capture), and with a working control a blank
// engine capture fails it.
const presentationControl = async (capture, width, height, kind = "webgpu") => {
  const canvas = new OffscreenCanvas(width, height);
  let device = null;
  let pixel = null;
  try {
    if (kind === "webgpu") {
      device = await (await navigator.gpu.requestAdapter()).requestDevice();
      const context = canvas.getContext("webgpu");
      context.configure({ device, format: navigator.gpu.getPreferredCanvasFormat(), alphaMode: "premultiplied" });
      const encoder = device.createCommandEncoder();
      encoder.beginRenderPass({ colorAttachments: [{
        view: context.getCurrentTexture().createView(), loadOp: "clear", storeOp: "store",
        clearValue: { r: 40 / 255, g: 160 / 255, b: 220 / 255, a: 1 },
      }] }).end();
      device.queue.submit([encoder.finish()]);
    } else {
      const gl = canvas.getContext("webgl2");
      gl.clearColor(40 / 255, 160 / 255, 220 / 255, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
    }
    pixel = Array.from(await capture(canvas));
  } catch (error) {
    skip("presentation-capture", "the positive control could not be captured: " + String(error?.message ?? error));
  } finally {
    device?.destroy();
  }
  if (pixel.every((value) => value === 0))
    skip("presentation-capture", "the positive control (a " + kind + " canvas cleared without the engine, through the same capture) captured blank", { control: pixel });
  return pixel;
};
const gpuEngine = async (Registry) => {
  if (!(await navigator.gpu?.requestAdapter())) skip("webgpu", "no WebGPU adapter");
  return Registry.create();
};
const webgpuAdapter = (engine) => {
  const { name = "", backend = "" } = JSON.parse(engine.diagnostics()).adapter ?? {};
  if (backend !== "BrowserWebGpu")
    skip("webgpu", "the engine runs on " + (backend || "no backend") + ", not WebGPU", { adapter: { name, backend } });
  return { name, backend };
};
${TRIAL_DECODES}
${DECODE_REFUSAL_SKIP}
const offersGpu = async () =>
  Boolean(await navigator.gpu?.requestAdapter().catch(() => null)) ||
  Boolean(new OffscreenCanvas(1, 1).getContext("webgl2"));
try {
${body}
} catch (error) {
  const message = String(error?.message ?? error);
  let refusal;
  if (error instanceof CaseSkip)
    report({ skipped: error.message, capability: error.capability, ...error.details });
  else if (/the canvas takes no GPU surface/.test(message) && !(await offersGpu()))
    report({ skipped: "this browser offers no WebGPU adapter and no WebGL2 context", capability: "webgl2" });
  else if ((refusal = await decodeRefusalSkip(error)) !== null) report(refusal);
  else if (VIDEO_IMPORT && !error?.beforeImport && importUnavailable(error))
    report({
      skipped: "this browser lost WebGPU importing a VideoFrame (" + String(error?.message ?? error) + ")",
      capability: "webgpu-video-import",
    });
  else report({ error: described(error), code: error?.code ?? null, stage: error?.failure?.stage ?? null });
}
</script></body></html>`;
}
