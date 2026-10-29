import test from "node:test";
import assert from "node:assert/strict";

import {
  CURRENT_SCHEMA_VERSION,
  EASING_PRESETS,
  TICKS_PER_SECOND,
  constant,
  cubicBezier,
  samplesToTicks,
  secondsToTicks,
  ticksToSamples,
  ticksToSeconds,
  AUDIO_EFFECT_STATE_MARGIN_MS,
  MAX_AUDIO_DELAY_MS,
  MAX_AUDIO_EFFECTS,
  REVERB_DELAY_MS,
  MAX_CLIPS,
  MAX_CLIPS_PER_TRACK,
  MAX_COMPOSITIONS,
  MAX_PROJECT_JSON_BYTES,
  MAX_TRANSITIONS_PER_TRACK,
  canonicalPaint,
  canonicalProject,
  canonicalRgba,
  validateProject,
  validateProjectJson,
} from "../dist/project.js";
import { readValidationCorpus } from "../../../scripts/lib/validation-corpus.mjs";

/** `document`'s first clip shown through one effect node. */
function withEffect(document, effect) {
  document.compositions = [
    {
      id: "look",
      nodes: [
        { id: "clip", op: { type: "clip" } },
        { id: "fx", op: { type: "effect", input: "clip", effect } },
      ],
      output: "fx",
    },
  ];
  document.timeline.tracks[0].clips[0].composition = { id: "look" };
  return document;
}

function project(overrides = {}) {
  return {
    schema_version: CURRENT_SCHEMA_VERSION,
    id: "p",
    name: "Project",
    settings: {
      width: 1920,
      height: 1080,
      frame_rate: { num: 30000, den: 1001 },
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
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "c-video",
              source_id: "video",
              timeline_range: { start: 0, end: TICKS_PER_SECOND },
              source_range: { start: 0, end: TICKS_PER_SECOND },
            },
          ],
        },
        {
          id: "a1",
          name: "Music",
          kind: "audio",
          clips: [
            {
              id: "c-music",
              source_id: "music",
              timeline_range: { start: 0, end: TICKS_PER_SECOND },
              source_range: { start: 0, end: TICKS_PER_SECOND },
            },
          ],
        },
      ],
    },
    ...overrides,
  };
}

function withAudioTrack(audio) {
  const base = project();
  const tracks = base.timeline.tracks.map((track) =>
    track.id === "a1" ? { ...track, audio } : track,
  );
  return { ...base, timeline: { tracks } };
}

test("shared Rust and TypeScript validation corpus agrees", async () => {
  const cases = await readValidationCorpus();
  for (const fixture of cases) {
    // An invalid case lists exactly the problems it is named for, so another defect cannot hide
    // the loss of its own rule.
    assert.equal(
      fixture.valid,
      fixture.problems === undefined,
      `${fixture.name}: only an invalid case lists problems`,
    );
    const expected = fixture.problems ?? [];
    // A `json` case is text whose spelling matters, which only `validateProjectJson` reads; any
    // other case must also agree once serialized.
    if (fixture.json === undefined) {
      assert.deepEqual(
        validateProject(fixture.project),
        expected,
        fixture.name,
      );
    }
    const text = fixture.json ?? JSON.stringify(fixture.project);
    assert.deepEqual(
      validateProjectJson(text),
      expected,
      `${fixture.name} as text`,
    );
  }
});

test("a chain of compositions as long as a project holds is validated without exhausting the stack", () => {
  // The engine refuses a group chain this deep, and so must validation, without a RangeError.
  const document = project();
  document.compositions = Array.from({ length: 10_000 }, (_, level) => ({
    id: `c${level}`,
    nodes: [
      level === 9_999
        ? { id: "clip", op: { type: "clip" } }
        : { id: "g", op: { type: "group", composition: `c${level + 1}` } },
    ],
    output: level === 9_999 ? "clip" : "g",
  }));
  document.timeline.tracks[0].clips[0].composition = { id: "c0" };
  assert.deepEqual(validateProject(document), [
    "composition c0 nests compositions more than 64 deep",
  ]);
  // The same chain closed into a loop is found by the same walk.
  document.compositions[9_999] = {
    id: "c9999",
    nodes: [{ id: "g", op: { type: "group", composition: "c0" } }],
    output: "g",
  };
  assert.deepEqual(validateProject(document), ["composition c0 uses itself"]);
});

