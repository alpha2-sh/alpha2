import test from "node:test";
import assert from "node:assert/strict";
import {
  createDocument,
  createEditor,
  dataValue,
  isDataBound,
  moveKeyframe,
  EditorError,
} from "../dist/index.js";

const SECOND = 240000;

const points = (id, source = "s") => ({
  id,
  kind: "points",
  anchor: { type: "source", source_id: source },
  channels: ["face"],
  interpolation: "linear",
  times: [0, SECOND],
  values: [0.25, 0.5, 0.75, 0.5],
});

const levels = (id, values = [0, 1]) => ({
  id,
  kind: "scalars",
  anchor: { type: "timeline" },
  channels: ["level"],
  interpolation: "linear",
  times: [0, SECOND],
  values,
});

const clip = (id, start, end) => ({
  id,
  source_id: "s",
  timeline_range: { start, end },
  source_range: { start, end },
  time_map: { type: "forward" },
});

const document = (data = []) =>
  createDocument({
    schema_version: 1,
    id: "p",
    name: "Data",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "s", kind: "video", locator: "test://s" },
      { id: "s2", kind: "video", locator: "test://s2" },
    ],
    timeline: {
      tracks: [
        { id: "v1", name: "V1", kind: "video", clips: [clip("c", 0, SECOND)] },
        {
          id: "v2",
          name: "V2",
          kind: "video",
          clips: [clip("d", 0, SECOND)],
        },
      ],
    },
    ...(data.length ? { data } : {}),
  });

const follow = (data, via = "c", space = "canvas") =>
  dataValue({ data, channel: "face", via: { clip: via }, space });

const bindPosition = (id, value) => ({
  type: "clip.update",
  id,
  patch: { visual: { position: value } },
});

test("data.put adds a set, replaces one with its id in place, and undoes", () => {
  const editor = createEditor({ document: document([levels("a")]) });
  assert.ok(editor.dispatch({ type: "data.put", dataSet: levels("b") }).ok);
  assert.deepEqual(
    editor.document.project.data.map((set) => set.id),
    ["a", "b"],
  );
  assert.ok(
    editor.dispatch({ type: "data.put", dataSet: levels("a", [0.5, 0.5]) }).ok,
  );
  assert.deepEqual(
    editor.document.project.data.map((set) => [set.id, set.values[0]]),
    [
      ["a", 0.5],
      ["b", 0],
    ],
  );
  assert.ok(editor.undo());
  assert.equal(editor.document.project.data[0].values[0], 0);
  assert.ok(editor.undo());
  assert.deepEqual(
    editor.document.project.data.map((set) => set.id),
    ["a"],
  );
  assert.ok(editor.redo());
  assert.ok(editor.redo());
  assert.equal(editor.document.project.data[0].values[0], 0.5);
});

test("data.put is validated like every command", () => {
  const editor = createEditor({ document: document() });
  const result = editor.dispatch({
    type: "data.put",
    dataSet: { ...levels("a"), values: [0] },
  });
  assert.equal(result.ok, false);
  assert.equal(result.error.code, "invalid_project");
  assert.match(result.error.message, /data set a/);
});

test("a set and a binding to it go in as one undo step", () => {
  const editor = createEditor({ document: document() });
  const alone = editor.dispatch(bindPosition("d", follow("track")));
  assert.equal(alone.ok, false);
  assert.match(alone.error.message, /unknown data set/);
  const before = editor.document;
  const result = editor.dispatch({
    type: "transaction",
    commands: [
      { type: "data.put", dataSet: points("track") },
      bindPosition("d", follow("track")),
    ],
  });
  assert.ok(result.ok, result.error?.message);
  const bound = editor.query().clips.get("d").clip.visual.position;
  assert.ok(isDataBound(bound));
  assert.ok(editor.undo());
  assert.equal(editor.document, before);
});

