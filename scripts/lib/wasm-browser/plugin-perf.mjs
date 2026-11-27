// Main-thread long tasks while bridge.extract runs alpha2.silence over a generated WAV of
// `?seconds=` (600 by default), in the extraction worker or with `?worker=0` on the page; only the
// run's success is judged.
//   node scripts/test-wasm-browser.mjs --case plugin-silence-long-tasks

export function pluginSilenceLongTasks() {
  return `
const { createEditor } = await import("/packages/alpha2-editor/dist/index.js");
const { createBrowserBridge } = await import("/packages/alpha2-editor/dist/browser/index.js");
const parameters = new URL(location.href).searchParams;
const seconds = Number(parameters.get("seconds") ?? 600);
const inWorker = parameters.get("worker") !== "0";
const rate = 16000;
const frames = seconds * rate;
const wav = new DataView(new ArrayBuffer(44 + frames * 2));
const text = (at, value) => [...value].forEach((char, index) => wav.setUint8(at + index, char.charCodeAt(0)));
text(0, "RIFF"); wav.setUint32(4, 36 + frames * 2, true); text(8, "WAVE"); text(12, "fmt ");
wav.setUint32(16, 16, true); wav.setUint16(20, 1, true); wav.setUint16(22, 1, true);
wav.setUint32(24, rate, true); wav.setUint32(28, rate * 2, true); wav.setUint16(32, 2, true);
wav.setUint16(34, 16, true); text(36, "data"); wav.setUint32(40, frames * 2, true);
let seed = 1;
for (let index = 0; index < frames; index++) {
  seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
  const voiced = Math.floor(index / rate) % 3 !== 2;
  const value = voiced ? Math.sin(index * 0.05) * 0.4 + ((seed >>> 16) / 65536 - 0.5) * 0.2 : 0;
  wav.setInt16(44 + index * 2, Math.round(value * 32767), true);
}
const manifest = "/plugins/target/browser-fixtures/first-party/silence/manifest.json";
const bytes = await (await fetch(manifest)).arrayBuffer();
const sha256 = [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
  .map((byte) => byte.toString(16).padStart(2, "0")).join("");
const editor = createEditor();
const bridge = createBrowserBridge({
  editor, audio: false, runtimeAssets: {
    wasmUrl: "/packages/alpha2/wasm/alpha2_wasm_bg.wasm",
    ...(inWorker ? { extractWorkerFactory: () => new Worker(
      "/worker-modules/packages/alpha2-editor/dist/browser/extract-worker-entry.js", { type: "module" }) } : {}),
  },
});
const { assetIds } = await bridge.importFiles([new File([wav.buffer], "long.wav", { type: "audio/wav" })]);
const tasks = [];
const observer = new PerformanceObserver((list) => tasks.push(...list.getEntries()));
observer.observe({ type: "longtask", buffered: false });
let loadedAt;
const started = performance.now();
const { result } = await bridge.extract({
  extractor: "alpha2.silence", version: 1, params: {},
  input: { from: "source", source_id: assetIds[0], range: { start: 0, end: seconds * 120000 } },
  outputs: { silence: "silence" },
}, {
  plugin: { manifest, sha256 },
  onProgress: (progress) => { if (progress.phase === "extract" && progress.stage === "pass") loadedAt ??= performance.now(); },
});
const ended = performance.now();
await new Promise((resolve) => setTimeout(resolve, 100));
observer.disconnect();
await bridge.dispose();
const durations = tasks.map((task) => task.duration);
const set = /** @type {Extract<import("/packages/alpha2/dist/project.js").DataSet, { kind: "intervals" }>} */ (result.dataSets[0]);
report({
  ok: set.intervals.length > 0,
  seconds, inWorker, loadMs: Math.round((loadedAt ?? ended) - started), extractMs: Math.round(ended - (loadedAt ?? started)),
  longTasks: durations.length, longTaskMs: Math.round(durations.reduce((sum, value) => sum + value, 0)),
  longestTaskMs: Math.round(Math.max(0, ...durations)), intervals: set.intervals.length,
});
`;
}
