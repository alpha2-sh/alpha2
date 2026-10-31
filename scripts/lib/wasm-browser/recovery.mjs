// GPU-loss recovery in a real browser: an SDK host in a worker, driven by `Alpha2Player`, through two
// GPU process crashes. Chrome withdraws GPU access from a page after its second GPU reset, so the
// case runs with Chrome's 3D API blocking on (see `caseLaunch` in cases.mjs) and its page driver
// crashes the GPU process when the page asks.

/** How long the page waits for a crash to reach the host and for the player to recover from it. */
const RECOVERY_WAIT_MS = 20_000;

// The page driver of `gpu-reset-ladder`: crashes Chrome's GPU process each time the page sets its
// title to `crash-<n>`, and once the page has reported.
export async function crashGpuOnSignal(cdp, sessionId) {
  let crashes = 0;
  for (;;) {
    const { result } = await cdp.send(
      "Runtime.evaluate",
      { expression: "document.title", returnByValue: true },
      sessionId,
    );
    const title = result?.value ?? "";
    if (title.startsWith("%7B") || title.startsWith("%7b")) return;
    if (title === `crash-${crashes + 1}`) {
      crashes += 1;
      await cdp.send("Browser.crashGpuProcess");
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

// After a first GPU process crash the worker host recovers on the GPU and draws again; after the
// second, the player moves it to the CPU renderer on a fresh canvas (`replaceSurface`) and it draws
// again, with no further GPU attempt. Each draw is read back through an export frame.
export function gpuResetLadder() {
  return `
const { WorkerRenderBackend } = await import("/packages/alpha2/dist/worker.js");
const { Alpha2Player } = await import("/packages/alpha2/dist/player.js");
const { pageGpuLosses } = await import("/packages/alpha2/dist/gpu-recovery.js");
const { detectBrowserCapabilities } = await import("/packages/alpha2/dist/robustness.js");
const capabilities = await detectBrowserCapabilities();
if (!capabilities.webgpu && !capabilities.webgl2) skip("webgpu", "the browser offers no GPU to lose", { capabilities });
// A browser whose only adapter is a software one (SwiftShader) is a GPU here too: mode gpu draws on it.
const mode = capabilities.renderer === "gpu" ? "auto" : "gpu";
const origin = location.origin;
const source = \`
import init, { WebEffectRegistry } from "\${origin}/packages/alpha2/wasm/alpha2_wasm.js";
import { createWasmRenderWorkerHost, installRenderWorker } from "\${origin}/packages/alpha2/dist/worker.js";
await init();
installRenderWorker(self, createWasmRenderWorkerHost(WebEffectRegistry, \${JSON.stringify(JSON.stringify({
  schema_version: 1, id: "ladder", name: "Ladder",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{ id: "c", source_id: "video",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 } }] }] },
}))}, { renderer: \${JSON.stringify(mode)} }));
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
const backend = new WorkerRenderBackend(worker);
const canvas = () => {
  const element = document.createElement("canvas");
  element.width = 64;
  element.height = 48;
  document.body.replaceChildren(element);
  return element.transferControlToOffscreen();
};
const frame = () => {
  const data = new Uint8Array(64 * 48 * 3 / 2).fill(128);
  data.fill(200, 0, 64 * 48);
  return new VideoFrame(data, { format: "I420", codedWidth: 64, codedHeight: 48, timestamp: 0,
    colorSpace: { primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false } });
};
const changes = [];
let surfaces = 0;
const player = new Alpha2Player(backend, {
  onRefill: async (requests) => {
    for (const request of requests)
      await player.uploadVideoFrame({ sourceId: request.sourceId, requestedTimeTicks: request.mediaTimeTicks,
        decodedTimeTicks: request.mediaTimeTicks, generation: request.generation, frame: frame() });
  },
  replaceSurface: async () => { surfaces += 1; return canvas(); },
  onRendererChange: (choice) => changes.push(choice),
});
await player.initialize(canvas());
const initial = backend.rendererChoice;
if (initial?.renderer !== "gpu") skip("webgpu", "the host draws on the CPU from the start", { initial, capabilities });
const workerCapabilities = await backend.detectCapabilities();
// What the export path reads back of the frame at 0: the luma the frame was drawn with.
const luma = async () => {
  try {
    const ticket = await backend.beginExport(0n, "h264", 0n);
    if (ticket.kind !== "rendered") return null;
    const exported = await ticket.readback;
    const copy = new Uint8Array(exported.allocationSize());
    const planes = await exported.copyTo(copy);
    exported.close();
    return copy[planes[0].offset + 24 * planes[0].stride + 32];
  } catch (error) {
    return String(error?.message ?? error);
  }
};
const presentOnce = async () => {
  try {
    const result = await player.present(0n);
    return { presented: result?.presentedTimeTicks === 0n };
  } catch (error) {
    return { error: String(error?.message ?? error), code: error?.code ?? null };
  }
};
// Presents until a recovery has happened and a frame presents after it, or the wait runs out.
const recoverAfter = async (recoveries) => {
  const deadline = performance.now() + ${RECOVERY_WAIT_MS};
  let last = null;
  while (performance.now() < deadline) {
    last = await presentOnce();
    if (last.code === "GPU_DEVICE_LOST") return { last, recovered: false };
    if (changes.length >= recoveries && last.presented) return { last, recovered: true };
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return { last, recovered: false };
};
const before = await presentOnce();
const lumaBefore = await luma();
// Headless SwiftShader loses its device presenting to a canvas, before any crash: the browser's.
if (lumaBefore !== 200 && backend.isDeviceLost())
  skip("webgpu-present", "the device was lost presenting to the canvas before any crash", { before, lumaBefore, initial });
document.title = "crash-1";
const first = await recoverAfter(1);
const lumaFirst = first.recovered ? await luma() : null;
const afterFirst = { choice: backend.rendererChoice, losses: pageGpuLosses.count(), surfaces };
document.title = "crash-2";
const second = await recoverAfter(2);
const lumaSecond = second.recovered ? await luma() : null;
const afterSecond = { choice: backend.rendererChoice, losses: pageGpuLosses.count(), surfaces };
// Later presentations draw on the CPU: no GPU is opened again.
for (let index = 0; index < 5; index += 1) await presentOnce();
const finalChoice = backend.rendererChoice;
const workerCapabilitiesAfter = await backend.detectCapabilities();
player.dispose();
report({
  // The worker's answer is what auto does there; only the gpu mode draws on a software adapter.
  ok: before.presented && lumaBefore === 200 &&
    (mode !== "auto" || workerCapabilities.renderer === initial.renderer) &&
    first.recovered && afterFirst.choice?.renderer === "gpu" && afterFirst.losses === 1 && lumaFirst === 200 &&
    second.recovered && afterSecond.choice?.renderer === "cpu" && afterSecond.losses === 2 && afterSecond.surfaces > afterFirst.surfaces &&
    lumaSecond === 200 && changes.length === 2 && finalChoice?.renderer === "cpu" && workerErrors.length === 0,
  mode, initial, workerCapabilities: workerCapabilities.renderer, lumaBefore,
  first: { ...first, luma: lumaFirst, ...afterFirst },
  second: { ...second, luma: lumaSecond, ...afterSecond },
  rendererChanges: changes.map((choice) => choice.renderer + "/" + choice.backend),
  workerCapabilitiesAfter: workerCapabilitiesAfter.renderer, workerErrors,
});
`;
}
