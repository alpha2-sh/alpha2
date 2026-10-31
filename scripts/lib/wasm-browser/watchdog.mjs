// GPU work sized to the watchdog: a heavy runtime effect over a 3840x2160 frame is cut into
// submissions under the watchdog's budget instead of one long draw.

// The heavy watchdog fixture over a 4K still, two frames on the machine's GPU. The first heavy
// frame is planned before the device's calibration has landed, at the rate assumed for the
// slowest GPUs, so it is cut on every GPU; later ones are cut where the GPU is slow. A software
// adapter would take minutes per frame, so the case runs on hardware only.
export function heavyEffectSplit() {
  return `
if (!navigator.gpu) skip("webgpu", "this browser offers no WebGPU");
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
const adapter = webgpuAdapter(engine);
const described = JSON.parse(engine.diagnostics()).adapter ?? {};
if (described.isFallbackAdapter || described.architecture === "swiftshader")
  skip("webgpu", "a software WebGPU adapter draws a 4K heavy frame in minutes", { adapter });
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
