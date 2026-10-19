// Cases on what the engine holds: wasm and GPU memory, uploads, canaries, budgets, result cache.

// The result cache keys a frame by source and upload generation: a revisit is found, and a new
// upload at the same time is drawn again.
export function resultCacheRevisit() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "revisit", name: "Revisit",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
  }] }] },
}));
engine.setResultCacheBudget(1 << 24);
const upload = (ticks, value) =>
  engine.uploadRgbaFrame("s", BigInt(ticks), 16, 16, new Uint8Array(16 * 16 * 4).fill(value));
const cache = () => JSON.parse(engine.diagnostics()).resultCache;
const render = (ticks) => {
  const before = cache();
  const played = JSON.parse(engine.renderAt(BigInt(ticks)));
  const after = cache();
  return {
    ticks, repeated: played.repeated, missing: played.missingSources.length,
    hits: after.hits - before.hits, inserts: after.inserts - before.inserts, ineligible: after.ineligible - before.ineligible,
  };
};
upload(0, 200);
upload(60000, 100);
const steps = [render(0), render(60000), render(0)];
// A new upload at time 0, the same pixels: a new generation, so frame 0 is drawn again.
upload(0, 200);
steps.push(render(60000), render(0));
const deviceLost = engine.deviceLost;
engine.free();
const [first, other, revisit, back, reuploaded] = steps;
report({
  ok: steps.every((step) => !step.repeated && step.missing === 0 && step.ineligible === 0) &&
    first.hits === 0 && first.inserts > 0 && other.hits === 0 && other.inserts > 0 &&
    revisit.hits > 0 && revisit.inserts === 0 &&
    back.hits > 0 && reuploaded.hits === 0 && reuploaded.inserts > 0 && !deviceLost,
  steps, deviceLost,
});
`;
}

export function planBudget() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
// One blur whose radius has 180,000 keyframes, drawn twenty times by each of two compositions:
// about 170 MiB of compiled steps each, within the engine's 256 MiB plan budget alone and past it
// together.
const keyframes = Array.from({ length: 180000 }, (_, time) => ({ time, value: { type: "float", value: 2 }, interpolation: { type: "hold" } }));
const drawing = (id) => ({
  id,
  nodes: [
    ...Array.from({ length: 20 }, (_, index) => ({ id: "use" + index, op: { type: "group", composition: "big" } })),
    { id: "merge", op: { type: "merge", layers: Array.from({ length: 20 }, (_, index) => ({ input: "use" + index })) } },
  ],
  output: "merge",
});
const clip = (id, composition, start) => ({
  id, source_id: "fill",
  timeline_range: { start, end: start + 12000 }, source_range: { start: 0, end: 12000 },
  composition: { id: composition },
});
const project = (clips) => JSON.stringify({
  schema_version: 1, id: "plans", name: "Plans",
  settings: { width: 32, height: 32, frame_rate: { num: 10, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "fill", kind: "generated", generator: { type: "fill", paint: { type: "solid", color: { red: 1, green: 0, blue: 0, alpha: 1 } } } }],
  compositions: [
    drawing("a"), drawing("b"),
    { id: "big", nodes: [
      { id: "clip", op: { type: "clip" } },
      { id: "blur", op: { type: "effect", input: "clip", effect: { id: "alpha2.effect.gaussian-blur", version: 1, params: { radius: { mode: "keyframes", keyframes } } } } },
    ], output: "blur" },
  ],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips }] },
});
const needs = (json, ticks) => {
  engine.loadProject(json);
  try {
    return { ok: true, needs: JSON.parse(engine.frameNeeds(ticks)).length };
  } catch (error) {
    return { ok: false, message: String(error?.message ?? error) };
  }
};
const alone = needs(project([clip("a1", "a", 0)]), 6000n);
const both = needs(project([clip("a1", "a", 0), clip("b1", "b", 12000)]), 6000n);
const later = needs(project([clip("a1", "a", 0), clip("b1", "b", 12000)]), 18000n);
engine.free();
report({
  ok: alone.ok && !both.ok && !later.ok && /hold more than 268435456 bytes/.test(both.message) && /hold more than/.test(later.message),
  alone, both, later,
});
`;
}

// A dropped GPUTexture or GPUBuffer frees nothing until GC, so releaseIdleMemory must destroy()
// what it lets go of, and free() must destroy the registry's device.
export function gpuMemoryRelease() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
const destroyed = { texture: 0, buffer: 0, device: 0 };
for (const [type, key] of [[GPUTexture, "texture"], [GPUBuffer, "buffer"], [GPUDevice, "device"]]) {
  const destroy = type.prototype.destroy;
  type.prototype.destroy = function (...args) {
    destroyed[key] += 1;
    return destroy.apply(this, args);
  };
}
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
engine.loadProject(JSON.stringify(project));
engine.uploadRgbaImage("image-source", 64, 48, new Uint8Array(64 * 48 * 4).fill(255));
engine.beginExport(0n, 8000n, 30, 1, "h264");
const first = await engine.exportFrame();
const before = { ...destroyed };
const released = engine.releaseIdleMemory();
const afterRelease = { ...destroyed };
const second = await engine.exportFrame();
const lostBeforeFree = engine.deviceLost;
engine.free();
report({
  ok: first.kind === "rendered" && second.kind === "rendered" && released > 0 &&
    afterRelease.texture > before.texture && afterRelease.buffer > before.buffer &&
    !lostBeforeFree && destroyed.device === 1,
  frames: [first.kind, second.kind],
  lostBeforeFree,
  released,
  texturesDestroyed: afterRelease.texture - before.texture,
  buffersDestroyed: afterRelease.buffer - before.buffer,
  devicesDestroyed: destroyed.device,
});
`;
}

// Canary buffers are pooled: after the first frames no frame creates one, and releaseIdleMemory
// destroys the idle ones.
export function readbackCanaryBuffers() {
  return `
