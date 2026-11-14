const PRESENT_HELPERS = `
// Capture in the presenting task; the next task may find the drawing buffer cleared.
const capture = (canvas, x, y, width = 1, height = 1) => {
  const shot = new VideoFrame(canvas, { timestamp: 0 });
  const probe = new OffscreenCanvas(canvas.width, canvas.height).getContext("2d");
  probe.drawImage(shot, 0, 0);
  shot.close();
  return probe.getImageData(x, y, width, height).data;
};
`;

// An unchanged frame submits no work and takes no canvas texture. Whether the compositor keeps
// showing it can only be checked by eye in a headed browser.
export function previewUnchangedFrame() {
  return `
if (!navigator.gpu) skip("webgpu", "this browser offers no WebGPU");
${PRESENT_HELPERS}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
let acquired = 0;
const acquire = GPUCanvasContext.prototype.getCurrentTexture;
GPUCanvasContext.prototype.getCurrentTexture = function (...args) {
  acquired += 1;
  return acquire.apply(this, args);
};
const control = await presentationControl((surface) => capture(surface, 4, 4), 8, 8);
const canvas = new OffscreenCanvas(8, 8);
const engine = await WebEffectRegistry.createForCanvas(canvas);
const adapter = webgpuAdapter(engine);
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
const same = render(4000);
await later();
const next = render(8000);
upload(60);
const reuploaded = render(12000);
const redrawn = read();
const deviceLost = engine.deviceLost;
engine.free();
const paced = JSON.stringify(size) === "[8,8]" &&
  !first.unchanged && first.submissions === 1 && first.acquired === 1 &&
  [same, next].every((step) => step.unchanged && !step.repeated && step.submissions === 0 && step.acquired === 0) &&
  !reuploaded.unchanged && reuploaded.submissions === 1 && reuploaded.acquired === 1;
const details = { size, first, same, next, reuploaded, drawn, redrawn, deviceLost, adapter, control };
// A device lost on presenting is the browser's.
if (paced && deviceLost)
  skip("webgpu-present", "the device was lost presenting to the canvas", details);
report({ ok: paced && drawn[0] > redrawn[0] && redrawn[0] > 0 && !deviceLost, ...details });
`;
}

export function previewFullResolution() {
  return `
if (!navigator.gpu) skip("webgpu", "this browser offers no WebGPU");
${PRESENT_HELPERS}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const [width, height] = [1280, 720];
const control = await presentationControl((surface) => capture(surface, 8, 8), 16, 16);
const canvas = new OffscreenCanvas(16, 16);
const engine = await WebEffectRegistry.createForCanvas(canvas);
const adapter = webgpuAdapter(engine);
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
const sized = JSON.stringify(half) === "[640,360]" && JSON.stringify(full) === "[1280,720]" &&
  refused === "INVALID_INPUT" && played.missingSources.length === 0 &&
  JSON.stringify(presented) === "[1280,720]";
const details = { half, full, refused, presented, wrong, deviceLost, adapter, control };
// A device lost on presenting is the browser's, but a blank capture fails since the control worked.
if (sized && deviceLost)
  skip("webgpu-present", "the device was lost presenting to the canvas", details);
report({ ok: sized && wrong === 0 && !deviceLost, ...details });
`;
}

// A caller that presents on every animation frame without awaiting, over a 2160p source, keeps at
// most one presentation running and one waiting, and the picture keeps advancing.
export function previewEveryAnimationFrame() {
  return `
const { WorkerRenderBackend } = await import("/packages/alpha2/dist/worker.js");
const { Alpha2Player } = await import("/packages/alpha2/dist/player.js");
const [width, height] = [3840, 2160];
const origin = location.origin;
const source = \`
import init, { WebEffectRegistry } from "\${origin}/packages/alpha2/wasm/alpha2_wasm.js";
import { createWasmRenderWorkerHost, installRenderWorker } from "\${origin}/packages/alpha2/dist/worker.js";
await init();
installRenderWorker(self, createWasmRenderWorkerHost(WebEffectRegistry, \${JSON.stringify(JSON.stringify({
  schema_version: 1, id: "uhd", name: "UHD",
  settings: { width: 3840, height: 2160, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{ id: "c", source_id: "video",
    timeline_range: { start: 0, end: 1200000 }, source_range: { start: 0, end: 1200000 } }] }] },
}))}));
self.postMessage({ type: "test-ready" });
\`;
const worker = new Worker(URL.createObjectURL(new Blob([source], { type: "text/javascript" })), { type: "module" });
await new Promise((resolve, reject) => {
  worker.addEventListener("message", function ready(event) {
    if (event.data?.type === "test-ready") { worker.removeEventListener("message", ready); resolve(); }
  });
  worker.addEventListener("error", (event) => reject(new Error(event.message)), { once: true });
});
const backend = new WorkerRenderBackend(worker);
let inFlight = 0;
let mostInFlight = 0;
let posted = 0;
const drawn = [];
const present = backend.present.bind(backend);
backend.present = async (timeTicks) => {
  posted += 1;
  inFlight += 1;
  mostInFlight = Math.max(mostInFlight, inFlight);
  try {
    const result = await present(timeTicks);
    if (result.missingFrames.length === 0) drawn.push(result.presentedTimeTicks);
    return result;
  } finally {
    inFlight -= 1;
  }
};
const element = document.createElement("canvas");
element.width = 640;
element.height = 360;
document.body.replaceChildren(element);
const luma = new Uint8Array(width * height * 3 / 2);
const player = new Alpha2Player(backend, {
  onRefill: async (requests) => {
    for (const request of requests) {
      luma.fill(16 + Number((request.mediaTimeTicks / 4000n) % 200n), 0, width * height);
      luma.fill(128, width * height);
      await player.uploadVideoFrame({ sourceId: request.sourceId, requestedTimeTicks: request.mediaTimeTicks,
        decodedTimeTicks: request.mediaTimeTicks, generation: request.generation,
        frame: new VideoFrame(luma, { format: "I420", codedWidth: width, codedHeight: height, timestamp: 0,
          colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false } }) });
    }
  },
});
await player.initialize(element.transferControlToOffscreen());
const failures = [];
let requested = 0;
let mostOutstanding = 0;
let outstanding = 0;
const started = performance.now();
await new Promise((resolve) => {
  const tick = () => {
    if (performance.now() - started > 3000) return resolve();
    const time = BigInt(requested++) * 4000n;
    outstanding += 1;
    mostOutstanding = Math.max(mostOutstanding, outstanding);
    player.present(time).catch((error) => failures.push(String(error?.message ?? error)))
      .finally(() => (outstanding -= 1));
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
});
const last = await player.present(BigInt(requested) * 4000n);
player.dispose();
worker.terminate();
const advancing = drawn.every((time, index) => index === 0 || time > drawn[index - 1]);
report({
  ok: mostInFlight === 1 && failures.length === 0 && drawn.length >= 3 && advancing
    && last?.presentedTimeTicks === BigInt(requested) * 4000n,
  requested, posted, mostInFlight, mostOutstanding, drawnCount: drawn.length,
  firstDrawn: String(drawn[0]), lastDrawn: String(drawn.at(-1)), failures: failures.slice(0, 3),
});
`;
}
