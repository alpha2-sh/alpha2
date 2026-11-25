// The editor's browser runtime running engine edits and extractions on the page.

const prelude = `
const { createDocument, createEditor } = await import("/packages/alpha2-editor/dist/index.js");
const { createBrowserBridge } = await import("/packages/alpha2-editor/dist/browser/index.js");
const runtimeAssets = { wasmUrl: "/packages/alpha2/wasm/alpha2_wasm_bg.wasm" };
const table = await (await fetch("/tests/cases/surface-conformance.json")).json();
const sha256 = async (bytes) => [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
  .map((byte) => byte.toString(16).padStart(2, "0")).join("");
const codeOf = (promise) => promise.then(() => "resolved", (error) => error?.code ?? String(error));
const { ALL_FORMATS, BlobSource, Input } = await import("mediabunny");
const importVideo = async (bridge, bytes) => {
  const input = new Input({ source: new BlobSource(bytes), formats: ALL_FORMATS });
  let codec = "no video track";
  let decodes = false;
  try {
    const track = await input.getPrimaryVideoTrack();
    codec = (await track?.getCodecParameterString()) ?? codec;
    const config = (await track?.canDecode()) ? await track.getDecoderConfig() : null;
    decodes = config !== null && (await VideoDecoder.isConfigSupported({
      ...config, hardwareAcceleration: "prefer-software",
    })).supported === true;
  } finally {
    input.dispose();
  }
  try {
    const { assetIds } = await bridge.importFiles([new File([bytes], "clip.mp4", { type: "video/mp4" })]);
    return assetIds[0];
  } catch (error) {
    if (!decodes && error?.cause?.code === "DECODE_UNAVAILABLE")
      skip("webcodecs-video", "this browser does not decode the case's source: " + codec);
    throw error;
  }
};
`;

// A plugin run through bridge.extract and applyExtraction gives the pinned data, loaded by the
// bridge, through the app's fetch or loaded once by the app for several runs; a cancelled run and
// a run whose source goes away meanwhile leave the document alone.
export function pluginEditorExtract() {
  return `${prelude}
const { dataHash } = await import("/scripts/lib/surface-conformance.mjs");
const fixture = table.extract.find((c) => c.name === "plugin-digest-source-h264-bt709-copy");
const manifest = "/plugins/target/browser-fixtures/fixtures/digest/manifest.json";
const plugin = { manifest, sha256: await sha256(await (await fetch(manifest)).arrayBuffer()) };
const editor = createEditor();
const bridge = createBrowserBridge({ editor, runtimeAssets, audio: false });
const bytes = await (await fetch("/" + fixture.assets.v)).blob();
const id = await importVideo(bridge, bytes);
const run = { ...fixture.request, input: { ...fixture.request.input, source_id: id } };

const removing = bridge.extract(run, { plugin });
const removed = editor.dispatch({ type: "asset.remove", id });
const late = await removing;
const refused = removed.ok === true
  ? /** @type {{ error?: { code: string } }} */ (editor.applyExtraction(late.result)).error?.code
  : removed.error.code;
editor.undo();

const { result, read } = await bridge.extract(run, { plugin });
const applied = editor.applyExtraction(result, { read });
const found = applied.ok === true ? await dataHash(editor.document.project.data) : applied.error.code;
const set = editor.document.project.data?.[0];
const madeFrom = set?.made_from?.content_sha256 === editor.document.assets[id].fingerprint?.content_sha256;

const { loadPlugin } = await import("alpha2/plugins");
const loaded = await loadPlugin(manifest, plugin.sha256);
const shared = await Promise.all([1, 2].map(async () =>
  dataHash((await bridge.extract(run, { plugin: loaded })).result.dataSets)));
const fetched = [];
const phases = new Set();
const fetching = await bridge.extract(run, {
  plugin: { ...plugin, fetch: (url, init) => (fetched.push(String(url)), fetch(url, init)) },
  onProgress: (progress) => phases.add(progress.phase),
});
const viaFetch = await dataHash(fetching.result.dataSets);
const sharedOk = shared.every((hash) => hash === found) && viaFetch === found
  && fetched.length >= 2 && phases.has("load") && phases.has("extract");

const before = editor.document;
const controller = new AbortController();
const cancelling = bridge.extract(run, { plugin, signal: controller.signal });
controller.abort();
const cancelled = await codeOf(cancelling);

report({
  ok: refused === "missing_asset" && found === fixture.sha256 && madeFrom && sharedOk
    && cancelled === "cancelled" && editor.document === before,
  refused, found, madeFrom, shared, viaFetch, fetched, phases: [...phases], cancelled,
});
await bridge.dispose();
`;
}

