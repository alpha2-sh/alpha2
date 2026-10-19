import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  createDocument,
  createEmptyDocument,
  createEditor,
  serializeDocument,
  parseDocument,
  EditorError,
  frameStart,
  frameAt,
  effectStack,
  compileEditorDocument,
  executeCommand,
  programEnd,
  sequenceEnd,
} from "../dist/index.js";
import { validateProject } from "alpha2/project";
const fixture = async (name) =>
  JSON.parse(
    await readFile(new URL(`./fixtures/${name}.json`, import.meta.url), "utf8"),
  );
const simple = () =>
  createDocument({
    schema_version: 1,
    id: "p",
    name: "Test",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30000, den: 1001 },
      sample_rate: 48000,
    },
    sources: [{ id: "s", kind: "video", locator: "test://s" }],
    timeline: {
      tracks: [
        {
          id: "t",
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "s",
              timeline_range: { start: 0, end: 1200000 },
              source_range: { start: 0, end: 1200000 },
              time_map: { type: "forward" },
            },
          ],
        },
        { id: "other", name: "Other", kind: "video", clips: [] },
      ],
    },
  });
for (const name of [
  "host-parity-project",
  "host-parity-complex-project",
  "host-shared-source-export-project",
  "host-audio-export-project",
])
  test(`raw round trip preserves every engine field: ${name}`, async () => {
    const raw = await fixture(name);
    assert.deepEqual(
      parseDocument(serializeDocument(createDocument(raw))).project,
      raw,
    );
  });
