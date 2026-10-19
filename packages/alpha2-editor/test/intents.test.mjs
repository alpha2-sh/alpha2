import test from "node:test";
import assert from "node:assert/strict";
import { createDocument, createEditor } from "../dist/index.js";

const S = 120000;
const document = () =>
  createDocument({
    schema_version: 1,
    id: "p",
    name: "Intents",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "v", kind: "video", locator: "test://v" },
      { id: "a", kind: "audio", locator: "test://a" },
    ],
    timeline: {
      tracks: [
        {
          id: "V1",
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "one",
              source_id: "v",
              timeline_range: { start: 0, end: 2 * S },
              source_range: { start: 0, end: 2 * S },
            },
            {
              id: "two",
              source_id: "v",
              timeline_range: { start: 2 * S, end: 4 * S },
              source_range: { start: 2 * S, end: 4 * S },
            },
          ],
        },
        {
          id: "A1",
          name: "Audio",
          kind: "audio",
          clips: [
            {
              id: "music",
              source_id: "a",
              timeline_range: { start: 4 * S, end: 5 * S },
              source_range: { start: 0, end: S },
            },
          ],
        },
      ],
    },
  });
const piece = (id, seconds = 1) => ({
  id,
  source_id: "v",
  timeline_range: { start: 0, end: seconds * S },
  source_range: { start: 0, end: seconds * S },
});
const range = (e, id) => e.query().clips.get(id).clip.timeline_range;

test("insert makes room on sync-locked tracks and moves later markers, as one undo", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "track.metadata", id: "A1", patch: { syncLock: true } });
  e.dispatch({
    type: "metadata.update",
    patch: {
      markers: [
        { id: "m0", time: S, name: "Before" },
        { id: "m1", time: 3 * S, name: "After" },
      ],
    },
  });
  const before = e.document;
  const placed = e.dispatch({
    type: "clip.place",
    mode: "insert",
    trackId: "V1",
    clip: piece("new"),
    at: 2 * S,
    name: "Insert",
  });
  assert.equal(placed.ok, true, placed.error?.message);
  assert.deepEqual(range(e, "new"), { start: 2 * S, end: 3 * S });
  assert.deepEqual(range(e, "two"), { start: 3 * S, end: 5 * S });
  assert.deepEqual(range(e, "music"), { start: 5 * S, end: 6 * S });
  assert.deepEqual(
    e.document.metadata.markers.map((m) => m.time),
    [S, 4 * S],
  );
  assert.equal(e.document.metadata.clips.new.name, "Insert");
  e.undo();
  assert.equal(e.document, before);
});

test("insert inside a clip splits it and names the part it cuts off", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "clip.metadata", id: "one", patch: { name: "Wide" } });
  assert.equal(
    e.dispatch({
      type: "clip.place",
      mode: "insert",
      trackId: "V1",
      clip: piece("new"),
      at: S,
      splitId: "one-b",
    }).ok,
    true,
  );
  assert.deepEqual(range(e, "one"), { start: 0, end: S });
  assert.deepEqual(range(e, "new"), { start: S, end: 2 * S });
  assert.deepEqual(range(e, "one-b"), { start: 2 * S, end: 3 * S });
  assert.equal(e.document.metadata.clips["one-b"].name, "Wide");
});

test("overwrite replaces what is there and drops a covered clip's composition", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "graph.create", clipId: "two" });
  assert.equal(e.document.project.compositions.length, 1);
  assert.equal(
    e.dispatch({
      type: "clip.place",
      mode: "overwrite",
      trackId: "V1",
      clip: piece("new", 3),
      at: S,
    }).ok,
    true,
  );
  assert.deepEqual(range(e, "one"), { start: 0, end: S });
  assert.equal(e.query().clips.has("two"), false);
  assert.equal(e.document.project.compositions, undefined);
  assert.deepEqual(range(e, "music"), { start: 4 * S, end: 5 * S });
});

