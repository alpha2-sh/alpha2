// runExtractor drives the engine's session in wasm over frames the browser decodes in software.

const prelude = `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { runExtractor } = await import("alpha2/extract");
const project = {
  schema_version: 1, id: "p", name: "P",
  settings: { width: 320, height: 180, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "v", kind: "video", locator: "v.mp4" }],
  timeline: { tracks: [] },
};
const media = async (name) => (await fetch("/tests/corpus/" + name)).blob();
`;

// The digest fixture: one scalars sample per frame, the bytes' FNV-1a hash in two halves.
export function extractDigest() {
  return `${prelude}
const fnv1a = (bytes) => {
  let hash = 0x811c9dc5;
  for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193) >>> 0;
  return hash;
};
const digest = {
  describe: () => ({
    id: "alpha2.test.digest", version: 1,
    input: { media: "video", from: ["source", "timeline"], format: "rgba8", max_size: 960 },
    params_schema: { type: "object" },
    outputs: [{ name: "digest", kind: "scalars", channels: ["hi", "lo"], interpolation: "hold" }],
  }),
  video(frame, out) {
    const hash = fnv1a(frame.data);
    out.sample("digest", frame.time, [(hash >>> 16) / 65536, (hash & 0xffff) / 65536]);
  },
};
const pinned = await (await fetch("/tests/golden/extract-recipe.json")).json();
const found = {};
const undecodable = {};
let ok = true;
// Every pinned file this browser decodes; H.264 always.
for (const name of Object.keys(pinned)) {
  let result;
  try {
    result = await runExtractor({
    binding: wasm, project, media: { v: await media(name) },
    request: {
      extractor: "alpha2.test.digest", version: 1,
      input: { from: "source", source_id: "v", range: { start: 0, end: 40000 }, size: 96 },
      outputs: { digest: "frames" },
    },
  }, digest);
  } catch (error) {
    if (error?.code !== "DECODE_UNAVAILABLE" || name === "h264-bt709-copy.mp4") throw error;
    undecodable[name] = error.message;
    continue;
  }
  const set = result.dataSets[0];
  found[name] = set.times.map((time, index) => [
    time,
    (Math.round(set.values[index * 2] * 65536) * 65536 + Math.round(set.values[index * 2 + 1] * 65536))
      .toString(16).padStart(8, "0"),
  ]);
  ok &&= JSON.stringify(found[name]) === JSON.stringify(pinned[name]);
}
report({ ok: ok && "h264-bt709-copy.mp4" in found, found, undecodable });
`;
}

// The scripted fixture emitting a value no data set can hold is refused with the session's error.
export function extractInvalidEmit() {
  return `${prelude}
const scripted = {
  describe: () => ({
    id: "alpha2.test.scripted", version: 1,
    input: { media: "video", from: ["source"], format: "luma8" },
    params_schema: { type: "object" },
    outputs: [{ name: "track", kind: "points", channels: ["a"] }],
  }),
  video(frame, out) {
    out.sample("track", frame.time, [20, 0]);
  },
};
let error;
try {
  await runExtractor({
    binding: wasm, project, media: { v: await media("h264-bt709-copy.mp4") },
    request: {
      extractor: "alpha2.test.scripted", version: 1,
      input: { from: "source", source_id: "v", range: { start: 0, end: 40000 }, size: 64 },
      outputs: { track: "t" },
    },
  }, scripted);
} catch (caught) {
  if (caught?.code === "DECODE_UNAVAILABLE") throw caught;
  error = caught;
}
report({
  ok: error?.code === "INVALID_EMIT" && error?.details?.index === 0,
  code: error?.code, message: error?.message, details: error?.details,
});
`;
}

// A timeline input reads frames the CPU renderer draws: a red fill gives red samples.
export function extractTimeline() {
  return `${prelude}
const engine = wasm.WebEffectRegistry.createCpu(new OffscreenCanvas(320, 180));
const timeline = {
  ...project,
  sources: [{ id: "red", kind: "generated", generator: { type: "fill",
    paint: { type: "solid", color: { red: 1, green: 0, blue: 0, alpha: 1 } } } }],
  timeline: { tracks: [{ id: "t", name: "T", kind: "video", clips: [{ id: "c", source_id: "red",
    timeline_range: { start: 0, end: 40000 }, source_range: { start: 0, end: 40000 } }] }] },
};
engine.loadProject(JSON.stringify(timeline));
const mean = {
  describe: () => ({
    id: "com.example.mean", version: 1,
    input: { media: "video", from: ["timeline"], format: "rgba8" },
    params_schema: { type: "object" },
    outputs: [{ name: "red", kind: "scalars", channels: ["r"] }],
  }),
  video(frame, out) {
    let sum = 0;
    for (let index = 0; index < frame.data.length; index += 4) sum += frame.data[index];
    out.sample("red", frame.time, [sum / (frame.data.length / 4) / 255]);
  },
};
const { dataSets } = await runExtractor({
  binding: wasm, project: timeline,
  timelineFrame: (time, width, height) => ({ width, height, rgba: engine.readRgbaAt(time, width, height) }),
  request: {
    extractor: "com.example.mean", version: 1,
    input: { from: "timeline", range: { start: 0, end: 40000 }, size: 64, frame_rate: { num: 30, den: 1 } },
    outputs: { red: "redness" },
  },
}, mean);
const set = dataSets[0];
report({ ok: set.times.length === 10 && set.values.every((value) => value === 1), times: set.times, values: set.values });
`;
}

