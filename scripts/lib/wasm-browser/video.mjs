// Browser video frame cases: color contract, conversion, chroma siting, anamorphic planes, and
// the presentation cache.

// An anamorphic frame's planes fill the frame its display size says, as its VideoFrame would;
// square pixels would leave it half as wide.
export function yuvAnamorphic() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await gpuEngine(WebEffectRegistry);
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "anamorphic", name: "Anamorphic",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const sdr = JSON.stringify({ primaries: "bt709", transfer: "bt709", matrix: "bt709", range: "full", dynamic_range: "sdr" });
// 32x48 stored, shown 64x48: white, full range.
const planes = new Uint8Array(32 * 48 + 2 * 16 * 24);
planes.fill(255, 0, 32 * 48);
planes.fill(128, 32 * 48);
engine.uploadYuvFrame("s", 0n, "I420", 32, 48, planes,
  Uint32Array.from([0, 32, 32 * 48, 16, 32 * 48 + 16 * 24, 16]), Uint32Array.from([0, 0, 32, 48]),
  64, 48, sdr, "left", 64, 48, 0, false);
const ticket = engine.beginExportAt(0n, "h264");
const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
const { offset, stride } = rendered.layout[0];
const row = rendered.pixels.subarray(offset + 24 * stride, offset + 24 * stride + 64);
const white = Array.from(row).filter((value) => value > 200).length;
engine.free();
report({ ok: white === 64, white });
`;
}

// A converted frame must not survive a reload that changes the color pipeline or its source's
// declared color, or the preview would keep the old tone map while an export uses the new one.
export function presentationCacheColor() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
const pq = { primaries: "bt2020", transfer: "pq", matrix: "bt2020_ncl", range: "limited", dynamic_range: "hdr" };
const load = (color, sourceColor) => engine.loadProject(JSON.stringify({
  schema_version: 1, id: "color", name: "Color",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000, color },
  sources: [{ id: "s", locator: "fixture://s", kind: "video", ...(sourceColor ? { color_metadata: sourceColor } : {}) }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const luma = async () => { try {
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") return rendered.kind;
  return rendered.pixels[rendered.layout[0].offset + 8 * rendered.layout[0].stride + 8];
} catch (error) { return String(error?.message ?? error); } };
// 10-bit limited-range PQ at code 700: about 900 nits, near white under a 1000-nit peak and well
// below it under 4000.
const planes = new Uint16Array(16 * 16 + 2 * 8 * 8);
planes.fill(700, 0, 256);
planes.fill(512, 256);
const upload = () => engine.uploadYuvFrame("s", 0n, "I420P10", 16, 16, new Uint8Array(planes.buffer),
  Uint32Array.from([0, 32, 512, 16, 640, 16]), Uint32Array.from([0, 0, 16, 16]),
  16, 16, JSON.stringify(pq), "left", 16, 16, 0, false);
const peak = (nits) => ({ hdr: "tone_map", hdr_peak_nits: nits });
load(peak(1000));
upload();
const at1000 = await luma();
load(peak(1000));
const keptOnSameColor = engine.uploadedFrameCount;
const sameColor = await luma();
load(peak(4000));
const keptOnNewPeak = engine.uploadedFrameCount;
const staleAt4000 = await luma();
upload();
const at4000 = await luma();
load(peak(4000), { ...pq, transfer: "hlg" });
const keptOnNewDeclaration = engine.uploadedFrameCount;
load(peak(4000), { ...pq, transfer: "hlg" });
upload();
load({ hdr: "reject" }, { ...pq, transfer: "hlg" });
const keptOnReject = engine.uploadedFrameCount;
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
report({
  ok: typeof at1000 === "number" && sameColor === at1000 && typeof at4000 === "number" && at4000 < at1000
    && keptOnSameColor === 1 && keptOnNewPeak === 0 && staleAt4000 === "waiting"
    && keptOnNewDeclaration === 0 && keptOnReject === 0 && uncaptured.length === 0,
  at1000, sameColor, staleAt4000, at4000, keptOnSameColor, keptOnNewPeak, keptOnNewDeclaration, keptOnReject,
  ...(uncaptured.length ? { uncaptured: uncaptured.slice(0, 2).map((event) => event.message.slice(0, 300)) } : {}),
});
`;
}