test("frame boundaries invert exactly at NTSC rates and reject unsafe arithmetic", () => {
  for (const fps of [
    { num: 30000, den: 1001 },
    { num: 60000, den: 1001 },
  ])
    for (const i of [0, 1, 2, 1000, 1000000]) {
      const tick = frameStart(i, fps);
      assert.equal(frameAt(tick, fps), i);
      if (i) assert.equal(frameAt(tick - 1, fps), i - 1);
    }
  assert.throws(
    () => frameStart(Number.MAX_SAFE_INTEGER, { num: 1, den: 1 }),
    RangeError,
  );
});
test("commands share constraints, are atomic and preserve untouched branch identity", () => {
  const e = createEditor({ document: simple() });
  const other = e.document.project.timeline.tracks[1];
  const before = e.document;
  const bad = {
    type: "transaction",
    commands: [
      { type: "clip.move", id: "c", at: 120000 },
      { type: "clip.move", id: "missing", at: 0 },
    ],
  };
  assert.equal(e.canExecute(bad).ok, false);
  assert.equal(e.dispatch(bad).ok, false);
  assert.equal(e.document, before);
  assert.equal(e.getSnapshot().canUndo, false);
  assert.equal(e.dispatch({ type: "clip.move", id: "c", at: 120000 }).ok, true);
  assert.equal(e.document.project.timeline.tracks[1], other);
  assert.equal(e.undo(), true);
  assert.equal(e.document, before);
  assert.equal(e.redo(), true);
});
test("gesture updates use fixed base, cancel and stale commits cannot overwrite", () => {
  const e = createEditor({ document: simple() });
  const before = e.document;
  const g = e.beginGesture();
  let notified = 0;
  const off = e.subscribe(() => notified++);
  g.update({ type: "clip.move", id: "c", at: 120000 });
  g.update({ type: "clip.move", id: "c", at: 240000 });
  assert.equal(e.document, before);
  assert.equal(notified, 2);
  assert.equal(e.getSnapshot().draft, g.candidate);
  assert.equal(
    e.draft.project.timeline.tracks[0].clips[0].timeline_range.start,
    240000,
  );
  assert.equal(e.getSnapshot().canUndo, false);
  assert.equal(g.update({ type: "clip.move", id: "c", at: -5 }).ok, false);
  assert.equal(e.draft, g.candidate);
  assert.equal(g.commit(), true);
  off();
  assert.equal(e.getSnapshot().draft, e.document);
  assert.equal(e.query().clips.get("c").clip.timeline_range.start, 240000);
  e.undo();
  assert.equal(e.document, before);
  const stale = e.beginGesture();
  stale.update({ type: "clip.move", id: "c", at: 900 });
  e.dispatch({ type: "project.update", patch: { name: "New" } });
  assert.equal(stale.commit(), false);
  assert.equal(e.document.project.name, "New");
  assert.equal(e.draft, e.document);
  const cancel = e.beginGesture();
  cancel.update({ type: "clip.move", id: "c", at: 8 });
  assert.notEqual(e.draft, e.document);
  cancel.cancel();
  assert.equal(e.getSnapshot().draft, e.document);
  assert.equal(cancel.commit(), false);
  assert.equal(e.cancelGesture(), false);
  const torn = e.beginGesture();
  torn.update({ type: "clip.move", id: "c", at: 16 });
  assert.equal(e.cancelGesture(), true);
  assert.equal(e.draft, e.document);
  assert.equal(torn.commit(), false);
  assert.equal(e.cancelGesture(), false);
});
test("clearHistory forgets undo and redo and keeps the document, generation, selection, clipboard and gesture", () => {
  const e = createEditor({ document: simple() });
  e.dispatch({ type: "project.update", patch: { name: "One" } });
  e.dispatch({ type: "project.update", patch: { name: "Two" } });
  e.undo();
  e.select(["c"]);
  e.copy();
  const gesture = e.beginGesture();
  gesture.update({ type: "clip.move", id: "c", at: 240000 });
  const before = e.getSnapshot();
  let heard = 0;
  const off = e.subscribe(() => heard++);
  e.clearHistory();
  off();
  const after = e.getSnapshot();
  assert.equal(heard, 1);
  assert.equal(after.canUndo, false);
  assert.equal(after.canRedo, false);
  assert.equal(after.document, before.document);
  assert.equal(after.revision, before.revision);
  assert.equal(after.generation, before.generation);
  assert.deepEqual(after.selection, ["c"]);
  assert.equal(gesture.commit(), true);
  assert.equal(e.query().clips.get("c").clip.timeline_range.start, 240000);
  assert.equal(e.undo(), true);
  assert.equal(e.undo(), false);
  e.redo();
  const pasting = e.document;
  assert.equal(e.paste(3_600_000).ok, true);
  assert.notEqual(e.document, pasting);
  e.clearHistory();
  heard = 0;
  const quiet = e.subscribe(() => heard++);
  e.clearHistory();
  quiet();
  assert.equal(heard, 0);
});
test("a gesture checks its moves by the project's rules and validates in full when it commits", () => {
  const e = createEditor({ document: simple() });
  const before = e.document;
  // A non-finite move is caught when the gesture commits, and nothing is committed.
  const g = e.beginGesture();
  assert.equal(
    g.update({ type: "track.metadata", id: "t", patch: { height: NaN } }).ok,
    true,
  );
  assert.equal(g.commit(), false);
  assert.equal(e.document, before);
  assert.equal(e.draft, e.document);
  assert.equal(
    e.dispatch({ type: "track.metadata", id: "t", patch: { height: NaN } }).ok,
    false,
  );
  const h = e.beginGesture();
  assert.equal(h.update({ type: "clip.move", id: "c", at: -5 }).ok, false);
  h.cancel();
});
test("a frozen document compiles once, serializing its key on first read", () => {
  const e = createEditor({ document: simple() });
  const compiled = compileEditorDocument(e.document);
  assert.equal(compileEditorDocument(e.document), compiled);
  assert.equal(compiled.revisionKey, JSON.stringify(e.document.project));
  e.dispatch({ type: "track.metadata", id: "t", patch: { enabled: false } });
  const hidden = compileEditorDocument(e.document);
  assert.notEqual(hidden, compiled);
  assert.deepEqual(
    hidden.project.timeline.tracks.map((t) => t.id),
    ["other"],
  );
});
test("the program ends where the last clip or cue of an enabled track ends", () => {
  const e = createEditor({ document: simple() });
  assert.equal(programEnd(e.document), 1200000);
  e.dispatch({
    type: "track.add",
    track: {
      id: "subs",
      name: "Subs",
      kind: "caption",
      captions: {
        cues: [{ timeline_range: { start: 0, end: 1800000 }, text: "Late" }],
      },
    },
  });
  assert.equal(programEnd(e.document), 1800000);
  e.dispatch({ type: "track.metadata", id: "subs", patch: { enabled: false } });
  // The timeline still holds the cue; the program, playback, and an export without a range do not.
  assert.equal(sequenceEnd(e.document.project), 1800000);
  assert.equal(programEnd(e.document), 1200000);
});
test("locked tracks reject direct and indirect edits and can be unlocked", () => {
  const e = createEditor({ document: simple() });
  e.dispatch({ type: "track.metadata", id: "t", patch: { locked: true } });
  for (const cmd of [
    { type: "clip.move", id: "c", at: 20 },
    { type: "track.remove", id: "t" },
    { type: "clip.split", id: "c", at: 100, newId: "b" },
  ])
    assert.equal(e.dispatch(cmd).error.code, "locked");
  assert.equal(
    e.dispatch({ type: "track.metadata", id: "t", patch: { locked: false } })
      .ok,
    true,
  );
  assert.equal(
    e.dispatch({ type: "clip.split", id: "c", at: 120000, newId: "b" }).ok,
    true,
  );
});
test("selection and subscriptions do not invalidate unrelated clip selectors", () => {
  const e = createEditor({ document: simple() });
  let calls = 0;
  const off = e.subscribeSelector(
    (s) => s.document.project.timeline.tracks[0].clips[0],
    () => calls++,
  );
  const snapshot = e.getSnapshot();
  assert.equal(e.getSnapshot(), snapshot);
  e.select(["c"]);
  assert.equal(calls, 0);
  e.dispatch({ type: "project.update", patch: { name: "Renamed" } });
  assert.equal(calls, 0);
  e.dispatch({ type: "clip.move", id: "c", at: 1 });
  assert.equal(calls, 1);
  off();
});
test("selecting what is already selected tells no one", () => {
  // A marquee move that adds or drops no clip notifies no listener.
  const e = createEditor({ document: simple() });
  let emits = 0;
  const off = e.subscribe(() => emits++);
  e.select(["c"]);
  assert.equal(emits, 1);
  const snapshot = e.getSnapshot();
  e.select(["c", "c", "unknown"]);
  e.select(["c"]);
  assert.equal(emits, 1);
  assert.equal(e.getSnapshot(), snapshot);
  e.select([]);
  assert.equal(emits, 2);
  assert.deepEqual(e.getSnapshot().selection, []);
  e.select([]);
  assert.equal(emits, 2);
  off();
});
test("multi-selection clipboard, delete and undo restore clips", () => {
  const e = createEditor({ document: simple() });
  e.select(["c"]);
  e.copy();
  assert.equal(e.paste(2400000, () => "copy").ok, true);
  assert.equal(e.query().clips.get("copy").clip.source_id, "s");
  e.undo();
  assert.equal(e.query().clips.has("copy"), false);
});
test("asset authoring and engine source revisions undo together", () => {
  const e = createEditor();
  const before = e.document;
  const source = {
    id: "g",
    kind: "generated",
    generator: {
      type: "fill",
      paint: { type: "solid", color: { red: 1, green: 0, blue: 0, alpha: 1 } },
    },
  };
  assert.equal(
    e.dispatch({
      type: "asset.put",
      source,
      asset: { id: "g", kind: "generated", name: "Shape", contentRevision: 1 },
    }).ok,
    true,
  );
  e.undo();
  assert.equal(e.document, before);
  e.redo();
  assert.equal(e.document.assets.g.contentRevision, 1);
});
test("malformed open leaves the current session intact and imports do not initialize DOM", () => {
  const e = createEditor({ document: simple() });
  const before = e.document;
  assert.throws(() => e.open({ ...createEmptyDocument(), version: 2 }));
  assert.equal(e.document, before);
  assert.throws(() => parseDocument('{"schema_version":99}'));
  assert.throws(() =>
    serializeDocument({
      ...before,
      assets: {
        bad: { id: "bad", kind: "video", name: "X", contentRevision: NaN },
      },
    }),
  );
  e.dispose();
  e.dispose();
  assert.throws(
    () => e.dispatch({ type: "project.update", patch: { name: "x" } }),
    /disposed/,
  );
});

test("history respects its byte budget and metadata changes reuse indexes", () => {
  const e = createEditor({ document: simple(), historyByteLimit: 1 });
  const index = e.query();
  e.dispatch({
    type: "metadata.update",
    patch: { markers: [{ id: "m", time: 1, name: "M" }] },
  });
  assert.equal(e.getSnapshot().canUndo, false);
  assert.equal(e.query(), index);
});

