import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { applyEdit } from "alpha2/apply-edit";
import {
  createDocument,
  createEditor,
  compositionUsers,
  dataValue,
  effectStack,
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
      // Parts the edit left alone keep the committed objects; the engine reads both alike.
      assert.equal(
        canonicalText(wasm, result.value.document.project),
        applied.projectJson,
      );
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

{
  const SECOND = 120000;
  const clip = (id, start, end) => ({
    id,
    source_id: "s",
    timeline_range: { start: start * SECOND, end: end * SECOND },
    source_range: { start: start * SECOND, end: end * SECOND },
    time_map: { type: "forward" },
  });
  const twoTracks = () => ({
    schema_version: 1,
    id: "p",
    name: "Locks",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "s", kind: "video", locator: "test://s" }],
    timeline: {
      tracks: [
        { id: "V1", name: "V1", kind: "video", clips: [clip("a", 0, 4)] },
        { id: "V2", name: "V2", kind: "video", clips: [clip("b", 0, 4)] },
      ],
    },
    data: [
      {
        id: "level",
        kind: "scalars",
        anchor: { type: "source", source_id: "s" },
        channels: ["level"],
        interpolation: "linear",
        times: Array.from({ length: 20_000 }, (_, i) => i * 24),
        values: Array.from({ length: 20_000 }, (_, i) => (i % 100) / 100),
      },
    ],
  });
  const range = (from, to) => ({
    list: [{ start: from * SECOND, end: to * SECOND }],
    anchor: { type: "timeline" },
  });
  for (const [name, request] of [
    [
      "split_at",
      {
        op: "split_at",
        times: { list: [2 * SECOND], anchor: { type: "timeline" } },
        target: { clips: ["b"] },
      },
    ],
    [
      "remove_ranges without ripple",
      {
        op: "remove_ranges",
        ranges: range(1, 2),
        target: { clips: ["b"] },
        ripple: "none",
      },
    ],
    [
      "remove_ranges rippling its track",
      {
        op: "remove_ranges",
        ranges: range(1, 2),
        target: { clips: ["b"] },
        ripple: "track",
      },
    ],
    ["an empty bake", { op: "bake", targets: [] }],
  ])
    test(
      `an edit result that leaves a locked track as it was goes in: ${name}`,
      { skip },
      async () => {
        const wasm = await engine();
        const editor = createEditor({ document: createDocument(twoTracks()) });
        assert.ok(
          editor.dispatch({
            type: "track.metadata",
            id: "V1",
            patch: { locked: true },
          }).ok,
        );
        const before = editor.document.project;
        const { project, read } = editor.editProject();
        const applied = applyEdit(wasm, project, request);
        const result = editor.applyEdit(applied, { read, request });
        assert.ok(result.ok, `${result.error?.code}: ${result.error?.message}`);
        const after = editor.document.project;
        // What the edit did not touch is the committed document's own objects.
        assert.equal(after.timeline.tracks[0], before.timeline.tracks[0]);
        assert.equal(after.data[0], before.data[0]);
        assert.equal(after.sources, before.sources);
      },
    );
}