// A same-size edit keeps the uploads and renders at once; a resize waits for the video frame to be
// uploaded again (the still and the audio are kept).
export function presentationCacheResize() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await gpuEngine(WebEffectRegistry);
const project = await fetch("/tests/cases/host-video-export-project.json").then(r => r.json());
project.sources.push({ id: "audio-extra", locator: "fixture://audio", kind: "audio" });
project.settings.width = 64;
project.settings.height = 64;
engine.loadProject(JSON.stringify(project));
const pixels = new Uint8Array(128 * 64 * 4).fill(255);
const upload = () => engine.uploadVideoFrame("video-source", 0n,
  new VideoFrame(pixels, { format: "RGBA", codedWidth: 128, codedHeight: 64, timestamp: 0 }));
upload();
engine.uploadRgbaImage("still-extra", 128, 64, pixels);
engine.uploadAudioBlock("audio-extra", 0n, 48000, 2, new Float32Array(2048));
const exported = async () => {
  engine.beginExport(0n, 4000n, 30, 1, "h264");
  const result = await engine.exportFrame();
  engine.endExport();
  return result;
};
const original = await exported();
project.name = "same-size edit";
engine.loadProject(JSON.stringify(project));
const sameSize = await exported();
project.settings.width = 128;
engine.loadProject(JSON.stringify(project));
const retained = engine.uploadedFrameCount;
const needsUpload = await exported();
upload();
const resized = await exported();
const luma = resized.kind === "rendered"
  ? Array.from({ length: 64 }, (_, row) => Array.from(resized.pixels.subarray(
      resized.layout[0].offset + row * resized.layout[0].stride,
      resized.layout[0].offset + row * resized.layout[0].stride + 128))).flat()
  : [];
const white = luma.filter(y => y > 220).length;
report({
  ok: original.kind === "rendered" && sameSize.kind === "rendered" &&
    needsUpload.kind === "waiting" && retained === 1 && engine.audioRetainedFrames() === 1024 && white === 8192,
  sameSize: sameSize.kind, resizedBeforeUpload: needsUpload.kind, retained, white,
});
`;
}

// SDR import/export preserves display-referred R'G'B' (YUV through the matrix alone). Check the
// browser's `copyExternalImageToTexture` conversion against decoded YUV.
export function videoColorContract() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const video = document.createElement("video");
video.muted = true;
video.src = "/tests/corpus/vp9-color.webm";
await new Promise((resolve, reject) => {
  video.onloadeddata = resolve;
  video.onerror = () => reject(new Error("the browser cannot decode the fixture"));
});
video.currentTime = 0;
await new Promise((resolve) => (video.readyState >= 2 ? resolve() : (video.oncanplay = resolve)));
const width = video.videoWidth;
const height = video.videoHeight;
const source = new VideoFrame(video, { timestamp: 0 });
const layout = { format: source.format, colorSpace: source.colorSpace.toJSON() };
if (source.format !== "I420" && source.format !== "NV12") {
  source.close();
  skip("yuv-video-frames", "the browser decodes this fixture to " + layout.format + ", not 8-bit 4:2:0");
}
const raw = new Uint8Array(source.allocationSize());
const planes = await source.copyTo(raw);
const plane = (index, w, h, step = 1, phase = 0) => {
  const out = new Uint8Array(w * h);
  for (let row = 0; row < h; row++)
    for (let column = 0; column < w; column++)
      out[row * w + column] = raw[planes[index].offset + row * planes[index].stride + column * step + phase];
  return out;
};
const expected = source.format === "I420"
  ? [plane(0, width, height), plane(1, width / 2, height / 2), plane(2, width / 2, height / 2)]
  : [plane(0, width, height), plane(1, width / 2, height / 2, 2, 0), plane(1, width / 2, height / 2, 2, 1)];
const canvas = document.createElement("canvas");
canvas.width = width;
canvas.height = height;
document.body.append(canvas);
const engine = await WebEffectRegistry.createForHtmlCanvas(canvas);
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "video-color", name: "Video color",
  settings: { width, height, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "clip", source_id: "video",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
// The decoder's own frame, as the product imports it. \`?source=cpu\` hands
// over the same planes and color space as a CPU frame instead, which gave the
// same result where both could be measured.
const decoderFrame = new URL(location.href).searchParams.get("source") !== "cpu";
const cpuFrame = decoderFrame ? source : new VideoFrame(raw, {
  format: source.format,
  codedWidth: source.codedWidth,
  codedHeight: source.codedHeight,
  timestamp: 0,
  layout: planes,
  colorSpace: source.colorSpace.toJSON(),
});
if (!decoderFrame) source.close();
const lostBefore = engine.deviceLost;
engine.uploadVideoFrame("video", 0n, cpuFrame);
// A loss before the import is not the browser's inability to import: it fails the case.
if (lostBefore) throw Object.assign(new Error("the device was lost before the upload"), { beforeImport: true });
const ticket = engine.beginExportAt(0n, "h264");
let rendered;
try {
  rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
} catch (error) {
  throw new Error(String(error?.message ?? error) + " gpu=" + JSON.stringify(JSON.parse(engine.diagnostics()).gpu));
}
if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + JSON.stringify(rendered));
// Tightly packed, through the export's own plane layout.
const exportedFrame = new VideoFrame(rendered.pixels, {
  format: "I420", codedWidth: width, codedHeight: height, timestamp: 0, layout: rendered.layout,
});
const exported = new Uint8Array(exportedFrame.allocationSize());
await exportedFrame.copyTo(exported);
exportedFrame.close();
const sizes = [width * height, width * height / 4, width * height / 4];
let offset = 0;
const worst = [], mean = [];
// Chroma is upsampled on the way in and averaged on the way out, which only
// matters where it changes: samples within 3 luma pixels of a block edge of
// the fixture (a gray ramp over flat colour blocks) are left out.
const nearEdge = (x, y) => Math.abs(y - 32) <= 3 || (y > 32 && [0, 64, 128, 192, 256].some((edge) => Math.abs(x - edge) <= 3));
for (let index = 0; index < 3; index++) {
  const planeWidth = index === 0 ? width : width / 2;
  const scale = index === 0 ? 1 : 2;
  let max = 0, sum = 0, count = 0;
  for (let i = 0; i < sizes[index]; i++) {
    const x = (i % planeWidth) * scale, y = Math.floor(i / planeWidth) * scale;
    if (nearEdge(x, y)) continue;
    const d = Math.abs(exported[offset + i] - expected[index][i]);
    max = Math.max(max, d); sum += d; count++;
  }
  worst.push(max); mean.push(+(sum / count).toFixed(3));
  offset += sizes[index];
}
const lumaBias = (() => { let s = 0; for (let i = 0; i < sizes[0]; i++) s += exported[i] - expected[0][i]; return +(s / sizes[0]).toFixed(3); })();
report({ ok: worst[0] <= 2 && worst[1] <= 3 && worst[2] <= 3, worst, mean, lumaBias, layout });
`;
}