test("append and roll", () => {
  const e = createEditor({ document: document() });
  e.dispatch({
    type: "clip.place",
    mode: "append",
    trackId: "V1",
    clip: piece("tail"),
    at: 0,
  });
  assert.deepEqual(range(e, "tail"), { start: 4 * S, end: 5 * S });
  const rolled = e.dispatch({
    type: "clip.roll",
    leftId: "one",
    rightId: "two",
    at: 3 * S,
  });
  assert.equal(rolled.ok, true, rolled.error?.message);
  assert.deepEqual(range(e, "one"), { start: 0, end: 3 * S });
  assert.deepEqual(range(e, "two"), { start: 3 * S, end: 4 * S });
  const apart = e.dispatch({
    type: "clip.roll",
    leftId: "one",
    rightId: "tail",
    at: 3 * S,
  });
  assert.equal(apart.error.code, "not_adjacent");
});

test("ripple edits reach sync-locked tracks, and a locked or cut one refuses", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "track.metadata", id: "A1", patch: { syncLock: true } });
  // Shortening the first clip pulls the audio after it back.
  assert.equal(
    e.dispatch({
      type: "clip.trim",
      id: "one",
      edge: "end",
      at: S,
      ripple: true,
    }).ok,
    true,
  );
  assert.deepEqual(range(e, "two"), { start: S, end: 3 * S });
  assert.deepEqual(range(e, "music"), { start: 3 * S, end: 4 * S });
  e.undo();
  // A sync-locked track that is also locked cannot move.
  e.dispatch({ type: "track.metadata", id: "A1", patch: { locked: true } });
  const locked = e.dispatch({
    type: "clip.trim",
    id: "one",
    edge: "end",
    at: S,
    ripple: true,
  });
  assert.equal(locked.error.code, "locked");
  e.dispatch({ type: "track.metadata", id: "A1", patch: { locked: false } });
  // Content across the edit point on a sync-locked track refuses the edit.
  const across = e.dispatch({
    type: "clip.trim",
    id: "two",
    edge: "end",
    at: 3 * S,
    ripple: true,
  });
  assert.equal(across.ok, true);
  e.undo();
  // The music now spans the end of the second clip, the edit point.
  e.dispatch({ type: "clip.move", id: "music", at: 3.5 * S });
  const conflict = e.dispatch({
    type: "clip.trim",
    id: "two",
    edge: "end",
    at: 3 * S,
    ripple: true,
  });
  assert.equal(conflict.error.code, "sync_conflict");
});

test("slip, slide, and retime are single undoable commands that respect locks", () => {
  const e = createEditor({ document: document() });
  const source = (id) => e.query().clips.get(id).clip.source_range;
  assert.equal(
    e.dispatch({ type: "clip.slip", id: "two", delta: S / 2 }).ok,
    true,
  );
  assert.deepEqual(source("two"), { start: 2.5 * S, end: 4.5 * S });
  assert.deepEqual(range(e, "two"), { start: 2 * S, end: 4 * S });
  const refused = e.dispatch({ type: "clip.slip", id: "one", delta: -1 });
  assert.equal(refused.ok, false);
  assert.equal(refused.error.code, "outside_source");
  assert.equal(
    e.dispatch({ type: "clip.slide", id: "two", delta: -S / 2 }).ok,
    true,
  );
  assert.deepEqual(range(e, "one"), { start: 0, end: 1.5 * S });
  assert.deepEqual(range(e, "two"), { start: 1.5 * S, end: 3.5 * S });
  e.undo();
  assert.deepEqual(range(e, "one"), { start: 0, end: 2 * S });
  // Half speed, rippled onto the sync-locked audio track and the marker after the clip.
  e.dispatch({ type: "track.metadata", id: "A1", patch: { syncLock: true } });
  e.dispatch({
    type: "marker.add",
    marker: {
      id: "m",
      time: 4 * S,
      end: 5 * S,
      name: "Music",
      color: "#ffcc00",
    },
  });
  assert.equal(
    e.dispatch({
      type: "clip.retime",
      id: "two",
      duration: 4 * S,
      ripple: true,
      keyframes: "stretch",
    }).ok,
    true,
  );
  assert.deepEqual(range(e, "two"), { start: 2 * S, end: 6 * S });
  assert.deepEqual(range(e, "music"), { start: 6 * S, end: 7 * S });
  assert.deepEqual(
    e.document.metadata.markers.map(({ time, end }) => [time, end]),
    [[6 * S, 7 * S]],
  );
  e.undo();
  assert.deepEqual(range(e, "two"), { start: 2 * S, end: 4 * S });
  // A locked track refuses them all.
  e.dispatch({ type: "track.metadata", id: "V1", patch: { locked: true } });
  for (const command of [
    { type: "clip.slip", id: "two", delta: 1 },
    { type: "clip.slide", id: "two", delta: 1 },
    { type: "clip.retime", id: "two", duration: S },
  ]) {
    const result = e.dispatch(command);
    assert.equal(result.ok, false, command.type);
    assert.equal(result.error.code, "locked", command.type);
  }
});