/** A project whose one source is an image sequence of `frames` numbered PNGs. */
const sequenceDocument = (frames) =>
  createDocument({
    schema_version: 1,
    id: "p",
    name: "Sequence",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 24, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      {
        id: "seq",
        kind: "video",
        locator: "test://seq",
        provider: {
          format: "image_sequence",
          frames: Array.from(
            { length: frames },
            (_, i) =>
              `browser://0f5e1c7a-5b3e-4c39-9d0e-2b6f0f7e1a11/frame/${i + 1}`,
          ),
          frame_rate: { num: 24, den: 1 },
          start_number: 1,
          missing: "reject",
        },
      },
    ],
    timeline: {
      tracks: [
        {
          id: "t",
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "seq",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
              time_map: { type: "forward" },
            },
          ],
        },
      ],
    },
  });

test("history's byte budget counts what a step does not share with the next, so a large shared source keeps every step", () => {
  // 100,000 frame locators serialize to about 6 MB shared by every step; the default keeps 120.
  const e = createEditor({ document: sequenceDocument(100_000) });
  const frames = e.document.project.sources[0].provider.frames;
  for (let i = 0; i < 130; i++)
    assert.equal(
      e.dispatch({
        type: "metadata.update",
        patch: { markers: [{ id: "m", time: i * 10, name: `x${i}` }] },
      }).ok,
      true,
    );
  assert.equal(e.document.project.sources[0].provider.frames, frames);
  let depth = 0;
  while (e.undo()) depth++;
  assert.equal(depth, 120);
  assert.equal(e.document.metadata.markers?.[0]?.name, "x9");
  e.dispose();
});

test("history's byte budget still counts what each step holds alone", () => {
  // Each step holds a marker named with 1 MiB of its own: a 3.5 MiB budget keeps three of them.
  const e = createEditor({
    document: simple(),
    historyByteLimit: 3.5 * 1024 * 1024,
  });
  for (let i = 0; i < 8; i++)
    e.dispatch({
      type: "metadata.update",
      patch: {
        markers: [{ id: "m", time: i, name: String(i).repeat(1024 * 1024) }],
      },
    });
  let depth = 0;
  while (e.undo()) depth++;
  assert.equal(depth, 3);
  assert.equal(e.document.metadata.markers[0].name[0], "4");
  e.dispose();
});

test("an effect stack is a chain of composition nodes that survives reorder, split, clipboard and undo", () => {
  const e = createEditor({ document: simple() });
  const effect = { id: "alpha2.effect.invert", version: 1, params: {} };
  const stack = (id) =>
    effectStack(e.document.project, e.query().clips.get(id).clip)?.map(
      (entry) => entry.id,
    );
  for (const instanceId of ["one", "two"])
    assert.equal(
      e.dispatch({ type: "effect.add", id: "c", instanceId, effect }).ok,
      true,
    );
  assert.deepEqual(stack("c"), ["one", "two"]);
  const composition = e.document.project.compositions[0];
  assert.equal(composition.output, "two");
  assert.deepEqual(
    composition.nodes.map((node) => [node.id, node.op.type, node.op.input]),
    [
      ["clip", "clip", undefined],
      ["one", "effect", "clip"],
      ["two", "effect", "one"],
    ],
  );
  assert.equal(
    e.dispatch({ type: "effect.reorder", id: "c", ids: ["two", "one"] }).ok,
    true,
  );
  assert.deepEqual(stack("c"), ["two", "one"]);
  e.undo();
  assert.deepEqual(stack("c"), ["one", "two"]);
  assert.equal(
    e.dispatch({
      type: "effect.update",
      id: "c",
      instanceId: "one",
      effect,
      bypass: true,
    }).ok,
    true,
  );
  assert.equal(
    effectStack(e.document.project, e.query().clips.get("c").clip)[0].bypass,
    true,
  );
  e.select(["c"]);
  e.copy();
  assert.equal(e.paste(2400000, () => "copy").ok, true);
  const copy = e.query().clips.get("copy").clip;
  assert.notEqual(copy.composition.id, "c-fx");
  assert.deepEqual(stack("copy"), ["one", "two"]);
  assert.equal(
    e.dispatch({ type: "effect.remove", id: "copy", instanceId: "one" }).ok,
    true,
  );
  assert.deepEqual(stack("c"), ["one", "two"]);
  assert.deepEqual(stack("copy"), ["two"]);
  assert.equal(
    e.dispatch({ type: "clip.split", id: "c", at: 120000, newId: "right" }).ok,
    true,
  );
  assert.deepEqual(stack("right"), ["one", "two"]);
  assert.notEqual(
    e.query().clips.get("right").clip.composition.id,
    e.query().clips.get("c").clip.composition.id,
  );
  // Deleting a clip removes the composition only it showed; an empty stack removes it too.
  assert.equal(e.dispatch({ type: "clip.delete", id: "copy" }).ok, true);
  for (const id of ["one", "two"])
    assert.equal(
      e.dispatch({ type: "effect.remove", id: "right", instanceId: id }).ok,
      true,
    );
  assert.equal(e.query().clips.get("right").clip.composition, undefined);
  assert.deepEqual(
    e.document.project.compositions.map((c) => c.id),
    ["c-fx"],
  );
});
test("a composition that is a graph is not edited as a stack", () => {
  const e = createEditor({ document: simple() });
  const effect = { id: "alpha2.effect.invert", version: 1, params: {} };
  e.dispatch({ type: "effect.add", id: "c", instanceId: "one", effect });
  const project = structuredClone(e.document.project);
  project.compositions[0].nodes.push({ id: "loose", op: { type: "clip" } });
  assert.equal(
    effectStack(project, project.timeline.tracks[0].clips[0]),
    undefined,
  );
});
test("clip.metadata renames a clip and respects track locks", () => {
  const e = createEditor({ document: simple() });
  assert.equal(
    e.dispatch({ type: "clip.metadata", id: "c", patch: { name: "Intro" } }).ok,
    true,
  );
  assert.equal(e.document.metadata.clips.c.name, "Intro");
  assert.equal(
    e.dispatch({ type: "clip.metadata", id: "missing", patch: { name: "x" } })
      .ok,
    false,
  );
  e.dispatch({ type: "track.metadata", id: "t", patch: { locked: true } });
  const locked = e.dispatch({
    type: "clip.metadata",
    id: "c",
    patch: { name: "Other" },
  });
  assert.equal(locked.ok, false);
  assert.equal(locked.error.code, "locked");
});

