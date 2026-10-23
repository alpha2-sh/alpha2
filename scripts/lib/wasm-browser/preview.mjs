// Cases on preview pacing: a frame equal to the one shown is neither drawn nor presented, and the
// preview scale sets the size frames render at.

const PRESENT_HELPERS = `
// The canvas as presented, captured in the task that presented it: the next task may find its
// drawing buffer cleared.
const capture = (canvas, x, y, width = 1, height = 1) => {
  const shot = new VideoFrame(canvas, { timestamp: 0 });
  const probe = new OffscreenCanvas(canvas.width, canvas.height).getContext("2d");
  probe.drawImage(shot, 0, 0);
  shot.close();
  return probe.getImageData(x, y, width, height).data;
};
// Headless Chrome and SwiftShader destroy a WebGPU device that presents to a canvas: there a case
// cannot see a presentation.
const presentUnsupported = async () => {
  const info = (await navigator.gpu.requestAdapter())?.info ?? {};
  return /HeadlessChrome/.test(navigator.userAgent) || /swiftshader/i.test([info.vendor, info.architecture, info.description].join(" "));
};
`;

// A still frame drawn again is reported unchanged and neither submits work nor takes a canvas
// texture, and a new upload is drawn. Whether a page canvas keeps showing it is the compositor's,
// which a headless browser cannot show: that is checked by eye in a headed one.
export function previewUnchangedFrame() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
${PRESENT_HELPERS}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
let acquired = 0;
const acquire = GPUCanvasContext.prototype.getCurrentTexture;
GPUCanvasContext.prototype.getCurrentTexture = function (...args) {
  acquired += 1;
  return acquire.apply(this, args);
};
const canvas = new OffscreenCanvas(8, 8);
const engine = await WebEffectRegistry.createForCanvas(canvas);
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "still", name: "Still",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "image" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const size = Array.from(engine.previewSize);
const upload = (value) =>
  engine.uploadRgbaImage("s", 16, 16, new Uint8Array(16 * 16 * 4).fill(value));
const submissions = () => JSON.parse(engine.diagnostics()).work.graphSubmissions;
const render = (ticks) => {
  const before = { submissions: submissions(), acquired };
  const played = JSON.parse(engine.renderAt(BigInt(ticks)));
  return {
    ticks, unchanged: played.unchanged, repeated: played.repeated,
    submissions: submissions() - before.submissions, acquired: acquired - before.acquired,
  };
};
const read = () => Array.from(capture(canvas, 4, 4));
const later = () => new Promise((resolve) => setTimeout(resolve, 20));
upload(200);
const first = render(0);
const drawn = read();
await later();
// The clip shows its one image at every time: a still.
const same = render(4000);
await later();
const next = render(8000);
upload(60);
const reuploaded = render(12000);
const redrawn = read();
const deviceLost = engine.deviceLost;
engine.free();
const unseen = (await presentUnsupported()) && deviceLost;
report({
  ok: JSON.stringify(size) === "[8,8]" &&
    !first.unchanged && first.submissions === 1 && first.acquired === 1 &&
    [same, next].every((step) => step.unchanged && !step.repeated && step.submissions === 0 && step.acquired === 0) &&
    !reuploaded.unchanged && reuploaded.submissions === 1 && reuploaded.acquired === 1 &&
    (unseen || (drawn[0] > redrawn[0] && redrawn[0] > 0 && !deviceLost)),
  size, first, same, next, reuploaded, drawn, redrawn, deviceLost,
  ...(unseen ? { presented: "unavailable in this browser (device lost on present)" } : {}),
});
`;
}

// At scale 1 a 1-pixel checker renders and presents at the document's size, every pixel its own;
// at the default half scale the surface is half the size.
export function previewFullResolution() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
${PRESENT_HELPERS}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const [width, height] = [1280, 720];
const canvas = new OffscreenCanvas(16, 16);
const engine = await WebEffectRegistry.createForCanvas(canvas);
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "checker", name: "Checker",
  settings: { width, height, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
const half = Array.from(engine.previewSize);
let refused = null;
try {
  engine.setPreviewScale(0);
} catch (error) {
  refused = error.code ?? String(error);
}
engine.setPreviewScale(1);
const full = Array.from(engine.previewSize);
canvas.width = full[0];
canvas.height = full[1];
engine.resizeSurface(full[0], full[1]);
const pixels = new Uint8Array(width * height * 4);
for (let y = 0; y < height; y++)
  for (let x = 0; x < width; x++) {
    const value = (x + y) % 2 ? 255 : 0;
    pixels.set([value, value, value, 255], (y * width + x) * 4);
  }
engine.uploadRgbaFrame("s", 0n, width, height, pixels);
const played = JSON.parse(engine.renderAt(0n));
const presented = [canvas.width, canvas.height];
const shown = capture(canvas, 0, 0, width, height);
let wrong = 0;
for (let y = 0; y < height; y++)
  for (let x = 0; x < width; x++) {
    const expected = (x + y) % 2 ? 255 : 0;
    if (Math.abs(shown[(y * width + x) * 4] - expected) > 1) wrong += 1;
  }
const deviceLost = engine.deviceLost;
engine.free();
// Headless Chrome on SwiftShader may also hand over a blank (transparent) picture.
const unseen = (await presentUnsupported()) && (deviceLost || shown.every((value) => value === 0));
report({
  ok: JSON.stringify(half) === "[640,360]" && JSON.stringify(full) === "[1280,720]" &&
    refused === "INVALID_INPUT" && played.missingSources.length === 0 &&
    JSON.stringify(presented) === "[1280,720]" && (unseen || (wrong === 0 && !deviceLost)),
  half, full, refused, presented, wrong, deviceLost,
  ...(unseen ? { shown: "unavailable in this browser (headless presentation)" } : {}),
});
`;
}