test("markers are edited by id and validated", () => {
  const e = createEditor({ document: document() });
  assert.equal(
    e.dispatch({
      type: "marker.add",
      marker: { id: "a", time: S, name: "Cut" },
    }).ok,
    true,
  );
  assert.equal(
    e.dispatch({
      type: "marker.add",
      marker: { id: "a", time: S, name: "Again" },
    }).ok,
    false,
  );
  assert.equal(
    e.dispatch({ type: "marker.update", id: "a", patch: { end: S / 2 } }).ok,
    false,
    "a range must end after it starts",
  );
  assert.equal(
    e.dispatch({
      type: "marker.update",
      id: "a",
      patch: { note: "Tighten", done: true, end: 2 * S },
    }).ok,
    true,
  );
  assert.deepEqual(e.document.metadata.markers, [
    { id: "a", time: S, name: "Cut", note: "Tighten", done: true, end: 2 * S },
  ]);
  assert.equal(e.dispatch({ type: "marker.remove", id: "a" }).ok, true);
  assert.equal(e.dispatch({ type: "marker.remove", id: "a" }).ok, false);
});

test("the timeline index lists clips and markers in time order, once per document", async () => {
  const { timelineIndex } = await import("../dist/index.js");
  const e = createEditor({ document: document() });
  e.dispatch({
    type: "marker.add",
    marker: { id: "m", time: 2 * S, end: 3 * S, name: "Beat", note: "hit" },
  });
  const index = timelineIndex(e.document);
  assert.deepEqual(
    index.map(({ kind, id, start, end }) => [kind, id, start, end]),
    [
      ["clip", "one", 0, 2 * S],
      ["marker", "m", 2 * S, 3 * S],
      ["clip", "two", 2 * S, 4 * S],
      ["clip", "music", 4 * S, 5 * S],
    ],
  );
  assert.equal(timelineIndex(e.document), index);
  e.dispatch({ type: "clip.slide", id: "two", delta: -S / 2 });
  assert.notEqual(timelineIndex(e.document), index);
});

test("the timeline index of 10,000 clips builds within its budget", async () => {
  const { timelineIndex } = await import("../dist/index.js");
  // A track holds at most 4,096 clips: 10,000 over three tracks.
  const tracks = [0, 1, 2].map((t) => ({
    id: `V${t}`,
    name: `Video ${t}`,
    kind: "video",
    clips: Array.from({ length: t < 2 ? 4_000 : 2_000 }, (_, i) => ({
      id: `c${t}-${i}`,
      source_id: "v",
      timeline_range: { start: i * S, end: (i + 1) * S },
      source_range: { start: 0, end: S },
    })),
  }));
  // The index is cached per document, so each run times a new one, not a cache lookup.
  const big = () =>
    createDocument({ ...document().project, timeline: { tracks } });
  // The fastest of three builds, so a GC pause or a busy CI machine is not the measurement.
  let elapsed = Infinity;
  let previous;
  for (let run = 0; run < 3; run += 1) {
    const fresh = big();
    const started = performance.now();
    const index = timelineIndex(fresh);
    elapsed = Math.min(elapsed, performance.now() - started);
    assert.equal(index.length, 10_000);
    assert.notEqual(index, previous, "each run builds its own index");
    previous = index;
  }
  // 250 ms, not 100: a busy runner can miss 100 ms alone; the budget catches index regressions.
  assert.ok(elapsed < 250, `${elapsed.toFixed(1)} ms`);
});