test("a clip's cache intent is set by clip.update and auto stays implicit", () => {
  const editor = createEditor({ document: simple() });
  const clip = () => editor.document.project.timeline.tracks[0].clips[0];
  editor.dispatch({ type: "clip.update", id: "c", patch: { cache: "pin" } });
  assert.equal(clip().cache, "pin");
  editor.dispatch({ type: "clip.update", id: "c", patch: { cache: "auto" } });
  assert.equal(
    "cache" in clip(),
    false,
    "auto is the default the engine omits",
  );
  assert.equal(editor.undo(), true);
  assert.equal(clip().cache, "pin");
});

test("a document that validates saves to a file that opens again", () => {
  // Indented, many small deep objects would take several times their compact size.
  const cues = Array.from({ length: 1_400_000 }, (_, i) => ({ t: i }));
  const base = simple();
  const big = {
    ...base,
    assets: {
      ...base.assets,
      notes: {
        id: "notes",
        kind: "generated",
        name: "Notes",
        contentRevision: 0,
        authoring: { cues },
      },
    },
  };
  const text = serializeDocument(big);
  assert.ok(text.length > 8 * 1024 * 1024, "the file is near its limit");
  assert.equal(
    parseDocument(text).assets.notes.authoring.cues.length,
    cues.length,
  );
  // Past the file limit even compact: refused where it is made, not on the next open.
  const huge = {
    ...big,
    assets: {
      ...big.assets,
      notes: {
        ...big.assets.notes,
        authoring: { cues, more: "x".repeat(120 * 1024 * 1024) },
      },
    },
  };
  assert.throws(
    () => serializeDocument(huge),
    (error) => error.code === "too_large",
  );
});

test("composition clocks take any finite time, a fraction as the tick it falls in", async () => {
  const { compositionTime, timelineTime } = await import("../dist/index.js");
  const clip = {
    id: "c",
    source_id: "s",
    timeline_range: { start: 1000, end: 9000 },
    source_range: { start: 0, end: 8000 },
    composition: { id: "comp", offset: 50, scale: { num: 2, den: 1 } },
  };
  // A curve samples between ticks: floating-point noise or a zoom's fraction.
  assert.equal(compositionTime(clip, 1500.75), compositionTime(clip, 1500));
  assert.equal(
    compositionTime(clip, 64999.99999999999),
    compositionTime(clip, 64999),
  );
  assert.equal(compositionTime(clip, 1500), 50 + 1000);
  assert.equal(timelineTime(clip, 1050.5), timelineTime(clip, 1051));
  assert.equal(timelineTime(clip, 1051), 1000 + 501);
  for (const bad of [Number.NaN, Infinity])
    assert.throws(
      () => compositionTime(clip, bad),
      (e) => e.code === "invalid_time",
    );
});

test("composition clocks map time as the engine does, by the shared corpus", async () => {
  const { compositionTime, timelineTime } = await import("../dist/index.js");
  const corpus = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/composition-time-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  for (const { name, offset, scale, start, time, first } of corpus.cases) {
    const clip = {
      id: "c",
      source_id: "s",
      timeline_range: { start, end: start + 1 },
      source_range: { start: 0, end: 1 },
      composition: { id: "comp", offset, ...(scale ? { scale } : {}) },
    };
    for (const [tick, expected] of time)
      assert.equal(compositionTime(clip, tick), expected, `${name}: ${tick}`);
    for (const [at, expected] of first)
      assert.equal(timelineTime(clip, at), expected, `${name}: ${at}`);
  }
});

test("names and text a view shows as text are refused when they are not text", () => {
  const base = simple();
  const asset = {
    id: "t",
    kind: "text",
    name: "Title",
    contentRevision: 0,
    authoring: { text: { content: "Hello", fontFamily: "sans-serif" } },
  };
  const valid = { ...base, assets: { ...base.assets, t: asset } };
  assert.doesNotThrow(() => serializeDocument(valid));
  for (const text of [null, 'TITLE "cube"\nLUT_3D_SIZE 2'])
    assert.doesNotThrow(() =>
      serializeDocument({
        ...valid,
        assets: {
          ...valid.assets,
          t: { ...asset, kind: "audio", authoring: { text } },
        },
      }),
    );
  const invalid = [
    [
      "a clip name",
      {
        ...valid,
        metadata: {
          ...valid.metadata,
          clips: { c: { name: { html: "<b>" } } },
        },
      },
    ],
    [
      "an asset name",
      { ...valid, assets: { ...valid.assets, t: { ...asset, name: 7 } } },
    ],
    [
      "an asset kind",
      {
        ...valid,
        assets: { ...valid.assets, t: { ...asset, kind: ["text"] } },
      },
    ],
    [
      "text content",
      {
        ...valid,
        assets: {
          ...valid.assets,
          t: { ...asset, authoring: { text: { content: { a: 1 } } } },
        },
      },
    ],
    [
      "a font family",
      {
        ...valid,
        assets: {
          ...valid.assets,
          t: { ...asset, authoring: { text: { content: "x", fontFamily: 3 } } },
        },
      },
    ],
    [
      "instance ids",
      {
        ...valid,
        metadata: { ...valid.metadata, instanceIds: { "v/cues": "a" } },
      },
    ],
    ["clip metadata", { ...valid, metadata: { ...valid.metadata, clips: [] } }],
  ];
  for (const [what, document] of invalid)
    assert.throws(
      () => serializeDocument(document),
      (error) => ["invalid_metadata", "invalid_asset"].includes(error.code),
      what,
    );
});

test("a marker's color is a #rrggbb color, so a view can paint it and request nothing", () => {
  const document = createEmptyDocument();
  const open = (marker) =>
    parseDocument(
      JSON.stringify({
        ...document,
        metadata: { ...document.metadata, markers: [marker] },
      }),
    );
  const marker = { id: "m", time: 0, end: 120000, name: "Intro" };
  assert.equal(
    open({ ...marker, color: "#F2a93b" }).metadata.markers[0].color,
    "#F2a93b",
  );
  assert.equal(open(marker).metadata.markers[0].color, undefined);
  for (const color of [
    "url(https://example.invalid/beacon)",
    "red",
    "#fff",
    "#f2a93b80",
    "#f2a93b; background-image: url(x)",
    "",
  ])
    assert.throws(
      () => open({ ...marker, color }),
      (error) => error.code === "invalid_metadata",
      color,
    );
  assert.throws(
    () => open(null),
    (error) => error.code === "invalid_metadata",
  );
});