// bridge.applyEdit called before the engine has loaded waits for it, then commits one step.
export function editorApplyEdit() {
  return `${prelude}
const editor = createEditor({ document: createDocument({
  schema_version: 1, id: "p", name: "P",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "red", kind: "generated", generator: { type: "fill",
    paint: { type: "solid", color: { red: 1, green: 0, blue: 0, alpha: 1 } } } }],
  timeline: { tracks: [{ id: "t", name: "T", kind: "video", clips: [{ id: "c", source_id: "red",
    timeline_range: { start: 0, end: 80000 }, source_range: { start: 0, end: 80000 },
    visual: { opacity: { mode: "data", data: "level", channel: "level" } } }] }] },
  data: [{ id: "level", kind: "scalars", anchor: { type: "timeline" }, channels: ["level"],
    interpolation: "linear", times: [0, 80000], values: [0, 1] }],
}) });
const bridge = createBrowserBridge({ editor, runtimeAssets, audio: false });
/** @type {import("/packages/alpha2/dist/apply-edit.js").EditRequest} */
const request = { op: "bake", targets: [{ clip: "c", property: "visual.opacity" }] };
const { report: made } = await bridge.applyEdit(request, {
  then: [{ type: "data.remove", id: "level" }],
});
const opacity = editor.query().clips.get("c").clip.visual.opacity;
const refusal = await bridge.applyEdit(request).then(() => null, (error) => error?.details?.refusal);
report({
  ok: opacity.mode === "keyframes" && !("data" in editor.document.project)
    && editor.getSnapshot().undoLabel === "bake" && made.created.length === 0
    && refusal === "nothing_to_bake",
  opacity, undoLabel: editor.getSnapshot().undoLabel, refusal,
});
await bridge.dispose();
`;
}

// bridge.conform reports a data set made from other media than its source's original now is.
export function editorConformStaleData() {
  return `${prelude}
const editor = createEditor();
const bridge = createBrowserBridge({ editor, runtimeAssets, audio: false });
const bytes = await (await fetch("/tests/corpus/h264-bt709-copy.mp4")).blob();
const id = await importVideo(bridge, bytes);
const fingerprint = editor.document.assets[id].fingerprint;
const madeFrom = { ...fingerprint, content_sha256: "0".repeat(64) };
const put = editor.dispatch({ type: "data.put", dataSet: {
  id: "old", kind: "intervals", anchor: { type: "source", source_id: id }, made_from: madeFrom,
  made_by: { extractor: "com.example.quiet", version: 1, params: {} },
  intervals: [{ start: 0, end: 4000 }],
} });
const conform = await bridge.conform("export_original");
const stale = conform.stale_data;
report({
  ok: put.ok && stale.length === 1 && stale[0].data === "old" && stale[0].source === id
    && stale[0].fields.includes("content"),
  put: put.ok === true ? true : put.error.message, stale,
});
await bridge.dispose();
`;
}

export function editorImportUndecodable() {
  return `${prelude}
const editor = createEditor();
const bridge = createBrowserBridge({ editor, runtimeAssets, audio: false });
const bytes = await (await fetch("/tests/corpus/prores-422-unsited.mov")).blob();
const caught = await bridge
  .importFiles([new File([bytes], "clip.mov", { type: "video/quicktime" })])
  .then(() => null, (error) => error);
if (caught === null) skip("webcodecs-video", "this browser decodes ProRes");
report({
  ok: caught?.code === "unsupported_media" && caught.cause?.code === "DECODE_UNAVAILABLE"
    && caught.cause.facts?.kind === "video" && Object.keys(editor.document.assets).length === 0,
  code: caught?.code ?? null, cause: caught?.cause?.code ?? null, facts: caught?.cause?.facts ?? null,
});
await bridge.dispose();
`;
}

