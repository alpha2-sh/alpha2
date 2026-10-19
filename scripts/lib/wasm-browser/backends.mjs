// Backend cases: WebGPU and WebGL2 smoke runs, context and device loss, edges and layers.

// Safari 16.4 to 16.6 has OffscreenCanvas without WebGL: the premultiplication probe must
// measure on a page canvas instead.
export function webgl2ProbeWithoutOffscreenWebgl() {
  return `
Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
const getContext = OffscreenCanvas.prototype.getContext;
OffscreenCanvas.prototype.getContext = function (type, ...rest) {
  return type === "webgl" || type === "webgl2" ? null : getContext.call(this, type, ...rest);
};
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const canvas = document.createElement("canvas");
canvas.width = 16;
canvas.height = 16;
const engine = await WebEffectRegistry.createForHtmlCanvas(canvas);
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "probe", name: "Probe",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const pixels = new Uint8Array(16 * 16 * 4).fill(255);
let uploaded;
try {
  engine.uploadPresentedVideoFrame("s", 0n, new VideoFrame(pixels, { format: "RGBA", codedWidth: 16, codedHeight: 16, timestamp: 0 }), 16, 16, 0, false);
  uploaded = true;
} catch (error) { uploaded = String(error?.message ?? error); }
let luma = null;
if (uploaded === true) {
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  luma = rendered.kind === "rendered" ? rendered.pixels[rendered.layout[0].offset + 8 * rendered.layout[0].stride + 8] : rendered.kind;
}
engine.free();
report({ ok: uploaded === true && luma > 230, uploaded, luma });
`;
}

