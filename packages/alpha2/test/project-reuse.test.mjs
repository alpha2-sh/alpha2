// validateProject reuses results for shared deep-frozen parts; it must agree with a full walk.
import test from "node:test";
import assert from "node:assert/strict";

import {
  TICKS_PER_SECOND,
  validateProject,
  validateProjectJson,
} from "../dist/project.js";
import * as edit from "../dist/edit.js";
import { readValidationCorpus } from "../../../scripts/lib/validation-corpus.mjs";

function deepFreeze(value) {
  const pending = [value];
  while (pending.length) {
    const next = pending.pop();
    if (!next || typeof next !== "object" || Object.isFrozen(next)) continue;
    Object.freeze(next);
    pending.push(...Object.values(next));
  }
  return value;
}

const walked = (project) => validateProjectJson(JSON.stringify(project));

function agrees(project, label) {
  const expected = walked(project);
  assert.deepEqual(validateProject(project), expected, label);
  assert.deepEqual(
    validateProject(structuredClone(project)),
    expected,
    `${label} (copy)`,
  );
  return expected;
}

const S = TICKS_PER_SECOND;
const clip = (id, start, source = "video", extra = {}) => ({
  id,
  source_id: source,
  timeline_range: { start: start * S, end: (start + 1) * S },
  source_range: { start: 0, end: S },
  ...extra,
});

function base() {
  return {
    schema_version: 1,
    id: "p",
    name: "Project",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "video", locator: "asset://video", kind: "video" },
      { id: "music", locator: "asset://music", kind: "audio" },
    ],
    timeline: {
      tracks: [
        {
          id: "v1",
          name: "V1",
          kind: "video",
          clips: [clip("a", 0), clip("b", 2), clip("c", 4)],
        },
        {
          id: "v2",
          name: "V2",
          kind: "video",
          clips: [clip("d", 0), clip("e", 2)],
        },
        {
          id: "a1",
          name: "A1",
          kind: "audio",
          clips: [clip("m", 0, "music")],
        },
      ],
    },
  };
}

function withTrack(project, index, change) {
  const tracks = [...project.timeline.tracks];
  tracks[index] = change(tracks[index]);
  return { ...project, timeline: { ...project.timeline, tracks } };
}

test("a frozen project's parts are checked again against what changed around them", () => {
  const valid = deepFreeze(base());
  assert.deepEqual(agrees(valid, "base"), []);
  agrees(
    deepFreeze({ ...valid, sources: [valid.sources[1]] }),
    "unknown source",
  );
  agrees(
    withTrack(valid, 1, (track) => ({
      ...track,
      clips: [...track.clips, valid.timeline.tracks[0].clips[0]],
    })),
    "a clip id on two tracks",
  );
  agrees(
    {
      ...valid,
      timeline: {
        tracks: [...valid.timeline.tracks, valid.timeline.tracks[0]],
      },
    },
    "a track twice",
  );
  agrees(
    withTrack(valid, 2, (track) => ({
      ...track,
      clips: [...track.clips, valid.timeline.tracks[0].clips[1]],
    })),
    "a video clip on an audio track",
  );
  const matted = deepFreeze(
    withTrack(valid, 1, (track) => ({
      ...track,
      clips: [
        clip("d", 0, "video", { matte: { source: "a", mode: "luma" } }),
        clip("e", 2),
      ],
    })),
  );
  assert.deepEqual(agrees(matted, "matte"), []);
  agrees(
    withTrack(matted, 0, (track) => ({
      ...track,
      clips: [
        clip("a", 0, "video", { matte: { source: "e", mode: "luma" } }),
        ...track.clips.slice(1),
      ],
    })),
    "a matte of a matted clip",
  );
  agrees(
    withTrack(matted, 0, (track) => ({
      ...track,
      clips: track.clips.slice(1),
    })),
    "a matte of a removed clip",
  );
  const composed = deepFreeze({
    ...valid,
    compositions: [
      {
        id: "look",
        nodes: [{ id: "clip", op: { type: "clip" } }],
        output: "clip",
      },
    ],
    timeline: {
      tracks: [
        withTrack(valid, 0, (track) => ({
          ...track,
          clips: [
            { ...track.clips[0], composition: { id: "look" } },
            ...track.clips.slice(1),
          ],
        })).timeline.tracks[0],
        ...valid.timeline.tracks.slice(1),
      ],
    },
  });
  assert.deepEqual(agrees(composed, "a composition"), []);
  agrees(
    deepFreeze({ ...composed, compositions: [] }),
    "an unknown composition",
  );
  agrees(
    withTrack(valid, 0, (track) => ({
      ...track,
      clips: [clip("a", 0), clip("b", 0.5), track.clips[2]],
    })),
    "an overlap",
  );
});

test("a frozen project's catalog is made again when its resources change", async () => {
  const cases = await readValidationCorpus();
  const resourced = deepFreeze(
    cases.find((fixture) => fixture.name === "effect-resource-ok").project,
  );
  assert.deepEqual(agrees(resourced, "a resource"), []);
  assert.notDeepEqual(
    agrees(
      deepFreeze({ ...resourced, effect_resources: [] }),
      "an unknown resource",
    ),
    [],
  );
});