test("a locked track keeps the bin assets and effect resources it draws, and only those", () => {
  // The lock covers the text assets and LUTs a locked track's clips read, not just the track.
  const base = simple();
  const document = {
    ...base,
    project: {
      ...base.project,
      sources: [
        ...base.project.sources,
        { id: "title", kind: "image", locator: "text://title" },
        { id: "free", kind: "image", locator: "text://free" },
      ],
      effect_resources: [
        { id: "lut", kind: "lut3d", locator: "test://look.cube" },
        { id: "other-lut", kind: "lut3d", locator: "test://other.cube" },
      ],
      timeline: {
        tracks: [
          {
            ...base.project.timeline.tracks[0],
            clips: [
              ...base.project.timeline.tracks[0].clips,
              {
                id: "words",
                source_id: "title",
                timeline_range: { start: 1200000, end: 1320000 },
                source_range: { start: 0, end: 120000 },
              },
            ],
          },
          base.project.timeline.tracks[1],
        ],
      },
    },
    assets: {
      title: {
        id: "title",
        kind: "text",
        name: "Title",
        contentRevision: 1,
        authoring: { text: { content: "Hello" } },
      },
      free: {
        id: "free",
        kind: "text",
        name: "Free",
        contentRevision: 1,
        authoring: { text: { content: "Free" } },
      },
      lut: { id: "lut", kind: "lut3d", name: "look.cube", contentRevision: 1 },
      "other-lut": {
        id: "other-lut",
        kind: "lut3d",
        name: "other.cube",
        contentRevision: 1,
      },
    },
  };
  const e = createEditor({ document: createDocument(document.project) });
  e.open(document);
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({
    type: "effect.add",
    id: "c",
    instanceId: "look",
    effect: {
      id: "alpha2.effect.lut-3d",
      version: 1,
      params: {},
      resources: { lut: "lut" },
    },
  });
  ok({ type: "track.metadata", id: "t", patch: { locked: true } });
  const refused = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, false, command.type);
    assert.equal(result.error.code, "locked", command.type);
    assert.equal(result.error.entityId, "t");
  };
  const text = (id, content) => ({
    type: "asset.put",
    asset: {
      ...e.document.assets[id],
      contentRevision: e.document.assets[id].contentRevision + 1,
      authoring: { text: { content } },
    },
  });
  refused(text("title", "Changed"));
  refused({
    type: "asset.put",
    asset: { ...e.document.assets.lut, contentRevision: 2 },
    resource: { id: "lut", kind: "lut3d", locator: "test://replaced.cube" },
  });
  // What the locked track does not draw is not locked with it.
  ok(text("free", "Changed"));
  ok({
    type: "asset.put",
    asset: { ...e.document.assets["other-lut"], contentRevision: 2 },
    resource: {
      id: "other-lut",
      kind: "lut3d",
      locator: "test://replaced.cube",
    },
  });
});

test("the track lock costs little when an edit leaves what it guards as it was", () => {
  // Guards against scanning every composition once per locked composition (seconds at 50 clips).
  const effect = { id: "alpha2.effect.invert", version: 1, params: {} };
  const tracks = Array.from({ length: 4 }, (_, k) => ({
    id: `t${k}`,
    name: `t${k}`,
    kind: "video",
    clips: Array.from({ length: 250 }, (_, i) => ({
      id: `c${k}-${i}`,
      source_id: "s",
      timeline_range: { start: i * 120000, end: i * 120000 + 60000 },
      source_range: { start: 0, end: 60000 },
    })),
  }));
  const raw = structuredClone(simple().project);
  raw.timeline.tracks = tracks;
  const e = createEditor({ document: createDocument(raw) });
  assert.equal(
    e.dispatch({
      type: "transaction",
      commands: tracks.flatMap((t) =>
        t.clips.map((c) => ({
          type: "effect.add",
          id: c.id,
          instanceId: `fx-${c.id}`,
          effect,
        })),
      ),
    }).ok,
    true,
  );
  const unlocked = e.document;
  for (const id of ["t1", "t2", "t3"])
    e.dispatch({ type: "track.metadata", id, patch: { locked: true } });
  const locked = e.document;
  const updates = {
    type: "transaction",
    commands: tracks[0].clips.slice(0, 50).map((c) => ({
      type: "clip.update",
      id: c.id,
      patch: { blend_mode: "screen" },
    })),
  };
  const time = (document) => {
    let best = Infinity;
    for (let i = 0; i < 3; i += 1) {
      const started = performance.now();
      assert.equal(executeCommand(document, updates).ok, true);
      best = Math.min(best, performance.now() - started);
    }
    return best;
  };
  const free = time(unlocked);
  const guarded = time(locked);
  assert.ok(
    guarded < free * 3 + 20,
    `${guarded.toFixed(1)} ms with three locked tracks, ${free.toFixed(1)} ms without`,
  );
});