// Premultiply exactly once before filtering. `rejectVideoFrame` makes WebGPU refuse VideoFrame
// sources as Firefox does, so the engine must fall back to canvas bitmaps.
export function transparentEdges(webgl, rejectVideoFrame = false) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const copies = { videoFrame: 0, bitmap: 0 };
${
  rejectVideoFrame
    ? `const copyExternalImage = GPUQueue.prototype.copyExternalImageToTexture;
GPUQueue.prototype.copyExternalImageToTexture = function (source, ...rest) {
  if (source.source instanceof VideoFrame) {
    copies.videoFrame += 1;
    throw new TypeError("GPUQueue.copyExternalImageToTexture: 'source' member of GPUCopyExternalImageSourceInfo is not an object of one of ImageBitmap, HTMLImageElement, HTMLCanvasElement, OffscreenCanvas.");
  }
  if (source.source instanceof ImageBitmap) {
    copies.bitmap += 1;
    if (copies.failBitmap) throw new DOMException("the bitmap copy failed", "OperationError");
  }
  return copyExternalImage.call(this, source, ...rest);
};`
    : ""
}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
// Separate source IDs: the engine keeps the video upload across the reload.
const load = (id, kind) => engine.loadProject(JSON.stringify({
  schema_version: 1, id: "edges", name: "Edges",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id, locator: "fixture://" + id, kind }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "clip", source_id: id,
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const exported = async () => {
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  const { offset, stride } = rendered.layout[0];
  return Array.from({ length: 48 }, (_, row) =>
    Array.from(rendered.pixels.subarray(offset + row * stride, offset + row * stride + 64))).flat();
};
const frame = (texel) => {
  const pixels = new Uint8Array(16 * 12 * 4);
  for (let y = 0; y < 12; y++) for (let x = 0; x < 16; x++) pixels.set(texel(x, y), (y * 16 + x) * 4);
  return new VideoFrame(pixels, { format: "RGBA", codedWidth: 16, codedHeight: 12, timestamp: 0 });
};
const worstDifference = (a, b) => Math.max(...a.map((value, index) => Math.abs(value - b[index])));
load("video", "video");
const square = async (hidden) => {
  engine.uploadPresentedVideoFrame("video", 0n, frame((x, y) =>
    x >= 4 && x < 12 && y >= 3 && y < 9 ? [255, 255, 255, 255] : [hidden, hidden, hidden, 0]), 64, 48, 0, false);
  return exported();
};
const overBlack = await square(0);
const overWhite = await square(255);
const edges = overBlack.filter((value) => value > 20 && value < 230).length;
engine.uploadPresentedVideoFrame("video", 0n, frame(() => [255, 255, 255, 128]), 64, 48, 0, false);
const halfFrame = await exported();
load("still", "image");
engine.uploadRgbaImage("still", 64, 48, new Uint8Array(64 * 48 * 4).fill(128));
const halfImage = await exported();
const edgeWorst = worstDifference(overBlack, overWhite);
const alphaWorst = worstDifference(halfFrame, halfImage);
let fellBack = true;
if (${rejectVideoFrame}) {
  fellBack = copies.videoFrame === 1 && copies.bitmap === 3;
  // A copy the browser refuses outright is this frame's failure, not a panic: the frame closes,
  // nothing is kept, and the next upload works.
  copies.failBitmap = true;
  const kept = engine.uploadedFrameCount;
  const refused = frame(() => [0, 0, 0, 255]);
  let failure;
  try {
    engine.uploadPresentedVideoFrame("video", 1n, refused, 64, 48, 0, false);
  } catch (error) {
    failure = error;
  }
  copies.failBitmap = false;
  copies.failure = failure && { code: failure.code, fatal: failure.fatal };
  fellBack &&= failure?.code === "INVALID_INPUT" && failure.fatal === false &&
    refused.format === null && engine.uploadedFrameCount === kept;
  engine.uploadPresentedVideoFrame("video", 1n, frame(() => [0, 0, 0, 255]), 64, 48, 0, false);
  fellBack &&= engine.uploadedFrameCount === kept + 1 && copies.videoFrame === 1;
}
report({
  ok: edgeWorst <= 1 && edges > 0 && alphaWorst <= 2 && fellBack && !engine.deviceLost,
  edgeWorst, edges, alphaWorst, halfLuma: [halfFrame[0], halfImage[0]], copies,
});
`;
}

// The only case that reaches the composite's second layer and the bounded-layer region copy.
export function layers(webgl) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
const COLORS = { a: [40, 90, 200], b: [230, 60, 30], c: [60, 220, 90] };
const visual = (x, y, scale) => ({
  position: { mode: "constant", value: { x, y } },
  anchor: { x: 0.5, y: 0.5 },
  scale: { mode: "constant", value: { x: scale, y: scale } },
  rotation_degrees: { mode: "constant", value: 0 },
  opacity: { mode: "constant", value: 1 },
});
const track = (id, visual) => ({ id, name: id, kind: "video", clips: [{
  id: "clip-" + id, source_id: id, ...(visual ? { visual } : {}),
  timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
}] });
const load = (tracks) => {
  engine.loadProject(JSON.stringify({
    schema_version: 1, id: "layers", name: "Layers",
    settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
    sources: Object.keys(COLORS).map((id) => ({ id, locator: "fixture://" + id, kind: "image" })),
    timeline: { tracks },
  }));
  for (const [id, color] of Object.entries(COLORS)) {
    engine.uploadRgbaImage(id, 64, 48, new Uint8Array(64 * 48 * 4).map((_, index) => index % 4 === 3 ? 255 : color[index % 4]));
  }
};
const exported = async () => {
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  return (x, y) => rendered.layout.map(({ offset, stride }, plane) =>
    plane === 0 ? rendered.pixels[offset + y * stride + x] : rendered.pixels[offset + (y >> 1) * stride + (x >> 1)]);
};
// What each colour reads back as alone, full frame.
const alone = {};
for (const id of Object.keys(COLORS)) {
  load([track(id)]);
  alone[id] = (await exported())(32, 24);
}
// B and C each a sixteenth of the frame, apart: B draws as the second layer over both targets, C
// copies B's region across before drawing.
load([track("a"), track("b", visual(0.25, 0.25, 0.25)), track("c", visual(0.75, 0.7, 0.25))]);
const at = await exported();
const points = { "a corner": [60, 4, "a"], "a middle": [32, 24, "a"], "b": [16, 12, "b"], "c": [48, 34, "c"], "a beside b": [28, 12, "a"], "a below c": [48, 46, "a"] };
const worst = {};
for (const [name, [x, y, id]] of Object.entries(points)) {
  worst[name] = Math.max(...at(x, y).map((value, plane) => Math.abs(value - alone[id][plane])));
}
const distinct = Math.min(...[["a", "b"], ["a", "c"], ["b", "c"]].map(([p, q]) => Math.max(...alone[p].map((value, plane) => Math.abs(value - alone[q][plane])))));
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: Object.values(worst).every((value) => value <= 1) && distinct > 20 && uncaptured.length === 0 && !engine.deviceLost,
  worst, alone, distinct,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

// countOneBits passes naga but its GLSL bitCount is not in GLSL ES 3.00: registration must fail
// with the compiler's diagnostic, and a plain effect beside it still registers.
export function webgl2EffectBackendCompile() {
  return `
Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
const effect = (id, body) => JSON.stringify({ id, version: 1, kind: "effect", inputs: 1, params: [], wgslBody: body });
const attempt = async (definition) => {
  try { return { registered: engine.registerCompiledEffect(await engine.compileEffect(definition)) }; } catch (error) {
    return { code: error?.code ?? null, message: String(error?.message ?? error) };
  }
};
const bits = await attempt(effect("test.bits", "fn effect(uv: vec2f, src: vec4f) -> vec4f { let n = countOneBits(u32(uv.x * 255.0)); return vec4f(src.rgb, f32(n) / 8.0); }"));
const plain = await attempt(effect("test.plain", "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }"));
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: bits.code === "EFFECT_ERROR" && bits.registered === undefined && plain.registered === "test.plain@1" && uncaptured.length === 0,
  bits, plain,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

// Without EXT_color_buffer_float, uploads must not ask for RENDER_ATTACHMENT on RGBA16F or
// R16F/RG16F textures, which invalidates them.
export function webgl2WithoutFloatColorBuffers() {
  return `
Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
const hidden = new Set(["EXT_color_buffer_float", "EXT_color_buffer_half_float", "EXT_texture_norm16"]);
const supported = WebGL2RenderingContext.prototype.getSupportedExtensions;
WebGL2RenderingContext.prototype.getSupportedExtensions = function () { return (supported.call(this) ?? []).filter((name) => !hidden.has(name)); };
const extension = WebGL2RenderingContext.prototype.getExtension;
WebGL2RenderingContext.prototype.getExtension = function (name) { return hidden.has(name) ? null : extension.call(this, name); };
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
const load = (kind) => engine.loadProject(JSON.stringify({
  schema_version: 1, id: "uploads", name: "Uploads",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const luma = async () => { try {
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  return rendered.pixels[rendered.layout[0].offset + 8 * rendered.layout[0].stride + 8];
} catch (error) { return String(error?.message ?? error); } };
// Half floats for a mid grey (0.5 linear light is about 188 encoded, luma about 177 limited range).
const half = 0x3800, one = 0x3c00;
load("image");
engine.uploadPresentedLinearImage("s", 16, 16, new Uint16Array(16 * 16 * 4).map((_, index) => index % 4 === 3 ? one : half), 16, 16);
const linear = await luma();
const linearErrors = JSON.parse(engine.diagnostics()).gpu.events.filter((event) => event.event === "uncaptured-error").length;
// 10-bit limited-range mid grey, 4:2:0.
load("video");
const sdr = JSON.stringify({ primaries: "bt709", transfer: "bt709", matrix: "bt709", range: "limited", dynamic_range: "sdr" });
const planes = new Uint16Array(16 * 16 + 2 * 8 * 8);
planes.fill(502, 0, 256);
planes.fill(512, 256);
engine.uploadYuvFrame("s", 0n, "I420P10", 16, 16, new Uint8Array(planes.buffer),
  Uint32Array.from([0, 32, 512, 16, 640, 16]), Uint32Array.from([0, 0, 16, 16]),
  16, 16, sdr, "left", 16, 16, 0, false);
const ten = await luma();
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: Math.abs(linear - 177) <= 4 && Math.abs(ten - 126) <= 3 && uncaptured.length === 0 && !engine.deviceLost,
  linear, ten, linearErrors,
  ...(uncaptured.length ? { uncaptured: uncaptured.slice(0, 2).map((event) => event.message.slice(0, 300)) } : {}),
});
`;
}

// Firefox unconfigures an OffscreenCanvas's WebGPU context on transferToImageBitmap; the next
// frame must configure it again, not report a lost device.
export function webgpuUnconfiguredSurface() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const canvas = new OffscreenCanvas(64, 48);
const engine = await WebEffectRegistry.createForCanvas(canvas);
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
engine.loadProject(JSON.stringify(project));
const COLOR = [230, 120, 40];
const image = new Uint8Array(64 * 48 * 4);
for (let index = 0; index < image.length; index += 4) image.set([...COLOR, 255], index);
engine.uploadRgbaImage("image-source", 64, 48, image);
const probe = new OffscreenCanvas(64, 48).getContext("2d");
const frames = [];
for (const step of ["first", "after-transfer", "after-unconfigure"]) {
  if (step === "after-unconfigure") canvas.getContext("webgpu").unconfigure();
  const entry = { step };
  try {
    engine.renderAt(0n);
    const bitmap = canvas.transferToImageBitmap();
    probe.clearRect(0, 0, 64, 48);
    probe.drawImage(bitmap, 0, 0);
    bitmap.close();
    entry.pixel = [...probe.getImageData(32, 24, 1, 1).data];
  } catch (error) {
    entry.error = String(error?.message ?? error);
  }
  entry.deviceLost = engine.deviceLost;
  frames.push(entry);
}
// The pixel is reported, not checked: headless Chrome on SwiftShader hands over a blank bitmap.
report({ ok: frames.every((entry) => !entry.error && !entry.deviceLost), frames });
`;
}

// Headless Chrome and SwiftShader can lose WebGPU on shared-image presentation.
export function smoke(webgl) {
  return `
${webgl ? '// Hide WebGPU so the engine takes its WebGL2 backend.\nObject.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const canvas = new OffscreenCanvas(64, 48);
const engine = await WebEffectRegistry.createForCanvas(canvas);
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
engine.loadProject(JSON.stringify(project));
// One colour, so the presented canvas is told from a cleared one; at frame 0 the project's exposure
// is 0 and leaves it as it is.
const COLOR = [230, 120, 40];
const image = new Uint8Array(64 * 48 * 4);
for (let index = 0; index < image.length; index += 4) image.set([...COLOR, 255], index);
engine.uploadRgbaImage("image-source", 64, 48, image);
engine.beginExport(0n, 4000n, 30, 1, "h264");
const frame = await engine.exportFrame();
// The planes arrive padded, at the offsets and strides of \`layout\`. A
// VideoFrame rejects a layout that does not fit the bytes it is given.
let tight = 0;
if (frame.kind === "rendered") {
  const video = new VideoFrame(frame.pixels, {
    format: "I420", codedWidth: 64, codedHeight: 48, timestamp: 0, layout: frame.layout,
  });
  tight = video.allocationSize();
  video.close();
}
const audio = engine.renderAudio(0n, 1024, true);
engine.renderAt(0n);
// The canvas as presented, captured in the task that presented it: the next task may find its
// drawing buffer cleared.
let shot = null;
try {
  shot = new VideoFrame(canvas, { timestamp: 0 });
} catch (error) {
  shot = String(error?.message ?? error);
}
await new Promise((resolve) => setTimeout(resolve, 500));
let pixel = null;
if (shot instanceof VideoFrame) {
  const probe = new OffscreenCanvas(64, 48).getContext("2d");
  probe.drawImage(shot, 0, 0);
  shot.close();
  pixel = [...probe.getImageData(32, 24, 1, 1).data];
}
// Browser WebGPU validates asynchronously: renderAt returning is no proof that the presenter's
// shader, pipeline and bind group were valid. The engine logs what the device reports; any
// uncaptured error fails the case.
const diagnostics = JSON.parse(engine.diagnostics());
const gpu = diagnostics.gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
// WebGL2 is Tier B, whose passes take 16 filter samples; the preview and export budgets ask for
// more, and the renderer says it applied 16. WebGPU applies what they ask.
const degradations = diagnostics.degradations;
const filterSamplesOk = ${webgl}
  ? diagnostics.maxFilterSamples === 16 &&
    degradations.length === 1 &&
    degradations[0].kind === "filterSamplesReduced" &&
    degradations[0].applied === 16 &&
    degradations[0].requested > 16
  : diagnostics.maxFilterSamples > 16 && degradations.length === 0;
const info = ${webgl} ? {} : (await navigator.gpu.requestAdapter())?.info ?? {};
// Headless Chrome and SwiftShader destroy a WebGPU device that presents to a canvas: there the case
// cannot see a presentation, and only there may the device be lost.
const presentUnsupported =
  !${webgl} && (/HeadlessChrome/.test(navigator.userAgent) || /swiftshader/i.test([info.vendor, info.architecture, info.description].join(" ")));
const presentedColor = pixel !== null && pixel[3] === 255 && pixel.slice(0, 3).every((value, channel) => Math.abs(value - COLOR[channel]) <= 3);
const presented = presentUnsupported && engine.deviceLost
  ? "unavailable in this browser (device lost on present)"
  : presentedColor || { pixel, shot: typeof shot === "string" ? shot : undefined, deviceLost: engine.deviceLost };
report({
  ok: frame.kind === "rendered" && tight === (64 * 48 * 3) / 2 && frame.layout.length === 3 && audio.length === 2 * 1024 &&
    uncaptured.length === 0 && filterSamplesOk && (presented === true || (presentUnsupported && engine.deviceLost)),
  ...(filterSamplesOk ? {} : { maxFilterSamples: diagnostics.maxFilterSamples, degradations }),
  frame: frame.kind,
  audioSamples: audio.length,
  presented,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

// WebGL loss must flag device loss and reject readback; a new binding waits for a restore, and
// without one fails as GPU_DEVICE_LOST instead of trapping inside glow.
export function webglContextLoss() {
  return `
// Hide WebGPU so the engine takes its WebGL2 backend.
Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
const pageErrors = [];
addEventListener("error", (event) => pageErrors.push(String(event.message)));
addEventListener("unhandledrejection", (event) => pageErrors.push(String(event.reason?.message ?? event.reason)));
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const canvas = document.createElement("canvas");
canvas.width = 64;
canvas.height = 48;
document.body.append(canvas);
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
const prepare = (engine) => {
  engine.loadProject(JSON.stringify(project));
  engine.uploadRgbaImage("image-source", 64, 48, new Uint8Array(64 * 48 * 4).fill(255));
};
const exportFrame = async (engine) => {
  try {
    const ticket = engine.beginExportAt(0n, "h264");
    if (ticket.kind === "rendered") await ticket.readback;
    return ticket.kind;
  } catch (error) {
    return "rejected: " + String(error?.message ?? error);
  }
};
const settle = (promise, ms) => Promise.race([
  promise.then((value) => ({ value }), (error) => ({ error })),
  new Promise((resolve) => setTimeout(() => resolve({ unsettled: true }), ms)),
]);
const engine = await WebEffectRegistry.createForHtmlCanvas(canvas);
prepare(engine);
const before = engine.deviceLost;
const gl = canvas.getContext("webgl2");
const lose = gl?.getExtension("WEBGL_lose_context");
if (!lose) throw new Error("this browser offers no WEBGL_lose_context");
lose.loseContext();
await new Promise((resolve) => setTimeout(resolve, 50));
const after = engine.deviceLost;
const exported = await exportFrame(engine);
engine.free();
// The browser restores a context only when its loss was cancelled; restore it while the new binding
// waits.
setTimeout(() => lose.restoreContext(), 200);
const again = await settle(WebEffectRegistry.createForHtmlCanvas(canvas), 15000);
let recreated = again.unsettled ? "unsettled" : again.error ? "rejected: " + String(again.error?.message ?? again.error) : "resolved";
let reexported = null;
let lostAgain = null;
let unrestored = null;
if (again.value) {
  prepare(again.value);
  reexported = await exportFrame(again.value);
  lostAgain = again.value.deviceLost;
  // Lost again and never restored: the next binding gives up with a typed loss.
  lose.loseContext();
  await new Promise((resolve) => setTimeout(resolve, 50));
  again.value.free();
  const started = performance.now();
  const third = await settle(WebEffectRegistry.createForHtmlCanvas(canvas), 15000);
  unrestored = third.unsettled
    ? { settled: false }
    : { settled: true, code: third.error?.code ?? null, fatal: third.error?.fatal ?? null, resolved: third.value !== undefined, ms: Math.round(performance.now() - started) };
}
report({
  ok: before === false && after === true && exported.startsWith("rejected") &&
    recreated === "resolved" && reexported === "rendered" && lostAgain === false &&
    unrestored?.settled === true && unrestored.code === "GPU_DEVICE_LOST" && !unrestored.resolved &&
    pageErrors.length === 0,
  before,
  after,
  exported,
  recreated,
  reexported,
  lostAgain,
  unrestored,
  pageErrors,
});
`;
}

// A context lost inside the render worker is reported, and recover() rebinds once it is restored.
export function webglWorkerRecovery() {
  return `
const origin = location.origin;
const source = \`
Object.defineProperty(WorkerNavigator.prototype, "gpu", { get: () => undefined });
const { installRenderWorker, createWasmRenderWorkerHost } = await import("\${origin}/packages/alpha2/dist/worker.js");
const wasm = await import("\${origin}/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const project = await fetch("\${origin}/tests/cases/host-parity-project.json").then((response) => response.text());
let canvas;
let lose;
const create = createWasmRenderWorkerHost(wasm.WebEffectRegistry, project);
const scope = {
  onmessage: null,
  postMessage: (message, transfer) => self.postMessage(message, transfer ?? []),
};
self.onmessage = (event) => {
  if (event.data?.type === "test-lose") {
    lose = canvas.getContext("webgl2").getExtension("WEBGL_lose_context");
    lose.loseContext();
    setTimeout(() => lose.restoreContext(), 300);
    self.postMessage({ type: "test-lost" });
    return;
  }
  if (event.data?.type === "initialize") canvas = event.data.canvas;
  scope.onmessage(event);
};
installRenderWorker(scope, create);
self.postMessage({ type: "test-ready" });
\`;
const worker = new Worker(URL.createObjectURL(new Blob([source], { type: "text/javascript" })), { type: "module" });
const workerErrors = [];
worker.addEventListener("error", (event) => workerErrors.push(String(event.message)));
await new Promise((resolve, reject) => {
  worker.addEventListener("message", function ready(event) {
    if (event.data?.type === "test-ready") { worker.removeEventListener("message", ready); resolve(); }
  });
  worker.addEventListener("error", (event) => reject(new Error(event.message)), { once: true });
});
const { WorkerRenderBackend } = await import("/packages/alpha2/dist/worker.js");
const backend = new WorkerRenderBackend(worker);
const canvas = document.createElement("canvas");
canvas.width = 64;
canvas.height = 48;
await backend.initialize(canvas.transferControlToOffscreen());
const settle = (promise, ms) => Promise.race([
  promise.then((value) => ({ value }), (error) => ({ error: String(error?.message ?? error), code: error?.code ?? null })),
  new Promise((resolve) => setTimeout(() => resolve({ unsettled: true }), ms)),
]);
const first = await settle(backend.present(0n), 10000);
await new Promise((resolve) => {
  worker.addEventListener("message", function lost(event) {
    if (event.data?.type === "test-lost") { worker.removeEventListener("message", lost); resolve(); }
  });
  worker.postMessage({ type: "test-lose" });
});
await new Promise((resolve) => setTimeout(resolve, 50));
// The worker learns of the loss with the next answer.
const duringLoss = await settle(backend.present(0n), 10000);
const lostBeforeRecover = backend.isDeviceLost();
const recovered = await settle(backend.recover(), 15000);
const lostAfterRecover = backend.isDeviceLost();
const afterRecover = await settle(backend.present(0n), 10000);
backend.dispose();
report({
  ok: first.value !== undefined && !duringLoss.unsettled && lostBeforeRecover === true &&
    !recovered.unsettled && recovered.error === undefined && lostAfterRecover === false &&
    afterRecover.value !== undefined && workerErrors.length === 0,
  first: first.unsettled ? "unsettled" : first.error ?? "presented",
  duringLoss: duringLoss.unsettled ? "unsettled" : duringLoss.error ?? "presented",
  lostBeforeRecover,
  recovered: recovered.unsettled ? "unsettled" : recovered.error ?? "recovered",
  lostAfterRecover,
  afterRecover: afterRecover.unsettled ? "unsettled" : afterRecover.error ?? "presented",
  workerErrors,
});
`;
}

// Work the browser drops (as Metal does after GPU hangs) must surface as device loss, not a
// "map-failed" readback.
export function webgpuDroppedWork() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
engine.loadProject(JSON.stringify(project));
engine.uploadRgbaImage("image-source", 64, 48, new Uint8Array(64 * 48 * 4).fill(255));
const exportFrame = async () => {
  try {
    const ticket = engine.beginExportAt(0n, "h264");
    if (ticket.kind === "rendered") await ticket.readback;
    return { kind: ticket.kind };
  } catch (error) {
    return { rejected: String(error?.message ?? error), code: error?.code ?? null, failure: error?.failure ? { code: error.failure.code, stage: error.failure.stage } : null };
  }
};
const first = await exportFrame();
const submit = GPUQueue.prototype.submit;
GPUQueue.prototype.submit = function () {};
const dropped = await exportFrame();
GPUQueue.prototype.submit = submit;
const lost = engine.deviceLost;
const later = await exportFrame();
engine.free();
report({
  ok: first.kind === "rendered" && dropped.failure?.code === "DEVICE_LOST" && dropped.failure?.stage === "canary" &&
    lost === true && later.code === "GPU_DEVICE_LOST",
  first,
  dropped,
  lost,
  later,
});
`;
}

// A frame rejected for a validation error fails with that error, and the binding renders on.
export function webgpuRejectedFrame() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
const requestDevice = GPUAdapter.prototype.requestDevice;
let device = null;
GPUAdapter.prototype.requestDevice = async function (...args) {
  device = await requestDevice.apply(this, args);
  return device;
};
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
GPUAdapter.prototype.requestDevice = requestDevice;
if (device === null) throw new Error("the binding's device was not seen");
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
engine.loadProject(JSON.stringify(project));
engine.uploadRgbaImage("image-source", 64, 48, new Uint8Array(64 * 48 * 4).fill(255));
const exportFrame = async () => {
  try {
    const ticket = engine.beginExportAt(0n, "h264");
    if (ticket.kind === "rendered") await ticket.readback;
    return { kind: ticket.kind };
  } catch (error) {
    return { rejected: String(error?.message ?? error), code: error?.code ?? null, fatal: error?.fatal ?? null, failure: error?.failure ? { code: error.failure.code, stage: error.failure.stage, resource: error.failure.resource } : null };
  }
};
const first = await exportFrame();
const spent = device.createCommandEncoder().finish();
device.queue.submit([spent]);
const submit = GPUQueue.prototype.submit;
GPUQueue.prototype.submit = function (buffers) {
  return submit.call(this, [spent, ...buffers]);
};
const rejected = await exportFrame();
GPUQueue.prototype.submit = submit;
const lost = engine.deviceLost;
const later = await exportFrame();
const cache = JSON.parse(engine.diagnostics());
engine.free();
report({
  ok: first.kind === "rendered" && rejected.failure?.code === "VALIDATION_FAILED" && rejected.failure?.stage === "rejected" &&
    rejected.fatal === false && lost === false && later.kind === "rendered",
  first,
  rejected,
  lost,
  later,
  gpu: cache.gpu,
});
`;
}