test("keyframe edits keep their order and fall back to a constant", async () => {
  const {
    moveKeyframe,
    removeKeyframe,
    setInterpolation,
    setHandles,
    setKeyframeValue,
    handlesOf,
    EASINGS,
  } = await import("../dist/index.js");
  const value = {
    mode: "keyframes",
    keyframes: [
      { time: 0, value: 0, interpolation: { type: "linear" } },
      { time: 10, value: 1, interpolation: { type: "linear" } },
      { time: 20, value: 0.5, interpolation: { type: "hold" } },
    ],
  };
  // Past its neighbour, a keyframe stops a tick short of it.
  const moved = moveKeyframe(value, 10, 25);
  assert.equal(moved.time, 19);
  assert.deepEqual(
    moved.value.keyframes.map((k) => k.time),
    [0, 19, 20],
  );
  assert.throws(() => moveKeyframe(value, 5, 6), /no keyframe at 5/);
  const eased = setInterpolation(value, 0, EASINGS.ease_in_out);
  assert.deepEqual(eased.keyframes[0].interpolation, EASINGS.ease_in_out);
  assert.deepEqual(
    setHandles(value, 10, { x1: -1, y1: 2, x2: 3, y2: -0.5 }).keyframes[1]
      .interpolation,
    { type: "cubic_bezier", x1: 0, y1: 2, x2: 1, y2: -0.5 },
  );
  assert.equal(setKeyframeValue(value, 20, 0.75).keyframes[2].value, 0.75);
  assert.deepEqual(handlesOf({ type: "linear" }), {
    x1: 1 / 3,
    y1: 1 / 3,
    x2: 2 / 3,
    y2: 2 / 3,
  });
  assert.equal(handlesOf({ type: "hold" }), null);
  let last = value;
  for (const time of [0, 10]) last = removeKeyframe(last, time);
  assert.deepEqual(removeKeyframe(last, 20), { mode: "constant", value: 0.5 });
  assert.deepEqual(
    value.keyframes.map((k) => k.time),
    [0, 10, 20],
  );
});

test("a ripple delete of a dissolve's incoming clip closes only its own time, markers too", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "track.metadata", id: "A1", patch: { syncLock: true } });
  const added = e.dispatch({
    type: "transition.add",
    trackId: "V1",
    fromId: "one",
    toId: "two",
    duration: S / 4,
    effect: { id: "alpha2.transition.crossfade", version: 1 },
  });
  assert.equal(added.ok, true, added.error?.message);
  const { end } = range(e, "two");
  assert.equal(
    e.dispatch({
      type: "clip.place",
      mode: "overwrite",
      trackId: "V1",
      clip: piece("three"),
      at: end,
    }).ok,
    true,
  );
  e.dispatch({
    type: "metadata.update",
    patch: { markers: [{ id: "m", time: 5 * S, name: "After" }] },
  });
  // "two" shares its first quarter second with "one"; the rest is its own.
  const { start } = range(e, "two");
  assert.equal(start, 1.75 * S);
  const deleted = e.dispatch({ type: "clip.delete", id: "two", ripple: true });
  assert.equal(deleted.ok, true, deleted.error?.message);
  const closed = end - 2 * S;
  assert.deepEqual(range(e, "three"), { start: 2 * S, end: 3 * S });
  assert.deepEqual(range(e, "music"), {
    start: 4 * S - closed,
    end: 5 * S - closed,
  });
  assert.deepEqual(
    e.document.metadata.markers.map((m) => m.time),
    [5 * S - closed],
  );
});

