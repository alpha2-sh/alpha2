import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { applyEdit } from "alpha2/apply-edit";
import { createDocument, createEditor, dataValue } from "../dist/index.js";
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

const FRAME = 8000; // 30 fps
const at = (frames) => frames * FRAME;

const clip = (id, start, end, extra = {}) => ({
  id,
  source_id: "s",
  timeline_range: { start: at(start), end: at(end) },
  source_range: { start: at(start), end: at(end) },
  time_map: { type: "forward" },
  ...extra,
});

/** A timeline-anchored set of each kind, plus a source-anchored one that edits never touch. */
const dataSets = () => [
  {
    id: "track",
    kind: "points",
    anchor: { type: "timeline" },
    channels: ["a", "b"],
    interpolation: "linear",
    times: [at(0), at(50), at(70), at(100), at(150)],
    values: [
      0.1,
      0.2,
      0.3,
      0.4,
      0.5,
      0.6,
      null,
      null,
      0.25,
      0.75,
      0.9,
      0.1,
      0.3,
      0.3,
      0.2,
      0.2,
      0.6,
      0.4,
      0.7,
      0.9,
    ],
  },
  {
    id: "level",
    kind: "scalars",
    anchor: { type: "timeline" },
    channels: ["level"],
    interpolation: "linear",
    times: [at(0), at(55), at(65), at(130)],
    values: [0, 0.333, 0.8, 1],
  },
  {
    id: "steady",
    kind: "similarity",
    anchor: { type: "timeline" },
    interpolation: "hold",
    times: [at(10), at(80)],
    values: [0.01, -0.02, 1.5, 1.1, 0.03, 0.04, -2, 0.9],
  },
  {
    id: "said",
    kind: "words",
    anchor: { type: "timeline" },
    words: [
      { start: at(40), end: at(65), text: "across" },
      { start: at(62), end: at(70), text: "inside" },
      { start: at(100), end: at(110), text: "after" },
    ],
  },
  {
    id: "quiet",
    kind: "intervals",
    anchor: { type: "timeline" },
    intervals: [
      { start: at(30), end: at(90) },
      { start: at(61), end: at(70) },
      { start: at(75), end: at(75) },
      { start: at(120), end: at(125) },
    ],
  },
  {
    id: "face",
    kind: "points",
    anchor: { type: "source", source_id: "s" },
    channels: ["face"],
    interpolation: "linear",
    times: [0, at(150)],
    values: [0.2, 0.2, 0.8, 0.8],
  },
];

const project = () => ({
  schema_version: 1,
  id: "p",
  name: "Data edits",
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
        id: "v1",
        name: "V1",
        kind: "video",
        clips: [
          clip("a", 0, 60),
          clip("b", 60, 72),
          clip("c", 72, 150, {
            visual: {
              opacity: dataValue({ data: "level", channel: "level" }),
              position: dataValue({
                data: "face",
                channel: "face",
                via: { clip: "c" },
                space: "picture",
              }),
            },
          }),
        ],
      },
      {
        id: "v2",
        name: "V2",
        kind: "video",
        clips: [clip("y", 0, 30), clip("z", 90, 120)],
      },
      {
        id: "subs",
        name: "Subs",
        kind: "caption",
        captions: {
          cues: [
            { timeline_range: { start: at(5), end: at(20) }, text: "one" },
            { timeline_range: { start: at(80), end: at(95) }, text: "two" },
          ],
        },
      },
    ],
  },
  data: dataSets(),
});

/** An editor whose ripples reach every track. */
function editor() {
  const session = createEditor({ document: createDocument(project()) });
  for (const id of ["v2", "subs"])
    assert.ok(
      session.dispatch({
        type: "track.metadata",
        id,
        patch: { syncLock: true },
      }).ok,
    );
  return session;
}

const plain = (value) => JSON.parse(JSON.stringify(value));

/** The engine's canonical form of `project`, which writes every default. */
const canonical = (wasm, project) =>
  applyEdit(wasm, plain(project), { op: "bake", targets: [] }).project;

test(
  "an editor ripple delete edits data and bindings as remove_ranges on the timeline",
  { skip },
  async () => {
    const wasm = await engine();
    const session = editor();
    const before = session.document.project;
    const result = session.dispatch({
      type: "clip.delete",
      id: "b",
      ripple: true,
    });
    assert.ok(result.ok, result.error?.message);
    const engineResult = applyEdit(wasm, plain(before), {
      op: "remove_ranges",
      ranges: {
        list: [{ start: at(60), end: at(72) }],
        anchor: { type: "timeline" },
      },
      target: { clips: ["b"] },
      ripple: "timeline",
    });
    assert.deepEqual(
      canonical(wasm, result.value.project),
      engineResult.project,
    );
  },
);

