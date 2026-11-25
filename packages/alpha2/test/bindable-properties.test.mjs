import test from "node:test";
import assert from "node:assert/strict";

import { bindableProperties, validateProject } from "../dist/project.js";

const range = (start, end) => ({ start, end });

function project() {
  return {
    schema_version: 1,
    id: "p",
    name: "P",
    settings: {
      width: 320,
      height: 180,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "v1", kind: "video", locator: "a.mp4" },
      { id: "v2", kind: "video", locator: "b.mp4" },
      { id: "a1", kind: "audio", locator: "c.m4a" },
    ],
    timeline: {
      tracks: [
        {
          id: "t1",
          name: "T1",
          kind: "video",
          clips: [
            {
              id: "c1",
              source_id: "v1",
              timeline_range: range(0, 120000),
              source_range: range(0, 120000),
              masks: [
                {
                  shape: {
                    type: "rectangle",
                    center: { mode: "constant", value: { x: 0.5, y: 0.5 } },
                    size: { mode: "constant", value: { x: 0.5, y: 0.5 } },
                  },
                },
              ],
            },
            {
              id: "c2",
              source_id: "v2",
              timeline_range: range(110000, 230000),
              source_range: range(0, 120000),
            },
          ],
          transitions: [
            {
              from_clip: "c1",
              to_clip: "c2",
              timeline_range: range(110000, 120000),
              easing: { type: "linear" },
              effect: {
                id: "alpha2.transition.directional",
                version: 1,
                params: {},
              },
            },
          ],
        },
        {
          id: "t2",
          name: "T2",
          kind: "audio",
          clips: [
            {
              id: "c3",
              source_id: "a1",
              timeline_range: range(0, 120000),
              source_range: range(0, 120000),
            },
          ],
        },
        { id: "subs", name: "Subs", kind: "caption", captions: { cues: [] } },
      ],
    },
    data: [
      {
        id: "points",
        anchor: { type: "timeline" },
        kind: "points",
        interpolation: "hold",
        channels: ["a"],
        times: [0],
        values: [0.5, 0.5],
      },
      {
        id: "scalars",
        anchor: { type: "timeline" },
        kind: "scalars",
        interpolation: "hold",
        channels: ["a"],
        times: [0],
        values: [0.5],
      },
      {
        id: "similarity",
        anchor: { type: "source", source_id: "v1" },
        kind: "similarity",
        interpolation: "hold",
        times: [0],
        values: [0, 0, 0, 1],
      },
      {
        id: "words",
        anchor: { type: "timeline" },
        kind: "words",
        words: [{ start: 0, end: 1000, text: "hi" }],
      },
    ],
  };
}

function ownerObject(doc, owner) {
  const tracks = doc.timeline.tracks;
  if (owner.kind === "clip")
    return tracks
      .flatMap((track) => track.clips ?? [])
      .find((clip) => clip.id === owner.clip);
  if (owner.kind === "track")
    return tracks.find((track) => track.id === owner.track);
  if (owner.kind === "transition")
    return tracks
      .find((track) => track.id === owner.track)
      .transitions.find(
        (entry) => entry.from_clip === owner.from && entry.to_clip === owner.to,
      );
  if (owner.kind === "master") return doc;
  throw new Error(owner.kind);
}

function bound(owner, property, kind) {
  const doc = project();
  const steps = property.replaceAll(/\[(\d+)\]/g, ".$1").split(".");
  let at = ownerObject(doc, owner);
  for (const step of steps.slice(0, -1)) at = at[step] ??= {};
  const last = steps.at(-1);
  if (kind === "similarity") at[last] = { data: "similarity" };
  else if (kind === "words")
    (at[last] ??= {}).from_words = { data: "words", max_chars: 32 };
  else at[last] = { mode: "data", data: kind, channel: "a" };
  return doc;
}

const OWNERS = [
  { kind: "clip", clip: "c1" },
  { kind: "clip", clip: "c3" },
  { kind: "track", track: "t2" },
  { kind: "track", track: "subs" },
  { kind: "transition", track: "t1", from: "c1", to: "c2" },
  { kind: "master" },
];

test("the base project of the bindable properties test is valid", () => {
  assert.deepEqual(validateProject(project()), []);
});

test("every bindable property takes the kinds it lists and refuses the others", () => {
  for (const owner of OWNERS) {
    const properties = bindableProperties(project(), owner);
    assert.ok(properties.length > 0, JSON.stringify(owner));
    for (const { property, accepts } of properties)
      for (const kind of ["points", "scalars", "similarity", "words"]) {
        if (property === "captions" ? kind !== "words" : kind === "words")
          continue;
        const problems = validateProject(bound(owner, property, kind));
        assert.equal(
          problems.length === 0,
          accepts.includes(kind),
          `${JSON.stringify(owner)} ${property} ${kind}: ${problems.join("; ")}`,
        );
      }
  }
});

test("bindable properties follow the owner's kind and what the project has", () => {
  const paths = (owner) =>
    bindableProperties(project(), owner).map((entry) => entry.property);
  assert.deepEqual(paths({ kind: "clip", clip: "c1" }), [
    "visual.position",
    "visual.scale",
    "visual.rotation_degrees",
    "visual.opacity",
    "masks[0].shape.center",
    "masks[0].shape.corner_radius",
    "masks[0].shape.rotation_degrees",
    "masks[0].shape.size",
    "masks[0].feather",
    "audio.gain",
    "audio.pan",
    "visual.correction",
  ]);
  assert.deepEqual(paths({ kind: "clip", clip: "c3" }), [
    "audio.gain",
    "audio.pan",
  ]);
  assert.deepEqual(paths({ kind: "track", track: "subs" }), ["captions"]);
  assert.deepEqual(paths({ kind: "clip", clip: "missing" }), []);
  assert.deepEqual(paths({ kind: "source", source: "v1" }), []);
});

test("a composition node's bindable properties are its effect's data parameters", async () => {
  const { readFile } = await import("node:fs/promises");
  const corpus = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/project-validation-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const { project: doc } = corpus.find(
    (entry) => entry.name === "data-node-param-follows",
  );
  assert.deepEqual(validateProject(doc), []);
  const blur = bindableProperties(doc, {
    kind: "node",
    composition: "k",
    node: "n",
  });
  assert.ok(
    blur.some(
      (entry) =>
        entry.property === "effect.params.radius" &&
        JSON.stringify(entry.accepts) === JSON.stringify(["scalars"]),
    ),
    JSON.stringify(blur),
  );
  assert.deepEqual(
    bindableProperties(doc, { kind: "node", composition: "k", node: "media" }),
    [],
  );
});
