import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  append,
  insert,
  move,
  moveToTrack,
  overwrite,
  retime,
  rippleDelete,
  rippleTrim,
  rollTrim,
  slide,
  slideLimits,
  slip,
  slipLimits,
  split,
  trim,
} from "../dist/edit.js";
import * as edit from "../dist/edit.js";
import { validateProject } from "../dist/project.js";

const corpus = JSON.parse(
  await readFile(
    new URL("../../../tests/cases/edit-corpus.json", import.meta.url),
    "utf8",
  ),
);

const operations = {
  split: (project, args) => split(project, args.clip, args.at, args.newId),
  trim: (project, args) => trim(project, args.clip, args.edge, args.to),
  rippleTrim: (project, args) =>
    rippleTrim(project, args.clip, args.edge, args.to, {
      syncTracks: args.syncTracks,
    }),
  rippleDelete: (project, args) =>
    rippleDelete(project, args.clip, { syncTracks: args.syncTracks }),
  rollTrim: (project, args) =>
    rollTrim(project, args.left, args.right, args.at),
  insert: (project, args) =>
    insert(project, args.track, args.clip, args.at, {
      splitId: args.splitId,
      syncTracks: args.syncTracks,
    }),
  overwrite: (project, args) =>
    overwrite(project, args.track, args.clip, args.at, {
      splitId: args.splitId,
    }),
  append: (project, args) => append(project, args.track, args.clip),
  move: (project, args) => move(project, args.clip, args.to),
  moveToTrack: (project, args) =>
    moveToTrack(project, args.clip, args.track, args.to),
  slip: (project, args) => slip(project, args.clip, args.delta),
  slide: (project, args) => slide(project, args.clip, args.delta),
  retime: (project, args) =>
    retime(project, args.clip, args.duration, {
      ripple: args.ripple,
      keyframes: args.keyframes,
      reverse: args.reverse,
      syncTracks: args.syncTracks,
    }),
};

test("the edit corpus: each operation's result or refusal", () => {
  assert.ok(corpus.length > 20);
  for (const fixture of corpus) {
    assert.deepEqual(validateProject(fixture.project), [], fixture.name);
    const before = structuredClone(fixture.project);
    const result = operations[fixture.operation](
      fixture.project,
      fixture.arguments,
    );
    assert.deepEqual(
      fixture.project,
      before,
      `${fixture.name} mutated its input`,
    );
    if (fixture.error === undefined) {
      assert.equal(
        result.ok,
        true,
        `${fixture.name}: ${JSON.stringify(result.error)}`,
      );
      assert.deepEqual(result.project, fixture.expected, fixture.name);
      assert.deepEqual(validateProject(result.project), [], fixture.name);
    } else {
      assert.equal(result.ok, false, fixture.name);
      assert.equal(result.error.code, fixture.error, fixture.name);
    }
  }
});

test("an invalid result names what it breaks", () => {
  const fixture = corpus.find(
    (entry) => entry.name === "move-onto-a-neighbour",
  );
  const result = move(fixture.project, "C", 200_000);
  assert.equal(result.ok, false);
  assert.match(result.error.problems.join(), /overlap/);
});

test("edits in deferValidation leave the check of their result to the caller", () => {
  const fixture = corpus.find(
    (entry) => entry.name === "move-onto-a-neighbour",
  );
  // The move breaks a model rule; deferred, it is returned for the caller to check, and an edit's
  // own refusals still stand.
  const deferred = edit.deferValidation(() => ({
    onto: move(fixture.project, "C", 200_000),
    fraction: move(fixture.project, "C", 0.5),
  }));
  assert.equal(deferred.onto.ok, true);
  assert.match(validateProject(deferred.onto.project).join(), /overlap/);
  assert.equal(deferred.fraction.ok, false);
  assert.equal(deferred.fraction.error.code, "invalid_time");
  // Outside it, and after one that threw, edits check again.
  assert.equal(move(fixture.project, "C", 200_000).ok, false);
  assert.throws(() =>
    edit.deferValidation(() => {
      throw new Error("stop");
    }),
  );
  assert.equal(move(fixture.project, "C", 200_000).ok, false);
});

