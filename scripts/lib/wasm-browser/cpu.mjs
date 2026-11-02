// CPU renderer cases. `--disable-gpu` alone still leaves headless Chrome SwiftShader WebGPU and
// WebGL2; for no GPU process at all run
//   ALPHA2_CHROME_GPU=none node scripts/test-wasm-browser.mjs --case renderer-auto

// `answer` is an expression for `detectBrowserCapabilities` on the rendering thread; `auto` picks
// "cpu" with no adapter or only a software one, as the native `auto` does.
const EXPECTED_RENDERER = (renderer, answer) => `
const expected = ${JSON.stringify(renderer)} === "auto" ? (${answer}).renderer : ${JSON.stringify(renderer)};
`;

const PROJECT = (width, height) => `{
  schema_version: 1, id: "cpu", name: "CPU",
  settings: { width: ${width}, height: ${height}, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  compositions: [{ id: "blur", nodes: [
    { id: "clip", op: { type: "clip" } },
    { id: "effect", op: { type: "effect", input: "clip", effect: { id: "alpha2.effect.gaussian-blur", version: 1,
      params: { radius: { mode: "constant", value: { type: "float", value: 0.01 } } } } } },
  ], output: "effect" }],
  timeline: { tracks: [
    { id: "base", name: "Base", kind: "video", clips: [
      { id: "plain", source_id: "video", timeline_range: { start: 0, end: 60000 }, source_range: { start: 0, end: 60000 } },
      { id: "blurred", source_id: "video", timeline_range: { start: 60000, end: 120000 }, source_range: { start: 60000, end: 120000 },
        composition: { id: "blur" } },
    ] },
    { id: "over", name: "Over", kind: "video", clips: [
      { id: "small", source_id: "video", timeline_range: { start: 30000, end: 120000 }, source_range: { start: 0, end: 90000 },
        visual: { position: { mode: "constant", value: { x: 0.7, y: 0.3 } }, anchor: { x: 0.5, y: 0.5 },
          scale: { mode: "constant", value: { x: 0.35, y: 0.35 } }, opacity: { mode: "constant", value: 1 },
          rotation_degrees: { mode: "constant", value: 12 } } },
    ] },
  ] },
}`;

