// CPU renderer cases: a browser with no WebGPU adapter and no WebGL2 context previews and exports
// on the engine's CPU renderer in wasm. `renderer` is the render host's option: "cpu" runs in any
// browser; "auto" draws on the CPU only where the browser has no GPU. With `--disable-gpu` alone,
// headless Chrome still offers SwiftShader WebGPU and WebGL2; turn that off as well:
//   ALPHA2_CHROME_GPU_FLAGS="--disable-gpu --disable-software-rasterizer" \
//     node scripts/test-wasm-browser.mjs --case renderer-auto

// A project of one decoded video (planar I420 frames, as WebCodecs decodes them), shown whole, then
// blurred, then scaled down and turned over itself.
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

/**
 * The render host as a page's worker runs it (in the page here): frames decoded as planar I420
 * `VideoFrame`s, previewed on an `OffscreenCanvas`, exported as `VideoFrame`s an H.264 (else VP9)
 * encoder takes, and decoded back. Reports the renderer the host chose and the time per frame.
 */
export function cpuWorkerHost(renderer, width, height) {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const { createWasmRenderWorkerHost } = await import("/packages/alpha2/dist/worker.js");
const { detectBrowserCapabilities } = await import("/packages/alpha2/dist/robustness.js");
const capabilities = await detectBrowserCapabilities();
const [width, height] = [${width}, ${height}];
const project = ${PROJECT(width, height)};
// The page's canvas, at the preview size the CPU renderer draws (half the project's).
const canvas = new OffscreenCanvas(width / 2, height / 2);
const host = await createWasmRenderWorkerHost(WebEffectRegistry, JSON.stringify(project), {
  renderer: ${JSON.stringify(renderer)},
})(canvas);
const choice = host.rendererChoice;
const expected = ${JSON.stringify(renderer)} === "auto" ? capabilities.renderer : ${JSON.stringify(renderer)};
if (choice?.renderer === "gpu") {
  // A GPU's preview and export are other cases'; headless Chrome loses a WebGPU device that
  // presents to a canvas.
  host.dispose();
  report({ ok: expected === "gpu" && choice.rendererReason === null, ...choice, capabilities });
} else {
// A planar I420 frame: a diagonal luma ramp that moves with the time, a bright square, and chroma
// that changes across the picture.
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
// Every frame of the one-second project at 30 fps.
const frameTicks = Array.from({ length: 30 }, (_, index) => BigInt(index * 4000));
// Time spent making and uploading source frames, which a page's decoder would spend: the times
// per frame below leave it out.
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
  // Each clip reads the one source at its own time; the host asks for what is missing.
  for (let attempt = 0; attempt < 4; attempt++) {
    const presented = await host.present(ticks);
    if (presented.missingFrames.length === 0) return presented;
    await supply(presented.missingFrames);
  }
  throw new Error("frame " + ticks + " never had its sources");
};
// Preview: every frame once with its sources uploaded, then again timed (the sources stay).
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
// What the canvas shows of the last frame, as limited-range luma at a grid of points, to compare
// with the export's.
const grid = Array.from({ length: 30 }, (_, index) => [(index % 6 + 0.5) / 6, (Math.floor(index / 6) + 0.5) / 5]);
let shown = null;
if (choice?.renderer === "cpu") {
  const pixels = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height).data;
  shown = grid.map(([u, v]) => {
    const at = (Math.floor(v * canvas.height) * canvas.width + Math.floor(u * canvas.width)) * 4;
    return 16 + (219 * (0.2126 * pixels[at] + 0.7152 * pixels[at + 1] + 0.0722 * pixels[at + 2])) / 255;
  });
}
// Export: every frame at full size, as VideoFrames an encoder takes.
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
  // An exporter supplies what a frame waits for, as the preview did: the uploaded-frame budget may
  // have let the preview's frames go.
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
    // The preview's points, from the export's luma (the 2x2 the preview pixel covers).
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
// The encoded stream decodes back to as many frames, the last one showing the square.
const decodedFrames = [];
const decoder = new VideoDecoder({
  output: async (frame) => { decodedFrames.push(frame); },
  error: (error) => { throw error; },
});
decoder.configure({ codec: encoding.codec, codedWidth: width, codedHeight: height });
for (const chunk of chunks) decoder.decode(chunk);
await decoder.flush();
decoder.close();
const last = decodedFrames.at(-1);
// A decoder hands back I420 or NV12, whose first plane is the luma.
const decodedFormat = last.format;
const lastCopy = new Uint8Array(last.allocationSize());
const lastPlanes = await last.copyTo(lastCopy);
const decodedSquare = /^(I4|NV12)/.test(decodedFormat)
  ? lastCopy[lastPlanes[0].offset + (height * 3 / 8) * lastPlanes[0].stride + width * 3 / 8]
  : null;
