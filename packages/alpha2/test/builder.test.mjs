import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  ProjectBuilder,
  sequentialIds,
  ticks,
  toTicks,
} from "../dist/builder.js";
import { validateProject } from "../dist/project.js";

test("the builder writes the minimal one-second tutorial project", async () => {
  const expected = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/builder-tutorial-project.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const builder = new ProjectBuilder({
    id: "tutorial-project",
    name: "Tutorial project",
    width: 1280,
    height: 720,
    frameRate: 30,
  });
  const image = builder.source("image", "asset://image-1", { id: "image-1" });
  builder
    .videoTrack({ id: "video-1", name: "Main video" })
    .clip({ source: image, at: 0, duration: 1 });
  const built = builder.build();
  // The builder names its generated clip `clip-1`, as the document does.
  assert.deepEqual(built, expected);
  assert.deepEqual(validateProject(built), []);
});

test("assets maps each media source the builder added to its locator", () => {
  const builder = new ProjectBuilder({
    name: "Assets",
    width: 640,
    height: 360,
    frameRate: 30,
  });
  const image = builder.source("image", "title.png");
  const music = builder.source("audio", "/media/music.m4a", { id: "music" });
  builder.generated({
    type: "fill",
    paint: {
      type: "solid",
      color: { red: 0, green: 0, blue: 0, alpha: 1 },
    },
  });
  assert.deepEqual(builder.assets(), {
    [image]: "title.png",
    [music]: "/media/music.m4a",
  });
});

test("seconds round to ticks, ticks are exact, and ids are deterministic", () => {
  assert.equal(toTicks(1 / 3), 40_000);
  assert.equal(toTicks(ticks(40_001)), 40_001);
  assert.throws(() => ticks(0.5), RangeError);
  const ids = sequentialIds();
  assert.deepEqual(
    [ids("clip"), ids("track"), ids("clip")],
    ["clip-1", "track-1", "clip-2"],
  );
});

test("the builder covers generated sources, transitions, audio, and captions", () => {
  const builder = new ProjectBuilder({
    name: "Everything",
    width: 640,
    height: 360,
    frameRate: { num: 30_000, den: 1_001 },
  });
  const backdrop = builder.generated({
    type: "fill",
    paint: {
      type: "solid",
      color: { red: 0.1, green: 0.1, blue: 0.2, alpha: 1 },
    },
  });
  const video = builder.source("video", "asset://clip.mp4", { hasAudio: true });
  const music = builder.source("audio", "asset://music.m4a");
  const track = builder.videoTrack();
  const first = track.clip({ source: backdrop, at: 0, duration: 2 });
  const second = track.clip({
    source: video,
    at: 1.5,
    duration: 3,
    sourceIn: 10,
    sourceDuration: 6,
    with: { blend_mode: "screen" },
  });
  track.transition(first, second, {
    id: "cross_dissolve",
    version: 1,
    params: {},
  });
  builder.audioTrack().clip({ source: music, at: 0, duration: ticks(540_000) });
  builder
    .captionTrack({ style: { font_size: 0.06 } })
    .cue({ at: 2, duration: 1, text: "Second" })
    .cue({ at: 0, duration: 2, text: "First" });
  const project = builder.build();
  assert.deepEqual(validateProject(project), []);
  const [videoTrack, audioTrack, captions] = project.timeline.tracks;
  assert.deepEqual(videoTrack.transitions[0].timeline_range, {
    start: 180_000,
    end: 240_000,
  });
  // Twice as much source as timeline: double speed.
  assert.deepEqual(videoTrack.clips[1].source_range, {
    start: 1_200_000,
    end: 1_920_000,
  });
  assert.equal(videoTrack.clips[1].blend_mode, "screen");
  assert.equal(audioTrack.kind, "audio");
  assert.deepEqual(
    captions.captions.cues.map((cue) => cue.text),
    ["First", "Second"],
  );
});

test("an invalid document is refused with every problem", () => {
  const builder = new ProjectBuilder({
    name: "Broken",
    width: 64,
    height: 64,
    frameRate: 30,
  });
  const image = builder.source("image", "asset://a.png");
  const track = builder.videoTrack();
  track.clip({ source: image, at: 0, duration: 2 });
  track.clip({ source: image, at: 1, duration: 2 });
  track.clip({ source: "missing", at: 5, duration: 1 });
  assert.throws(
    () => builder.build(),
    /overlap.*unknown source|unknown source.*overlap/s,
  );
});