// The render host run in the page: I420 frames previewed, exported, encoded and decoded back.
export function cpuWorkerHost(renderer, width, height) {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const { createWasmRenderWorkerHost } = await import("/packages/alpha2/dist/worker.js");
const { detectBrowserCapabilities } = await import("/packages/alpha2/dist/robustness.js");
const capabilities = await detectBrowserCapabilities();
const [width, height] = [${width}, ${height}];
const project = ${PROJECT(width, height)};
const canvas = new OffscreenCanvas(width / 2, height / 2);
const host = await createWasmRenderWorkerHost(WebEffectRegistry, JSON.stringify(project), {
  renderer: ${JSON.stringify(renderer)},
})(canvas);
const choice = host.rendererChoice;
${EXPECTED_RENDERER(renderer, "capabilities")}
if (choice?.renderer === "gpu") {
  // A device lost on presenting (headless Chrome, SwiftShader) is the browser's.
  const data = new Uint8Array(width * height * 3 / 2).fill(128);
  const frame = () => new VideoFrame(data, { format: "I420", codedWidth: width, codedHeight: height, timestamp: 0,
    colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false } });
  let presented = await host.present(0n);
  for (let attempt = 0; attempt < 4 && presented.missingFrames.length > 0; attempt++) {
    await Promise.all(presented.missingFrames.map((missing) => host.uploadVideoFrame({
      sourceId: missing.sourceId, requestedTimeTicks: missing.mediaTimeTicks,
      decodedTimeTicks: missing.mediaTimeTicks, generation: 0, frame: frame(),
    })));
    presented = await host.present(0n);
  }
  const deviceLost = host.isDeviceLost();
  host.dispose();
  const details = { ...choice, presentedTimeTicks: String(presented.presentedTimeTicks), deviceLost, capabilities };
  const chosen = expected === "gpu" && choice.rendererReason === null;
  if (chosen && deviceLost) skip("webgpu-present", "the device was lost presenting to the canvas", details);
  report({ ok: chosen && presented.presentedTimeTicks === 0n && !deviceLost, ...details });
} else {
const decoded = (ticks) => {
  const shift = Number(ticks / 4000n);
  const luma = width * height;
  const data = new Uint8Array(luma * 3 / 2);
  for (let y = 0; y < height; y++)
    for (let x = 0; x < width; x++)
      data[y * width + x] = 16 + ((x + y + shift * 8) % 200);
  for (let y = height / 4; y < height / 2; y++)
    data.fill(235, y * width + width / 4, y * width + width / 2);
  for (let y = 0; y < height / 2; y++)
    for (let x = 0; x < width / 2; x++) {
      data[luma + y * (width / 2) + x] = 64 + (x * 128) / (width / 2);
      data[luma * 5 / 4 + y * (width / 2) + x] = 64 + (y * 128) / (height / 2);
    }
  return new VideoFrame(data, {
    format: "I420", codedWidth: width, codedHeight: height, timestamp: Number(ticks) * 1e6 / 120000,
    colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false },
  });
};
const frameTicks = Array.from({ length: 30 }, (_, index) => BigInt(index * 4000));
// Supply time stands in for a page's decoder and is excluded from the per-frame times.
let supplyMs = 0;
const supply = async (missingFrames) => {
  const started = performance.now();
  await Promise.all(missingFrames.map((missing) => host.uploadVideoFrame({
    sourceId: missing.sourceId, requestedTimeTicks: missing.mediaTimeTicks,
    decodedTimeTicks: missing.mediaTimeTicks, generation: 0, frame: decoded(missing.mediaTimeTicks),
  })));
  supplyMs += performance.now() - started;
};
const upload = async (ticks) => {
  for (let attempt = 0; attempt < 4; attempt++) {
    const presented = await host.present(ticks);
    if (presented.missingFrames.length === 0) return presented;
    await supply(presented.missingFrames);
  }
  throw new Error("frame " + ticks + " never had its sources");
};
for (const ticks of frameTicks) await upload(ticks);
supplyMs = 0;
const previewStarted = performance.now();
let presentedFrames = 0;
for (const ticks of frameTicks) {
  host.reset();
  const presented = await upload(ticks);
  if (presented.presentedTimeTicks === ticks) presentedFrames += 1;
}
const previewMs = (performance.now() - previewStarted - supplyMs) / frameTicks.length;
const previewSupplyMs = supplyMs / frameTicks.length;
const grid = Array.from({ length: 30 }, (_, index) => [(index % 6 + 0.5) / 6, (Math.floor(index / 6) + 0.5) / 5]);
let shown = null;
if (choice?.renderer === "cpu") {
  const pixels = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height).data;
  shown = grid.map(([u, v]) => {
    const at = (Math.floor(v * canvas.height) * canvas.width + Math.floor(u * canvas.width)) * 4;
    return 16 + (219 * (0.2126 * pixels[at] + 0.7152 * pixels[at + 1] + 0.0722 * pixels[at + 2])) / 255;
  });
}
const codecs = [
  { codec: "avc1.640028", engine: "h264", avc: { format: "annexb" } },
  { codec: "vp09.00.10.08", engine: "vp9" },
];
let encoding;
for (const candidate of codecs) {
  const config = { codec: candidate.codec, width, height, bitrate: 4_000_000, framerate: 30, ...(candidate.avc ? { avc: candidate.avc } : {}) };
  if ((await VideoEncoder.isConfigSupported(config)).supported) { encoding = { ...candidate, config }; break; }
}
if (encoding === undefined) throw new Error("this browser encodes neither H.264 nor VP9");
const chunks = [];
const encoder = new VideoEncoder({ output: (chunk) => chunks.push(chunk), error: (error) => { throw error; } });
encoder.configure(encoding.config);
const exported = [];
supplyMs = 0;
let encodeMs = 0;
const exportStarted = performance.now();
for (const [index, ticks] of frameTicks.entries()) {
  // The uploaded-frame budget may have dropped the preview's frames.
  let ticket = host.beginExport(ticks, encoding.engine, ticks);
  for (let attempt = 0; ticket.kind === "waiting" && attempt < 4; attempt++) {
    await supply(ticket.missingFrames);
    ticket = host.beginExport(ticks, encoding.engine, ticks);
  }
  if (ticket.kind !== "rendered") throw new Error("export frame " + ticks + " waits for " + ticket.missingSources);
  const frame = await ticket.readback;
  if (index === frameTicks.length - 1) {
    const copy = new Uint8Array(frame.allocationSize());
    const planes = await frame.copyTo(copy);
    const luma = (x, y) => copy[planes[0].offset + y * planes[0].stride + x];
    exported.push(luma(width * 3 / 8, height * 3 / 8));
    exported.push(grid.map(([u, v]) => {
      const [x, y] = [2 * Math.floor(u * width / 2), 2 * Math.floor(v * height / 2)];
      return (luma(x, y) + luma(x + 1, y) + luma(x, y + 1) + luma(x + 1, y + 1)) / 4;
    }));
  }
  const encodeStarted = performance.now();
  encoder.encode(frame, { keyFrame: index === 0 });
  frame.close();
  encodeMs += performance.now() - encodeStarted;
}
const exportMs = (performance.now() - exportStarted - supplyMs - encodeMs) / frameTicks.length;
const exportSupplyMs = supplyMs / frameTicks.length;
await encoder.flush();
encoder.close();
// A hardware decoder may fail before its first frame, so retry once in software. Hold only the
// newest frame: a decoder whose surfaces are all held stops silently and its flush never settles.
const decodeAll = async (hardwareAcceleration) => {
  let count = 0;
  let last = null;
  let failure = null;
  const decoder = new VideoDecoder({
    output: (frame) => { count += 1; last?.close(); last = frame; },
    error: (error) => { failure ??= error; },
  });
  try {
    decoder.configure({ codec: encoding.codec, codedWidth: width, codedHeight: height, hardwareAcceleration });
    for (const chunk of chunks) decoder.decode(chunk);
    await decoder.flush();
  } catch (error) {
    failure ??= error;
  }
  if (decoder.state !== "closed") decoder.close();
  return { count, last, failure };
};
let decoding = await decodeAll("no-preference");
let decoderRetried = null;
if (decoding.failure !== null && decoding.count === 0) {
  decoderRetried = String(decoding.failure?.message ?? decoding.failure);
  decoding = await decodeAll("prefer-software");
}
if (decoding.failure !== null) {
  decoding.last?.close();
  throw decoding.failure;
}
const decodedCount = decoding.count;
const last = decoding.last;
// Some browsers decode to RGB rather than YUV.
const decodedFormat = last.format;
const decodedSquare = await lumaAt(last, width * 3 / 8, height * 3 / 8);
last.close();
const report_ = {
  renderer: choice?.renderer, rendererReason: choice?.rendererReason, capabilities: { webgpu: capabilities.webgpu, webgl2: capabilities.webgl2, renderer: capabilities.renderer },
  size: [width, height], codec: encoding.codec, frames: frameTicks.length, presentedFrames, encodedChunks: chunks.length,
  decodedFrames: decodedCount, previewMsPerFrame: Number(previewMs.toFixed(1)), exportMsPerFrame: Number(exportMs.toFixed(1)),
  supplyMsPerFrame: { preview: Number(previewSupplyMs.toFixed(1)), export: Number(exportSupplyMs.toFixed(1)) },
  exportedSquare: exported[0], decodedSquare, decodedFormat, decoderRetried,
  previewDifference: shown === null ? null : Math.round(Math.max(...shown.map((value, index) => Math.abs(value - exported[1][index])))),
};
host.dispose();
report({
  ok: report_.renderer === expected && (expected === "gpu") === (report_.rendererReason === null) &&
    presentedFrames === frameTicks.length && decodedCount === frameTicks.length &&
    exported[0] > 200 && Math.abs(decodedSquare - exported[0]) <= 12 &&
    (shown === null || report_.previewDifference <= 24),
  ...report_,
});
}
`;
}

// The render worker on a transferred page canvas; reports which renderer it made.
export function rendererWorker(renderer) {
  return `
