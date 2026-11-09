// The editor's browser runtime running engine edits and extractions on the page.

const prelude = `
const { createDocument, createEditor } = await import("/packages/alpha2-editor/dist/index.js");
const { createBrowserBridge } = await import("/packages/alpha2-editor/dist/browser/index.js");
const runtimeAssets = { wasmUrl: "/packages/alpha2/wasm/alpha2_wasm_bg.wasm" };
const table = await (await fetch("/tests/cases/surface-conformance.json")).json();
const sha256 = async (bytes) => [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
  .map((byte) => byte.toString(16).padStart(2, "0")).join("");
const codeOf = (promise) => promise.then(() => "resolved", (error) => error?.code ?? String(error));
`;

// A plugin run through bridge.extract and applyExtraction gives the pinned data; a cancelled run
// and a run whose source goes away meanwhile leave the document alone.
export function pluginEditorExtract() {
  return `${prelude}
const { dataHash } = await import("/scripts/lib/surface-conformance.mjs");
const fixture = table.extract.find((c) => c.name === "plugin-digest-source-h264-bt709-copy");
const manifest = "/plugins/target/browser-fixtures/fixtures/digest/manifest.json";
const plugin = { manifest, sha256: await sha256(await (await fetch(manifest)).arrayBuffer()) };
const editor = createEditor();
const bridge = createBrowserBridge({ editor, runtimeAssets, audio: false });
const bytes = await (await fetch("/" + fixture.assets.v)).blob();
const { assetIds: [id] } = await bridge.importFiles([new File([bytes], "clip.mp4", { type: "video/mp4" })]);
const run = { ...fixture.request, input: { ...fixture.request.input, source_id: id } };

const removing = bridge.extract(run, { plugin });
const removed = editor.dispatch({ type: "asset.remove", id });
const late = await removing;
const refused = removed.ok ? editor.applyExtraction(late.result).error?.code : removed.error.code;
editor.undo();

const { result, read } = await bridge.extract(run, { plugin });
const applied = editor.applyExtraction(result, { read });
const found = applied.ok ? await dataHash(editor.document.project.data) : applied.error.code;
const set = editor.document.project.data?.[0];
const madeFrom = set?.made_from?.content_sha256 === editor.document.assets[id].fingerprint?.content_sha256;

const before = editor.document;
const controller = new AbortController();
const cancelling = bridge.extract(run, { plugin, signal: controller.signal });
controller.abort();
const cancelled = await codeOf(cancelling);

report({
  ok: refused === "missing_asset" && found === fixture.sha256 && madeFrom
    && cancelled === "cancelled" && editor.document === before,
  refused, found, madeFrom, cancelled,
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
const { assetIds: [id] } = await bridge.importFiles([new File([bytes], "clip.mp4", { type: "video/mp4" })]);
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
  put: put.ok ? true : put.error.message, stale,
});
await bridge.dispose();
`;
}
