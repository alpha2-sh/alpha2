// Binding surface cases: error codes, effect registration, the feature probe, page-thread hashing,
// and the text rasterizer.

// Export refusals carry INVALID_INPUT and are not fatal; loading a project mid-export ends that
// export, whose next step reports INVALID_STATE.
export function engineErrorCodes() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
const refusal = (run) => {
  try { run(); return null; } catch (error) { return { code: error.code, fatal: error.fatal, message: String(error.message) }; }
};
// An odd 4:2:0 size is an INVALID_INPUT refusal, not a fatal error, as on the native host, so a
// caller does not retry the export.
engine.loadProject(JSON.stringify({ ...project, settings: { ...project.settings, width: 65, height: 49 } }));
const odd = refusal(() => engine.beginExport(0n, 8000n, 30, 1, "h264"));
// The stateless export the SDK worker uses refuses it the same way, before it renders, rather than
// failing its readback as a GPU error.
const oddAt = refusal(() => engine.beginExportAt(0n, "h264"));
// A frame limit has no keyword at all.
engine.loadProject(JSON.stringify(project));
const tooLong = refusal(() => engine.beginExport(0n, 120000n * 400000n, 30, 1, "h264"));
const empty = refusal(() => engine.beginExport(8000n, 8000n, 30, 1, "h264"));
// Loading a project ends a running export; its next step renders nothing of the new one.
engine.beginExport(0n, 8000n, 30, 1, "h264");
engine.loadProject(JSON.stringify({ ...project, settings: { ...project.settings, width: 32, height: 24 } }));
let reloaded = null;
try { await engine.exportFrame(); } catch (error) { reloaded = { code: error.code, message: String(error.message) }; }
engine.free();
const invalid = (refused) => refused?.code === "INVALID_INPUT" && refused.fatal === false;
report({
  ok: invalid(odd) && /64x48/.test(odd.message) && invalid(oddAt) && /64x48/.test(oddAt.message) && invalid(tooLong) && invalid(empty) && reloaded?.code === "INVALID_STATE",
  odd, oddAt, tooLong, empty, reloaded,
});
`;
}

// A refusal carries the code it names, whatever its message holds, and is fatal only when the
// binding can render no more.
export function bindingRefusalCodes() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const engine = await wasm.WebEffectRegistry.create();
const base = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
const refusal = (run) => {
  try { run(); return null; } catch (error) {
    return { code: error?.code ?? null, fatal: error?.fatal ?? null, name: error?.name ?? typeof error, message: String(error?.message ?? error) };
  }
};
const sdr = JSON.stringify({ primaries: "bt709", transfer: "bt709", matrix: "bt709", range: "limited", dynamic_range: "sdr" });
const pq = JSON.stringify({ primaries: "bt2020", transfer: "pq", matrix: "bt2020_ncl", range: "limited", dynamic_range: "hdr" });
const yuv = (id, { bytes = 96, color = sdr, rotation = 0 } = {}) => engine.uploadYuvFrame(
  id, 0n, "I420", 8, 8, new Uint8Array(bytes).fill(128),
  Uint32Array.from([0, 8, 64, 4, 80, 4]), Uint32Array.from([0, 0, 8, 8]),
  8, 8, color, "left", 8, 8, rotation, false,
);
const refused = {};
refused.noProject = refusal(() => engine.frameNeeds(0n));
engine.loadProject(JSON.stringify({ ...base, settings: { ...base.settings, color: { hdr: "reject" } } }));
refused.planeOverrun = refusal(() => yuv("image-source", { bytes: 50 }));
refused.rotation = refusal(() => yuv("image-source", { rotation: 45 }));
for (const id of ["missing-shot", "device-lost-scene", "shader-intro"])
  refused["hdr:" + id] = refusal(() => yuv(id, { color: pq }));
refused.rgbaLength = refusal(() => engine.uploadRgbaFrame("image-source", 0n, 4, 4, new Uint8Array(10)));
refused.unknownFrame = refusal(() => engine.presentUploadedFrame("missing-frame", 0n));
refused.audioRate = refusal(() => engine.uploadAudioBlock("encode-track", 0n, 44100, 2, new Float32Array(64)));
// Each audio refusal names its source and its own reason.
refused.audioPlanes = refusal(() => engine.uploadAudioBlock("encode-track", 0n, 48000, 3, new Float32Array(64)));
// An upload no window holds is refused from its length, before its samples reach wasm memory, which
// never shrinks.
const memory = (await wasm.default()).memory;
const before = memory.buffer.byteLength;
refused.audioTooLong = refusal(() => engine.uploadAudioBlock("encode-track", 0n, 48000, 2, new Float32Array(2 * 48000 * 700)));
const grownMiB = (memory.buffer.byteLength - before) / (1 << 20);
refused.captionFormat = refusal(() => wasm.parseCaptions("1", "ass"));
const waveform = new wasm.WaveformBuilder(1, 4, 1);
waveform.finish();
refused.waveformFinished = refusal(() => waveform.push(new Float32Array(4)));
// The engine's four reasons a composition cannot draw its output, in node IDs that hold the
// keywords a message-matching classifier would misread (missing, device lost, shader).
const look = (composition, sources = []) => {
  engine.loadProject(JSON.stringify({ ...base, sources: [...base.sources, ...sources], compositions: composition }));
  return refusal(() => engine.frameNeeds(0n));
};
const incomplete = {
  noOutput: look([{ id: "image-clip-look", nodes: [{ id: "clip", op: { type: "clip" } }], output: null }]),
  missingInput: look([{ id: "image-clip-look", nodes: [{ id: "clip", op: { type: "clip" } }, { id: "device-lost-blur", op: { type: "effect", input: null, effect: { id: "alpha2.effect.gaussian-blur", version: 1 } } }], output: "device-lost-blur" }]),
  unboundGroupInput: look([{ id: "image-clip-look", inputs: ["In"], nodes: [{ id: "missing-in", op: { type: "group_input", index: 0 } }], output: "missing-in" }]),
  clipWithoutClip: look(
    [
      { id: "image-clip-look", nodes: [{ id: "shader-title", op: { type: "media", source_id: "titled" } }], output: "shader-title" },
      { id: "title", nodes: [{ id: "clip", op: { type: "clip" } }], output: "clip" },
    ],
    [{ id: "titled", kind: "composition", composition: "title" }],
  ),
};
// A VideoFrame upload goes through the page's color conversion, so an HDR frame is refused whatever
// the policy (here tone_map): by its own transfer, or by the color its source declares.
engine.loadProject(JSON.stringify({ ...base, sources: [...base.sources, { id: "declared-pq", locator: "fixture://declared-pq", kind: "video", color_metadata: JSON.parse(pq) }] }));
const i420 = (colorSpace) => new VideoFrame(new Uint8Array(8 * 8 * 3 / 2).fill(128), { format: "I420", codedWidth: 8, codedHeight: 8, timestamp: 0, colorSpace });
const taggedPq = i420({ primaries: "bt2020", transfer: "pq", matrix: "bt2020-ncl", fullRange: false });
refused.hdrVideoFrame = refusal(() => engine.uploadVideoFrame("image-source", 0n, taggedPq));
const declaredPq = i420({ primaries: "bt709", transfer: "bt709", matrix: "bt709", fullRange: false });
refused.declaredHdrVideoFrame = refusal(() => engine.uploadPresentedVideoFrame("declared-pq", 0n, declaredPq, 8, 8, 0, false));
const closed = taggedPq.format === null && declaredPq.format === null;
// Still usable: a complete project renders.
engine.loadProject(JSON.stringify(base));
engine.uploadRgbaImage("image-source", 64, 48, new Uint8Array(64 * 48 * 4).fill(255));
engine.beginExport(0n, 8000n, 30, 1, "h264");
const rendered = (await engine.exportFrame()).kind;
engine.endExport();
// A stateless frame belongs to no export, so it carries no index for a caller to take as frame 0.
const at = engine.beginExportAt(0n, "h264");
const atFrame = at.kind === "rendered" ? await at.readback : at;
const unindexed = at.kind === "rendered" && !("index" in at) && atFrame.kind === "rendered" && !("index" in atFrame);
const deviceLost = engine.deviceLost;
engine.free();
const is = (value, code) => value?.code === code && value.fatal === false && value.name === "Alpha2Error";
const wrong = [
  ...Object.entries({
    noProject: "INVALID_STATE", planeOverrun: "INVALID_INPUT", rotation: "INVALID_INPUT",
    "hdr:missing-shot": "INVALID_INPUT", "hdr:device-lost-scene": "INVALID_INPUT", "hdr:shader-intro": "INVALID_INPUT",
    rgbaLength: "INVALID_INPUT", unknownFrame: "MISSING_INPUT", audioRate: "INVALID_INPUT",
    audioPlanes: "INVALID_INPUT", audioTooLong: "RESOURCE_LIMIT",
    hdrVideoFrame: "INVALID_INPUT", declaredHdrVideoFrame: "INVALID_INPUT",
    captionFormat: "INVALID_INPUT", waveformFinished: "INVALID_STATE",
  }).filter(([key, code]) => !is(refused[key], code)).map(([key]) => key),
  ...Object.keys(incomplete).filter((key) => !is(incomplete[key], "INCOMPLETE_COMPOSITION")),
  ...Object.entries({
    audioRate: /encode-track.*44100 Hz.*48000 Hz/, audioPlanes: /encode-track.*3 speakers/,
    audioTooLong: /encode-track/, hdrVideoFrame: /image-source.*uploadYuvFrame/, declaredHdrVideoFrame: /declared-pq.*uploadYuvFrame/,
    // The binding's own refusal under hdr "reject", which names the source, not the renderer's.
    "hdr:missing-shot": /hdr is "reject".*missing-shot$/, "hdr:device-lost-scene": /hdr is "reject".*device-lost-scene$/,
    "hdr:shader-intro": /hdr is "reject".*shader-intro$/,
  }).filter(([key, pattern]) => !pattern.test(refused[key]?.message ?? "")).map(([key]) => key + " message"),
];
report({
  ok: wrong.length === 0 && grownMiB < 16 && closed && rendered === "rendered" && unindexed && !deviceLost,
  wrong, refused, incomplete, grownMiB, closed, rendered, unindexed, deviceLost,
});
`;
}