test("data.remove is refused while read, listing the places", () => {
  const editor = createEditor({ document: document([points("track")]) });
  assert.ok(editor.dispatch(bindPosition("d", follow("track"))).ok);
  const result = editor.dispatch({ type: "data.remove", id: "track" });
  assert.equal(result.ok, false);
  assert.ok(result.error instanceof EditorError);
  assert.equal(result.error.code, "data_referenced");
  assert.equal(result.error.entityId, "track");
  assert.match(result.error.message, /clip d visual\.position/);
  assert.deepEqual(result.error.details.references, [
    {
      data: "track",
      owner: { kind: "clip", clip: "d" },
      property: "visual.position",
      via: { clip: "c" },
    },
  ]);
  const unknown = editor.dispatch({ type: "data.remove", id: "nope" });
  assert.equal(unknown.error.code, "invalid_command");
});

test("data.remove after the binding goes in the same transaction", () => {
  const editor = createEditor({ document: document([points("track")]) });
  assert.ok(editor.dispatch(bindPosition("d", follow("track"))).ok);
  const result = editor.dispatch({
    type: "transaction",
    commands: [
      bindPosition("d", { mode: "constant", value: { x: 0.5, y: 0.5 } }),
      { type: "data.remove", id: "track" },
    ],
  });
  assert.ok(result.ok, result.error?.message);
  assert.equal("data" in result.value.project, false);
});

test("asset.remove is refused while data sets are anchored to its source", () => {
  const start = document([points("other", "s2")]);
  const editor = createEditor({
    document: {
      ...start,
      assets: {
        s2: { id: "s2", kind: "video", name: "s2", contentRevision: 0 },
      },
    },
  });
  const refused = editor.dispatch({ type: "asset.remove", id: "s2" });
  assert.equal(refused.error.code, "data_referenced");
  assert.deepEqual(refused.error.details.dataSets, ["other"]);
  const result = editor.dispatch({
    type: "transaction",
    commands: [
      { type: "data.remove", id: "other" },
      { type: "asset.remove", id: "s2" },
    ],
  });
  assert.ok(result.ok, result.error?.message);
  assert.equal(
    result.value.project.sources.some((s) => s.id === "s2"),
    false,
  );
});

test("removing a clip another binding reads through is refused with the binding", () => {
  const editor = createEditor({ document: document([points("track")]) });
  assert.ok(editor.dispatch(bindPosition("d", follow("track"))).ok);
  for (const command of [
    { type: "clip.delete", id: "c" },
    { type: "clip.delete", id: "c", ripple: true },
    { type: "track.remove", id: "v1" },
  ]) {
    const result = editor.dispatch(command);
    assert.equal(result.ok, false, command.type);
    assert.equal(result.error.code, "data_referenced");
    assert.match(result.error.message, /clip d visual\.position/);
  }
  assert.ok(
    editor.dispatch(bindPosition("c", follow("track", "c", "picture"))).ok,
  );
  assert.ok(
    editor.dispatch(bindPosition("d", follow("track", "d", "picture"))).ok,
  );
  assert.ok(editor.dispatch({ type: "clip.delete", id: "c" }).ok);
});

test("a locked track refuses a data change it reads, and no other", () => {
  const editor = createEditor({
    document: document([levels("a"), levels("b")]),
  });
  assert.ok(
    editor.dispatch({
      type: "clip.update",
      id: "d",
      patch: {
        visual: { opacity: dataValue({ data: "a", channel: "level" }) },
      },
    }).ok,
  );
  assert.ok(
    editor.dispatch({
      type: "track.metadata",
      id: "v2",
      patch: { locked: true },
    }).ok,
  );
  const read = editor.dispatch({
    type: "data.put",
    dataSet: levels("a", [1, 1]),
  });
  assert.equal(read.error?.code, "locked");
  assert.ok(
    editor.dispatch({ type: "data.put", dataSet: levels("b", [1, 1]) }).ok,
  );
});

test("keyframe helpers refuse a data-bound value with data_bound", () => {
  const value = dataValue({ data: "a", channel: "level" });
  assert.deepEqual(value, { mode: "data", data: "a", channel: "level" });
  assert.equal(isDataBound(value), true);
  assert.equal(isDataBound({ mode: "constant", value: 1 }), false);
  assert.throws(
    () => moveKeyframe(value, 0, 10),
    (error) =>
      error instanceof EditorError &&
      error.code === "data_bound" &&
      error.entityId === "a",
  );
});