for (const frame of decodedFrames) frame.close();
const report_ = {
  renderer: choice?.renderer, rendererReason: choice?.rendererReason, capabilities: { webgpu: capabilities.webgpu, webgl2: capabilities.webgl2, renderer: capabilities.renderer },
  size: [width, height], codec: encoding.codec, frames: frameTicks.length, presentedFrames, encodedChunks: chunks.length,
  decodedFrames: decodedFrames.length, previewMsPerFrame: Number(previewMs.toFixed(1)), exportMsPerFrame: Number(exportMs.toFixed(1)),
  supplyMsPerFrame: { preview: Number(previewSupplyMs.toFixed(1)), export: Number(exportSupplyMs.toFixed(1)) },
  exportedSquare: exported[0], decodedSquare, decodedFormat,
  // How far the preview's luma is from the export's at the worst point.
  previewDifference: shown === null ? null : Math.round(Math.max(...shown.map((value, index) => Math.abs(value - exported[1][index])))),
};
host.dispose();
report({
  ok: report_.renderer === expected && (expected === "gpu") === (report_.rendererReason === null) &&
    presentedFrames === frameTicks.length && decodedFrames.length === frameTicks.length &&
    exported[0] > 200 && Math.abs(decodedSquare - exported[0]) <= 12 &&
    (shown === null || report_.previewDifference <= 24),
  ...report_,
});
}
`;
}

// The render worker as a page runs it: a page canvas handed to a module worker, which makes its
// renderer as `renderer` says and reports which one it made; a frame previews and exports through
// the worker. With no GPU in the browser, "auto" draws on the CPU there.
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
const expected = ${JSON.stringify(renderer)} === "auto" ? capabilities.renderer : ${JSON.stringify(renderer)};
if (choice?.renderer === "gpu") {
  // A GPU's worker is other cases'; headless Chrome loses a WebGPU device that presents.
  backend.dispose();
  report({ ok: expected === "gpu" && choice.rendererReason === null, ...choice, capabilities: capabilities.renderer });
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

// The CPU renderer's time per frame on the grid of the native CPU bench
// (crates/alpha2-render/benches/cpu_frame.rs, without its labels): four 1080p I420 sources, each
// new every frame and shown at half scale in one quadrant. Export packs 1080p I420; preview draws
// at half size onto a canvas. Uploading the planes is timed apart, as a page's decoder's time.
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

// The CPU renderer refuses runtime WGSL effects, by registration and in a project, with
// EFFECT_ERROR, and still draws a project of built-ins.
export function cpuRuntimeEffectRefused() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = WebEffectRegistry.createCpu();
const capability = JSON.parse(engine.capabilityReport());
let registration = null;
try {
  await engine.compileEffect(JSON.stringify({
    id: "page.tint", version: 1, kind: "effect", wgslBody: "return sample_input(uv);",
  }));
} catch (error) {
  registration = { code: error.code, message: error.message };
}
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "tint", name: "Tint",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "image" }],
  compositions: [{ id: "tinted", nodes: [
    { id: "clip", op: { type: "clip" } },
    { id: "effect", op: { type: "effect", input: "clip", effect: { id: "page.tint", version: 1 } } },
  ], output: "effect" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
    composition: { id: "tinted" },
  }] }] },
}));
engine.uploadRgbaImage("s", 16, 16, new Uint8Array(16 * 16 * 4).fill(255));
let rendering = null;
try {
  engine.beginExportAt(0n, "h264");
} catch (error) {
  rendering = { code: error.code, message: error.message };
}
const deviceLost = engine.deviceLost;
engine.free();
report({
  ok: registration?.code === "EFFECT_ERROR" && /only on a GPU/.test(registration.message) &&
    rendering?.code === "EFFECT_ERROR" && /page.tint/.test(rendering.message) &&
    capability.gpu.backend === "cpu" && capability.features["renderer.cpu"].available &&
    !capability.features["effects.runtime"].available && !deviceLost,
  registration, rendering, backend: capability.gpu.backend,
});
`;
}