// Left-sited SDR chroma (H.264, HEVC) must convert as a native host does: planes copied out and
// converted at the decoder's siting. The browser's own conversion sites chroma elsewhere, and is
// measured too to show the case tells the two apart.
export function sdrChromaSiting() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
const { FrameSupply } = await import("/packages/alpha2/dist/frame-supply.js");
await init();
const width = 64, height = 16;
const cw = width / 2, ch = height / 2;
const planes = new Uint8Array(width * height + 2 * cw * ch);
planes.fill(126, 0, width * height);
for (let y = 0; y < ch; y++) for (let x = 0; x < cw; x++) {
  const red = (x >> 2) % 2 === 0;
  planes[width * height + y * cw + x] = red ? 90 : 200;
  planes[width * height + cw * ch + y * cw + x] = red ? 220 : 70;
}
const layout = [{ offset: 0, stride: width }, { offset: width * height, stride: cw }, { offset: width * height + cw * ch, stride: cw }];
const colorSpace = { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false };
const project = JSON.stringify({
  schema_version: 1, id: "siting", name: "Siting",
  settings: { width, height, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
});
// The export's U and V samples on one chroma row, for a frame \`upload\` hands the engine.
const exported = async (upload) => {
  const canvas = document.createElement("canvas");
  canvas.width = width; canvas.height = height;
  document.body.append(canvas);
  const engine = await WebEffectRegistry.createForHtmlCanvas(canvas);
  engine.loadProject(project);
  await upload(engine);
  const ticket = engine.beginExportAt(0n, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  const out = [];
  for (const plane of rendered.layout.slice(1))
    for (let x = 0; x < cw; x++) out.push(rendered.pixels[plane.offset + 2 * plane.stride + x]);
  engine.free();
  return out;
};
const supplied = (videoConversion) => (engine) =>
  new FrameSupply(engine, videoConversion === undefined ? {} : { videoConversion }).supply(0n, {
    width, height,
    liveSourceIds: new Set(["s"]),
    getLayer: () => ({ id: "s", kind: "video" }),
    pixelsFor: async () => ({
      kind: "video-frame",
      frame: new VideoFrame(planes, { format: "I420", codedWidth: width, codedHeight: height, timestamp: 0, layout, colorSpace }),
      chromaSiting: "left",
    }),
    isCurrent: () => true,
  });
const sdr = JSON.stringify({ primaries: "bt709", transfer: "bt709", matrix: "bt709", range: "limited", dynamic_range: "sdr" });
const reference = await exported((engine) => engine.uploadYuvFrame("s", 0n, "I420", width, height, planes,
  Uint32Array.from(layout.flatMap((p) => [p.offset, p.stride])), Uint32Array.from([0, 0, width, height]),
  width, height, sdr, "left", width, height, 0, false));
const worst = (values) => Math.max(...values.map((value, index) => Math.abs(value - reference[index])));
const supply = worst(await exported(supplied()));
const browser = worst(await exported(supplied("browser")));
report({ ok: supply <= 1 && browser >= 8, supply, browser });
`;
}

// copyExternalImageToTexture into an unpremultiplied, srgb-tagged rgba8unorm texture must keep
// display-referred R'G'B' without a transfer-function change.
export function browserVideoConversion() {
  return `
const video = document.createElement("video");
video.muted = true;
video.src = "/tests/corpus/vp9-color.webm";
await new Promise((resolve, reject) => {
  video.onloadeddata = resolve;
  video.onerror = () => reject(new Error("the browser cannot decode the fixture"));
});
const frame = new VideoFrame(video, { timestamp: 0 });
const width = frame.displayWidth, height = frame.displayHeight;
if (frame.format !== "I420" && frame.format !== "NV12") {
  const format = frame.format;
  frame.close();
  skip("yuv-video-frames", "the browser decodes this fixture to " + format + ", not 8-bit 4:2:0");
}
const raw = new Uint8Array(frame.allocationSize());
const planes = await frame.copyTo(raw);
const sample = (index, x, y, step = 1, phase = 0) =>
  raw[planes[index].offset + y * planes[index].stride + x * step + phase];
const yuv = (x, y) => {
  const cx = x >> 1, cy = y >> 1;
  return frame.format === "I420"
    ? [sample(0, x, y), sample(1, cx, cy), sample(2, cx, cy)]
    : [sample(0, x, y), sample(1, cx, cy, 2, 0), sample(1, cx, cy, 2, 1)];
};
const adapter = await navigator.gpu?.requestAdapter();
if (!adapter) throw new Error("this Chrome exposes no WebGPU adapter");
const device = await adapter.requestDevice();
const texture = device.createTexture({
  size: [width, height], format: "rgba8unorm",
  usage: GPUTextureUsage.COPY_DST | GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC,
});
// A decoder-backed frame loses the device on some headless setups, so the
// copy can take a CPU frame built from the same planes and color space
// (?source=cpu); the conversion under test is the same either way.
const cpu = new URL(location.href).searchParams.get("source") === "cpu";
const copied = cpu
  ? new VideoFrame(raw, {
      format: frame.format, codedWidth: frame.codedWidth, codedHeight: frame.codedHeight,
      timestamp: 0, layout: planes, colorSpace: frame.colorSpace.toJSON(),
    })
  : frame;
device.queue.copyExternalImageToTexture(
  { source: copied, flipY: false },
  { texture, colorSpace: "srgb", premultipliedAlpha: false },
  [width, height],
);
const row = Math.ceil((width * 4) / 256) * 256;
const buffer = device.createBuffer({ size: row * height, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ });
const encoder = device.createCommandEncoder();
encoder.copyTextureToBuffer({ texture }, { buffer, bytesPerRow: row }, [width, height]);
device.queue.submit([encoder.finish()]);
await buffer.mapAsync(GPUMapMode.READ);
const rgba = new Uint8Array(buffer.getMappedRange());
// BT.709 limited range, as the fixture is tagged.
const matrix = ([y, u, v]) => {
  const Y = (y - 16) / 219, Cb = (u - 128) / 224, Cr = (v - 128) / 224;
  const kr = 0.2126, kb = 0.0722, kg = 1 - kr - kb;
  return [Y + 2 * (1 - kr) * Cr, Y - (2 * kb * (1 - kb) / kg) * Cb - (2 * kr * (1 - kr) / kg) * Cr, Y + 2 * (1 - kb) * Cb];
};
const clamp = (x) => Math.min(1, Math.max(0, x));
const bt709ToSrgb = (e) => {
  const linear = e < 0.081 ? e / 4.5 : Math.pow((e + 0.099) / 1.099, 1 / 0.45);
  return linear <= 0.0031308 ? 12.92 * linear : 1.055 * Math.pow(linear, 1 / 2.4) - 0.055;
};
const stats = () => ({ worst: 0, sum: 0, bias: 0 });
const contract = stats(), reencoded = stats();
let count = 0;
// The fixture is a gray ramp over flat colour blocks. Browsers upsample
// chroma in their own way, which only matters where chroma changes, so pixels
// within 3 of a block edge are left out; the rest are compared at chroma
// sample sites (even x and y).
const nearEdge = (x, y) => Math.abs(y - 32) <= 3 || (y > 32 && [0, 64, 128, 192, 256].some((edge) => Math.abs(x - edge) <= 3));
for (let y = 0; y < height; y += 2)
  for (let x = 0; x < width; x += 2) {
    if (nearEdge(x, y)) continue;
    const encoded = matrix(yuv(x, y)).map(clamp);
    for (let c = 0; c < 3; c++) {
      const actual = rgba[y * row + x * 4 + c];
      for (const [target, value] of [[contract, encoded[c]], [reencoded, bt709ToSrgb(encoded[c])]]) {
        const d = actual - value * 255;
        target.worst = Math.max(target.worst, Math.abs(d));
        target.sum += Math.abs(d);
        target.bias += d;
      }
    }
    count += 3;
  }
const summary = (s) => ({ worst: +s.worst.toFixed(2), mean: +(s.sum / count).toFixed(2), bias: +(s.bias / count).toFixed(2) });
frame.close();
report({
  ok: contract.worst <= 2 * 1.01,
  frame: { format: planes.length, colorSpace: frame.colorSpace?.toJSON?.() },
  contract: summary(contract),
  reencodedToSrgb: summary(reencoded),
  adapter: adapter.info?.description || adapter.info?.vendor || "",
});
`;
}

// A decoder the browser picks may fail on a stream it claims to decode (Chromium's VA-API decoders
// on Linux); the package's decoder must then decode again in software. Every decoder the page makes
// is wrapped so that the first one configured without asking for software fails its first decode
// with an EncodingError, as such a decoder does, whatever this browser's own decoders would do.
export function decoderSoftwareFallback() {
  return `
const Native = globalThis.VideoDecoder;
if (Native === undefined) skip("webcodecs-video", "this browser has no WebCodecs VideoDecoder");
let injected = 0;
const configured = [];
globalThis.VideoDecoder = class extends Native {
  #error;
  #failing = false;
  #reported = false;
  constructor(init) {
    super(init);
    this.#error = init.error;
  }
  configure(config) {
    configured.push(config.hardwareAcceleration ?? "no-preference");
    this.#failing = config.hardwareAcceleration !== "prefer-software" && injected === 0;
    if (this.#failing) injected += 1;
    return super.configure(config);
  }
  decode(chunk) {
    if (!this.#failing) return super.decode(chunk);
    if (!this.#reported) {
      this.#reported = true;
      setTimeout(() => this.#error(new DOMException("injected decoder failure", "EncodingError")), 0);
    }
  }
};
const { MediabunnyVideoDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
const bytes = new Uint8Array(await (await fetch("/tests/corpus/h264-long-gop.mp4")).arrayBuffer());
const decoder = await MediabunnyVideoDecoder.open("clip", bytes);
let decoded;
try {
  decoded = await decoder.refill({ sourceId: "clip", mediaTimeTicks: 0n, reason: "seek", generation: 0 });
} finally {
  globalThis.VideoDecoder = Native;
}
const frame = decoded.frame;
const size = [frame.displayWidth, frame.displayHeight];
frame.close();
const softwareFallback = decoder.softwareFallback ?? null;
decoder.close();
report({
  ok: injected === 1 && JSON.stringify(size) === "[128,96]" && /^EncodingError/.test(softwareFallback ?? ""),
  injected, configured, size, softwareFallback,
});
`;
}

// A phone's HDR video (HEVC Main 10, HLG) through what a user runs: the SDK (its import probe, then
// the render worker host fed by its decoder, as a page drives it) and the editor's import. Either
// the source renders a picture on every route, or the SDK and the editor both refuse it at import,
// typed and not retried. The controls render, unless this browser has no decoder for their codec.
export function hdrHevcSource() {
  return `
const T = 120000;
const FPS = 30;
const TICKS_PER_FRAME = T / FPS;
const FRAMES = 3;
const HLG = { primaries: "bt2020", transfer: "hlg", matrix: "bt2020_ncl", range: "limited", dynamic_range: "hdr" };
const FILES = [
  { path: "/tests/corpus/hevc-main10-hlg.mp4", hdr: true, subject: true },
  { path: "/tests/corpus/hdr-hlg.mp4", hdr: true },
  { path: "/tests/corpus/h264-padded.mp4", hdr: false },
  { path: "/tests/corpus/hevc-main10-padded.mp4", hdr: false },
];
const REFUSALS = new Set(["PLANAR_DECODE_UNAVAILABLE", "DECODE_UNAVAILABLE"]);
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const { probePlanarDecode, MediabunnyVideoDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
const { createWasmRenderWorkerHost } = await import("/packages/alpha2/dist/worker.js");
const { classifyBrowserFailure } = await import("/packages/alpha2/dist/export-failure.js");
const { isRetryableFailure } = await import("/packages/alpha2/dist/export-retry.js");
const { AssetLibrary } = await import("/packages/alpha2-editor/dist/browser/runtime/assets.js");
if (!(await navigator.gpu?.requestAdapter())) skip("webgpu", "no WebGPU adapter");
const STEP_MS = 30000;
const within = (what, work) => {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(what + " did not settle within " + STEP_MS + " ms")), STEP_MS);
  });
  return Promise.race([Promise.resolve().then(work), late]).finally(() => clearTimeout(timer));
};
const refusalOf = (error) => {
  const failure = classifyBrowserFailure(error, { phase: "decode" });
  return {
    name: error?.name ?? null,
    code: error?.code ?? null,
    causeCode: error?.cause?.code ?? null,
    message: String(error?.message ?? error).slice(0, 300),
    failure: failure.code,
    retryable: isRetryableFailure(failure),
  };
};
const lumaOf = (pixels, plane, width, height) => {
  const step = Math.max(1, Math.floor(Math.min(width, height) / 32));
  let sum = 0, squares = 0, count = 0;
  for (let y = 0; y < height; y += step)
    for (let x = 0; x < width; x += step) {
      const value = pixels[plane.offset + y * plane.stride + x];
      sum += value;
      squares += value * value;
      count += 1;
    }
  const mean = sum / count;
  return { mean: Math.round(mean), std: Math.round(Math.sqrt(Math.max(0, squares / count - mean * mean))) };
};
const projectFor = (id, size, color) => ({
  schema_version: 1, id: "hdr-hevc-source", name: "HDR HEVC source",
  settings: {
    width: size[0], height: size[1], frame_rate: { num: FPS, den: 1 }, sample_rate: 48000,
    color: { hdr: "tone_map", hdr_peak_nits: 1000 },
  },
  sources: [{ id, locator: "fixture://" + id, kind: "video", ...(color ? { color_metadata: color } : {}) }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "clip", source_id: id,
    timeline_range: { start: 0, end: FRAMES * TICKS_PER_FRAME },
    source_range: { start: 0, end: FRAMES * TICKS_PER_FRAME },
  }] }] },
});
// The SDK's decoder feeding the render worker host, as a page's export does: three frames, their
// luma read back.
const render = async (file, id, color) => {
  const decoder = await MediabunnyVideoDecoder.open(id, file);
  let host;
  try {
    const first = await decoder.refill({ sourceId: id, mediaTimeTicks: 0n, reason: "seek", generation: 0 });
    const size = [first.frame.displayWidth, first.frame.displayHeight];
    first.frame.close();
    host = await createWasmRenderWorkerHost(WebEffectRegistry, JSON.stringify(projectFor(id, size, color)))(
      new OffscreenCanvas(size[0], size[1]),
    );
    const supply = async (missingFrames) => {
      for (const missing of missingFrames) {
        const decoded = await decoder.refill({
          sourceId: missing.sourceId, mediaTimeTicks: missing.mediaTimeTicks, reason: "prefetch", generation: 0,
        });
        await host.uploadVideoFrame({
          sourceId: missing.sourceId,
          requestedTimeTicks: missing.mediaTimeTicks,
          decodedTimeTicks: decoded.decodedTimeTicks,
          generation: 0,
          frame: decoded.frame,
          ...(decoded.chromaSiting === undefined ? {} : { chromaSiting: decoded.chromaSiting }),
        });
      }
    };
    const luma = [];
    for (let index = 0; index < FRAMES; index += 1) {
      const ticks = BigInt(index * TICKS_PER_FRAME);
      let ticket = host.beginExport(ticks, "h264", ticks);
      for (let round = 0; ticket.kind === "waiting" && round < 4; round += 1) {
        await supply(ticket.missingFrames);
        ticket = host.beginExport(ticks, "h264", ticks);
      }
      if (ticket.kind !== "rendered") throw new Error("frame " + index + " still waits for its source");
      const frame = await ticket.readback;
      try {
        const copy = new Uint8Array(frame.allocationSize());
        const planes = await frame.copyTo(copy);
        luma.push(lumaOf(copy, planes[0], frame.codedWidth, frame.codedHeight));
      } finally {
        frame.close();
      }
    }
    return luma;
  } finally {
    host?.dispose();
    decoder.close();
  }
};
const results = [];
for (const entry of FILES) {
  const name = entry.path.split("/").at(-1);
  const bytes = await (await fetch(entry.path)).arrayBuffer();
  const file = new File([bytes], name, { type: "video/mp4" });
  const result = { name, subject: entry.subject === true };
  try {
    result.sdk = await within("sdk probe of " + name, () =>
      probePlanarDecode(file, { declared: entry.hdr ? HLG : null }),
    );
  } catch (error) {
    result.sdk = { refused: refusalOf(error) };
  }
  if (!result.sdk.refused) {
    try {
      result.luma = await within("render of " + name, () => render(file, "clip", entry.hdr ? HLG : null));
    } catch (error) {
      result.renderError = refusalOf(error);
    }
  }
  const library = new AssetLibrary();
  try {
    const asset = await within("editor import of " + name, () => library.importFile(file));
    result.editor = { imported: asset.kind };
  } catch (error) {
    result.editor = { refused: refusalOf(error) };
  } finally {
    library.dispose();
  }
  results.push(result);
}
// A picture: some contrast in every frame, not limited-range black or a flat grey.
const renders = (result) =>
  Array.isArray(result.luma) && result.luma.length === FRAMES && result.luma.every((l) => l.std >= 4);