// Overlapping registrations both settle, the binding answers meanwhile, and freeing it mid-compile
// neither traps nor leaves a promise unsettled.
export function effectRegistrationOverlap() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const effect = (id) => JSON.stringify({ id, version: 1, kind: "effect", params: [], wgslBody: "fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }" });
// The parent binding's one-step registerEffect, for the case's run against it.
const register = (engine, json) => Promise.resolve().then(() => engine.compileEffect
  ? engine.compileEffect(json).then((compiled) => engine.registerCompiledEffect(compiled))
  : engine.registerEffect(json));
const settle = (promise) => Promise.race([
  promise.then((value) => ({ value }), (error) => ({ error: String(error?.message ?? error) })),
  new Promise((resolve) => setTimeout(() => resolve({ unsettled: true }), 5000)),
]);
const pageErrors = [];
addEventListener("error", (event) => pageErrors.push(String(event.message)));
const engine = await WebEffectRegistry.create();
const both = [register(engine, effect("test.a")), register(engine, effect("test.b"))];
// The compiles have begun (the parent's future took its borrow on its first poll) and wait for the
// GPU process.
for (let tick = 0; tick < 4; tick += 1) await Promise.resolve();
let meanwhile;
try { meanwhile = { deviceLost: engine.deviceLost, catalog: engine.builtinCatalog().length > 0 }; } catch (error) { meanwhile = { error: String(error?.message ?? error) }; }
const overlapped = await Promise.all(both.map(settle));
const again = await settle(register(engine, effect("test.a")));
engine.free();
const other = await WebEffectRegistry.create();
const freed = settle(register(other, effect("test.c")));
other.free();
const afterFree = await freed;
report({
  ok: overlapped.every((result) => typeof result.value === "string") && meanwhile.error === undefined
    && /already registered/.test(again.error ?? "") && afterFree.unsettled === undefined && pageErrors.length === 0,
  overlapped, meanwhile, again, afterFree, pageErrors,
});
`;
}

// The capability probe's two modules validate in a browser that runs the packaged modules, and a
// validator without a feature is reported as such before anything is fetched.
export function wasmFeatureProbe() {
  return `