// Preview with sound on, where one clip names an audio stream its file does not have: that lane
// plays silent and is reported once, while the picture and the other clip's sound play on.
export function editorAudioLaneFailure() {
  return `${prelude}
const twoStreams = await (await fetch("/tests/corpus/audio-two-streams.mov")).blob();
const tone = new AudioBuffer({ numberOfChannels: 2, length: 96000, sampleRate: 48000 });
tone.getChannelData(0).fill(0.25);
tone.getChannelData(1).fill(0.25);
const mediaAdapter = {
  async stage(file) {
    const runtime = file.name === "tone"
      ? { id: "tone", kind: "audio", name: "tone", audioBuffer: tone, hasAudio: true, duration: 2 }
      : { id: "mics", kind: "audio", name: "mics", file, hasAudio: true, audioDecodable: true, duration: 2 };
    return { runtime, description: {}, dispose() {} };
  },
};
const errors = [];
const editor = createEditor();
const bridge = createBrowserBridge({ editor, runtimeAssets, mediaAdapter, onError: (error) => errors.push(error) });
await bridge.importFiles([new File([""], "tone"), new File([twoStreams], "mics")]);
const span = { start: 0, end: 240000 };
/** @type {import("/packages/alpha2/dist/project.js").MediaSource} */
const color = { id: "color", kind: "generated", generator: { type: "fill",
  paint: { type: "solid", color: { red: 0.2, green: 0.4, blue: 0.8, alpha: 1 } } } };
const result = editor.dispatch({ type: "transaction", commands: [
  { type: "asset.put", asset: { id: "color", kind: "generated", name: "Color", contentRevision: 0 }, source: color },
  { type: "track.add", track: { id: "v", kind: "video", name: "V", clips: [
    { id: "picture", source_id: "color", timeline_range: span, source_range: span } ] } },
  { type: "track.add", track: { id: "a", kind: "audio", name: "A", clips: [
    { id: "tone-clip", source_id: "tone", timeline_range: span, source_range: span },
    { id: "mics-clip", source_id: "mics", timeline_range: span, source_range: span, audio: { stream: 5 } } ] } },
] });
if (result.ok === false) throw result.error;
const canvas = document.createElement("canvas");
await bridge.bindPreview(canvas);
await bridge.play();
const startTick = bridge.tick;
await new Promise((resolve) => setTimeout(resolve, 1500));
const playingWithSound =
  /** @type {{ host: { audioLatencySource?: string } } | null} */ (bridge.diagnostics)?.host.audioLatencySource ?? null;
const advanced = bridge.tick - startTick;
bridge.pause();
const reported = errors.map((error) => [error.code, error.entityId ?? null]);
if (reported.some(([code]) => code === "audio_unavailable") && /did not start|could not start/.test(errors[0]?.message ?? ""))
  skip("audio-autostart", "audio output did not start in this browser: " + errors[0].message);
report({
  ok: reported.length === 1 && reported[0][0] === "audio_unavailable" && reported[0][1] === "mics"
    && playingWithSound !== null && advanced > 60000,
  reported, playingWithSound, advanced, firstError: errors[0]?.message ?? null,
});
await bridge.dispose();
`;
}

// An export holding audio this browser cannot decode is refused naming it; with undecodableAudio
// "silence" it runs, keeps the other clip's sound and lists the source as written silent.
export function editorExportUndecodableAudio() {
  return `${prelude}
const fetched = async (name) => new File([await (await fetch("/tests/corpus/" + name)).blob()], name);
const editor = createEditor();
const bridge = createBrowserBridge({ editor, runtimeAssets, audio: false });
await bridge.importFiles([await fetched("audio-ac3-5.1.mp4"), await fetched("audio-only-aac-priming.m4a")]);
const [surround, voice] = Object.values(editor.document.assets).map((asset) => asset.id);
if (bridge.getAsset(surround).audioDecodable !== false)
  skip("ac3-audio-decode", "this browser decodes AC-3 audio, so nothing is undecodable");
if (bridge.getAsset(voice).audioDecodable !== true)
  skip("webcodecs-audio", "this browser decodes no AAC audio");
const span = { start: 0, end: 60000 };
const result = editor.dispatch({ type: "transaction", commands: [
  { type: "asset.put", asset: { id: "color", kind: "generated", name: "Color", contentRevision: 0 },
    source: { id: "color", kind: "generated", generator: { type: "fill",
      paint: { type: "solid", color: { red: 0.2, green: 0.4, blue: 0.8, alpha: 1 } } } } },
  { type: "track.add", track: { id: "v", kind: "video", name: "V", clips: [
    { id: "picture", source_id: "color", timeline_range: span, source_range: span }] } },
  { type: "track.add", track: { id: "a", kind: "audio", name: "A", clips: [
    { id: "surround-clip", source_id: surround, timeline_range: span, source_range: span }] } },
  { type: "track.add", track: { id: "b", kind: "audio", name: "B", clips: [
    { id: "voice-clip", source_id: voice, timeline_range: span, source_range: span }] } },
] });
if (result.ok === false) throw result.error;
await bridge.conform("export_original");
let refused = null;
try {
  bridge.startExport({ output: { kind: "memory" } });
} catch (error) {
  refused = { code: error.code, names: error.message.includes("audio-ac3-5.1.mp4"), cause: error.cause?.code };
}
const job = bridge.startExport({ output: { kind: "memory" }, undecodableAudio: "silence" });
const artifact = await job.result;
const blob = artifact.kind === "blob" ? artifact.blob : /** @type {{ readonly file: File }} */ (artifact).file;
const input = new Input({ source: new BlobSource(blob), formats: ALL_FORMATS });
const audioTrack = await input.getPrimaryAudioTrack();
input.dispose();
report({
  ok: refused?.code === "unsupported_media" && refused.names && refused.cause === "DECODE_UNAVAILABLE"
    && JSON.stringify(artifact.report?.silentAudioSources) === JSON.stringify([surround])
    && audioTrack !== null,
  refused, silent: artifact.report?.silentAudioSources ?? null, audio: audioTrack !== null,
});
await /** @type {{ release?: () => Promise<void> }} */ (artifact).release?.();
await bridge.dispose();
`;
}