test("slip and slide limits are the deltas the edits take", () => {
  for (const [name, limits, edit, clip] of [
    ["slip-past-the-recorded-source-end", slipLimits, slip, "B"],
    ["slide-past-a-neighbour-s-recorded-source-end", slideLimits, slide, "B"],
    [
      "slide-later-extends-the-clip-before-and-trims-the-clip-after",
      slideLimits,
      slide,
      "B",
    ],
  ]) {
    const { project } = corpus.find((entry) => entry.name === name);
    const range = limits(project, clip);
    assert.equal(range.ok, true, name);
    for (const delta of [range.min, range.max])
      assert.equal(edit(project, clip, delta).ok, true, `${name} ${delta}`);
    for (const delta of [range.min - 1, range.max + 1])
      assert.equal(edit(project, clip, delta).ok, false, `${name} ${delta}`);
  }
});

test("clips moved together are checked where they all land", () => {
  const { project } = corpus.find((entry) => entry.name === "move-a-clip");
  // A [0, 1 s) and B [1 s, 2 s) meet on t1: either order moves them as one.
  for (const order of [
    ["A", "B"],
    ["B", "A"],
  ]) {
    const starts = { A: 60_000, B: 180_000 };
    const result = edit.moveClips(
      project,
      order.map((clip) => ({ clip, to: starts[clip] })),
    );
    assert.equal(result.ok, true, JSON.stringify(result.error));
    const clips = result.project.timeline.tracks[0].clips;
    assert.deepEqual(
      clips.map((c) => c.timeline_range.start),
      [60_000, 180_000, 300_000],
      `${order}`,
    );
  }
  assert.equal(move(project, "A", 60_000).ok, false, "A alone lands on B");
  const twice = edit.moveClips(project, [
    { clip: "C", to: 400_000 },
    { clip: "C", to: 500_000 },
  ]);
  assert.equal(twice.ok, false);
  assert.equal(twice.error.code, "duplicate_id");
  const overlap = edit.moveClips(project, [{ clip: "C", to: 200_000 }]);
  assert.equal(overlap.ok, false);
  assert.equal(overlap.error.code, "invalid_result");
  const across = edit.moveClips(project, [{ clip: "A", track: "t2" }]);
  assert.equal(across.ok, false);
  assert.equal(across.error.code, "incompatible_track");
});

test("a split clip's right part draws its own copies of the groups in its composition", () => {
  const { project } = corpus.find((entry) => entry.name === "move-a-clip");
  const blur = { id: "alpha2.effect.gaussian-blur", version: 1 };
  const grouped = {
    ...project,
    compositions: [
      {
        id: "look",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          {
            id: "g",
            op: { type: "group", composition: "soft", inputs: ["clip"] },
          },
          {
            id: "h",
            op: { type: "group", composition: "soft", inputs: ["g"] },
          },
        ],
        output: "h",
      },
      {
        id: "soft",
        inputs: ["In"],
        nodes: [
          { id: "in", op: { type: "group_input", index: 0 } },
          { id: "fx", op: { type: "effect", input: "in", effect: blur } },
        ],
        output: "fx",
      },
    ],
    timeline: {
      tracks: project.timeline.tracks.map((track, i) =>
        i === 0
          ? {
              ...track,
              clips: track.clips.map((clip) =>
                clip.id === "A"
                  ? { ...clip, composition: { id: "look" } }
                  : clip,
              ),
            }
          : track,
      ),
    },
  };
  assert.deepEqual(validateProject(grouped), []);
  const result = split(grouped, "A", 60_000, "A2");
  assert.equal(result.ok, true, JSON.stringify(result.error));
  const compositions = Object.fromEntries(
    result.project.compositions.map((c) => [c.id, c]),
  );
  assert.deepEqual(Object.keys(compositions), [
    "look",
    "soft",
    "look~A2",
    "soft~A2",
  ]);
  // Both group nodes of the copy draw the one copy of their definition.
  assert.deepEqual(
    compositions["look~A2"].nodes
      .filter((n) => n.op.type === "group")
      .map((n) => n.op.composition),
    ["soft~A2", "soft~A2"],
  );
  assert.deepEqual(compositions["soft~A2"].nodes, compositions.soft.nodes);
  const copied = edit.copyComposition(grouped, "look", "B");
  assert.deepEqual(
    [...copied.copies],
    [
      ["look", "look~B"],
      ["soft", "soft~B"],
    ],
  );
});