const { WorkerRenderBackend } = await import("/packages/alpha2/dist/worker.js");
const { detectBrowserCapabilities } = await import("/packages/alpha2/dist/robustness.js");
const capabilities = await detectBrowserCapabilities();
const origin = location.origin;
const source = \`
import init, { WebEffectRegistry } from "\${origin}/packages/alpha2/wasm/alpha2_wasm.js";
import { createWasmRenderWorkerHost, installRenderWorker } from "\${origin}/packages/alpha2/dist/worker.js";
await init();
installRenderWorker(self, createWasmRenderWorkerHost(WebEffectRegistry, \${JSON.stringify(JSON.stringify({
  schema_version: 1, id: "w", name: "W",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{ id: "c", source_id: "video",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 } }] }] },
}))}, { renderer: \${JSON.stringify(${JSON.stringify(renderer)})} }));
self.postMessage({ type: "test-ready" });
\`;
const worker = new Worker(URL.createObjectURL(new Blob([source], { type: "text/javascript" })), { type: "module" });
// A message posted before the module has installed its loop would be lost.
await new Promise((resolve, reject) => {
  worker.addEventListener("message", function ready(event) {
    if (event.data?.type === "test-ready") { worker.removeEventListener("message", ready); resolve(); }
  });
  worker.addEventListener("error", (event) => reject(new Error(event.message)), { once: true });
});
const backend = new WorkerRenderBackend(worker);
const canvas = document.createElement("canvas");
canvas.width = 32;
canvas.height = 24;
document.body.append(canvas);
await backend.initialize(canvas.transferControlToOffscreen());
const choice = backend.rendererChoice;
const workerCapabilities = await backend.detectCapabilities();
${EXPECTED_RENDERER(renderer, "workerCapabilities")}
if (choice?.renderer === "gpu") {
  // A device lost on presenting (headless Chrome, SwiftShader) is the browser's.
  const data = new Uint8Array(64 * 48 * 3 / 2).fill(128);
  await backend.uploadVideoFrame({
    sourceId: "video", requestedTimeTicks: 0n, decodedTimeTicks: 0n, generation: 0,
    frame: new VideoFrame(data, { format: "I420", codedWidth: 64, codedHeight: 48, timestamp: 0,
      colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false } }),
  });
  const presented = await backend.present(0n);
  const deviceLost = backend.isDeviceLost();
  backend.dispose();
  const details = { ...choice, presentedTimeTicks: String(presented.presentedTimeTicks), deviceLost, capabilities: capabilities.renderer };
  const chosen = expected === "gpu" && choice.rendererReason === null;
  if (chosen && deviceLost) skip("webgpu-present", "the device was lost presenting to the canvas", details);
  report({ ok: chosen && presented.presentedTimeTicks === 0n && !deviceLost, ...details });
} else {
const data = new Uint8Array(64 * 48 * 3 / 2).fill(128);
data.fill(200, 0, 64 * 48);
await backend.uploadVideoFrame({
  sourceId: "video", requestedTimeTicks: 0n, decodedTimeTicks: 0n, generation: 0,
  frame: new VideoFrame(data, { format: "I420", codedWidth: 64, codedHeight: 48, timestamp: 0,
    colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false } }),
});
const presented = await backend.present(0n);
const ticket = await backend.beginExport(0n, "h264", 0n);
const frame = ticket.kind === "rendered" ? await ticket.readback : null;
let luma = null;
if (frame !== null) {
  const copy = new Uint8Array(frame.allocationSize());
  const planes = await frame.copyTo(copy);
  luma = copy[planes[0].offset + 24 * planes[0].stride + 32];
  frame.close();
}
backend.dispose();
report({
  ok: choice?.renderer === expected && presented.presentedTimeTicks === 0n && luma === 200,
  ...choice, presentedTimeTicks: String(presented.presentedTimeTicks), luma, capabilities: capabilities.renderer,
});
}
`;
}

