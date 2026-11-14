// A heavy effect over a 4K frame must be split into submissions under the watchdog's budget. The
// first frame is planned before calibration at the slowest-GPU rate, so it is split everywhere.
// Hardware only: a software adapter takes minutes per frame.
export function heavyEffectSplit() {
  return `
if (!navigator.gpu) skip("webgpu", "this browser offers no WebGPU");
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
const adapter = webgpuAdapter(engine);
const described = JSON.parse(engine.diagnostics()).adapter ?? {};
if (described.isFallbackAdapter || described.architecture === "swiftshader")
  skip("hardware-webgpu", "a software WebGPU adapter draws a 4K heavy frame in minutes", { adapter });
const [heavy] = await fetch("/tests/cases/gpu-watchdog/heavy-effects.json").then((response) => response.json());
engine.registerCompiledEffect(await engine.compileEffect(JSON.stringify(heavy)));
const project = await fetch("/tests/cases/gpu-watchdog/heavy-3840x2160.json").then((response) => response.json());
project.sources = [{ id: "still", locator: "fixture://still", kind: "image" }];
project.timeline.tracks[0].clips[0].source_id = "still";
engine.loadProject(JSON.stringify(project));
const pixels = new Uint8Array(64 * 48 * 4).map((_, index) => (index % 4 === 3 ? 255 : (index * 37) & 0xff));
engine.uploadRgbaImage("still", 64, 48, pixels);
engine.takeGpuSubmissionStats();
const frames = [];
for (const timeTicks of [0n, 5000n]) {
  const started = performance.now();
  const ticket = engine.beginExportAt(timeTicks, "h264");
  const rendered = ticket.kind === "rendered" ? await ticket.readback : ticket;
  if (rendered.kind !== "rendered") throw new Error("the frame did not render: " + rendered.kind);
  const [{ offset }] = rendered.layout;
  frames.push({ ms: Math.round(performance.now() - started), luma: rendered.pixels[offset] });
}
const stats = JSON.parse(engine.takeGpuSubmissionStats());
const gpu = JSON.parse(engine.diagnostics()).gpu;
const uncaptured = gpu.events.filter((event) => event.event === "uncaptured-error");
const lost = engine.deviceLost;
engine.free();
report({
  ok: stats.frames >= 1 && stats.splitFrames > 0 && stats.maxPerFrame > 1 && uncaptured.length === 0 && !lost,
  adapter, stats, frames, lost,
  ...(uncaptured.length ? { uncaptured } : {}),
});
`;
}

// A frame cut into several submissions, one of them rejected and the last one run, fails with the
// rejection instead of delivering wrong pixels; the binding renders on.
export function splitFrameRejectedSubmission() {
  return `
if (!navigator.gpu) skip("webgpu", "this browser offers no WebGPU");
const requestDevice = GPUAdapter.prototype.requestDevice;
let device = null;
GPUAdapter.prototype.requestDevice = async function (...args) {
  device = await requestDevice.apply(this, args);
  return device;
};
const { default: init, WebEffectRegistry, setGpuWatchdogMs } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
GPUAdapter.prototype.requestDevice = requestDevice;
if (device === null) throw new Error("the binding's device was not seen");
const adapter = webgpuAdapter(engine);
const described = JSON.parse(engine.diagnostics()).adapter ?? {};
if (described.isFallbackAdapter || described.architecture === "swiftshader")
  skip("hardware-webgpu", "a software WebGPU adapter draws a 4K heavy frame in minutes", { adapter });
// Every planned pass becomes its own submission.
setGpuWatchdogMs(1);
const [heavy] = await fetch("/tests/cases/gpu-watchdog/heavy-effects.json").then((response) => response.json());
engine.registerCompiledEffect(await engine.compileEffect(JSON.stringify(heavy)));
const project = await fetch("/tests/cases/gpu-watchdog/heavy-3840x2160.json").then((response) => response.json());
project.sources = [{ id: "still", locator: "fixture://still", kind: "image" }];
project.timeline.tracks[0].clips[0].source_id = "still";
engine.loadProject(JSON.stringify(project));
engine.uploadRgbaImage("still", 64, 48, new Uint8Array(64 * 48 * 4).map((_, index) => (index % 4 === 3 ? 255 : (index * 37) & 0xff)));
const exportFrame = async () => {
  try {
    const ticket = engine.beginExportAt(0n, "h264");
    if (ticket.kind === "rendered") await ticket.readback;
    return { kind: ticket.kind };
  } catch (error) {
    return { rejected: String(error?.message ?? error), fatal: error?.fatal ?? null, failure: error?.failure ? { code: error.failure.code, stage: error.failure.stage } : null };
  }
};
const first = await exportFrame();
const spent = device.createCommandEncoder().finish();
device.queue.submit([spent]);
const submit = GPUQueue.prototype.submit;
let submissions = 0;
GPUQueue.prototype.submit = function (buffers) {
  submissions += 1;
  return submit.call(this, submissions === 1 ? [spent, ...buffers] : buffers);
};
const rejected = await exportFrame();
GPUQueue.prototype.submit = submit;
const lost = engine.deviceLost;
const later = await exportFrame();
engine.free();
report({
  ok: first.kind === "rendered" && submissions > 1 && rejected.failure?.code === "VALIDATION_FAILED" &&
    rejected.failure?.stage === "rejected" && rejected.fatal === false && lost === false && later.kind === "rendered",
  adapter, first, submissions, rejected, lost, later,
});
`;
}