/** How long `run` takes, in milliseconds, and what it returned. */
function timed(run) {
  const started = performance.now();
  const value = run();
  return { value, ms: performance.now() - started };
}

/**
 * Validation of a document as big as a project holds: well under a second where the engine's own
 * model takes milliseconds.
 */
const LINEAR_MS = 2_000;

test("clips that all overlap are refused for their first uncovered overlap, in time linear in the clips", () => {
  const clips = (count) =>
    Array.from({ length: count }, (_, index) => ({
      id: `c${index}`,
      source_id: "video",
      timeline_range: { start: 0, end: 1 },
      source_range: { start: 0, end: 1 },
    }));
  // As many as a track holds: the engine refuses it for c0 and c1.
  const full = project();
  full.timeline.tracks[0].clips = clips(4_096);
  const within = timed(() => validateProject(full));
  assert.deepEqual(within.value, [
    "clips c0 and c1 overlap without a transition",
  ]);
  assert.ok(within.ms < LINEAR_MS, `${within.ms} ms`);
  // More than a track holds: refused for the count before anything is compared.
  const over = project();
  over.timeline.tracks[0].clips = clips(8_000);
  const beyond = timed(() => validateProject(over));
  assert.deepEqual(beyond.value, ["track v1 has too many clips"]);
  assert.ok(beyond.ms < LINEAR_MS, `${beyond.ms} ms`);
});

test("a value of a union is walked by its forms only until one fits", async () => {
  const cases = await readValidationCorpus();
  const document = structuredClone(
    cases.find((fixture) => fixture.name === "keyframes-ok").project,
  );
  let walks = 0;
  const counted = (value) =>
    new Proxy(value, {
      ownKeys(target) {
        walks += 1;
        return Reflect.ownKeys(target);
      },
    });
  const visual = document.timeline.tracks[0].clips[0].visual;
  // An animated value's first form is the constant one: a constant fits it and no other is tried.
  visual.opacity = counted({ mode: "constant", value: 0.5 });
  assert.deepEqual(validateProject(document), []);
  assert.equal(
    walks,
    1,
    "the keyframes form was walked after the constant fit",
  );
  // A value no form fits is still reported against the form its tag selects.
  visual.opacity = { mode: "constant", value: "half" };
  assert.deepEqual(validateProject(document), [
    "project.timeline.tracks[0].clips[0].visual.opacity.value must be number",
  ]);
});

test("a document with more shape problems than a call takes arguments is reported, not thrown", () => {
  const document = project();
  document.timeline.tracks[0].clips[0].visual = {
    opacity: {
      mode: "keyframes",
      keyframes: Array.from({ length: 200_000 }, (_, time) => ({
        time,
        value: "x",
        interpolation: { type: "linear" },
      })),
    },
  };
  const problems = validateProject(document);
  const keyframes = problems.filter((problem) =>
    /visual\.opacity\.keyframes\[\d+\]\.value must be number$/.test(problem),
  );
  assert.equal(keyframes.length, 200_000);
});

test("group nodes are checked against their target's controls in time linear in the document", () => {
  const document = project();
  const controls = Array.from({ length: 20_000 }, (_, index) => ({
    name: `c${index}`,
    node: "fx",
    param: `p${index}`,
  }));
  document.compositions = [
    {
      id: "target",
      nodes: [
        { id: "clip", op: { type: "clip" } },
        {
          id: "fx",
          op: {
            type: "effect",
            input: "clip",
            effect: { id: "alpha2.effect.brightness", version: 1 },
          },
        },
      ],
      output: "fx",
      controls,
    },
    ...Array.from({ length: 40 }, (_, index) => ({
      id: `groups${index}`,
      nodes: Array.from({ length: 256 }, (_, node) => ({
        id: `g${node}`,
        op: {
          type: "group",
          composition: "target",
          controls: {
            c1: { mode: "constant", value: { type: "float", value: 0.5 } },
          },
        },
      })),
      output: "g0",
    })),
  ];
  const { value, ms } = timed(() => validateProject(document));
  assert.ok(
    value.includes("composition target has invalid group inputs or controls"),
    value.join("\n"),
  );
  assert.ok(ms < LINEAR_MS, `${ms} ms`);
});