test(
  "an editor ripple trim edits data and bindings as remove_ranges on the timeline",
  { skip },
  async () => {
    const wasm = await engine();
    const session = editor();
    const before = session.document.project;
    const result = session.dispatch({
      type: "clip.trim",
      id: "a",
      edge: "end",
      at: at(52),
      ripple: true,
    });
    assert.ok(result.ok, result.error?.message);
    const engineResult = applyEdit(wasm, plain(before), {
      op: "remove_ranges",
      ranges: {
        list: [{ start: at(52), end: at(60) }],
        anchor: { type: "timeline" },
      },
      target: { clips: ["a"] },
      ripple: "timeline",
    });
    assert.deepEqual(
      canonical(wasm, result.value.project),
      engineResult.project,
    );
  },
);

test(
  "an editor split points the right part's bindings at it, as split_at does",
  { skip },
  async () => {
    const wasm = await engine();
    const session = editor();
    const before = session.document.project;
    const result = session.dispatch({
      type: "clip.split",
      id: "c",
      at: at(100),
      newId: "c.1",
    });
    assert.ok(result.ok, result.error?.message);
    const right = session.query().clips.get("c.1").clip;
    assert.deepEqual(right.visual.position.via, { clip: "c.1" });
    assert.deepEqual(session.query().clips.get("c").clip.visual.position.via, {
      clip: "c",
    });
    const engineResult = applyEdit(wasm, plain(before), {
      op: "split_at",
      times: { list: [at(100)], anchor: { type: "timeline" } },
      target: { clips: ["c"] },
    });
    assert.deepEqual(
      canonical(wasm, result.value.project),
      engineResult.project,
    );
  },
);

test("a ripple confined to some tracks leaves timeline data alone", () => {
  const session = createEditor({ document: createDocument(project()) });
  const before = session.document.project.data;
  assert.ok(
    session.dispatch({ type: "clip.delete", id: "b", ripple: true }).ok,
  );
  assert.equal(session.document.project.data, before);
});

test("a ripple that opens time keeps every moment outside the opening", () => {
  const session = editor();
  const result = session.dispatch({
    type: "clip.retime",
    id: "b",
    duration: at(22),
    ripple: true,
  });
  assert.ok(result.ok, result.error?.message);
  const data = new Map(result.value.project.data.map((set) => [set.id, set]));
  // Retiming b to 22 frames opens frames 72 to 82, where samples hold the value read at 72.
  assert.deepEqual(data.get("level").times, [
    0,
    at(55),
    at(65),
    at(72),
    at(82),
    at(140),
  ]);
  const held = data.get("level").values[3];
  assert.equal(data.get("level").values[4], held);
  assert.ok(held > 0.8 && held < 1);
  assert.deepEqual(
    data.get("said").words.map((w) => [w.start / FRAME, w.end / FRAME]),
    [
      [40, 65],
      [62, 70],
      [110, 120],
    ],
  );
  assert.deepEqual(
    data.get("quiet").intervals.map((i) => [i.start / FRAME, i.end / FRAME]),
    [
      [30, 100],
      [61, 70],
      [85, 85],
      [130, 135],
    ],
  );
  assert.deepEqual(data.get("face"), dataSets()[5]);
});

test("paste points a clip's bindings through itself at its new id", () => {
  const session = createEditor({ document: createDocument(project()) });
  assert.ok(
    session.dispatch({
      type: "clip.update",
      id: "z",
      patch: {
        visual: {
          position: dataValue({
            data: "face",
            channel: "face",
            via: { clip: "c" },
            space: "canvas",
          }),
        },
      },
    }).ok,
  );
  session.copy(["c", "z"]);
  let next = 0;
  const pasted = session.paste(at(200), () => `copy${++next}`);
  assert.ok(pasted.ok, pasted.error?.message);
  const clips = session.query().clips;
  assert.deepEqual(clips.get("copy1").clip.visual.position.via, {
    clip: "copy1",
  });
  assert.deepEqual(clips.get("copy2").clip.visual.position.via, { clip: "c" });
});

test("deleting a clip a binding reads through is refused in alpha2/edit too", async () => {
  const edit = await import("alpha2/edit");
  const document = createDocument(project()).project;
  const bound = structuredClone(document);
  bound.timeline.tracks[1].clips[1].visual = {
    position: dataValue({
      data: "face",
      channel: "face",
      via: { clip: "c" },
      space: "canvas",
    }),
  };
  const result = edit.rippleDelete(bound, "c");
  assert.equal(result.ok, false);
  assert.equal(result.error.code, "read_through");
  assert.match(result.error.message, /clip z visual\.position/);
});