const { detectBrowserCapabilities } = await import("/packages/alpha2/dist/robustness.js");
const here = await detectBrowserCapabilities();
const validate = WebAssembly.validate;
WebAssembly.validate = () => false;
let without;
try { without = await detectBrowserCapabilities(); } finally { WebAssembly.validate = validate; }
const { default: init } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
report({
  ok: here.wasm === true && !here.diagnostics.some((kind) => kind.startsWith("wasm-"))
    && without.wasm === false
    && without.diagnostics.includes("wasm-simd-unavailable")
    && without.diagnostics.includes("wasm-reference-types-unavailable"),
  here: { wasm: here.wasm, diagnostics: here.diagnostics },
  without: { wasm: without.wasm, diagnostics: without.diagnostics },
});
`;
}

// Hashing on the page's thread must let frames and timers run between slices: Chrome ran a
// `scheduler.yield()` continuation ahead of both.
export function sha256BlobYields() {
  return `
const { sha256Blob } = await import("/packages/alpha2/dist/sha256.js");
const size = 128 << 20;
const chunk = new Uint8Array(8 << 20);
for (let i = 0; i < chunk.length; i++) chunk[i] = i * 31;
const file = new File(Array.from({ length: size / chunk.length }, () => chunk), "big.bin");
await sha256Blob(new File([chunk], "warm.bin"));
let running = true, last = performance.now();
const gaps = [];
const frame = (now) => { gaps.push(now - last); last = now; if (running) requestAnimationFrame(frame); };
requestAnimationFrame(frame);
await new Promise((resolve) => setTimeout(resolve, 200));
gaps.length = 0;
last = performance.now();
let timers = 0, timing = true;
const arm = () => setTimeout(() => { timers++; if (timing) arm(); }, 10);
arm();
const started = performance.now();
await sha256Blob(file);
const hashMs = performance.now() - started;
timing = false;
running = false;
const sorted = [...gaps].sort((a, b) => a - b);
const p50 = sorted[Math.floor((sorted.length - 1) / 2)] ?? Infinity;
report({
  ok: timers >= 5 && p50 < 50,
  hashMs: Math.round(hashMs), timers, frames: gaps.length, frameGapP50Ms: Math.round(p50),
  frameGapMaxMs: Math.round(sorted.at(-1) ?? 0),
});
`;
}

// The browser's text rasterizer must draw tests/golden/text_raster.json's pinned pixels.
export function textRasterParity() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { WasmTextRasterizer } = await import("/packages/alpha2/dist/text.js");
const golden = await (await fetch("/tests/golden/text_raster.json")).json();
const rasterizer = new WasmTextRasterizer(wasm);
for (const font of golden.fonts) rasterizer.addFont(font.family, new Uint8Array(await (await fetch("/" + font.path)).arrayBuffer()));
const fnv1a = (bytes) => {
  let hash = 0xcbf29ce484222325n;
  for (const byte of bytes) hash = ((hash ^ BigInt(byte)) * 0x100000001b3n) & 0xffffffffffffffffn;
  return hash.toString(16).padStart(16, "0");
};
const mismatches = [];
for (const fixture of golden.cases) {
  const raster = rasterizer.rasterize({ ...fixture.request, contentVersion: BigInt(fixture.contentVersion) });
  const actual = {
    width: raster.texture.width, height: raster.texture.height,
    layoutX: raster.layoutX, layoutY: raster.layoutY, layoutWidth: raster.layoutWidth, layoutHeight: raster.layoutHeight,
    firstBaseline: raster.firstBaseline, ascent: raster.ascent, descent: raster.descent, lineCount: raster.lineCount,
    pixelsFnv1a: fnv1a(raster.rgba8),
  };
  const differing = Object.keys(fixture.expected).filter((key) => actual[key] !== fixture.expected[key]);
  if (differing.length) mismatches.push(fixture.name + ": " + differing.map((key) => key + " " + actual[key] + " != " + fixture.expected[key]).join(", "));
}
rasterizer.close();
report({ ok: mismatches.length === 0, cases: golden.cases.length, mismatches });
`;
}