test("more tracks than a project holds are refused before their ducking is followed", () => {
  // Every track ducks from a, and a and b duck from each other: every walk is as long as the track list.
  const ducking = (sidechain_track) => ({
    ducking: {
      sidechain_track,
      threshold: 0.1,
      depth_db: -12,
      attack_ms: 5,
      release_ms: 100,
    },
  });
  const tracks = (count) => [
    ...Array.from({ length: count }, (_, index) => ({
      id: `t${index}`,
      name: "Track",
      kind: "audio",
      clips: [],
      audio: ducking("a"),
    })),
    { id: "a", name: "A", kind: "audio", clips: [], audio: ducking("b") },
    { id: "b", name: "B", kind: "audio", clips: [], audio: ducking("a") },
  ];
  const over = project();
  over.timeline.tracks = tracks(2_000);
  const beyond = timed(() => validateProject(over));
  assert.deepEqual(beyond.value, [
    "project must not contain more than 256 tracks",
  ]);
  assert.ok(beyond.ms < LINEAR_MS, `${beyond.ms} ms`);
  // Within the limit the cycle is still found, for the two tracks on it only.
  const within = project();
  within.timeline.tracks = tracks(254);
  assert.deepEqual(validateProject(within), [
    "track a has an unusable ducking sidechain",
    "track b has an unusable ducking sidechain",
  ]);
});

test("a minimal project validates and serializes as the wire document", () => {
  const document = project();
  assert.deepEqual(validateProject(document), []);
  const decoded = JSON.parse(JSON.stringify(document));
  assert.equal(decoded.schema_version, CURRENT_SCHEMA_VERSION);
  assert.equal(decoded.timeline.tracks[1].clips[0].source_id, "music");
});

test("effect resource references resolve through the typed project table", () => {
  const document = project({
    effect_resources: [
      { id: "look", kind: "lut3d", locator: "asset://look.cube" },
    ],
  });
  withEffect(document, {
    id: "alpha2.effect.lut-3d",
    version: 1,
    resources: { lut: "look" },
  });
  assert.deepEqual(validateProject(document), []);

  document.compositions[0].nodes[1].op.effect.resources.lut = "missing";
  assert.ok(
    validateProject(document).some((problem) =>
      problem.includes("unknown effect resource missing"),
    ),
  );
});

test("tick conversion round trips seconds and samples exactly", () => {
  assert.equal(secondsToTicks(1), TICKS_PER_SECOND);
  assert.equal(ticksToSeconds(TICKS_PER_SECOND), 1);
  assert.equal(samplesToTicks(48000, 48000), TICKS_PER_SECOND);
  assert.equal(ticksToSamples(TICKS_PER_SECOND, 48000), 48000);
  // A sample rate that does not divide the tick rate still floors predictably.
  assert.equal(samplesToTicks(1, 44100), Math.trunc(TICKS_PER_SECOND / 44100));
  assert.equal(samplesToTicks(0, 0), 0);
  assert.equal(ticksToSamples(1000, 0), 0);
});

test("validation reports structural problems without opening media", () => {
  assert.ok(
    validateProject({
      ...project(),
      schema_version: CURRENT_SCHEMA_VERSION + 1,
    }).some((problem) => problem.includes("schema_version")),
  );

  const emptyRange = project();
  emptyRange.timeline.tracks[0].clips[0].timeline_range = {
    start: 10,
    end: 10,
  };
  assert.ok(
    validateProject(emptyRange).some((problem) =>
      problem.includes("empty timeline_range"),
    ),
  );

  const unknownSource = project();
  unknownSource.timeline.tracks[0].clips[0].source_id = "absent";
  assert.ok(
    validateProject(unknownSource).some((problem) =>
      problem.includes("unknown source"),
    ),
  );

  const wrongDomain = project();
  wrongDomain.timeline.tracks[1].clips[0].source_id = "video";
  assert.ok(
    validateProject(wrongDomain).some((problem) =>
      problem.includes("cannot sit on a audio track"),
    ),
  );
});