test("a transaction validates where it lands once, and an edit's own check is not repeated", () => {
  // A transaction validates once, where it lands, not after each command.
  const S = 120000;
  const clips = Array.from({ length: 20 }, (_, i) => ({
    id: `c${i}`,
    source_id: "s",
    timeline_range: { start: i * S, end: (i + 1) * S },
    source_range: { start: 0, end: S },
  }));
  const raw = structuredClone(simple().project);
  raw.timeline.tracks = [{ id: "t", name: "Video", kind: "video", clips }];
  const document = createDocument(raw);
  // Validation reads every source's locator; the edits do not.
  let reads = 0;
  const source = { ...document.project.sources[0] };
  const { locator } = source;
  Object.defineProperty(source, "locator", {
    enumerable: true,
    get() {
      reads += 1;
      return locator;
    },
  });
  const counted = {
    ...document,
    project: { ...document.project, sources: [source] },
  };
  const split = executeCommand(counted, {
    type: "transaction",
    commands: clips.map((c, i) => ({
      type: "clip.split",
      id: c.id,
      at: i * S + S / 2,
      newId: `right-${i}`,
    })),
  });
  assert.equal(split.ok, true, split.error?.message);
  assert.ok(reads < clips.length / 2, `${reads} reads for 20 splits`);
  reads = 0;
  const moved = executeCommand(
    counted,
    { type: "clip.move", id: "c19", at: 21 * S },
    { gesture: true },
  );
  assert.equal(moved.ok, true, moved.error?.message);
  assert.equal(reads, 1, "the move's own validation only");
  // Only where the transaction lands is checked: two clips that meet move right in either order.
  const room = executeCommand(document, {
    type: "transaction",
    commands: [
      { type: "clip.delete", id: "c2" },
      { type: "clip.delete", id: "c3" },
    ],
  });
  assert.equal(room.ok, true);
  for (const order of [
    ["c0", "c1"],
    ["c1", "c0"],
  ]) {
    const pair = executeCommand(room.value, {
      type: "transaction",
      commands: order.map((id) => ({
        type: "clip.move",
        id,
        at: (id === "c0" ? 1 : 2) * S,
      })),
    });
    assert.equal(pair.ok, true, `${order}: ${pair.error?.message}`);
  }
  const onto = executeCommand(document, {
    type: "transaction",
    commands: [{ type: "clip.move", id: "c0", at: S / 2 }],
  });
  assert.equal(onto.ok, false);
  assert.equal(onto.error.code, "invalid_project");
  assert.match(onto.error.message, /overlap/);
});

test("track flags, node positions, group names and asset locators a file holds must be what their types say", () => {
  const file = JSON.parse(serializeDocument(simple()));
  const refused = {
    "a track flag that is not a boolean": (d) =>
      (d.metadata.tracks = { t: { locked: "yes" } }),
    "a sync lock that is not a boolean": (d) =>
      (d.metadata.tracks = { t: { syncLock: 1 } }),
    "a track color that is not #rrggbb": (d) =>
      (d.metadata.tracks = { t: { color: "url(https://example.test/b.png)" } }),
    "track metadata that is not an object": (d) =>
      (d.metadata.tracks = { t: 5 }),
    "a node position that is not one": (d) =>
      (d.metadata.graphLayout = { comp: { n: null } }),
    "a node position off the canvas's numbers": (d) =>
      (d.metadata.graphLayout = { comp: { n: { x: "1", y: 2 } } }),
    "node positions that are not by composition": (d) =>
      (d.metadata.graphLayout = [1]),
    "a group name that is not text": (d) =>
      (d.metadata.graphNames = { comp: [1, 2] }),
    "an asset locator that is not text": (d) => (d.assets.s.locator = { x: 1 }),
    "an offline flag that is not a boolean": (d) => (d.assets.s.missing = "no"),
  };
  for (const [what, change] of Object.entries(refused)) {
    const changed = structuredClone(file);
    change(changed);
    assert.throws(
      () => parseDocument(JSON.stringify(changed)),
      (e) =>
        e instanceof EditorError &&
        ["invalid_metadata", "invalid_asset"].includes(e.code),
      what,
    );
  }
  const valid = structuredClone(file);
  valid.metadata.tracks = {
    t: { locked: true, enabled: false, syncLock: true, color: "#aa00FF" },
  };
  valid.metadata.graphLayout = { comp: { n: { x: -3.5, y: 2 } } };
  valid.metadata.graphNames = { comp: "Look" };
  valid.assets.s = { ...valid.assets.s, locator: "browser://s", missing: true };
  assert.doesNotThrow(() => parseDocument(JSON.stringify(valid)));
});

test("a malformed file is refused with an editor error naming the check it fails", () => {
  const file = JSON.parse(serializeDocument(simple()));
  const project = file.project;
  const deep = (levels) => "[".repeat(levels) + "]".repeat(levels);
  const refused = {
    "{": "invalid_document",
    null: "invalid_document",
    "[]": "invalid_document",
    '"text"': "invalid_document",
    '{"schema_version":99}': "invalid_project",
    [JSON.stringify({ ...project, timeline: { tracks: {} } })]:
      "invalid_project",
    [JSON.stringify({ ...project, sources: undefined })]: "invalid_project",
    [JSON.stringify({
      ...project,
      timeline: {
        tracks: [
          { id: "c", name: "C", kind: "caption", captions: { cues: "x" } },
        ],
      },
    })]: "invalid_project",
    [JSON.stringify({ ...file, assets: { ...file.assets, x: null } })]:
      "invalid_asset",
    [JSON.stringify({ ...file, assets: [] })]: "invalid_metadata",
    [JSON.stringify({
      ...file,
      metadata: { ...file.metadata, markers: [null] },
    })]: "invalid_metadata",
    // Deeper than the engine reads, well inside the file size limit.
    [JSON.stringify({
      ...file,
      metadata: { ...file.metadata, graphLayout: "@" },
    }).replace('"@"', deep(20000))]: "invalid_document",
    [JSON.stringify({
      ...project,
      name: "@",
    }).replace('"@"', deep(20000))]: "invalid_document",
  };
  for (const [text, code] of Object.entries(refused))
    assert.throws(
      () => parseDocument(text),
      (e) => e instanceof EditorError && e.code === code,
      text.slice(0, 80),
    );
  // Up to the engine's depth is read: a document is one level deeper than its project.
  const nested = (levels) => {
    let value = 0;
    for (let i = 0; i < levels; i += 1) value = [value];
    return value;
  };
  const authored = structuredClone(file);
  authored.assets.s = { ...authored.assets.s, authoring: nested(125) };
  assert.doesNotThrow(() => parseDocument(JSON.stringify(authored)));
  authored.assets.s = { ...authored.assets.s, authoring: nested(126) };
  assert.throws(
    () => parseDocument(JSON.stringify(authored)),
    (e) => e.code === "invalid_document",
  );
});

