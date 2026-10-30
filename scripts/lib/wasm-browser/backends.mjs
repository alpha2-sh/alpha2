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

// A bitmap upload renders as the frame upload does, and `uploadPresentedFrame` sends bitmaps only
// where WebGPU refused a frame. `rejectVideoFrame` refuses VideoFrame copies as Firefox does.
export function bitmapUploads(webgl, rejectVideoFrame = false) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
${
  rejectVideoFrame
    ? `const copyExternalImage = GPUQueue.prototype.copyExternalImageToTexture;
GPUQueue.prototype.copyExternalImageToTexture = function (source, ...rest) {
  if (source.source instanceof VideoFrame)
    throw new TypeError("GPUQueue.copyExternalImageToTexture: 'source' member of GPUCopyExternalImageSourceInfo is not an object of one of ImageBitmap, HTMLImageElement, HTMLCanvasElement, OffscreenCanvas.");
  return copyExternalImage.call(this, source, ...rest);
};`
    : ""
}
let draws = 0;
const drawImage = OffscreenCanvasRenderingContext2D.prototype.drawImage;
OffscreenCanvasRenderingContext2D.prototype.drawImage = function (...args) {
  draws += 1;
  return drawImage.apply(this, args);
};
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
const { uploadPresentedFrame } = await import("/packages/alpha2/dist/presented-frames.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "bitmaps", name: "Bitmaps",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "clip", source_id: "video",
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
const pictures = {
  edges: (x, y) => (x >= 4 && x < 12 && y >= 3 && y < 9 ? [255, 255, 255, 255] : [0, 0, 0, 0]),
  half: () => [255, 255, 255, 128],
  corner: (x, y) => (x < 4 && y < 3 ? [255, 255, 255, 255] : [40, 40, 40, 255]),
};
const before = engine.videoFrameCopy;
const worst = {};
for (const [name, texel] of Object.entries(pictures)) {
  const rotation = name === "corner" ? 90 : 0;
  engine.uploadPresentedVideoFrame("video", 0n, frame(texel), 64, 48, rotation, false);
  const sent = await exported();
  const bitmap = await createImageBitmap(frame(texel), { premultiplyAlpha: "none", resizeWidth: 16, resizeHeight: 12 });
  engine.uploadPresentedImageBitmap("video", 0n, bitmap, 64, 48, rotation, false);
  const drawn = await exported();
  worst[name] = Math.max(...sent.map((value, index) => Math.abs(value - drawn[index])));
}
const mode = engine.videoFrameCopy;
const drawsBefore = draws;
const handed = frame(pictures.half);
await uploadPresentedFrame(engine, "video", 0n, handed, 64, 48, 0, false);
const helperDraws = draws - drawsBefore;
// Firefox refuses every VideoFrame; Chrome and Safari take them.
const expected = ${webgl ? '["canvas"]' : rejectVideoFrame ? '["bitmap"]' : '["direct", "bitmap"]'};
report({
  ok: Object.values(worst).every((value) => value <= ${webgl ? 2 : 1}) && expected.includes(mode) &&
    before === ${webgl ? '"canvas"' : '"untried"'} && helperDraws === ${webgl ? 1 : 0} &&
    handed.format === null && !engine.deviceLost,
  worst, before, mode, helperDraws,
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
const effect = (id, body) => JSON.stringify({ id, version: 1, kind: "effect", params: [], wgslBody: body });
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

// Bloom as a group of three effect nodes, the last reading the blurred bright parts by name: the
// group draws what its nodes draw inline, and the glow reaches past the white half it comes from.
export function namedInputs(webgl) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
const float = (name, value, max) => ({ name, type: "float", default: value, min: 0, max, unit: "normalized" });
for (const definition of [
  { id: "test.bright-pass", version: 1, kind: "effect", params: [float("threshold", 0.7, 1)],
    wgslBody: "fn effect(_uv: vec2f, src: vec4f) -> vec4f { let luma = dot(src.rgb, vec3f(0.2126, 0.7152, 0.0722)); return select(vec4f(0.0), src, luma >= params.threshold); }" },
  { id: "test.add-glow", version: 1, kind: "effect", inputs: [{ name: "glow" }], params: [float("intensity", 1, 4)],
    wgslBody: "fn effect(uv: vec2f, src: vec4f) -> vec4f { return min(src + sample_glow(uv) * params.intensity, vec4f(1.0)); }" },
]) engine.registerCompiledEffect(await engine.compileEffect(JSON.stringify(definition)));
const constant = (value) => ({ mode: "constant", value: { type: "float", value } });
const passes = (source, radius, intensity) => [
  { id: "bright", op: { type: "effect", input: source, effect: { id: "test.bright-pass", version: 1 } } },
  { id: "blur", op: { type: "effect", input: "bright", effect: { id: "alpha2.effect.gaussian-blur", version: 1, params: { radius: constant(radius) } } } },
  { id: "combine", op: { type: "effect", input: source, inputs: { glow: "blur" }, effect: { id: "test.add-glow", version: 1, params: { intensity: constant(intensity) } } } },
];
const bloom = { id: "bloom", inputs: ["Source"],
  controls: [{ name: "radius", node: "blur", param: "radius" }, { name: "intensity", node: "combine", param: "intensity" }],
  nodes: [{ id: "source", op: { type: "group_input", index: 0 } }, ...passes("source", 0.012, 1)], output: "combine" };
const looks = {
  inline: (intensity) => [{ id: "look", nodes: [{ id: "clip", op: { type: "clip" } }, ...passes("clip", 0.25, intensity)], output: "combine" }],
  grouped: (intensity) => [bloom, { id: "look", nodes: [{ id: "clip", op: { type: "clip" } },
    { id: "glow", op: { type: "group", composition: "bloom", inputs: ["clip"], controls: { radius: constant(0.25), intensity: constant(intensity) } } }], output: "glow" }],
};
// White over the left half, transparent over the right.
const left = new Uint8Array(64 * 48 * 4).map((_, index) => ((index >> 2) % 64 < 32 ? 255 : 0));
const draw = async (compositions) => {
  engine.loadProject(JSON.stringify({
    schema_version: 1, id: "bloom", name: "Bloom",
    settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
    sources: [{ id: "left", locator: "fixture://left", kind: "image" }],
    compositions,
    timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{ id: "c", source_id: "left",
      timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 }, composition: { id: "look" } }] }] },
  }));
  engine.uploadRgbaImage("left", 64, 48, left);
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  const [{ offset, stride }] = rendered.layout;
  return { pixels: Array.from(rendered.pixels), luma: (x, y) => rendered.pixels[offset + y * stride + x] };
};
const inline = await draw(looks.inline(2));
const grouped = await draw(looks.grouped(2));
const plain = await draw(looks.grouped(0));
const differing = inline.pixels.filter((value, index) => value !== grouped.pixels[index]).length;
const glow = { right: grouped.luma(40, 24), plainRight: plain.luma(40, 24), left: grouped.luma(8, 24) };
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: differing === 0 && glow.right > glow.plainRight + 8 && glow.left > 200 && uncaptured.length === 0 && !engine.deviceLost,
  differing, glow,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