test("audio gain and pan are range checked like the engine checks them", () => {
  const tooLoud = withAudioTrack(undefined);
  tooLoud.timeline.tracks[1].clips[0].audio = { gain: constant(100) };
  assert.ok(
    validateProject(tooLoud).some((problem) =>
      problem.includes("gain outside"),
    ),
  );

  const offCentre = project();
  offCentre.timeline.tracks[1].clips[0].audio = { pan: constant(2) };
  assert.ok(
    validateProject(offCentre).some((problem) =>
      problem.includes("pan outside"),
    ),
  );

  const negativeFade = project();
  negativeFade.timeline.tracks[1].clips[0].audio = { fade_in: -1 };
  assert.ok(
    validateProject(negativeFade).some((problem) =>
      problem.includes("negative fade"),
    ),
  );

  const unordered = project();
  unordered.timeline.tracks[1].clips[0].audio = {
    gain: {
      mode: "keyframes",
      keyframes: [
        { time: 100, value: 1, interpolation: { type: "linear" } },
        { time: 0, value: 0, interpolation: { type: "linear" } },
      ],
    },
  };
  assert.ok(
    validateProject(unordered).some((problem) =>
      problem.includes("invalid gain keyframes"),
    ),
  );

  const valid = project();
  valid.timeline.tracks[1].clips[0].audio = {
    gain: {
      mode: "keyframes",
      keyframes: [
        { time: 0, value: 0, interpolation: { type: "linear" } },
        { time: 100, value: 1, interpolation: { type: "linear" } },
      ],
    },
    pan: constant(-0.5),
    fade_in: 12000,
    effects: [{ type: "reverb", room_size: 0.5, damping: 0.5, mix: 0.2 }],
  };
  assert.deepEqual(validateProject(valid), []);
});

test("the audio effects a project holds at once are bounded, as the engine bounds them", () => {
  const eq = {
    type: "eq",
    bands: [{ kind: "peaking", frequency_hz: 1000, q: 1, gain_db: 3 }],
  };
  const start = 10 * TICKS_PER_SECOND;
  const withClips = (count, period, effects) => {
    const document = project();
    const [template] = document.timeline.tracks[1].clips;
    const length = template.timeline_range.end - template.timeline_range.start;
    document.timeline.tracks[1].clips = Array.from(
      { length: count },
      (_, index) => ({
        ...template,
        id: `c-music-${index}`,
        timeline_range: {
          start: start + period * index,
          end: start + period * index + length,
        },
        audio: { effects },
      }),
    );
    return document;
  };
  const margin = (AUDIO_EFFECT_STATE_MARGIN_MS * TICKS_PER_SECOND) / 1000;
  const eqs = Array(16).fill(eq);
  assert.deepEqual(
    validateProject(withClips(MAX_AUDIO_EFFECTS / 16, 0, eqs)),
    [],
  );
  assert.deepEqual(
    validateProject(withClips(MAX_AUDIO_EFFECTS / 16 + 1, 0, eqs)),
    [
      `project has ${MAX_AUDIO_EFFECTS + 16} audio effects holding state at tick ${start - margin}; the maximum at once is ${MAX_AUDIO_EFFECTS}`,
    ],
  );
  const apart = 10 * TICKS_PER_SECOND;
  assert.deepEqual(
    validateProject(withClips(MAX_AUDIO_EFFECTS / 16 + 1, apart, eqs)),
    [],
  );
  // 751 reverbs one after another hold one reverb's delay at a time; at once they are over.
  const reverb = [{ type: "reverb", room_size: 0.5, damping: 0.5, mix: 0.25 }];
  assert.deepEqual(validateProject(withClips(751, apart, reverb)), []);
  assert.deepEqual(validateProject(withClips(2000, apart, reverb)), []);
  assert.deepEqual(validateProject(withClips(751, 0, reverb)), [
    `project audio effects hold ${751 * REVERB_DELAY_MS} ms of delay at tick ${start - margin}; the maximum at once is ${MAX_AUDIO_DELAY_MS} ms`,
  ]);
});