test("hiding a track another reads from keeps what it reads, and a file with one hidden opens and edits", () => {
  const clip = (id, extra = {}) => ({
    id,
    source_id: "s",
    timeline_range: { start: 0, end: 120000 },
    source_range: { start: 0, end: 120000 },
    ...extra,
  });
  const ducking = {
    sidechain_track: "vo",
    threshold: 0.1,
    depth_db: -12,
    attack_ms: 10,
    release_ms: 200,
  };
  const document = createDocument({
    schema_version: 1,
    id: "p",
    name: "Hidden",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "s", kind: "video", locator: "test://s", has_audio: true },
      { id: "voice", kind: "audio", locator: "test://voice" },
    ],
    timeline: {
      tracks: [
        {
          id: "fg",
          name: "Foreground",
          kind: "video",
          clips: [clip("a", { matte: { source: "b", mode: "alpha" } })],
        },
        {
          id: "mt",
          name: "Matte",
          kind: "video",
          clips: [
            clip("b"),
            clip("x", { timeline_range: { start: 120000, end: 240000 } }),
          ],
        },
        {
          id: "vo",
          name: "Voice",
          kind: "audio",
          clips: [clip("v", { source_id: "voice" })],
        },
        {
          id: "mus",
          name: "Music",
          kind: "audio",
          clips: [clip("m", { source_id: "voice" })],
          audio: { ducking },
        },
      ],
    },
  });
  const e = createEditor({ document });
  for (const id of ["mt", "vo"]) {
    const hidden = e.dispatch({
      type: "track.metadata",
      id,
      patch: { enabled: false },
    });
    assert.equal(hidden.ok, true, hidden.error?.message);
  }
  const { project, diagnostics } = compileEditorDocument(e.document);
  assert.deepEqual(diagnostics, []);
  const tracks = Object.fromEntries(
    project.timeline.tracks.map((t) => [t.id, t]),
  );
  // The matte stays, alone and silent; what the hidden tracks draw and play goes.
  assert.deepEqual(
    tracks.mt.clips.map((c) => c.id),
    ["b"],
  );
  assert.equal(tracks.mt.audio.muted, true);
  assert.equal(tracks.vo, undefined);
  assert.equal(tracks.mus.audio.ducking, null);
  const opened = createEditor({
    document: parseDocument(serializeDocument(e.document)),
  });
  const renamed = opened.dispatch({
    type: "project.update",
    patch: { name: "Renamed" },
  });
  assert.equal(renamed.ok, true, renamed.error?.message);
});

test("a clip only hidden clips use as their matte is still not drawn, and still plays", () => {
  const clip = (id, extra = {}) => ({
    id,
    source_id: "s",
    timeline_range: { start: 0, end: 120000 },
    source_range: { start: 0, end: 120000 },
    ...extra,
  });
  const fading = {
    position: { mode: "constant", value: { x: 0.25, y: 0.5 } },
    anchor: { x: 0.5, y: 0.5 },
    scale: { mode: "constant", value: { x: 1, y: 1 } },
    rotation_degrees: { mode: "constant", value: 0 },
    opacity: {
      mode: "keyframes",
      keyframes: [
        { time: 0, value: 0, interpolation: { type: "linear" } },
        { time: 60000, value: 1, interpolation: { type: "hold" } },
      ],
    },
  };
  const document = createDocument({
    schema_version: 1,
    id: "p",
    name: "Mattes",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "s", kind: "video", locator: "test://s", has_audio: true }],
    timeline: {
      tracks: [
        {
          id: "fg",
          name: "Foreground",
          kind: "video",
          clips: [
            clip("a", { matte: { source: "b", mode: "alpha" } }),
            clip("a2", {
              timeline_range: { start: 120000, end: 240000 },
              matte: { source: "w", mode: "luma" },
            }),
            clip("a3", {
              timeline_range: { start: 240000, end: 360000 },
              matte: { source: "h", mode: "alpha" },
            }),
            clip("a4", {
              timeline_range: { start: 360000, end: 480000 },
              matte: { source: "n", mode: "alpha" },
            }),
          ],
        },
        {
          id: "fg2",
          name: "Also",
          kind: "video",
          clips: [
            clip("c", {
              timeline_range: { start: 120000, end: 240000 },
              matte: { source: "w", mode: "alpha" },
            }),
          ],
        },
        {
          id: "mt",
          name: "Mattes",
          kind: "video",
          clips: [
            clip("b", {
              visual: fading,
              audio: { gain: { mode: "constant", value: 0.5 } },
            }),
            clip("w", { timeline_range: { start: 120000, end: 240000 } }),
            clip("n", { timeline_range: { start: 360000, end: 480000 } }),
          ],
        },
        {
          id: "hm",
          name: "Hidden mattes",
          kind: "video",
          clips: [
            clip("h", { timeline_range: { start: 240000, end: 360000 } }),
          ],
        },
      ],
    },
  });
  const e = createEditor({ document });
  for (const id of ["fg", "hm"])
    assert.equal(
      e.dispatch({ type: "track.metadata", id, patch: { enabled: false } }).ok,
      true,
    );
  const { project, diagnostics } = compileEditorDocument(e.document);
  assert.deepEqual(diagnostics, []);
  const tracks = Object.fromEntries(
    project.timeline.tracks.map((t) => [t.id, t]),
  );
  const clips = Object.fromEntries(
    project.timeline.tracks.flatMap((t) => t.clips ?? []).map((c) => [c.id, c]),
  );
  // b mattes only the hidden a: it draws nothing but keeps its sound and other settings.
  assert.deepEqual(clips.b.visual, {
    ...fading,
    opacity: { mode: "constant", value: 0 },
  });
  assert.deepEqual(clips.b.audio, { gain: { mode: "constant", value: 0.5 } });
  assert.equal(tracks.mt.audio?.muted ?? false, false);
  assert.equal(clips.w, e.document.project.timeline.tracks[2].clips[1]);
  // A clip without visual properties gets the engine's defaults, with no opacity.
  assert.deepEqual(clips.n.visual, {
    position: { mode: "constant", value: { x: 0.5, y: 0.5 } },
    anchor: { x: 0.5, y: 0.5 },
    scale: { mode: "constant", value: { x: 1, y: 1 } },
    rotation_degrees: { mode: "constant", value: 0 },
    opacity: { mode: "constant", value: 0 },
  });
  assert.equal(tracks.hm, undefined);
  assert.equal(tracks.fg, undefined);
  assert.equal(
    e.dispatch({ type: "track.metadata", id: "fg", patch: { enabled: true } })
      .ok,
    true,
  );
  const shown = compileEditorDocument(e.document).project;
  assert.deepEqual(
    shown.timeline.tracks.find((t) => t.id === "mt").clips[0].visual,
    fading,
  );
});

test("a pasted clip keeps the name its original had when copied", () => {
  const e = createEditor({ document: simple() });
  assert.equal(
    e.dispatch({ type: "clip.metadata", id: "c", patch: { name: "Intro" } }).ok,
    true,
  );
  e.copy(["c"]);
  e.dispatch({ type: "clip.metadata", id: "c", patch: { name: "Later" } });
  const pasted = e.paste(1200000, () => "copy");
  assert.equal(pasted.ok, true, pasted.error?.message);
  assert.deepEqual(e.document.metadata.clips.copy, { name: "Intro" });
});