// Matches the native bench (crates/alpha2-render/benches/cpu_frame.rs): four 1080p I420 sources,
// new every frame, one per quadrant. Plane uploads are timed separately.
export function cpuGrid() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const [width, height, frames] = [1920, 1080, 10];
const canvas = new OffscreenCanvas(width / 2, height / 2);
const engine = WebEffectRegistry.createCpu(canvas);
const clip = (index) => ({
  id: "c" + index, source_id: "v" + index,
  timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  visual: {
    position: { mode: "constant", value: { x: 0.25 + 0.5 * (index % 2), y: 0.25 + 0.5 * Math.floor(index / 2) } },
    anchor: { x: 0.5, y: 0.5 }, scale: { mode: "constant", value: { x: 0.5, y: 0.5 } },
    rotation_degrees: { mode: "constant", value: 0 }, opacity: { mode: "constant", value: 1 },
  },
});
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "grid", name: "Grid",
  settings: { width, height, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [0, 1, 2, 3].map((index) => ({ id: "v" + index, locator: "fixture://v" + index, kind: "video" })),
  timeline: { tracks: [0, 1, 2, 3].map((index) => ({ id: "t" + index, name: "T" + index, kind: "video", clips: [clip(index)] })) },
}));
engine.setUploadedFrameBudget(4 * 1024 * 1024 * 1024 - 1);
const luma = width * height;
const planes = new Uint8Array(luma * 3 / 2);
for (let index = 0; index < luma; index++) planes[index] = 16 + (index % 211);
planes.fill(128, luma);
const color = JSON.stringify({ primaries: "bt709", transfer: "bt709", matrix: "bt709", range: "limited", dynamic_range: "sdr" });
const layout = Uint32Array.from([0, width, luma, width / 2, luma * 5 / 4, width / 2]);
const visible = Uint32Array.from([0, 0, width, height]);
let uploadMs = 0;
const supply = (ticks) => {
  const started = performance.now();
  for (let index = 0; index < 4; index++)
    engine.uploadYuvFrame("v" + index, ticks, "I420", width, height, planes, layout, visible, width, height,
      color, "left", width, height, 0, false);
  uploadMs += performance.now() - started;
};
const timed = (draw) => {
  uploadMs = 0;
  const started = performance.now();
  for (let frame = 0; frame < frames; frame++) {
    const ticks = BigInt(frame * 4000);
    engine.clearFrames();
    supply(ticks);
    draw(ticks);
  }
  return (performance.now() - started - uploadMs) / frames;
};
let exported = 0;
const exportMs = timed((ticks) => {
  const ticket = engine.beginExportAt(ticks, "h264");
  if (ticket.kind === "rendered") exported += 1;
});
const exportUploadMs = uploadMs / frames;
let presented = 0;
const previewMs = timed((ticks) => {
  const played = JSON.parse(engine.renderAt(ticks));
  if (played.missingSources.length === 0) presented += 1;
});
const previewUploadMs = uploadMs / frames;
const previewSize = Array.from(engine.previewSize);
engine.free();
report({
  ok: exported === frames && presented === frames,
  size: [width, height], previewSize, frames,
  exportMsPerFrame: Number(exportMs.toFixed(1)), previewMsPerFrame: Number(previewMs.toFixed(1)),
  uploadMsPerFrame: { export: Number(exportUploadMs.toFixed(1)), preview: Number(previewUploadMs.toFixed(1)) },
});
`;
}

export function cpuRuntimeEffectRefusedWhenDrawn() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = WebEffectRegistry.createCpu();
const capability = JSON.parse(engine.capabilityReport());
const caught = (work) => {
  try {
    work();
    return null;
  } catch (error) {
    return { code: error.code, message: error.message };
  }
};
const registered = engine.registerCompiledEffect(await engine.compileEffect(JSON.stringify({
  id: "page.tint", version: 1, kind: "effect", wgslBody: "return sample_input(uv);",
})));
let malformed = null;
try {
  await engine.compileEffect(JSON.stringify({ id: "page.bad", version: 1, kind: "effect" }));
} catch (error) {
  malformed = { code: error.code, message: error.message };
}
const settings = { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 };
const clip = (composition) => ({
  id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  ...(composition ? { composition: { id: composition } } : {}),
});
const sources = [{ id: "s", locator: "fixture://s", kind: "image" }];
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "plain", name: "Plain", settings, sources,
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [clip(null)] }] },
}));
const unusedEffects = engine.projectRuntimeEffects();
const unused = caught(() => engine.beginExport(0n, 120000n, 0, 0, "h264"));
engine.endExport();
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "tint", name: "Tint", settings, sources,
  compositions: [{ id: "tinted", nodes: [
    { id: "clip", op: { type: "clip" } },
    { id: "effect", op: { type: "effect", input: "clip", effect: { id: "page.tint", version: 1 } } },
  ], output: "effect" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [clip("tinted")] }] },
}));
engine.uploadRgbaImage("s", 16, 16, new Uint8Array(16 * 16 * 4).fill(255));
const drawnEffects = engine.projectRuntimeEffects();
const drawn = caught(() => engine.beginExport(0n, 120000n, 0, 0, "h264"));
const still = caught(() => engine.beginExportAt(0n, "h264"));
const deviceLost = engine.deviceLost;
engine.free();
const named = (refusal) => refusal?.code === "EFFECT_ERROR" && /page\\.tint@1 is a runtime WGSL effect/.test(refusal.message) &&
  /renderer "gpu"/.test(refusal.message);
report({
  ok: registered === "page.tint@1" && malformed?.code === "EFFECT_ERROR" &&
    unusedEffects.length === 0 && unused === null &&
    drawnEffects.join() === "page.tint@1" && named(drawn) && named(still) &&
    capability.gpu.backend === "cpu" && capability.features["renderer.cpu"].available &&
    !capability.features["effects.runtime"].available && !deviceLost,
  registered, malformed, unusedEffects, unused, drawnEffects, drawn, still, backend: capability.gpu.backend,
});
`;
}