const refusedTyped = (refusal) =>
  refusal !== undefined && refusal.retryable === false &&
  (REFUSALS.has(refusal.code) || REFUSALS.has(refusal.causeCode));
const verdict = (result) => {
  if (renders(result) && result.editor.imported) return "renders";
  if (refusedTyped(result.sdk.refused) && refusedTyped(result.editor.refused)) return "refused-at-import";
  // No decoder for the codec at all (HEVC in Chrome on Linux): the SDK's decoder refuses it when it
  // opens, typed, and the editor refuses the import. Chrome on an Iris Xe says it decodes 10-bit
  // HEVC and then refuses the configuration, which the SDK meets at its first frame.
  const undecodable = result.sdk.refused ?? result.renderError;
  const typed = undecodable?.code === "DECODE_UNAVAILABLE" && !undecodable.retryable;
  const refusedConfig = /^(NotSupportedError|OperationError)$/.test(undecodable?.name ?? "");
  if ((typed || refusedConfig) && result.editor.refused?.retryable === false) return "no-decoder";
  return "inconsistent";
};
for (const result of results) result.verdict = verdict(result);
const subject = results.find((result) => result.subject);
const controls = results.filter((result) => !result.subject);
report({
  ok:
    (subject.verdict === "renders" || subject.verdict === "refused-at-import") &&
    controls.every((result) => result.verdict === "renders" || result.verdict === "no-decoder"),
  subject: subject.verdict,
  results,
});
`;
}
