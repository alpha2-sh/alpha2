import test from "node:test";
import assert from "node:assert/strict";
import {
  createEditor,
  createDocument,
  serializeDocument,
  parseDocument,
} from "../dist/index.js";
const project = {
  schema_version: 1,
  id: "p",
  name: "Transition",
  settings: {
    width: 64,
    height: 64,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48000,
  },
  sources: [
    {
      id: "s",
      kind: "generated",
      generator: {
        type: "fill",
        paint: {
          type: "solid",
          color: { red: 1, green: 0, blue: 0, alpha: 1 },
        },
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
            id: "a",
            source_id: "s",
            timeline_range: { start: 0, end: 360000 },
            source_range: { start: 0, end: 360000 },
          },
          {
            id: "b",
            source_id: "s",
            timeline_range: { start: 360000, end: 720000 },
            source_range: { start: 0, end: 360000 },
            audio: {
              gain: {
                mode: "keyframes",
                keyframes: [
                  { time: 360000, value: 0, interpolation: { type: "linear" } },
                  { time: 720000, value: 1, interpolation: { type: "linear" } },
                ],
              },
            },
          },
        ],
      },
    ],
  },
};
test("transition creation, resizing, removal move animation atomically and preserve identity through undo", async () => {
  const editor = createEditor({ document: createDocument(project) });
  const original = editor.document;
  const result = editor.dispatch({
    type: "transition.add",
    trackId: "t",
    fromId: "a",
    toId: "b",
    duration: 60000,
    effect: { id: "alpha2.transition.crossfade", version: 1 },
  });
  assert.equal(result.ok, true, result.error?.message);
  let track = editor.document.project.timeline.tracks[0];
  assert.equal(track.clips[1].timeline_range.start, 300000);
  assert.equal(track.clips[1].audio.gain.keyframes[0].time, 300000);
  const id = editor.document.metadata.instanceIds["t/transitions"][0];
  assert.equal(
    editor.dispatch({
      type: "transition.resize",
      trackId: "t",
      instanceId: id,
      duration: 120000,
    }).ok,
    true,
  );
  track = editor.document.project.timeline.tracks[0];
  assert.equal(track.clips[1].timeline_range.start, 240000);
  assert.equal(track.transitions[0].timeline_range.start, 240000);
  assert.equal(
    serializeDocument(parseDocument(serializeDocument(editor.document))),
    serializeDocument(editor.document),
  );
  assert.equal(
    editor.dispatch({ type: "transition.remove", trackId: "t", instanceId: id })
      .ok,
    true,
  );
  assert.deepEqual(
    editor.document.project.timeline.tracks[0].clips,
    original.project.timeline.tracks[0].clips,
  );
  editor.undo();
  assert.equal(editor.document.metadata.instanceIds["t/transitions"][0], id);
  const before = editor.document;
  assert.equal(
    editor.dispatch({
      type: "transition.resize",
      trackId: "t",
      instanceId: id,
      duration: 360000,
    }).ok,
    false,
  );
  assert.equal(editor.document, before);
  editor.dispatch({ type: "track.metadata", id: "t", patch: { locked: true } });
  assert.equal(
    editor.dispatch({ type: "transition.remove", trackId: "t", instanceId: id })
      .ok,
    false,
  );
  await editor.dispose();
});