test("audio tracks may overlay while visual tracks need transitions", () => {
  const overlayed = project();
  overlayed.timeline.tracks[1].clips.push({
    ...overlayed.timeline.tracks[1].clips[0],
    id: "c-music-2",
    timeline_range: { start: TICKS_PER_SECOND / 2, end: TICKS_PER_SECOND * 2 },
  });
  assert.deepEqual(validateProject(overlayed), []);

  const stacked = project();
  stacked.timeline.tracks[0].clips.push({
    ...stacked.timeline.tracks[0].clips[0],
    id: "c-video-2",
    timeline_range: { start: TICKS_PER_SECOND / 2, end: TICKS_PER_SECOND * 2 },
  });
  assert.ok(
    validateProject(stacked).some((problem) =>
      problem.includes("overlap without a transition"),
    ),
  );

  // A transition covering the intersection makes the visual overlap legal.
  stacked.timeline.tracks[0].transitions = [
    {
      from_clip: "c-video",
      to_clip: "c-video-2",
      timeline_range: { start: TICKS_PER_SECOND / 2, end: TICKS_PER_SECOND },
      easing: EASING_PRESETS.easeInOut,
      effect: { id: "alpha2.transition.fade", version: 1 },
    },
  ];
  assert.deepEqual(validateProject(stacked), []);

  for (const easing of [
    ...Object.values(EASING_PRESETS),
    cubicBezier(0.34, 1.56, 0.64, 1),
  ]) {
    stacked.timeline.tracks[0].transitions[0].easing = easing;
    assert.deepEqual(validateProject(stacked), []);
  }
  // x1 and x2 keep the curve a function of time.
  stacked.timeline.tracks[0].transitions[0].easing = cubicBezier(1.2, 0, 1, 1);
  assert.ok(
    validateProject(stacked).some((problem) =>
      problem.includes("invalid clip transition"),
    ),
  );
  // The easing is part of the document's shape: required, and one of a closed set.
  delete stacked.timeline.tracks[0].transitions[0].easing;
  assert.deepEqual(validateProject(stacked), [
    "project.timeline.tracks[0].transitions[0].easing is required",
  ]);
  stacked.timeline.tracks[0].transitions[0].easing = { type: "bounce" };
  assert.match(
    validateProject(stacked)[0],
    /^project\.timeline\.tracks\[0\]\.transitions\[0\]\.easing\.type must be one of "linear", "cubic_bezier"$/,
  );
});

test("a video source declares whether it carries audio", () => {
  const document = project();
  assert.equal(document.sources[0].has_audio, undefined);
  const withAudio = {
    ...document,
    sources: [{ ...document.sources[0], has_audio: true }, document.sources[1]],
  };
  assert.deepEqual(validateProject(withAudio), []);
});

test("ducking may name any track", () => {
  assert.deepEqual(
    validateProject(
      withAudioTrack({
        ducking: {
          sidechain_track: "a1",
          threshold: 0.1,
          depth_db: -12,
          attack_ms: 5,
          release_ms: 100,
        },
      }),
    ).filter((problem) => problem.includes("ducking")),
    ["track a1 has an unusable ducking sidechain"],
  );

  // Every track carries an audio bus, so a visual track may drive a sidechain.
  assert.deepEqual(
    validateProject(
      withAudioTrack({
        ducking: {
          sidechain_track: "v1",
          threshold: 0.1,
          depth_db: -12,
          attack_ms: 5,
          release_ms: 100,
        },
      }),
    ),
    [],
  );

  assert.ok(
    validateProject(
      withAudioTrack({
        ducking: {
          sidechain_track: "absent",
          threshold: 0.1,
          depth_db: -12,
          attack_ms: 5,
          release_ms: 100,
        },
      }),
    ).some((problem) => problem.includes("unknown track")),
  );
});

test("validation mirrors effect, visual, bus, transition, and cycle invariants", () => {
  const emptyLocator = project();
  emptyLocator.sources[0].locator = "";
  assert.ok(
    validateProject(emptyLocator).some((problem) =>
      problem.includes("empty locator"),
    ),
  );

  const invalidVisual = project();
  invalidVisual.timeline.tracks[0].clips[0].visual = {
    position: constant({ x: 0.5, y: 0.5 }),
    anchor: { x: 0.5, y: 0.5 },
    scale: constant({ x: 1, y: 1 }),
    rotation_degrees: constant(0),
    opacity: constant(2),
  };
  assert.ok(
    validateProject(invalidVisual).some((problem) =>
      problem.includes("invalid visual properties"),
    ),
  );

  const invalidEffect = withEffect(project(), {
    id: "",
    version: 0,
    params: { "not-valid!": constant({ type: "float", value: 1 }) },
  });
  assert.ok(
    validateProject(invalidEffect).some((problem) =>
      problem.includes("node fx of composition look has an invalid setting"),
    ),
  );
  // JSON has no NaN (`JSON.stringify` writes `null`), so it is a shape error.
  withEffect(invalidEffect, {
    id: "a",
    version: 1,
    params: { amount: constant({ type: "float", value: NaN }) },
  });
  assert.deepEqual(validateProject(invalidEffect), [
    "project.compositions[0].nodes[1].op.effect.params.amount.value.value must be number",
  ]);

  const invalidMaster = {
    ...project(),
    audio: { limiter: { threshold: 0, release_ms: 0 } },
  };
  assert.ok(
    validateProject(invalidMaster).some((problem) =>
      problem.includes("master bus"),
    ),
  );

  const invalidTransition = project();
  invalidTransition.timeline.tracks[0].transitions = [
    {
      from_clip: "c-video",
      to_clip: "absent",
      timeline_range: { start: 0, end: 1 },
      easing: EASING_PRESETS.linear,
      effect: { id: "alpha2.transition.fade", version: 1 },
    },
  ];
  assert.ok(
    validateProject(invalidTransition).some((problem) =>
      problem.includes("invalid clip transition"),
    ),
  );

  const cycle = project();
  cycle.timeline.tracks[0].audio = {
    ducking: {
      sidechain_track: "a1",
      threshold: 0.1,
      depth_db: -12,
      attack_ms: 5,
      release_ms: 100,
    },
  };
  cycle.timeline.tracks[1].audio = {
    ducking: {
      sidechain_track: "v1",
      threshold: 0.1,
      depth_db: -12,
      attack_ms: 5,
      release_ms: 100,
    },
  };
  assert.ok(
    validateProject(cycle).some((problem) =>
      problem.includes("unusable ducking sidechain"),
    ),
  );
});