// The digest fixture plugin's wasm artifact, loaded from its manifest, gives the native bytes.
export function pluginDigest() {
  return `${prelude}
const { loadPlugin } = await import("alpha2/plugins");
const manifestUrl = "/plugins/target/browser-fixtures/fixture-digest/manifest.json";
const manifestBytes = await (await fetch(manifestUrl)).arrayBuffer();
const sha256 = [...new Uint8Array(await crypto.subtle.digest("SHA-256", manifestBytes))]
  .map((byte) => byte.toString(16).padStart(2, "0")).join("");
let refused;
try {
  await loadPlugin(manifestUrl, "0".repeat(64));
} catch (error) {
  refused = error.code;
}
const plugin = await loadPlugin(manifestUrl, sha256);
const pinned = await (await fetch("/tests/golden/extract-recipe.json")).json();
const name = "h264-bt709-copy.mp4";
const { dataSets } = await runExtractor({
  binding: wasm, project, media: { v: await media(name) }, plugin,
  request: {
    extractor: "alpha2.test.digest", version: 1,
    input: { from: "source", source_id: "v", range: { start: 0, end: 40000 }, size: 96 },
    outputs: { digest: "frames" },
  },
}, plugin.extractor());
const set = dataSets[0];
const found = set.times.map((time, index) => [
  time,
  (Math.round(set.values[index * 2] * 65536) * 65536 + Math.round(set.values[index * 2 + 1] * 65536))
    .toString(16).padStart(8, "0"),
]);
report({
  ok: refused === "PLUGIN_HASH_MISMATCH" && JSON.stringify(found) === JSON.stringify(pinned[name])
    && set.made_by.plugin?.artifact_sha256 === plugin.artifactSha256,
  refused, found, madeBy: set.made_by,
});
`;
}

// Every case of tests/cases/surface-conformance.json the page can run, which covers edits through
// wasm, the digest fixture over each source this browser decodes (H.264 always), the packed
// plugins' wasm artifacts, and timeline inputs drawn by the CPU renderer.
export function pluginConformance() {
  return `${prelude}
const { loadPlugin } = await import("alpha2/plugins");
const { applyEdit } = await import("alpha2/apply-edit");
const { dataHash, digestValues } = await import("/scripts/lib/surface-conformance.mjs");
const table = await (await fetch("/tests/cases/surface-conformance.json")).json();
const sha256 = async (bytes) => [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
  .map((byte) => byte.toString(16).padStart(2, "0")).join("");
const sorted = (value) =>
  Array.isArray(value) ? value.map(sorted)
    : value !== null && typeof value === "object"
      ? Object.fromEntries(Object.keys(value).sort().map((key) => [key, sorted(value[key])]))
      : value;
const same = (a, b) => JSON.stringify(sorted(a)) === JSON.stringify(sorted(b));
const failed = [];
for (const fixture of table.edit) {
  const { expected } = fixture;
  try {
    const edited = applyEdit(wasm, table.projects[fixture.project], fixture.request);
    const hash = await sha256(new TextEncoder().encode(edited.projectJson));
    if (hash !== expected.project_sha256 || !same(edited.report, expected.report))
      failed.push(fixture.name);
  } catch (error) {
    if (error?.code !== expected.code || error?.details?.refusal !== expected.refusal) failed.push(fixture.name);
  }
}
const plugins = {};
const plugin = async (name) => {
  const url = "/plugins/target/browser-fixtures/" + name + "/manifest.json";
  plugins[name] ??= await loadPlugin(url, await sha256(await (await fetch(url)).arrayBuffer()));
  return plugins[name];
};
const found = {};
const undecodable = {};
let h264Unavailable;
for (const fixture of table.extract) {
  const project = table.projects[fixture.project];
  const media = {};
  for (const [id, path] of Object.entries(fixture.assets)) media[id] = await (await fetch("/" + path)).blob();
  let timelineFrame;
  if (fixture.request.input.from === "timeline") {
    const engine = wasm.WebEffectRegistry.createCpu(
      new OffscreenCanvas(project.settings.width, project.settings.height));
    engine.loadProject(JSON.stringify(project));
    timelineFrame = (time, width, height) => ({ width, height, rgba: engine.readRgbaAt(time, width, height) });
  }
  const loaded = fixture.extractor.plugin === undefined ? undefined : await plugin(fixture.extractor.plugin);
  const extractor = loaded?.extractor() ?? {
    describe: () => fixture.extractor.client,
    video(frame, out) {
      out.sample("digest", frame.time, digestValues(frame.data));
    },
  };
  let result;
  try {
    result = await runExtractor({
      binding: wasm, audioBinding: wasm, project, media, timelineFrame, plugin: loaded,
      request: fixture.request,
    }, extractor);
  } catch (error) {
    if (error?.code !== "DECODE_UNAVAILABLE") throw error;
    undecodable[fixture.name] = error.message;
    h264Unavailable ??= fixture.name.includes("h264") ? error : undefined;
    continue;
  }
  found[fixture.name] = await dataHash(result.dataSets);
  if (found[fixture.name] !== fixture.sha256) failed.push(fixture.name);
}
// Without H.264 the case skips, unless something it could run failed.
if (h264Unavailable !== undefined && failed.length === 0) throw h264Unavailable;
report({ ok: failed.length === 0, failed, found, undecodable });
`;
}
