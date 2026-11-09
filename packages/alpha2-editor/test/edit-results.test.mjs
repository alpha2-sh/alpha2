import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { applyEdit } from "alpha2/apply-edit";
import {
  createDocument,
  createEditor,
  dataValue,
  serializeDocument,
} from "../dist/index.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = new URL("../../alpha2/wasm/", import.meta.url);
const packaged = existsSync(new URL("alpha2_wasm_bg.wasm", wasmDirectory));
// Skipped before `npm run build`; ALPHA2_REQUIRE=wasm makes that a failure.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

async function engine() {
  const wasm = await import(new URL("alpha2_wasm.js", wasmDirectory).href);
  wasm.initSync({
    module: readFileSync(new URL("alpha2_wasm_bg.wasm", wasmDirectory)),
  });
  return wasm;
}

const corpus = JSON.parse(
  readFileSync(
    new URL("../../../tests/cases/surface-conformance.json", import.meta.url),
    "utf8",
  ),
);

const sha256 = (text) => createHash("sha256").update(text).digest("hex");

const canonicalText = (wasm, project) =>
  applyEdit(wasm, project, { op: "bake", targets: [] }).projectJson;

test(
  "edit results go in as the engine made them, and undo restores the document",
  { skip },
  async () => {
    const wasm = await engine();
    const cases = corpus.edit.filter((c) => c.expected.refusal === undefined);
    assert.ok(cases.length >= 10);
    for (const fixture of cases) {
      const editor = createEditor({
        document: createDocument(corpus.projects[fixture.project]),
      });
      const before = serializeDocument(editor.document);
      const { project, read } = editor.editProject();
      const applied = applyEdit(wasm, project, fixture.request);
      const result = editor.applyEdit(applied, {
        read,
        request: fixture.request,
      });
      assert.ok(result.ok, `${fixture.name}: ${result.error?.message}`);
      assert.deepEqual(result.value.report, fixture.expected.report);
      assert.deepEqual(result.value.document.project, applied.project);
      assert.equal(
        sha256(canonicalText(wasm, editor.document.project)),
        fixture.expected.project_sha256,
        fixture.name,
      );
      for (const { id } of fixture.expected.report.created)
        assert.ok(editor.query().clips.has(id), `${fixture.name} ${id}`);
      assert.equal(editor.getSnapshot().undoLabel, fixture.request.op);
      assert.ok(editor.undo());
      assert.equal(serializeDocument(editor.document), before, fixture.name);
      assert.equal(editor.getSnapshot().redoLabel, fixture.request.op);
    }
  },
);

const SECOND = 240000;
const project = () => ({
  schema_version: 1,
  id: "p",
  name: "Edits",
  settings: {
    width: 64,
    height: 48,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48000,
  },
  sources: [{ id: "s", kind: "video", locator: "test://s" }],
  timeline: {
    tracks: [
      {
        id: "v",
        name: "V",
        kind: "video",
        clips: [
          {
            id: "c",
            source_id: "s",
            timeline_range: { start: 0, end: 4 * SECOND },
            source_range: { start: 0, end: 4 * SECOND },
            time_map: { type: "forward" },
            visual: {
              opacity: dataValue({ data: "level", channel: "level" }),
            },
          },
        ],
      },
    ],
  },
  data: [
    {
      id: "level",
      kind: "scalars",
      anchor: { type: "timeline" },
      channels: ["level"],
      interpolation: "linear",
      times: [0, 4 * SECOND],
      values: [0, 1],
    },
  ],
});

const bake = {
  op: "bake",
  targets: [{ clip: "c", property: "visual.opacity" }],
};
const split = {
  op: "split_at",
  times: { list: [2 * SECOND], anchor: { type: "timeline" } },
  target: { clips: ["c"] },
};

test(
  "a committed change after editProject makes the result stale",
  { skip },
  async () => {
    const wasm = await engine();
    const editor = createEditor({ document: createDocument(project()) });
    const { project: read0, read } = editor.editProject();
    const applied = applyEdit(wasm, read0, split);
    const level = editor.document.project.data[0];
    assert.ok(
      editor.dispatch({
        type: "data.put",
        dataSet: { ...level, values: [1, 0] },
      }).ok,
    );
    const stale = editor.applyEdit(applied, { read, request: split });
    assert.equal(stale.error?.code, "stale_edit");
    const missing = editor.applyEdit(applied, { request: split });
    assert.equal(missing.error?.code, "invalid_command");
    assert.ok(editor.undo());
    assert.ok(editor.applyEdit(applied, { read, request: split }).ok);
  },
);

test(
  "then runs in the same undo step and a refusal commits nothing",
  { skip },
  async () => {
    const wasm = await engine();
    const editor = createEditor({ document: createDocument(project()) });
    const start = editor.document;
    const { project: base, read } = editor.editProject();
    const baked = applyEdit(wasm, base, bake);
    const refused = editor.applyEdit(baked, {
      read,
      request: bake,
      then: [
        { type: "data.remove", id: "level" },
        { type: "clip.delete", id: "nope" },
      ],
    });
    assert.equal(refused.ok, false);
    assert.equal(editor.document, start);
    assert.equal(editor.getSnapshot().canUndo, false);
    const result = editor.applyEdit(baked, {
      read,
      request: bake,
      then: [{ type: "data.remove", id: "level" }],
      label: "Bake opacity",
    });
    assert.ok(result.ok, result.error?.message);
    assert.equal("data" in editor.document.project, false);
    assert.equal(
      editor.query().clips.get("c").clip.visual.opacity.mode,
      "keyframes",
    );
    assert.equal(editor.getSnapshot().undoLabel, "Bake opacity");
    assert.ok(editor.undo());
    assert.equal(editor.document, start);
    assert.equal(editor.editProject().read.document, start);
  },
);

test(
  "created clips inherit metadata from the clip they were cut from",
  { skip },
  async () => {
    const wasm = await engine();
    const editor = createEditor({ document: createDocument(project()) });
    assert.ok(
      editor.dispatch({
        type: "clip.metadata",
        id: "c",
        patch: { name: "Interview" },
      }).ok,
    );
    const { project: base, read } = editor.editProject();
    const applied = applyEdit(wasm, base, split);
    const result = editor.applyEdit(applied, { read, request: split });
    assert.ok(result.ok, result.error?.message);
    const [created] = result.value.report.created;
    assert.deepEqual(created, { id: "c.1", from: "c" });
    assert.equal(editor.document.metadata.clips["c.1"].name, "Interview");
    const removal = {
      op: "remove_ranges",
      ranges: {
        list: [{ start: 2 * SECOND, end: 4 * SECOND }],
        anchor: { type: "timeline" },
      },
      target: { clips: ["c.1"] },
    };
    const next = editor.editProject();
    const removed = editor.applyEdit(applyEdit(wasm, next.project, removal), {
      read: next.read,
      request: removal,
    });
    assert.ok(removed.ok, removed.error?.message);
    assert.equal("c.1" in editor.document.metadata.clips, false);
  },
);

test(
  "a locked track refuses an edit result that changes it",
  { skip },
  async () => {
    const wasm = await engine();
    const editor = createEditor({ document: createDocument(project()) });
    assert.ok(
      editor.dispatch({
        type: "track.metadata",
        id: "v",
        patch: { locked: true },
      }).ok,
    );
    const { project: base, read } = editor.editProject();
    const result = editor.applyEdit(applyEdit(wasm, base, split), {
      read,
      request: split,
    });
    assert.equal(result.error?.code, "locked");
  },
);