test("serialized validation mirrors the engine's parse budget", () => {
  const document = project();
  assert.deepEqual(validateProjectJson(JSON.stringify(document)), []);

  // The engine measures UTF-8 bytes; multi-byte text is over the limit while its UTF-16 length is
  // still well under it.
  const padding = "한".repeat(MAX_PROJECT_JSON_BYTES / 3);
  const oversized = JSON.stringify(project({ name: padding }));
  assert.ok(
    oversized.length < MAX_PROJECT_JSON_BYTES,
    "the naive measure must not catch this",
  );
  const problems = validateProjectJson(oversized);
  assert.equal(problems.length, 1);
  assert.match(problems[0], /over the \d+ byte limit/);
});

test("serialized validation reports malformed JSON instead of throwing", () => {
  const problems = validateProjectJson('{"schema_version":');
  assert.equal(problems.length, 1);
  assert.match(problems[0], /malformed/);
});

test("serialized validation still reports structural problems", () => {
  const document = project({ schema_version: CURRENT_SCHEMA_VERSION + 1 });
  assert.ok(
    validateProjectJson(JSON.stringify(document)).some((problem) =>
      problem.includes("schema_version"),
    ),
  );
});

/**
 * A document at a count limit, made rather than stored; `document_with` in the model's tests makes
 * the same ones.
 */