// `auto` refuses a software adapter and draws on the CPU, naming the adapter; `gpu` uses it.
export function rendererFallbackAdapter() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const { createWasmRenderWorkerHost } = await import("/packages/alpha2/dist/worker.js");
const { detectBrowserCapabilities } = await import("/packages/alpha2/dist/robustness.js");
const capabilities = await detectBrowserCapabilities();
const project = JSON.stringify({
  schema_version: 1, id: "f", name: "F",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [], timeline: { tracks: [] },
});
const make = async (renderer) => {
  try {
    const host = await createWasmRenderWorkerHost(WebEffectRegistry, project, { renderer })(new OffscreenCanvas(64, 48));
    const choice = host.rendererChoice;
    host.dispose();
    return choice;
  } catch (error) {
    return { error: String(error?.message ?? error), code: error?.code ?? null };
  }
};
const auto = await make("auto");
const gpu = await make("gpu");
const software = capabilities.webgpuFallbackAdapter === true ||
  (!capabilities.webgpu && capabilities.webgl2 && capabilities.renderer === "cpu");
const details = { auto, gpu, capabilities: { webgpu: capabilities.webgpu, webgpuFallbackAdapter: capabilities.webgpuFallbackAdapter,
  webgl2: capabilities.webgl2, webgl2Renderer: capabilities.webgl2Renderer, renderer: capabilities.renderer } };
if (!capabilities.webgpu && !capabilities.webgl2) skip("webgl2", "the browser offers no GPU adapter at all", details);
const ok = software
  ? auto.renderer === "cpu" && /software adapter .+, which renderer "auto" does not draw on/.test(auto.rendererReason ?? "") &&
    gpu.renderer === "gpu" && gpu.rendererReason === null
  : auto.renderer === "gpu" && gpu.renderer === "gpu" && auto.backend === gpu.backend;
report({ ok, software, ...details });
`;
}