test("pasting with nothing copied, or any command that changes nothing, keeps the history", () => {
  const e = createEditor({ document: simple() });
  e.dispatch({ type: "project.update", patch: { name: "Renamed" } });
  e.undo();
  const before = e.getSnapshot();
  assert.equal(before.canRedo, true);
  const pasted = e.paste(0);
  assert.equal(pasted.ok, true);
  assert.equal(pasted.value, e.document);
  assert.equal(e.dispatch({ type: "transaction", commands: [] }).ok, true);
  const after = e.getSnapshot();
  assert.equal(after.revision, before.revision);
  assert.equal(after.canUndo, before.canUndo);
  assert.equal(after.canRedo, true);
  assert.equal(e.redo(), true);
  assert.equal(e.document.project.name, "Renamed");
});

test("a document of 10,000 clips is made and opened within its budget", () => {
  // Guards against quadratic mask naming (10 s for 10,000 clips).
  const S = 120000;
  const tracks = [0, 1, 2].map((t) => ({
    id: `V${t}`,
    name: `Video ${t}`,
    kind: "video",
    clips: Array.from({ length: t < 2 ? 4_000 : 2_000 }, (_, i) => ({
      id: `c${t}-${i}`,
      source_id: "s",
      timeline_range: { start: i * S, end: (i + 1) * S },
      source_range: { start: 0, end: S },
    })),
  }));
  const project = {
    schema_version: 1,
    id: "p",
    name: "Big",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "s", kind: "video", locator: "test://s" }],
    timeline: { tracks },
  };
  // The fastest of three, each of a new project object so no check is skipped as known.
  const best = (run) => {
    let elapsed = Infinity;
    let result;
    for (let i = 0; i < 3; i += 1) {
      const started = performance.now();
      result = run();
      elapsed = Math.min(elapsed, performance.now() - started);
    }
    return { elapsed, result };
  };
  const made = best(() => createDocument({ ...project }));
  assert.equal(made.result.metadata.instanceIds["c0-0/masks"].length, 0);
  // A file without the ids gets them on open.
  const file = JSON.parse(serializeDocument(made.result));
  file.metadata.instanceIds = {};
  const text = JSON.stringify(file);
  const opened = best(() => parseDocument(text));
  assert.deepEqual(
    opened.result.metadata.instanceIds,
    made.result.metadata.instanceIds,
  );
  // About 0.1 s each here; the budget is for the quadratic regression, not for the machine.
  assert.ok(made.elapsed < 1_000, `made in ${made.elapsed.toFixed(1)} ms`);
  assert.ok(
    opened.elapsed < 1_000,
    `opened in ${opened.elapsed.toFixed(1)} ms`,
  );
});

test("a gesture step on a 10,000-clip document costs a small part of validating it", () => {
  // Guards against each step revalidating the whole project and renaming every clip's masks.
  const S = 120000;
  const tracks = [0, 1, 2].map((t) => ({
    id: `V${t}`,
    name: `Video ${t}`,
    kind: "video",
    clips: Array.from({ length: t < 2 ? 4_000 : 2_000 }, (_, i) => ({
      id: `c${t}-${i}`,
      source_id: "s",
      timeline_range: { start: 2 * i * S, end: (2 * i + 1) * S },
      source_range: { start: 0, end: S },
    })),
  }));
  const project = {
    schema_version: 1,
    id: "p",
    name: "Big",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "s", kind: "video", locator: "test://s" }],
    timeline: { tracks },
  };
  const copies = Array.from({ length: 5 }, () => structuredClone(project));
  const median = (times) => times.sort((a, b) => a - b)[times.length >> 1];
  const timed = (count, run) =>
    median(
      Array.from({ length: count }, (_, i) => {
        const started = performance.now();
        run(i);
        return performance.now() - started;
      }),
    );
  const whole = timed(5, (i) => validateProject(copies[i]));
  const e = createEditor({ document: createDocument(project) });
  for (const step of [
    (at) => ({ type: "clip.move", id: "c0-0", at }),
    (at) => ({
      type: "transaction",
      commands: [{ type: "clips.move", moves: [{ id: "c0-0", at }] }],
    }),
  ]) {
    const gesture = e.beginGesture();
    const update = timed(20, (i) => {
      assert.equal(gesture.update(step((i + 1) * 100)).ok, true);
    });
    gesture.cancel();
    assert.ok(
      update < whole / 4,
      `${step(0).type}: a step ${update.toFixed(2)} ms, validating ${whole.toFixed(2)} ms`,
    );
  }
});

test("masks a command or gesture gives a clip of a frozen document are named, and undo returns the names", () => {
  const mask = {
    shape: { type: "rectangle" },
    center: { mode: "constant", value: { x: 0.5, y: 0.5 } },
    size: { mode: "constant", value: { x: 0.5, y: 0.5 } },
  };
  const e = createEditor({ document: simple() });
  const ids = () => e.document.metadata.instanceIds["c/masks"];
  assert.deepEqual(ids(), []);
  const gesture = e.beginGesture();
  assert.equal(
    gesture.update({ type: "clip.update", id: "c", patch: { masks: [mask] } })
      .ok,
    true,
  );
  assert.deepEqual(e.draft.metadata.instanceIds["c/masks"], ["c/masks/0"]);
  gesture.cancel();
  assert.deepEqual(ids(), []);
  e.dispatch({ type: "clip.update", id: "c", patch: { masks: [mask, mask] } });
  assert.deepEqual(ids(), ["c/masks/0", "c/masks/1"]);
  e.dispatch({ type: "clip.update", id: "c", patch: { masks: [mask] } });
  assert.deepEqual(ids(), ["c/masks/0"]);
  e.undo();
  assert.deepEqual(ids(), ["c/masks/0", "c/masks/1"]);
  assert.equal(createEditor({ document: e.document }).document, e.document);
  const reopened = createEditor({
    document: {
      ...e.document,
      metadata: { ...e.document.metadata, instanceIds: Object.freeze({}) },
    },
  });
  assert.deepEqual(reopened.document.metadata.instanceIds["c/masks"], [
    "c/masks/0",
    "c/masks/1",
  ]);
});