function documentWith(limit, count) {
  const clip = (index, source) => ({
    id: `c${index}`,
    source_id: source,
    timeline_range: { start: index * 10, end: index * 10 + 10 },
    source_range: { start: 0, end: 10 },
  });
  const track = (id, clips, transitions) => ({
    id,
    name: id,
    kind: "video",
    clips,
    transitions,
  });
  const range = (from, to) =>
    Array.from({ length: to - from }, (_, offset) => from + offset);
  const sources = [{ id: "v", kind: "video", locator: "v.mp4" }];
  const tracks = [];
  const compositions = [];
  if (limit === "sources") {
    for (const index of range(1, count))
      sources.push({ id: `s${index}`, kind: "audio", locator: `${index}.m4a` });
  } else if (limit === "clips_on_a_track") {
    tracks.push(
      track(
        "t",
        range(0, count).map((index) => clip(index, "v")),
        [],
      ),
    );
  } else if (limit === "clips") {
    for (let index = 0; index < count; index += MAX_CLIPS_PER_TRACK) {
      const end = Math.min(index + MAX_CLIPS_PER_TRACK, count);
      tracks.push(
        track(
          `t${index}`,
          range(index, end).map((at) => clip(at, "v")),
          [],
        ),
      );
    }
  } else if (limit === "transitions_on_a_track") {
    const transition = {
      from_clip: "c0",
      to_clip: "c1",
      timeline_range: { start: 5, end: 10 },
      easing: { type: "linear" },
      effect: { id: "cross_dissolve", version: 1, params: {} },
    };
    tracks.push(
      track("t", [clip(0, "v"), clip(1, "v")], Array(count).fill(transition)),
    );
  } else if (limit === "compositions") {
    for (const index of range(0, count))
      compositions.push({
        id: `k${index}`,
        nodes: [{ id: "clip", op: { type: "clip" } }],
        output: "clip",
      });
  }
  return JSON.stringify({
    schema_version: CURRENT_SCHEMA_VERSION,
    id: "limits",
    name: "Limits",
    settings: {
      width: 64,
      height: 36,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources,
    compositions,
    timeline: { tracks },
  });
}

test("count limits admit their maximum and refuse one more", () => {
  for (const [limit, maximum, problem] of [
    ["sources", MAX_CLIPS, /more than \d+ sources/],
    ["clips_on_a_track", MAX_CLIPS_PER_TRACK, /has too many clips/],
    ["clips", MAX_CLIPS, /more than \d+ clips/],
    [
      "transitions_on_a_track",
      MAX_TRANSITIONS_PER_TRACK,
      /too many transitions/,
    ],
    ["compositions", MAX_COMPOSITIONS, /more than \d+ compositions/],
  ]) {
    const at = validateProjectJson(documentWith(limit, maximum));
    const over = validateProjectJson(documentWith(limit, maximum + 1));
    assert.ok(
      over.some((found) => problem.test(found)),
      `${limit} at ${maximum + 1}: ${over.slice(0, 3)}`,
    );
    // Two clips hold one transition, so a full track of them is refused, but not for its count.
    if (limit === "transitions_on_a_track") {
      assert.ok(
        !at.some((found) => problem.test(found)),
        `${limit} at ${maximum}`,
      );
    } else {
      assert.deepEqual(at, [], `${limit} at ${maximum}`);
    }
  }
});

/** Where `value` still holds a form the engine reads but never writes. */
function nonCanonical(value, path, found) {
  if (Array.isArray(value)) {
    value.forEach((item, index) =>
      nonCanonical(item, `${path}[${index}]`, found),
    );
    return found;
  }
  if (typeof value !== "object" || value === null) return found;
  for (const [key, field] of Object.entries(value)) {
    const at = `${path}.${key}`;
    if (key === "color" && typeof field === "string") found.push(at);
    if (key === "padding" || key === "margin" || key === "translate") {
      const sides =
        field?.mode === "constant"
          ? [field.value]
          : (field?.keyframes ?? []).map((k) => k.value);
      if (sides.some((side) => typeof side !== "object")) found.push(at);
    }
    if (key === "paint" || key === "background") {
      const paints =
        field?.mode === "constant"
          ? [field.value]
          : field?.mode === "keyframes"
            ? field.keyframes.map((k) => k.value)
            : key === "paint"
              ? [field]
              : [];
      if (
        paints.some((paint) => typeof paint !== "object" || !("type" in paint))
      )
        found.push(at);
    }
    nonCanonical(field, at, found);
  }
  return found;
}

test("canonicalProject writes every valid corpus document in the forms the engine writes", async () => {
  for (const fixture of await readValidationCorpus()) {
    if (!fixture.valid || fixture.json !== undefined) continue;
    const before = JSON.stringify(fixture.project);
    const canonical = canonicalProject(fixture.project);
    assert.equal(
      JSON.stringify(fixture.project),
      before,
      `${fixture.name}: input unchanged`,
    );
    assert.deepEqual(nonCanonical(canonical, "project", []), [], fixture.name);
    assert.deepEqual(validateProject(canonical), [], fixture.name);
    assert.equal(
      canonicalProject(canonical),
      canonical,
      `${fixture.name}: a canonical document is returned itself`,
    );
  }
});

test("a hex color becomes the components the engine reads, spelled as it writes them", () => {
  // 30/255, 34/255 and 48/255 as 32-bit floats, spelled shortest, as the engine writes them.
  assert.deepEqual(canonicalRgba("#1e2230"), {
    red: 0.11764706,
    green: 0.13333334,
    blue: 0.1882353,
    alpha: 1,
  });
  assert.deepEqual(canonicalRgba("#f008"), {
    red: 1,
    green: 0,
    blue: 0,
    alpha: 0.53333336,
  });
  const object = { red: 0.5, green: 0.5, blue: 0.5, alpha: 1 };
  assert.equal(canonicalRgba(object), object);
  assert.throws(() => canonicalRgba("#ggg"), /invalid color "#ggg"/);
  assert.deepEqual(canonicalPaint("#000"), {
    type: "solid",
    color: { red: 0, green: 0, blue: 0, alpha: 1 },
  });
  assert.deepEqual(canonicalPaint(object), { type: "solid", color: object });
  assert.deepEqual(
    canonicalPaint({
      type: "conic_gradient",
      center: { x: 0.5, y: 0.5 },
      stops: [
        { offset: 0, color: "#fff" },
        { offset: 1, color: object },
      ],
    }).stops.map((stop) => stop.color),
    [{ red: 1, green: 1, blue: 1, alpha: 1 }, object],
  );
});

test("canonicalProject writes a layout's sides and offsets as objects", () => {
  const layout = {
    type: "layout",
    root: {
      padding: constant("8px 0.02"),
      background: constant("#000"),
      children: [{ margin: constant(0.01), translate: constant("10px") }],
    },
  };
  const project = {
    schema_version: CURRENT_SCHEMA_VERSION,
    id: "p",
    name: "P",
    settings: {
      width: 1920,
      height: 1080,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "g", kind: "generated", generator: layout }],
    timeline: { tracks: [] },
  };
  const root = canonicalProject(project).sources[0].generator.root;
  assert.deepEqual(root.padding.value, {
    top: "8px",
    right: 0.02,
    bottom: "8px",
    left: 0.02,
  });
  assert.deepEqual(root.background.value, {
    type: "solid",
    color: { red: 0, green: 0, blue: 0, alpha: 1 },
  });
  assert.deepEqual(root.children[0].margin.value, {
    top: 0.01,
    right: 0.01,
    bottom: 0.01,
    left: 0.01,
  });
  assert.deepEqual(root.children[0].translate.value, { x: "10px", y: 0 });
});