// A WebGL2 shader stage samples at most 16 textures: an effect's source pair, six LUTs and eight
// named inputs fill them and compile to GLSL on the real compiler; one more LUT is refused before
// the device is asked.
export function webgl2EffectTextureBudget() {
  return `
Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
const range = (count) => Array.from({ length: count }, (_, index) => index);
const definition = (id, luts) => JSON.stringify({
  id, version: 1, kind: "effect",
  inputs: range(8).map((index) => ({ name: "in" + index })),
  resources: range(luts).map((index) => ({ name: "lut" + index, kind: "lut3d" })),
  // Every texture read, so no compiler drops a sampler the count includes.
  wgslBody: "fn effect(uv: vec2f, src: vec4f) -> vec4f { var sum = src; "
    + range(8).map((index) => "sum += sample_in" + index + "(uv); ").join("")
    + range(luts).map((index) => "sum += vec4f(sample_lut" + index + "(src.rgb), 0.0); ").join("")
    + "return sum / 16.0; }",
});
const attempt = async (json) => {
  try { return { registered: engine.registerCompiledEffect(await engine.compileEffect(json)) }; } catch (error) {
    return { code: error?.code ?? null, message: String(error?.message ?? error) };
  }
};
const full = await attempt(definition("test.sixteen", 6));
const over = await attempt(definition("test.seventeen", 7));
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: full.registered === "test.sixteen@1" && over.code === "EFFECT_ERROR" && /17 textures/.test(over.message)
    && uncaptured.length === 0 && !engine.deviceLost,
  full, over,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

// Shapes through the vector pass: an animated shapes source whose fill rule leaves a hole and
// whose square moves with its source time, and a path mask, read back as export luma. The pass's
// shader compiles and draws on the backend, coverage and all.
export function shapes(webgl) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
const constant = (value) => ({ mode: "constant", value });
const white = { type: "solid", color: { red: 1, green: 1, blue: 1, alpha: 1 } };
const square = (x0, y0, x1, y1) => ({ closed: true, vertices: [
  { point: { x: x0, y: y0 } }, { point: { x: x1, y: y0 } }, { point: { x: x1, y: y1 } }, { point: { x: x0, y: y1 } } ] });
const ring = { subpaths: [square(0.0, 0.0, 0.5, 1.0), square(0.125, 0.25, 0.375, 0.75)] };
const shapes = { type: "shapes", items: [
  { type: "rectangle",
    center: { mode: "keyframes", keyframes: [
      { time: 0, value: { x: 0.625, y: 0.25 }, interpolation: { type: "linear" } },
      { time: 120000, value: { x: 0.875, y: 0.25 }, interpolation: { type: "hold" } } ] },
    size: constant({ x: 0.125, y: 0.25 }) },
  { type: "fill", paint: constant(white) },
  { type: "path", path: constant(ring) },
  { type: "fill", rule: "evenodd", paint: constant(white) },
] };
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "shapes", name: "Shapes",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [
    { id: "shapes", kind: "generated", generator: shapes },
    { id: "white", kind: "generated", generator: { type: "fill", paint: white } },
  ],
  timeline: { tracks: [
    { id: "masked", name: "Masked", kind: "video", clips: [{ id: "m", source_id: "white",
      timeline_range: { start: 0, end: 240000 }, source_range: { start: 0, end: 240000 },
      masks: [{ shape: { type: "path", path: constant({ subpaths: [square(0.5, 0.5, 1.0, 1.0)] }) },
        feather: constant(0.02) }] }] },
    { id: "shapes", name: "Shapes", kind: "video", clips: [{ id: "s", source_id: "shapes",
      timeline_range: { start: 0, end: 240000 }, source_range: { start: 0, end: 240000 } }] },
  ] },
}));
const draw = async (time) => {
  const ticket = engine.beginExportAt(BigInt(time), "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  const [{ offset, stride }] = rendered.layout;
  return (x, y) => rendered.pixels[offset + y * stride + x];
};
const first = await draw(0);
const later = await draw(120000);
const luma = {
  ring: first(4, 24), hole: first(16, 24), masked: first(56, 40), unmasked: first(56, 8),
  squareThen: first(40, 12), squareLater: later(40, 12), squareMoved: later(56, 12),
};
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: luma.ring > 200 && luma.hole < 40 && luma.masked > 200 && luma.unmasked < 40
    && luma.squareThen > 200 && luma.squareLater < 40 && luma.squareMoved > 200
    && uncaptured.length === 0 && !engine.deviceLost,
  luma,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

/**
 * A layout generator drawn end to end in the browser: boxes placed by flexbox, a group faded as
 * one, a clipped child, and text laid out with a registered font.
 */
export function layout(webgl) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(160, 90));
engine.addFont("Alpha2 Fixture", new Uint8Array(await (await fetch("/tests/fonts/Alpha2Fixture.ttf")).arrayBuffer()), 0);
const constant = (value) => ({ mode: "constant", value });
const white = constant({ type: "solid", color: { red: 1, green: 1, blue: 1, alpha: 1 } });
const length = constant;
const at = (left, top, side, extra = {}) => ({ position: "absolute",
  inset: { left: length(left), top: length(top) }, width: length(side), height: length(side), ...extra });
const root = { children: [
  // A white square: pixels 9..27 on both axes (0.1 and 0.3 of the 90 px shorter side).
  at(0.1, 0.1, 0.2, { background: white }),
  // Two white squares faded as one group, 45..72 and 58..85 across: where they overlap it is as
  // bright as where one lies alone.
  { position: "absolute", inset: { left: length(0.5), top: length(0.1) }, width: length(0.6), height: length(0.3),
    opacity: constant(0.5), children: [at(0, 0, 0.3, { background: white }), at(0.15, 0, 0.3, { background: white })] },
  // A box that clips a white child twice its size.
  { position: "absolute", inset: { left: length(0.1), top: length(0.5) }, width: length(0.3), height: length(0.3),
    overflow: "hidden", children: [{ width: length(0.6), height: length(0.6), flex_shrink: constant(0), background: white }] },
  // Text right of the clipped box, from (54, 49).
  { position: "absolute", inset: { left: length(0.6), top: length(0.55) },
    children: [{ text: constant("Hi"), font_size: constant(0.3) }] },
] };
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "layout", name: "Layout",
  settings: { width: 160, height: 90, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "layout", kind: "generated", generator: { type: "layout", root } }],
  timeline: { tracks: [{ id: "t", name: "T", kind: "video", clips: [{ id: "c", source_id: "layout",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 } }] }] },
}));
const ticket = engine.beginExportAt(0n, "h264");
const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
const [{ offset, stride }] = rendered.layout;
const y = (x, row) => rendered.pixels[offset + row * stride + x];
let text = 0;
for (let row = 45; row < 88; row++) for (let x = 54; x < 110; x++) text = Math.max(text, y(x, row));
const luma = {
  square: y(18, 18), outside: y(4, 4), faded: y(50, 18), overlap: y(65, 18),
  clipped: y(18, 54), pastClip: y(40, 54), text,
};
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: luma.square > 200 && luma.outside < 40 && luma.faded > 100 && luma.faded < 210
    && Math.abs(luma.overlap - luma.faded) <= 3 && luma.clipped > 200 && luma.pastClip < 40
    && luma.text > 200 && uncaptured.length === 0 && !engine.deviceLost,
  luma,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

/**
 * A grid layout drawn end to end in the browser, in the authoring forms: two `fr` columns and
 * rows with pixel gaps and padding, a cell spanning a row, hex colors, and text in the style it
 * inherits from the grid.
 */
export function layoutGrid(webgl) {
  return `
${webgl ? 'Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });' : 'if (!navigator.gpu) throw new Error("this browser offers no WebGPU");'}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(160, 90));
engine.addFont("Alpha2 Fixture", new Uint8Array(await (await fetch("/tests/fonts/Alpha2Fixture.ttf")).arrayBuffer()), 0);
const constant = (value) => ({ mode: "constant", value });
// Content 140 by 70 px: columns 10..75 and 85..150, rows 10..40 and 50..80.
const root = {
  display: "grid", grid_template_columns: "1fr 1fr", grid_template_rows: "repeat(2, 1fr)",
  gap: constant("10px"), padding: constant("10px"), font_size: constant("27px"), fill: "#ffffff",
  children: [
    { background: constant("#fff") },
    { text: constant("Hi"), justify_self: "center" },
    { grid_column: "1 / -1", background: constant("#ffffffff") },
  ],
};
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "grid", name: "Grid",
  settings: { width: 160, height: 90, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "grid", kind: "generated", generator: { type: "layout", root } }],
  timeline: { tracks: [{ id: "t", name: "T", kind: "video", clips: [{ id: "c", source_id: "grid",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 } }] }] },
}));
const ticket = engine.beginExportAt(0n, "h264");
const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
const [{ offset, stride }] = rendered.layout;
const y = (x, row) => rendered.pixels[offset + row * stride + x];
let text = 0;
for (let row = 10; row < 40; row++) for (let x = 85; x < 150; x++) text = Math.max(text, y(x, row));
const luma = {
  cell: y(40, 25), padding: y(5, 5), columnGap: y(80, 25), rowGap: y(40, 45),
  spanLeft: y(40, 65), spanRight: y(120, 65), text,
};
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: luma.cell > 200 && luma.padding < 40 && luma.columnGap < 40 && luma.rowGap < 40
    && luma.spanLeft > 200 && luma.spanRight > 200 && luma.text > 200
    && uncaptured.length === 0 && !engine.deviceLost,
  luma,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}
