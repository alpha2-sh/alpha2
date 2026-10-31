// Browser export throughput on the machine's GPU: the packaged engine in a render worker exports a
// project whose video is a 1080p H.264 source, through the package's own decode, export and mux
// path, as an app does. Only correctness is judged (every frame exported); the times are recorded.
// Not in the default case list: it needs the release gate's perf inputs and minutes of a real GPU.
//   node scripts/release-gate/perf.mjs --make-source
//   node scripts/test-wasm-browser.mjs --case export-perf
// Page parameters: `?project=` and `?source=` (repository paths; by default the release gate's
// P2r project and decode source under target/release-gate/perf/) and `?seconds=` (default 10).

export function exportPerf() {
  return `
const parameters = new URL(location.href).searchParams;
const projectPath = parameters.get("project") ?? "/target/release-gate/perf/p2r.json";
const sourcePath = parameters.get("source") ?? "/target/release-gate/perf/source-1080p.mp4";
const seconds = Number(parameters.get("seconds") ?? 10);
const fetched = async (path) => {
  const response = await fetch(path);
  if (!response.ok) throw new Error(path + " answered " + response.status + "; run node scripts/release-gate/perf.mjs --make-source first");
  return response;
};
const project = await (await fetched(projectPath)).json();
const source = await (await fetched(sourcePath)).blob();
const unsupported = project.sources.filter((entry) => !["video", "generated", "composition"].includes(entry.kind));
if (unsupported.length > 0)
  throw new Error("the export-perf page supplies video sources only, not " + unsupported.map((entry) => entry.id + " (" + entry.kind + ")").join(", "));
const { WorkerRenderBackend, WorkerExportBackend } = await import("/packages/alpha2/dist/worker.js");
const { Alpha2Exporter, exportToSink } = await import("/packages/alpha2/dist/exporter.js");
const { canEncodeVideoTrack, MediabunnySink, MediabunnyVideoDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
const { decoderConsumerId } = await import("/packages/alpha2/dist/media.js");
const origin = location.origin;
const worker = new Worker(URL.createObjectURL(new Blob([\`
import init, { WebEffectRegistry } from "\${origin}/packages/alpha2/wasm/alpha2_wasm.js";
import { createWasmRenderWorkerHost, installRenderWorker } from "\${origin}/packages/alpha2/dist/worker.js";
await init();
installRenderWorker(self, createWasmRenderWorkerHost(WebEffectRegistry, \${JSON.stringify(JSON.stringify(project))}));
self.postMessage({ type: "test-ready" });
\`], { type: "text/javascript" })), { type: "module" });
// A message posted before the module has installed its loop would be lost.
await new Promise((resolve, reject) => {
  worker.addEventListener("message", function ready(event) {
    if (event.data?.type === "test-ready") { worker.removeEventListener("message", ready); resolve(); }
  });
  worker.addEventListener("error", (event) => reject(new Error(event.message)), { once: true });
});
const backend = new WorkerRenderBackend(worker);
const canvas = new OffscreenCanvas(project.settings.width / 2, project.settings.height / 2);
await backend.initialize(canvas);
const renderer = backend.rendererChoice;
// One decoder per video source, every one reading the same file.
const decoders = new Map();
for (const entry of project.sources.filter((entry) => entry.kind === "video"))
  decoders.set(entry.id, await MediabunnyVideoDecoder.open(entry.id, source));
// What a player's refill does: one decode per source and time, through the cursor of the clip that
// asked first, uploaded before the frame is asked for again.
const refill = async (waiting) => {
  const asked = new Set();
  for (const { clipId, readerId, sourceId, mediaTimeTicks } of waiting.missingFrames) {
    const key = sourceId + "\\u0000" + mediaTimeTicks;
    if (asked.has(key)) continue;
    asked.add(key);
    const decoder = decoders.get(sourceId);
    if (decoder === undefined) throw new Error("no decoder for source " + sourceId);
    const decoded = await decoder.refill({
      sourceId, mediaTimeTicks, reason: "prefetch", generation: 0, consumerId: decoderConsumerId(clipId, readerId),
    });
    await backend.uploadVideoFrame(decoded);
  }
};
const rate = { num: BigInt(project.settings.frame_rate.num), den: BigInt(project.settings.frame_rate.den) };
const exporter = new Alpha2Exporter(new WorkerExportBackend(backend, "h264"), rate, 0n, BigInt(seconds * 120000));
const expected = Number(exporter.progress.total);
// The native side's rate: 8 Mb/s.
const videoRateControl = { bitrate: 8_000_000, bitrateMode: "variable" };
const encoding = await canEncodeVideoTrack("avc", {
  width: project.settings.width, height: project.settings.height, rate: videoRateControl,
});
if (encoding === null) skip("webcodecs-video", "this browser has no H.264 encoder at the project's size");
const sink = new MediabunnySink({
  container: "mp4",
  videoCodec: "avc",
  videoRateControl,
  ...(encoding.codecString === undefined ? {} : { videoCodecString: encoding.codecString }),
  frameRate: project.settings.frame_rate,
  destination: { kind: "memory", maxBytes: 1 << 30 },
});
let result;
try {
  result = await exportToSink(exporter, sink, refill);
} finally {
  for (const decoder of decoders.values()) decoder.close();
  backend.dispose();
}
const adapter = await navigator.gpu?.requestAdapter();
const info = adapter?.info ?? null;
report({
  ok: result.frames === expected,
  renderer,
  frames: result.frames,
  expected,
  renderMs: Math.round(result.renderMs),
  fps: +result.fps.toFixed(2),
  performance: result.performance,
  adapter: info && { vendor: info.vendor, architecture: info.architecture, device: info.device, description: info.description },
  isFallbackAdapter: info === null ? null : Boolean(info.isFallbackAdapter ?? adapter.isFallbackAdapter),
  bytes: sink.output?.buffer?.byteLength ?? null,
});
`;
}