if (!navigator.gpu) throw new Error("this browser offers no WebGPU");
const canaryLabel = /canary/;
const created = [];
const destroyed = [];
const createBuffer = GPUDevice.prototype.createBuffer;
GPUDevice.prototype.createBuffer = function (descriptor, ...rest) {
  if (canaryLabel.test(descriptor?.label ?? "")) created.push(descriptor.label);
  return createBuffer.call(this, descriptor, ...rest);
};
const destroy = GPUBuffer.prototype.destroy;
GPUBuffer.prototype.destroy = function (...args) {
  if (canaryLabel.test(this.label ?? "")) destroyed.push(this.label);
  return destroy.apply(this, args);
};
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
const project = await fetch("/tests/cases/host-parity-project.json").then((response) => response.json());
engine.loadProject(JSON.stringify(project));
engine.uploadRgbaImage("image-source", 64, 48, new Uint8Array(64 * 48 * 4).fill(255));
const exportFrame = async () => {
  const ticket = engine.beginExportAt(0n, "h264");
  if (ticket.kind === "rendered") await ticket.readback;
  return ticket.kind;
};
const kinds = [];
for (let frame = 0; frame < 3; frame++) kinds.push(await exportFrame());
const warm = created.length;
for (let frame = 0; frame < 24; frame++) kinds.push(await exportFrame());
const steady = created.length - warm;
engine.releaseIdleMemory();
const destroyedOnRelease = destroyed.length;
engine.free();
report({
  ok: kinds.every((kind) => kind === "rendered") && warm > 0 && steady === 0 && destroyedOnRelease >= warm,
  kinds: [...new Set(kinds)],
  createdWarming: warm,
  createdOver24Frames: steady,
  destroyedOnRelease,
});
`;
}

// The uploaded-frame budget bounds what video, aliased and still uploads hold together, and a
// budget of zero releases them all.
export function frameCacheBound() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
const pixels = new Uint8Array(256 * 256 * 4).fill(255);
const budget = pixels.length * 4;
engine.setUploadedFrameBudget(budget);
let peak = 0;
for (let i = 0; i < 100; i++) {
  engine.uploadRgbaFrame("video", 0n, 256, 256, pixels);
  engine.aliasVideoFrame("video", 0n, 1n);
  engine.uploadRgbaImage("image", 256, 256, pixels);
  peak = Math.max(peak, Number(engine.uploadedFrameBytes));
}

engine.setUploadedFrameBudget(0);
report({ ok: peak <= budget && engine.uploadedFrameBytes === 0n, peak, budget });
`;
}

// The engine drops stills oldest-first by count and byte budget; `holdsImage` tells a page its
// cached still is gone. `releaseSourceFrames` drops one source's uploads only.
export function stillResidency() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.create();
const pixels = new Uint8Array(4 * 4 * 4).fill(255);
const logo = engine.uploadRgbaImage("logo", 4, 4, pixels).generation;
const heldFirst = engine.holdsImage("logo", logo);
for (let i = 0; i < 128; i++) engine.uploadRgbaImage("photo-" + i, 4, 4, pixels);
const heldPastLimit = engine.holdsImage("logo", logo);
const again = engine.uploadRgbaImage("logo", 4, 4, pixels).generation;
engine.setUploadedFrameBudget(pixels.length);
engine.uploadRgbaFrame("video", 0n, 4, 4, pixels);
const heldPastBudget = engine.holdsImage("logo", again);
engine.setUploadedFrameBudget(1 << 30);
engine.uploadRgbaFrame("changed", 0n, 4, 4, pixels);
const still = engine.uploadRgbaImage("kept", 4, 4, pixels).generation;
const released = engine.releaseSourceFrames("changed");
const aliasAfterRelease = engine.aliasVideoFrame("changed", 0n, 1n);
const keptHeld = engine.holdsImage("kept", still);
report({
  ok: heldFirst && !heldPastLimit && !heldPastBudget && released && !aliasAfterRelease && keptHeld,
  heldFirst, heldPastLimit, heldPastBudget, released, aliasAfterRelease, keptHeld,
});
`;
}

// Per-source limits alone can exceed wasm32's 4 GB address space. Upload 1.4 GB of audio and verify
// memory stays near the engine's 512 MiB shared budget.
export function wasmMemoryBound(rolling = false) {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
const wasm = await init();
const engine = await WebEffectRegistry.create();
const sources = Array.from({ length: 60 }, (_, index) => ({ id: "s" + index, locator: "fixture://" + index, kind: "audio" }));
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "memory", name: "Memory",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources, timeline: { tracks: [] },
}));
const minute = new Float32Array(2 * 60 * 48000).fill(0.1);
let rejected = 0;
for (const source of sources) {
  try { engine.uploadAudioBlock(source.id, 0n, 48000, 2, minute); } catch { rejected++; }
  ${
    rolling
      ? `engine.trimAudioBefore(source.id, 59n * 120000n);
  engine.uploadAudioBlock(source.id, 60n * 48000n, 48000, 2, minute.subarray(0, 2 * 48000));`
      : ""
  }
}
const memoryMiB = wasm.memory.buffer.byteLength / (1 << 20);
const retainedMiB = engine.audioRetainedBytes() / (1 << 20);
report({
  ok: rejected === 0 && retainedMiB <= 512 && memoryMiB < ${rolling ? 256 : 1024},
  uploadedMiB: Math.round((sources.length * minute.byteLength) / (1 << 20)),
  retainedMiB: Math.round(retainedMiB),
  memoryMiB: Math.round(memoryMiB),
});
`;
}