{
  const SECOND = 120000;
  const clip = (id, start, end) => ({
    id,
    source_id: "s",
    timeline_range: { start: start * SECOND, end: end * SECOND },
    source_range: { start: start * SECOND, end: end * SECOND },
    time_map: { type: "forward" },
  });
  const project = () => ({
    schema_version: 1,
    id: "p",
    name: "Shared",
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
          id: "V1",
          name: "V1",
          kind: "video",
          clips: [clip("a", 0, 4), clip("b", 4, 6)],
        },
      ],
    },
  });
  const effect = { id: "alpha2.effect.invert", version: 1, params: {} };
  const split = {
    op: "split_at",
    times: { list: [2 * SECOND], anchor: { type: "timeline" } },
    target: { clips: ["a"] },
  };
  /** An editor whose clip `a` has one effect, split in two by the engine. */
  async function shared() {
    const wasm = await engine();
    const editor = createEditor({ document: createDocument(project()) });
    assert.ok(
      editor.dispatch({ type: "effect.add", id: "a", instanceId: "fx", effect })
        .ok,
    );
    const { project: base, read } = editor.editProject();
    const result = editor.applyEdit(applyEdit(wasm, base, split), {
      read,
      request: split,
    });
    assert.ok(result.ok, result.error?.message);
    const right = result.value.report.created[0].id;
    const composition = (id) =>
      editor.query().clips.get(id).clip.composition?.id;
    assert.equal(composition(right), composition("a"));
    return { editor, right, composition };
  }
  const stack = (editor, id) =>
    effectStack(
      editor.document.project,
      editor.query().clips.get(id).clip,
    )?.map((entry) => `${entry.id}${entry.bypass ? "(bypassed)" : ""}`);

  test(
    "an effect edit on one part of an engine split leaves the other part's",
    { skip },
    async () => {
      const { editor, right, composition } = await shared();
      const first = editor.document.project.compositions.find(
        (c) => c.id === composition("a"),
      );
      assert.deepEqual(
        compositionUsers(editor.document.project, composition("a")).clips,
        ["a", right],
      );
      assert.ok(
        editor.dispatch({
          type: "effect.update",
          id: right,
          instanceId: "fx",
          effect,
          bypass: true,
        }).ok,
      );
      assert.deepEqual(stack(editor, "a"), ["fx"]);
      assert.deepEqual(stack(editor, right), ["fx(bypassed)"]);
      assert.equal(
        editor.document.project.compositions.find(
          (c) => c.id === composition("a"),
        ),
        first,
      );
    },
  );

  test(
    "removing the last effect of one part keeps the other part's",
    { skip },
    async () => {
      const { editor, right } = await shared();
      assert.ok(
        editor.dispatch({ type: "effect.remove", id: right, instanceId: "fx" })
          .ok,
      );
      assert.deepEqual(stack(editor, "a"), ["fx"]);
      assert.equal(editor.query().clips.get(right).clip.composition, undefined);
    },
  );

  test(
    "clip.detachComposition gives one part its own graph",
    { skip },
    async () => {
      const { editor, right, composition } = await shared();
      assert.ok(
        editor.dispatch({ type: "clip.detachComposition", id: right }).ok,
      );
      assert.notEqual(composition(right), composition("a"));
      assert.deepEqual(stack(editor, right), ["fx"]);
      // A clip that owns its graph already is left as it is.
      const before = editor.document;
      assert.equal(
        editor.dispatch({ type: "clip.detachComposition", id: "b" }).value,
        before,
      );
    },
  );

  test(
    "an engine edit that removes a clip whole removes the composition only it drew",
    { skip },
    async () => {
      const wasm = await engine();
      const editor = createEditor({ document: createDocument(project()) });
      assert.ok(
        editor.dispatch({
          type: "effect.add",
          id: "b",
          instanceId: "fx",
          effect,
        }).ok,
      );
      const drawn = editor.query().clips.get("b").clip.composition.id;
      assert.ok(
        editor.dispatch({
          type: "metadata.update",
          patch: { graphNames: { [drawn]: "Look" } },
        }).ok,
      );
      const request = {
        op: "remove_ranges",
        ranges: {
          list: [{ start: 4 * SECOND, end: 6 * SECOND }],
          anchor: { type: "timeline" },
        },
        target: { clips: ["b"] },
      };
      const { project: base, read } = editor.editProject();
      const result = editor.applyEdit(applyEdit(wasm, base, request), {
        read,
        request,
      });
      assert.ok(result.ok, result.error?.message);
      assert.deepEqual(editor.document.project.compositions ?? [], []);
      assert.equal(editor.document.metadata.graphNames?.[drawn], undefined);
      assert.equal("b" in editor.document.metadata.clips, false);
    },
  );
}