test("clips show compositions: an effect chain, or a graph added by id", () => {
  const builder = new ProjectBuilder({
    name: "Looks",
    width: 64,
    height: 64,
    frameRate: 30,
  });
  const image = builder.source("image", "asset://a.png");
  const other = builder.source("image", "asset://b.png");
  const blur = { id: "alpha2.effect.gaussian-blur", version: 1 };
  const invert = { id: "alpha2.effect.invert", version: 1 };
  const graph = builder.composition(
    [
      { id: "clip", op: { type: "clip" } },
      { id: "b", op: { type: "media", source_id: other } },
      {
        id: "out",
        op: {
          type: "merge",
          layers: [{ input: "clip" }, { input: "b", blend_mode: "multiply" }],
        },
      },
    ],
    "out",
  );
  const track = builder.videoTrack();
  track.clip({ source: image, at: 0, duration: 1, effects: [blur, invert] });
  track.clip({
    source: image,
    at: 1,
    duration: 1,
    with: { composition: { id: graph } },
  });
  const project = builder.build();
  assert.deepEqual(validateProject(project), []);
  const [chain, merged] = project.timeline.tracks[0].clips;
  assert.equal(merged.composition.id, graph);
  const composition = project.compositions.find(
    (c) => c.id === chain.composition.id,
  );
  assert.deepEqual(
    composition.nodes.map((node) => [node.id, node.op.input ?? null]),
    [
      ["clip", null],
      ["effect-0", "clip"],
      ["effect-1", "effect-0"],
    ],
  );
  assert.equal(composition.output, "effect-1");
  assert.deepEqual(composition.nodes[2].op.effect, invert);
});

test("generated ids skip the ids already given explicitly", () => {
  const builder = new ProjectBuilder({
    name: "Mixed ids",
    width: 64,
    height: 64,
    frameRate: 30,
  });
  const image = builder.source("image", "asset://a.png", { id: "source-1" });
  const other = builder.source("image", "asset://b.png");
  assert.equal(other, "source-2");
  const graph = builder.composition(
    [{ id: "clip", op: { type: "clip" } }],
    "clip",
    { id: "composition-1" },
  );
  builder.videoTrack({ id: "track-2" });
  const track = builder.videoTrack();
  assert.equal(track.id, "track-1");
  assert.equal(builder.captionTrack().id, "track-3");
  const ids = [
    track.clip({ id: "clip-2", source: image, at: 0, duration: 1 }),
    track.clip({ source: image, at: 1, duration: 1 }),
    track.clip({ source: image, at: 2, duration: 1 }),
    // Its effect chain is a composition the builder names, past the explicit `composition-1`.
    track.clip({
      source: other,
      at: 3,
      duration: 1,
      effects: [{ id: "alpha2.effect.invert", version: 1 }],
    }),
    track.clip({
      source: other,
      at: 4,
      duration: 1,
      with: { composition: { id: graph } },
    }),
  ];
  assert.deepEqual(ids, ["clip-2", "clip-1", "clip-3", "clip-4", "clip-5"]);
  const project = builder.build();
  assert.deepEqual(
    project.compositions.map((composition) => composition.id),
    ["composition-1", "composition-2"],
  );
  assert.equal(
    project.timeline.tracks[1].clips[3].composition.id,
    "composition-2",
  );
});

test("a clip id is unique across tracks, and an id generator that only repeats is refused", () => {
  const builder = new ProjectBuilder({
    name: "Ids",
    width: 64,
    height: 64,
    frameRate: 30,
    ids: (kind) => `${kind}-only`,
  });
  const image = builder.source("image", "asset://a.png");
  builder.videoTrack({ id: "a" }).clip({ source: image, at: 0, duration: 1 });
  const second = builder.videoTrack({ id: "b" });
  assert.throws(
    () => second.clip({ source: image, at: 0, duration: 1 }),
    (error) => error instanceof RangeError && /clip-only/.test(error.message),
  );
  assert.throws(() => builder.source("image", "asset://b.png"), RangeError);
  // An explicit id is not checked until build, which reports the repeat.
  second.clip({ id: "clip-only", source: image, at: 1, duration: 1 });
  assert.throws(() => builder.build(), /duplicate clip id clip-only/);
});

test("effects and a composition given in `with` together are refused", () => {
  const builder = new ProjectBuilder({
    name: "Both",
    width: 64,
    height: 64,
    frameRate: 30,
  });
  const image = builder.source("image", "asset://a.png");
  const graph = builder.composition(
    [{ id: "clip", op: { type: "clip" } }],
    "clip",
  );
  const track = builder.videoTrack();
  assert.throws(
    () =>
      track.clip({
        source: image,
        at: 0,
        duration: 1,
        effects: [{ id: "alpha2.effect.invert", version: 1 }],
        with: { composition: { id: graph, offset: 5 } },
      }),
    (error) =>
      error instanceof RangeError &&
      /effects/.test(error.message) &&
      /composition/.test(error.message),
  );
  const project = builder.build();
  assert.deepEqual(project.timeline.tracks[0].clips, []);
  assert.deepEqual(
    project.compositions.map((composition) => composition.id),
    [graph],
  );
  // Either one alone, or effects with `composition: null`, is fine.
  track.clip({
    source: image,
    at: 0,
    duration: 1,
    effects: [{ id: "alpha2.effect.invert", version: 1 }],
    with: { composition: null },
  });
  assert.deepEqual(validateProject(builder.build()), []);
});