test("a marker in the time a ripple closes refuses the edit instead of landing on what moves in", () => {
  // A marker inside a deleted clip moves onto the content before the cut; it never refuses it.
  const three = () => {
    const e = createEditor({ document: document() });
    e.dispatch({ type: "track.metadata", id: "A1", patch: { syncLock: true } });
    e.dispatch({ type: "clip.delete", id: "music" });
    assert.equal(
      e.dispatch({
        type: "clip.place",
        mode: "append",
        trackId: "V1",
        clip: piece("three", 2),
        at: 0,
      }).ok,
      true,
    );
    return e;
  };
  const marked = (marker) => {
    const e = three();
    e.dispatch({ type: "marker.add", marker: { name: "M", ...marker } });
    return e;
  };
  for (const [marker, id] of [
    [{ id: "m", time: S }, "one"],
    [{ id: "m", time: 3 * S }, "two"],
    [{ id: "m", time: 1.5 * S, end: 3 * S }, "two"],
    [{ id: "m", time: 3 * S, end: 5 * S }, "two"],
  ]) {
    const e = marked(marker);
    const before = e.document;
    const refused = e.dispatch({ type: "clip.delete", id, ripple: true });
    assert.equal(refused.ok, false, JSON.stringify(marker));
    assert.equal(refused.error.code, "sync_conflict");
    assert.equal(refused.error.entityId, "m");
    assert.equal(e.document, before);
  }
  // Markers outside it stay before the cut or move with what follows; a range ending at the cut is
  // untouched.
  const e = three();
  e.dispatch({
    type: "metadata.update",
    patch: {
      markers: [
        { id: "a", time: S, end: 2 * S, name: "A" },
        { id: "b", time: 4 * S, name: "B" },
        { id: "c", time: 4.5 * S, end: 5 * S, name: "C" },
      ],
    },
  });
  const deleted = e.dispatch({ type: "clip.delete", id: "two", ripple: true });
  assert.equal(deleted.ok, true, deleted.error?.message);
  assert.deepEqual(
    e.document.metadata.markers.map(({ time, end }) => [time, end]),
    [
      [S, 2 * S],
      [2 * S, undefined],
      [2.5 * S, 3 * S],
    ],
  );
  // A ripple trim that closes time follows the same rule.
  const trimmed = marked({ id: "m", time: 3.5 * S });
  const trim = trimmed.dispatch({
    type: "clip.trim",
    id: "two",
    edge: "end",
    at: 3 * S,
    ripple: true,
  });
  assert.equal(trim.ok, false);
  assert.equal(trim.error.code, "sync_conflict");
});

test("a placed copy of a clip, or a clip a new track brings, edits its own copy of a composition", () => {
  // A clip placed by insert, overwrite or append gets its own copy of the composition.
  const effect = { id: "alpha2.effect.invert", version: 1, params: {} };
  const stackOf = (e, id) =>
    e.document.project.compositions
      .find((c) => c.id === e.query().clips.get(id).clip.composition.id)
      .nodes.filter((n) => n.op.type === "effect")
      .map((n) => n.id);
  for (const mode of ["insert", "overwrite", "append"]) {
    const e = createEditor({ document: document() });
    e.dispatch({ type: "effect.add", id: "one", instanceId: "a", effect });
    const original = e.query().clips.get("one").clip;
    const placed = e.dispatch({
      type: "clip.place",
      mode,
      trackId: "V1",
      clip: { ...original, id: "copy" },
      at: 6 * S,
    });
    assert.equal(placed.ok, true, placed.error?.message);
    const copy = e.query().clips.get("copy").clip.composition.id;
    assert.notEqual(copy, original.composition.id, mode);
    assert.equal(
      e.dispatch({ type: "effect.add", id: "copy", instanceId: "b", effect })
        .ok,
      true,
    );
    assert.deepEqual(stackOf(e, "one"), ["a"], mode);
    assert.deepEqual(stackOf(e, "copy"), ["a", "b"], mode);
  }
  const e = createEditor({ document: document() });
  e.dispatch({ type: "effect.add", id: "one", instanceId: "a", effect });
  const original = e.query().clips.get("one").clip;
  const added = e.dispatch({
    type: "track.add",
    track: {
      id: "V2",
      name: "Copies",
      kind: "video",
      clips: [
        { ...original, id: "first" },
        {
          ...original,
          id: "second",
          timeline_range: { start: 3 * S, end: 5 * S },
        },
      ],
    },
  });
  assert.equal(added.ok, true, added.error?.message);
  const ids = ["one", "first", "second"].map(
    (id) => e.query().clips.get(id).clip.composition.id,
  );
  assert.equal(new Set(ids).size, 3);
});