test("a time that is not a whole number of ticks is refused, never thrown", () => {
  const { project } = corpus.find(
    (entry) => entry.name === "roll-before-the-source-start",
  );
  const clip = {
    id: "N",
    source_id: "v2",
    timeline_range: { start: 0, end: 30_000 },
    source_range: { start: 0, end: 30_000 },
  };
  for (const [name, call] of [
    ["split", () => split(project, "A", 60_000.5, "A2")],
    ["trim", () => trim(project, "A", "end", 90_000.5)],
    ["rippleTrim", () => rippleTrim(project, "A", "end", 90_000.5)],
    ["rollTrim", () => rollTrim(project, "A", "B", 110_000.5)],
    ["slip", () => slip(project, "B", 0.5)],
    ["slide", () => slide(project, "B", -0.5)],
    ["retime", () => retime(project, "B", 60_000.5)],
    [
      "insert",
      () => insert(project, "t1", clip, 4.1 * 120_000, { splitId: "S" }),
    ],
    ["overwrite", () => overwrite(project, "t1", clip, 100_000.5)],
    ["move", () => move(project, "C", 250_000.5)],
    ["moveToTrack", () => moveToTrack(project, "C", "t1", Number.NaN)],
    ["moveClips", () => edit.moveClips(project, [{ clip: "C", to: Infinity }])],
  ]) {
    const result = call();
    assert.equal(result.ok, false, name);
    assert.equal(result.error.code, "invalid_time", name);
  }
  assert.equal(
    split(project, "A", 2 ** 53 + 2, "A2").error.code,
    "invalid_time",
  );
});

test("a ripple delete closes the time its clip alone held, and says which", () => {
  const { project } = corpus.find(
    (entry) =>
      entry.name ===
      "ripple-delete-of-a-dissolve-s-incoming-clip-keeps-the-gap-after-it",
  );
  // B [110000, 240000) shares [110000, 120000) with A.
  assert.deepEqual(edit.rippleDeleteSpan(project, "B"), {
    start: 120_000,
    end: 240_000,
  });
  assert.equal(edit.rippleDeleteSpan(project, "nothing"), undefined);
});

test("a cut on the edge of a fade keeps both halves' fades", () => {
  const { project } = corpus.find(
    (entry) => entry.name === "split-divides-range-source-and-fades",
  );
  // A [0, 120000) fades in over 6000 and out over 12000.
  for (const at of [6_000, 108_000]) {
    const result = split(project, "A", at, "A2");
    assert.equal(result.ok, true, `${at}: ${JSON.stringify(result.error)}`);
  }
  for (const at of [5_999, 108_001])
    assert.equal(split(project, "A", at, "A2").error.code, "in_fade", `${at}`);
});