test("a frozen object that could still change is checked each time", () => {
  // A getter could answer differently next time.
  const project = base();
  let id = "a";
  const first = project.timeline.tracks[0].clips[0];
  Object.defineProperty(first, "id", {
    get: () => id,
    enumerable: true,
  });
  deepFreeze(project);
  assert.deepEqual(validateProject(project), []);
  id = "b";
  assert.deepEqual(validateProject(project), ["duplicate clip id b"]);
  const outside = base();
  Object.freeze(outside.timeline.tracks[0]);
  Object.freeze(outside);
  assert.deepEqual(validateProject(outside), []);
  outside.timeline.tracks[0].clips.push(clip("d", 6));
  assert.deepEqual(validateProject(outside), ["duplicate clip id d"]);
});

function random(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function parts(value, path = [], key = "", found = []) {
  if (!value || typeof value !== "object") return found;
  found.push({ path, value, key });
  for (const [name, item] of Object.entries(value))
    parts(
      item,
      [...path, Array.isArray(value) ? Number(name) : name],
      Array.isArray(value) ? key : name,
      found,
    );
  return found;
}

function mutate(project, next) {
  const all = parts(project);
  const pick = (list) => list[Math.floor(next() * list.length)];
  const { path, value } = pick(all);
  const elsewhere = (key) =>
    all
      .filter((part) => part.key === key && part.value !== value)
      .map((part) => part.value);
  let changed;
  if (Array.isArray(value)) {
    changed = [...value];
    const at = Math.floor(next() * Math.max(1, value.length));
    const op = next();
    if (op < 0.3) changed.splice(at, 1);
    else if (op < 0.55 && value.length) changed.push(value[at]);
    else if (op < 0.75 && value.length > 1)
      [changed[0], changed[at]] = [changed[at], changed[0]];
    else {
      const items = elsewhere(all.find((part) => part.value === value).key)
        .filter(Array.isArray)
        .flat();
      if (items.length) changed.splice(at, 0, pick(items));
      else changed.pop();
    }
  } else {
    changed = { ...value };
    const keys = Object.keys(value);
    if (!keys.length) return project;
    const key = pick(keys);
    const op = next();
    const item = value[key];
    if (op < 0.2) delete changed[key];
    else if (op < 0.55) {
      const values = all
        .flatMap((part) =>
          !Array.isArray(part.value) && key in part.value
            ? [part.value[key]]
            : [],
        )
        .filter((other) => other !== item);
      if (values.length) changed[key] = pick(values);
    } else if (typeof item === "number")
      changed[key] = Number.isInteger(item)
        ? item + pick([-1, 1, -S, S, 2 * S])
        : item * pick([0.5, 2, -1]);
    else if (typeof item === "string") changed[key] = `${item}x`;
    else if (typeof item === "boolean") changed[key] = !item;
  }
  const freezing = next();
  const seal = (copy) => {
    if (freezing < 0.5) return deepFreeze(copy);
    if (freezing < 0.75) return Object.freeze(copy);
    return copy;
  };
  let replaced = seal(changed);
  for (let depth = path.length - 1; depth >= 0; depth -= 1) {
    const parent = path.slice(0, depth).reduce((at, step) => at[step], project);
    const copy = Array.isArray(parent) ? [...parent] : { ...parent };
    copy[path[depth]] = replaced;
    replaced = seal(copy);
  }
  return replaced;
}

test("projects made by changing frozen projects are validated as their text is", async () => {
  const cases = await readValidationCorpus();
  const bases = [
    deepFreeze(base()),
    ...cases
      .filter((fixture) => fixture.valid && fixture.json === undefined)
      .map((fixture) => deepFreeze(structuredClone(fixture.project))),
  ];
  let seed = 1;
  let invalid = 0;
  let checked = 0;
  for (const start of bases) {
    let valid = start;
    validateProject(valid);
    for (let round = 0; round < 40; round += 1) {
      const next = random(seed++);
      let project = next() < 0.5 ? start : valid;
      const steps = 1 + Math.floor(next() * 3);
      for (let step = 0; step < steps; step += 1)
        project = mutate(project, next);
      const expected = walked(project);
      assert.deepEqual(
        validateProject(project),
        expected,
        `seed ${seed - 1}: ${JSON.stringify(project).slice(0, 400)}`,
      );
      checked += 1;
      if (expected.length === 0) valid = project;
      else invalid += 1;
    }
  }
  assert.ok(
    invalid > checked / 5 && invalid < checked - checked / 5,
    `${invalid} of ${checked}`,
  );
});

function bigProject(count) {
  const tracks = [];
  for (let t = 0; t * 4000 < count; t += 1)
    tracks.push({
      id: `V${t}`,
      name: `V${t}`,
      kind: "video",
      clips: Array.from({ length: Math.min(4000, count - t * 4000) }, (_, i) =>
        clip(`c${t}-${i}`, 2 * i),
      ),
    });
  return { ...base(), timeline: { tracks } };
}

function fastest(runs, run) {
  let best = Infinity;
  for (let i = 0; i < runs; i += 1) {
    const started = performance.now();
    run(i);
    best = Math.min(best, performance.now() - started);
  }
  return best;
}

test("an edit of a frozen 10,000-clip project is checked for a small part of a whole check", () => {
  const project = deepFreeze(bigProject(10_000));
  assert.deepEqual(validateProject(project), []);
  const copies = Array.from({ length: 5 }, () => structuredClone(project));
  const whole = fastest(5, (i) => validateProject(copies[i]));
  const edited = fastest(10, (i) => {
    const result = edit.move(project, "c0-0", (i + 1) * 100);
    assert.equal(result.ok, true);
  });
  // About 3 % here; a full walk would take about 95 %.
  assert.ok(
    edited < whole / 4,
    `an edit ${edited.toFixed(2)} ms, a whole check ${whole.toFixed(2)} ms`,
  );
});