test("canonicalProject writes a clip's visual whole, from the engine's defaults", () => {
  const project = {
    schema_version: CURRENT_SCHEMA_VERSION,
    id: "p",
    name: "P",
    settings: {
      width: 1920,
      height: 1080,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "v", kind: "video", locator: "a.mp4" }],
    timeline: {
      tracks: [
        {
          id: "t",
          name: "T",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "v",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
              visual: { opacity: constant(0.5) },
            },
          ],
          transitions: [],
        },
      ],
    },
  };
  assert.deepEqual(validateProject(project), []);
  const canonical = canonicalProject(project);
  assert.deepEqual(canonical.timeline.tracks[0].clips[0].visual, {
    opacity: constant(0.5),
    position: constant({ x: 0.5, y: 0.5 }),
    anchor: { x: 0.5, y: 0.5 },
    scale: constant({ x: 1, y: 1 }),
    rotation_degrees: constant(0),
  });
  assert.deepEqual(project.timeline.tracks[0].clips[0].visual, {
    opacity: constant(0.5),
  });
  assert.equal(canonicalProject(canonical), canonical);
});

test("canonicalProject shares what it does not change and leaves what it cannot read", () => {
  const frozen = (value) => {
    if (typeof value === "object" && value !== null) {
      Object.values(value).forEach(frozen);
      Object.freeze(value);
    }
    return value;
  };
  const project = frozen({
    schema_version: CURRENT_SCHEMA_VERSION,
    id: "p",
    name: "P",
    settings: {
      width: 64,
      height: 36,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      {
        id: "a",
        kind: "generated",
        generator: { type: "fill", paint: "#f00" },
      },
      {
        id: "b",
        kind: "generated",
        generator: {
          type: "fill",
          paint: {
            type: "solid",
            color: { red: 0, green: 0, blue: 1, alpha: 1 },
          },
        },
      },
      {
        id: "c",
        kind: "generated",
        generator: {
          type: "layout",
          root: { color: "#0f0", children: [{ text: constant("a") }] },
        },
      },
    ],
    timeline: { tracks: [] },
  });
  const canonical = canonicalProject(project);
  assert.notEqual(canonical, project);
  // The source already canonical is the same object; the others are read.
  assert.equal(canonical.sources[1], project.sources[1]);
  assert.equal(canonical.settings, project.settings);
  assert.deepEqual(canonical.sources[0].generator.paint, {
    type: "solid",
    color: { red: 1, green: 0, blue: 0, alpha: 1 },
  });
  assert.deepEqual(canonical.sources[2].generator.root.color, {
    red: 0,
    green: 1,
    blue: 0,
    alpha: 1,
  });
  assert.equal(
    canonical.sources[2].generator.root.children,
    project.sources[2].generator.root.children,
  );
  // A frozen document read again is not walked again: the same result comes back.
  assert.equal(canonicalProject(project), canonical);
  // A string that is not a color stays, for validation to report.
  const broken = {
    ...project,
    sources: [
      {
        id: "a",
        kind: "generated",
        generator: { type: "fill", paint: "#ggg" },
      },
    ],
  };
  const read = canonicalProject(broken);
  assert.equal(read.sources[0].generator.paint, "#ggg");
  assert.match(validateProject(read).join(" "), /invalid color "#ggg"/);
});