test("splitting the right part again names the copies from the same root", () => {
  const fixture = corpus.find(
    (entry) =>
      entry.name ===
      "split-gives-the-right-half-its-own-composition-from-the-cut",
  );
  const project = structuredClone(fixture.project);
  const clipId = fixture.arguments.clip;
  // Cuts anywhere along the clip, so no fades.
  for (const track of project.timeline.tracks)
    for (const clip of track.clips ?? []) delete clip.audio;
  let edited = project;
  let current = clipId;
  const { start, end } = edited.timeline.tracks
    .flatMap((t) => t.clips ?? [])
    .find((c) => c.id === current).timeline_range;
  const step = Math.floor((end - start) / 41);
  for (let n = 1; n <= 40; n += 1) {
    const next = `0123456789abcdef-${n}`;
    const result = split(edited, current, start + n * step, next);
    assert.equal(result.ok, true, JSON.stringify(result.error));
    edited = result.project;
    current = next;
  }
  const longest = Math.max(...edited.compositions.map((c) => c.id.length));
  const root = Math.max(...project.compositions.map((c) => c.id.length));
  assert.ok(
    longest <= root + "~0123456789abcdef-40".length,
    `longest composition id ${longest}`,
  );
});

test("slide limits refuse or narrow wherever slide does", () => {
  const named = (name) =>
    structuredClone(corpus.find((entry) => entry.name === name).project);
  const clips = (project) => project.timeline.tracks[0].clips;
  const takes = (project, clip, range) => {
    for (const delta of [range.min, range.max])
      assert.equal(slide(project, clip, delta).ok, true, `${delta}`);
    for (const delta of [range.min - 1, range.max + 1])
      assert.equal(slide(project, clip, delta).ok, false, `${delta}`);
  };

  const inTransition = named("slide-a-clip-in-a-transition");
  const refused = slideLimits(inTransition, "B");
  assert.equal(refused.ok, false);
  assert.equal(refused.error.code, "in_transition");

  // A looping clip after it cannot move its start: only 0.
  const looping = named(
    "slide-later-extends-the-clip-before-and-trims-the-clip-after",
  );
  clips(looping).find((c) => c.id === "D").time_map = {
    type: "loop",
    mode: "ping_pong",
  };
  assert.deepEqual(validateProject(looping), []);
  assert.deepEqual(slideLimits(looping, "B"), { ok: true, min: 0, max: 0 });
  assert.equal(slide(looping, "B", 1).ok, false);
  assert.equal(slide(looping, "B", -1).ok, false);

  // The clip before it may not shrink past the dissolve at its start.
  const dissolve = named("slide-a-clip-in-a-transition").timeline.tracks[0]
    .transitions[0];
  const before = named(
    "slide-later-extends-the-clip-before-and-trims-the-clip-after",
  );
  const track = before.timeline.tracks[0];
  track.clips = [
    {
      id: "Z",
      source_id: "v2",
      timeline_range: { start: 0, end: 60_000 },
      source_range: { start: 0, end: 60_000 },
    },
    {
      ...track.clips.find((c) => c.id === "A"),
      timeline_range: { start: 50_000, end: 120_000 },
      source_range: { start: 50_000, end: 120_000 },
    },
    ...track.clips.filter((c) => c.id !== "A"),
  ];
  track.transitions = [
    {
      ...dissolve,
      from_clip: "Z",
      to_clip: "A",
      timeline_range: { start: 50_000, end: 60_000 },
    },
  ];
  assert.deepEqual(validateProject(before), []);
  const early = slideLimits(before, "B");
  assert.equal(early.min, -60_000);
  takes(before, "B", early);

  // The clip after it may not start past the dissolve at its end.
  const after = named(
    "slide-later-extends-the-clip-before-and-trims-the-clip-after",
  );
  const later = after.timeline.tracks[0];
  later.clips = later.clips.map((c) =>
    c.id === "C"
      ? {
          ...c,
          timeline_range: { start: 290_000, end: 360_000 },
          source_range: { start: 110_000, end: 180_000 },
        }
      : c,
  );
  later.transitions = [
    {
      ...dissolve,
      from_clip: "D",
      to_clip: "C",
      timeline_range: { start: 290_000, end: 300_000 },
    },
  ];
  assert.deepEqual(validateProject(after), []);
  const late = slideLimits(after, "B");
  assert.equal(late.max, 50_000);
  takes(after, "B", late);
});
