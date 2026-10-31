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

// WebCodecsVideoDecoder answers each time of an H.264 stream with B-frames with the frame
// presented then, whatever order the browser's decoder puts frames out in (Safari 16.4 to 26.3
// put them out in decode order; the decode-order simulation does the same in Chrome).
export function webcodecsDecodeOrder() {
  return `
if (typeof VideoDecoder === "undefined") skip("webcodecs-video", "this browser has no WebCodecs VideoDecoder");
const { WebCodecsVideoDecoder } = await import("/packages/alpha2/dist/media.js");
const mb = await import("mediabunny");
const bytes = new Uint8Array(await (await fetch("/tests/corpus/h264-long-gop.mp4")).arrayBuffer());
const input = new mb.Input({ source: new mb.BufferSource(bytes), formats: mb.ALL_FORMATS });
const track = await input.getPrimaryVideoTrack();
const config = await track.getDecoderConfig();
const packets = new mb.EncodedPacketSink(track);
const times = [];
let decodeOrderDiffers = false;
let previous = -Infinity;
for await (const packet of packets.packets()) {
  if (packet.timestamp < previous) decodeOrderDiffers = true;
  previous = packet.timestamp;
  times.push(packet.timestamp);
}
times.sort((a, b) => a - b);
const decoder = new WebCodecsVideoDecoder("s", {
  config,
  chunksFrom: async function* (mediaTimeTicks, signal) {
    const seconds = Number(mediaTimeTicks) / 120000;
    const key = (await packets.getKeyPacket(seconds)) ?? (await packets.getFirstKeyPacket());
    for await (const packet of packets.packets(key)) {
      if (signal.aborted) return;
      yield packet.toEncodedVideoChunk();
    }
  },
});
// Every frame asked for in order, then a few backwards: the answer is the frame whose own time
// is the time asked for.
const wrong = [];
const asked = [...times.keys(), times.length - 2, times.length >> 1, 3];
for (const index of asked) {
  const ticks = BigInt(Math.round(times[index] * 120000));
  const decoded = await decoder.refill({ sourceId: "s", mediaTimeTicks: ticks, reason: "prefetch", generation: 1 });
  if (decoded.decodedTimeTicks !== ticks) wrong.push({ index, asked: Number(ticks), got: Number(decoded.decodedTimeTicks) });
  decoded.frame.close();
}
decoder.close();
input.dispose();
report({ ok: decodeOrderDiffers && wrong.length === 0, decodeOrderDiffers, frames: asked.length, wrong: wrong.slice(0, 5) });
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
const project = JSON.stringify({
  schema_version: 1, id: "video-color", name: "Video color",
  settings: { width, height, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "clip", source_id: "video",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
});
// The decoder's own frame, as the product imports it. \`?source=cpu\` hands
// over the same planes and color space as a CPU frame instead: its size is the
// visible rect's, which copyTo laid the planes out for.
const decoderFrame = new URL(location.href).searchParams.get("source") !== "cpu";
const frame = decoderFrame ? source : new VideoFrame(raw, {
  format: source.format,
  codedWidth: source.visibleRect.width,
  codedHeight: source.visibleRect.height,
  timestamp: 0,
  layout: planes,
  colorSpace: source.colorSpace.toJSON(),
});
const { FrameSupply } = await import("/packages/alpha2/dist/frame-supply.js");
// The export of \`frame\` handed over by \`upload\`, against the decoded planes. Chroma is
// upsampled on the way in and averaged on the way out, which only matters
// where it changes: a sample is flat when its 3x3 expected neighbourhood
// spans at most 2 codes, and only flat samples are held to the worst-code
// bound; the others (edges) are held to a mean bound. A chroma edge sample
// may be off by most of the step between two blocks; their mean measured 3.7
// to 4.0 codes in every browser and route, so 6 leaves room for rounding and
// fails a conversion that moves whole edges.
const EDGE_MEAN_BOUND = 6;
const measure = async (upload) => {
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  document.body.append(canvas);
  const engine = await WebEffectRegistry.createForHtmlCanvas(canvas);
  engine.loadProject(project);
  const lostBefore = engine.deviceLost;
  await upload(engine, frame.clone());
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
  engine.free();
  const sizes = [width * height, width * height / 4, width * height / 4];
  let offset = 0;
  const worst = [], mean = [], edgeMean = [];
  const samples = [];
  for (let index = 0; index < 3; index++) {
    const planeWidth = index === 0 ? width : width / 2;
    const planeHeight = sizes[index] / planeWidth;
    const want = expected[index];
    let max = 0, sum = 0, count = 0, edgeSum = 0, edgeCount = 0;
    for (let y = 0; y < planeHeight; y++)
      for (let x = 0; x < planeWidth; x++) {
        let low = 255, high = 0;
        for (let dy = -1; dy <= 1; dy++)
          for (let dx = -1; dx <= 1; dx++) {
            const value = want[Math.min(planeHeight - 1, Math.max(0, y + dy)) * planeWidth +
              Math.min(planeWidth - 1, Math.max(0, x + dx))];
            low = Math.min(low, value);
            high = Math.max(high, value);
          }
        const flat = high - low <= 2;
        const got = exported[offset + y * planeWidth + x];
        const d = Math.abs(got - want[y * planeWidth + x]);
        if (d > 0) samples.push({ plane: index, x, y, got, want: want[y * planeWidth + x], flat, d });
        if (flat) { max = Math.max(max, d); sum += d; count++; }
        else { edgeSum += d; edgeCount++; }
      }
    worst.push(max);
    mean.push(+(sum / Math.max(1, count)).toFixed(3));
    edgeMean.push(+(edgeSum / Math.max(1, edgeCount)).toFixed(3));
    offset += sizes[index];
  }
  // The 8 largest differences of flat samples and of edge samples, each with its plane position,
  // and where the flat samples over the bound lie.
  samples.sort((a, b) => b.d - a.d);
  const largest = (flat) => samples.filter((sample) => sample.flat === flat).slice(0, 8)
    .map(({ plane, x, y, got, want }) => ({ plane, x, y, got, want }));
  const over = samples.filter((sample) => sample.flat && sample.d > (sample.plane === 0 ? 2 : 3));
  const span = (key) => over.length === 0 ? null : [Math.min(...over.map((sample) => sample[key])), Math.max(...over.map((sample) => sample[key]))];
  const lumaBias = (() => { let s = 0; for (let i = 0; i < sizes[0]; i++) s += exported[i] - expected[0][i]; return +(s / sizes[0]).toFixed(3); })();
  return {
    ok: worst[0] <= 2 && worst[1] <= 3 && worst[2] <= 3 && edgeMean.every((value) => value <= EDGE_MEAN_BOUND),
    worst, mean, edgeMean, flatOver: { count: over.length, x: span("x"), y: span("y") },
    worstFlat: largest(true), worstEdge: largest(false), lumaBias,
  };
};
// The contract is the product's default route: the frame's planes copied out
// and converted by the engine (FrameSupply). The browser's own conversion of
// the frame (\`uploadVideoFrame\`) is measured as information: on some decoders'
// GPU frames it reads past the visible picture at its right edge.
const contract = await measure((engine, copy) => new FrameSupply(engine).supply(0n, {
  width, height,
  liveSourceIds: new Set(["video"]),
  getLayer: () => ({ id: "video", kind: "video" }),
  pixelsFor: async () => ({ kind: "video-frame", frame: copy }),
  isCurrent: () => true,
}));
let browserRoute;
try {
  const { ok: withinContract, ...measured } = await measure((engine, copy) => engine.uploadVideoFrame("video", 0n, copy));
  browserRoute = { withinContract, ...measured };
} catch (error) {
  browserRoute = { error: String(error?.message ?? error) };
}
frame.close();
if (!decoderFrame) source.close();
report({ ok: contract.ok, source: decoderFrame ? "decoder" : "cpu", ...contract, layout, browserRoute });
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
// `degraded`: a source this browser decodes only to frames without planes is previewed instead,
// through the browser's tone mapping (FrameSupply's present-degraded, the editor's default), with
// the binding naming it in degradedSources and refusing its export typed.
export function hdrHevcSource({ degraded = false } = {}) {
  return `
const DEGRADED = ${degraded};
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
const { DecoderRefillCoordinator } = await import("/packages/alpha2/dist/media.js");
const { Alpha2Exporter, exportToSink } = await import("/packages/alpha2/dist/exporter.js");
const { classifyBrowserFailure } = await import("/packages/alpha2/dist/export-failure.js");
const { isRetryableFailure } = await import("/packages/alpha2/dist/export-retry.js");
const { AssetLibrary } = await import("/packages/alpha2-editor/dist/browser/runtime/assets.js");
const { FrameSupply } = await import("/packages/alpha2/dist/frame-supply.js");
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
    softwareDecoder: error?.facts?.softwareDecoder ?? null,
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
// The SDK's playback route as a page's export runs it: its decoders behind a refill coordinator
// feeding the render worker host, through exportToSink, which asks for a frame again in software
// when the host sends it back. Three frames, their luma read back.
const render = async (file, id, color) => {
  const probe = await MediabunnyVideoDecoder.open(id, file);
  let size;
  try {
    const first = await probe.refill({ sourceId: id, mediaTimeTicks: 0n, reason: "seek", generation: 0 });
    size = [first.frame.displayWidth, first.frame.displayHeight];
    first.frame.close();
  } finally {
    probe.close();
  }
  const host = await createWasmRenderWorkerHost(WebEffectRegistry, JSON.stringify(projectFor(id, size, color)), { renderer: "gpu" })(
    new OffscreenCanvas(size[0], size[1]),
  );
  const coordinator = new DecoderRefillCoordinator(
    (sourceId, { decode }) =>
      MediabunnyVideoDecoder.open(sourceId, file, decode === "software" ? { hardwareAcceleration: "prefer-software" } : {}),
    (decoded) => host.uploadVideoFrame(decoded),
  );
  try {
    const luma = [];
    const exporter = new Alpha2Exporter(
      {
        begin: async (ticks, outputTicks) => host.beginExport(ticks, "h264", outputTicks),
        isDeviceLost: () => host.isDeviceLost(),
        abort() {},
      },
      { num: BigInt(FPS), den: 1n },
      0n,
      BigInt(FRAMES * TICKS_PER_FRAME),
    );
    await exportToSink(
      exporter,
      {
        async writeVideo({ frame }) {
          try {
            const copy = new Uint8Array(frame.allocationSize());
            const planes = await frame.copyTo(copy);
            luma.push(lumaOf(copy, planes[0], frame.codedWidth, frame.codedHeight));
          } finally {
            frame.close();
          }
        },
        async finish() {},
        abort() {},
      },
      (waiting) =>
        Promise.all(
          waiting.missingFrames.map((missing) =>
            coordinator.refill({
              sourceId: missing.sourceId,
              mediaTimeTicks: missing.mediaTimeTicks,
              reason: "prefetch",
              generation: 0,
              consumerId: missing.clipId,
              ...(missing.decode === undefined ? {} : { decode: missing.decode }),
            }),
          ),
        ).then(() => {}),
    );
    return { luma, recovery: host.readbackRecovery };
  } finally {
    coordinator.close();
    host.dispose();
  }
};
// The preview of a source the browser hands over only degraded: three frames through a FrameSupply
// that presents them so, the canvas's luma read after each, then an export of the first frame,
// which the binding must refuse.
const previewDegraded = async (file, id, color) => {
  const probe = await MediabunnyVideoDecoder.open(id, file);
  let size;
  try {
    const first = await probe.refill({ sourceId: id, mediaTimeTicks: 0n, reason: "seek", generation: 0 });
    size = [first.frame.displayWidth, first.frame.displayHeight];
    first.frame.close();
  } finally {
    probe.close();
  }
  const canvas = new OffscreenCanvas(size[0], size[1]);
  const engine = await WebEffectRegistry.createForCanvas(canvas);
  engine.loadProject(JSON.stringify(projectFor(id, size, color)));
  engine.setPreviewScale?.(1);
  const supply = new FrameSupply(engine, { opaqueEngineFrames: "present-degraded" });
  const decoders = new Map();
  const decoderFor = (decode) => {
    let opened = decoders.get(decode);
    if (opened === undefined) {
      opened = MediabunnyVideoDecoder.open(id, file, decode === "software" ? { hardwareAcceleration: "prefer-software" } : {});
      decoders.set(decode, opened);
    }
    return opened;
  };
  const copy = new OffscreenCanvas(size[0], size[1]).getContext("2d");
  const frames = [];
  try {
    for (let index = 0; index < FRAMES; index += 1) {
      const ticks = BigInt(index * TICKS_PER_FRAME);
      const playback = await supply.render(ticks, {
        width: size[0], height: size[1],
        liveSourceIds: new Set([id]),
        getLayer: () => ({ id, kind: "video" }),
        pixelsFor: async (_layer, seconds, options) => {
          const decoder = await decoderFor(options.decode ?? "default");
          const decoded = await decoder.refill({
            sourceId: id, mediaTimeTicks: BigInt(Math.round(seconds * T)), reason: "prefetch", generation: 0,
          });
          return { kind: "video-frame", frame: decoded.frame, chromaSiting: decoded.chromaSiting };
        },
        isCurrent: () => true,
      });
      const bitmap = canvas.transferToImageBitmap();
      copy.drawImage(bitmap, 0, 0);
      bitmap.close();
      const rgba = copy.getImageData(0, 0, size[0], size[1]).data;
      const luma = new Uint8Array(size[0] * size[1]);
      for (let i = 0; i < luma.length; i += 1)
        luma[i] = Math.round(0.2126 * rgba[i * 4] + 0.7152 * rgba[i * 4 + 1] + 0.0722 * rgba[i * 4 + 2]);
      frames.push({
        degradedSources: playback?.degradedSources ?? null,
        luma: lumaOf(luma, { offset: 0, stride: size[0] }, size[0], size[1]),
      });
    }
    let exportRefusal = null;
    try {
      const ticket = engine.beginExportAt(0n, "h264");
      if (ticket.kind === "rendered") (await ticket.readback);
    } catch (error) {
      exportRefusal = refusalOf(error);
    }
    return { frames, exportRefusal, readback: supply.readbackRecovery };
  } finally {
    for (const opened of decoders.values()) void opened.then((decoder) => decoder.close(), () => {});
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
  // The playback route runs even after the import probe refused: it must refuse alike.
  try {
    const rendered = await within("render of " + name, () => render(file, "clip", entry.hdr ? HLG : null));
    result.luma = rendered.luma;
    result.recovery = rendered.recovery;
  } catch (error) {
    result.renderError = refusalOf(error);
  }
  // Decodes, but only to frames without planes: what a degraded preview can show.
  const opaqueOnly = result.sdk.refused?.code === "PLANAR_DECODE_UNAVAILABLE" && result.sdk.refused.softwareDecoder !== null;
  if (DEGRADED && opaqueOnly) {
    try {
      result.degraded = await within("degraded preview of " + name, () => previewDegraded(file, "clip", entry.hdr ? HLG : null));
    } catch (error) {
      result.degradedError = refusalOf(error);
    }
  }
  const library = new AssetLibrary(DEGRADED ? { hdrWithoutPlanes: "preview-degraded" } : {});
  try {
    const asset = await within("editor import of " + name, () => library.importFile(file));
    result.editor = { imported: asset.kind, planarDecode: asset.planarDecode ?? null };
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
const previewsDegraded = (result) =>
  result.degraded !== undefined
  && result.degraded.frames.length === FRAMES
  && result.degraded.frames.every((frame) => frame.luma.std >= 4 && frame.degradedSources?.includes("clip"))
  && result.degraded.exportRefusal?.code === "PLANAR_DECODE_UNAVAILABLE"
  && result.degraded.exportRefusal.retryable === false
  && result.editor.planarDecode === "unavailable";
const verdict = (result) => {
  if (renders(result) && !result.sdk.refused && result.editor.imported) return "renders";
  if (DEGRADED && previewsDegraded(result) && refusedTyped(result.sdk.refused) && refusedTyped(result.renderError))
    return "degraded-preview";
  // Refused at import by the SDK and the editor, and by the SDK's playback route at its first frame.
  // With a degraded preview offered, only a track the browser cannot decode at all is refused.
  if (refusedTyped(result.sdk.refused) && refusedTyped(result.editor.refused) && refusedTyped(result.renderError)
    && !(DEGRADED && result.sdk.refused.softwareDecoder !== null))
    return "refused-at-import";
  // No decoder for the codec at all (HEVC in Chrome on Linux): the SDK's decoder refuses it when it
  // opens, typed, and the editor refuses the import. A browser that says it decodes 10-bit HEVC and
  // then refuses the configuration is met by the SDK at its first frame, refused just as typed.
  const undecodable = result.sdk.refused ?? result.renderError;
  const typed = undecodable?.code === "DECODE_UNAVAILABLE" && !undecodable.retryable;
  if (typed && refusedTyped(result.renderError) && result.editor.refused?.retryable === false) return "no-decoder";
  return "inconsistent";
};
for (const result of results) result.verdict = verdict(result);
const subject = results.find((result) => result.subject);
const controls = results.filter((result) => !result.subject);
report({
  ok:
    ["renders", "refused-at-import", "degraded-preview"].includes(subject.verdict) &&
    controls.every((result) => result.verdict === "renders" || result.verdict === "no-decoder"),
  subject: subject.verdict,
  results,
});
`;
}
