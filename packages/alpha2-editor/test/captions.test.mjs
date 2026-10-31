// Subtitle files as bin assets: the cues stay in the document until inserted, then open caption
// tracks from the insertion point.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import {
  captionRows,
  captionsAsset,
  captionsDuration,
  captionTrackCommands,
  createEditor,
  createEmptyDocument,
  parseDocument,
  serializeDocument,
} from "../dist/index.js";
import { createBrowserBridge } from "../dist/browser/index.js";
import { MAX_CAPTION_FILE_BYTES, MAX_CAPTION_FILE_CUES } from "alpha2/project";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";
import { assertScalesBelow } from "../../../scripts/lib/timing.mjs";

const cue = (start, end, text) => ({
  timeline_range: { start, end },
  text,
});
const rows = [
  [cue(0, 120000, "One"), cue(240000, 360000, "Three")],
  [cue(60000, 180000, "Overlapping")],
];

test("a captions asset keeps its cues in the document and touches no track", () => {
  const editor = createEditor({ document: createEmptyDocument() });
  const asset = captionsAsset("subs", "movie.srt", rows);
  assert.equal(editor.dispatch({ type: "asset.put", asset }).ok, true);
  assert.equal(editor.document.project.timeline.tracks.length, 0);
  assert.equal(editor.document.project.sources.length, 0);
  const reopened = parseDocument(serializeDocument(editor.document));
  assert.deepEqual(captionRows(reopened.assets.subs), rows);
  assert.equal(captionsDuration(reopened.assets.subs), 360000);
  assert.deepEqual(captionRows({ ...asset, kind: "text" }), []);
});

test("inserting opens one caption track per row, cues moved to the insertion point", () => {
  const editor = createEditor({ document: createEmptyDocument() });
  const asset = captionsAsset("subs", "movie.srt", rows);
  editor.dispatch({ type: "asset.put", asset });
  let n = 0;
  const { trackIds, commands } = captionTrackCommands(asset, 120000, {
    newId: () => `t${++n}`,
  });
  assert.deepEqual(trackIds, ["t1", "t2"]);
  assert.equal(editor.dispatch({ type: "transaction", commands }).ok, true);
  const tracks = editor.document.project.timeline.tracks;
  assert.deepEqual(
    tracks.map((t) => [t.name, t.kind]),
    [
      ["movie 1", "caption"],
      ["movie 2", "caption"],
    ],
  );
  assert.deepEqual(tracks[0].captions.cues, [
    cue(120000, 240000, "One"),
    cue(360000, 480000, "Three"),
  ]);
  assert.deepEqual(tracks[1].captions.cues, [
    cue(180000, 300000, "Overlapping"),
  ]);
  assert.throws(
    () => captionTrackCommands(captionsAsset("e", "empty.vtt", []), 0),
    /no captions/,
  );
});

test("a captions asset whose rows are not cues is refused when the document opens", () => {
  const document = createEmptyDocument();
  const malformed = [
    [[null]],
    [[{}]],
    [[{ timeline_range: null, text: "x" }]],
    [[1]],
    [["x"]],
    [[cue(0, 1, 7)]],
    [[cue(5, 5, "Empty")]],
    [[cue(-1, 5, "Before zero")]],
    [[cue(0, 2 ** 60, "Unsafe")]],
    [null],
    "rows",
  ];
  for (const rows of malformed) {
    const asset = { ...captionsAsset("subs", "movie.srt", []) };
    asset.authoring = { captions: { rows } };
    const text = JSON.stringify({
      ...document,
      assets: { ...document.assets, subs: asset },
    });
    assert.throws(
      () => parseDocument(text),
      /Invalid captions/,
      JSON.stringify(rows),
    );
    // What reads the rows promises nothing for a document it was not given to validate.
    assert.deepEqual(captionRows(asset), [], JSON.stringify(rows));
    assert.equal(captionsDuration(asset), 0, JSON.stringify(rows));
  }
  const unauthored = { ...captionsAsset("subs", "movie.srt", rows) };
  delete unauthored.authoring;
  assert.throws(
    () =>
      parseDocument(
        JSON.stringify({
          ...document,
          assets: { ...document.assets, subs: unauthored },
        }),
      ),
    /Invalid captions/,
  );
});

const wasmFile = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
  "alpha2_wasm_bg.wasm",
);
// Skips without the packaged wasm (before `npm run build`); ALPHA2_REQUIRE=wasm fails instead.
const noWasm =
  !existsSync(wasmFile) &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

/** A subtitle file as the bridge reads one: its name, size and text. */
const subtitles = (name, text, size = Buffer.byteLength(text)) => ({
  name,
  size,
  arrayBuffer: async () => {
    if (size > Buffer.byteLength(text))
      throw Error(`${name} was read although it is too large`);
    return new TextEncoder().encode(text).buffer;
  },
});
const stamp = (ms) =>
  `${String(Math.floor(ms / 60000)).padStart(2, "0")}:${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}.${String(ms % 1000).padStart(3, "0")}`;

test(
  "a subtitle file past the import limits is refused, without the bin changing",
  { skip: noWasm },
  async () => {
    const editor = createEditor();
    const bridge = createBrowserBridge({
      editor,
      audio: false,
      runtimeAssets: { wasmUrl: readFileSync(wasmFile) },
    });
    try {
      const cue = "00:00.000 --> 00:01.000\nx\n\n";
      const refused = async (file, message) => {
        await assert.rejects(bridge.importCaptions(file), (error) => {
          assert.equal(error.name, "EditorError");
          assert.equal(error.code, "invalid_captions");
          assert.match(error.message, message);
          return true;
        });
        assert.deepEqual(Object.keys(editor.document.assets), []);
      };
      const cues = (count) =>
        subtitles("cues.vtt", `WEBVTT\n\n${cue.repeat(count)}`);
      await refused(
        subtitles("huge.srt", "", MAX_CAPTION_FILE_BYTES + 1),
        new RegExp(`huge.srt is ${MAX_CAPTION_FILE_BYTES + 1} bytes`),
      );
      // One cue past the file limit: refused in about the time reading takes, which is linear.
      const crowded = `WEBVTT\n\n${cue.repeat(MAX_CAPTION_FILE_CUES + 1)}`;
      await refused(
        subtitles("crowded.vtt", crowded),
        new RegExp(`crowded.vtt: .*more than ${MAX_CAPTION_FILE_CUES} cues`),
      );
      await assertScalesBelow(
        "a file of more cues than the limit",
        MAX_CAPTION_FILE_CUES + 1,
        cues,
        (file) => refused(file, /more than/),
      );
      // The file limit's cues, all at once: a row each, found in n log n time.
      const all = `WEBVTT\n\n${cue.repeat(MAX_CAPTION_FILE_CUES)}`;
      await refused(
        subtitles("all.vtt", all),
        /all.vtt: the captions need 65536 caption tracks/,
      );
      await assertScalesBelow(
        "cues all at once",
        MAX_CAPTION_FILE_CUES / 8,
        cues,
        (file) => refused(file, /caption tracks/),
      );
      await refused(subtitles("bare.vtt", "no header"), /bare.vtt: .*WEBVTT/);
      const fits = Array.from(
        { length: 3 },
        (_, i) => `${stamp(i * 1000)} --> ${stamp(i * 1000 + 900)}\nCue ${i}\n`,
      ).join("\n");
      const { assetId, skipped } = await bridge.importCaptions(
        subtitles("fits.vtt", `WEBVTT\n\n${fits}`),
      );
      assert.equal(skipped, 0);
      assert.equal(captionRows(editor.document.assets[assetId]).length, 1);
    } finally {
      await bridge.dispose();
      editor.dispose();
    }
  },
);

test("a cue added or moved before others takes its place in time, with its id", () => {
  // cue.add places a cue by its start rather than appending it, or a caption before the track's
  // last cue would be refused as invalid captions; cue.update moves a cue to its new index.
  const editor = createEditor({ document: createEmptyDocument() });
  const ok = (command) => {
    const result = editor.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({
    type: "track.add",
    track: {
      id: "C",
      name: "Captions",
      kind: "caption",
      clips: [],
      captions: { cues: [cue(10 * 120000, 12 * 120000, "Late")] },
    },
  });
  const [late] = editor.document.metadata.instanceIds["C/cues"];
  ok({
    type: "cue.add",
    trackId: "C",
    instanceId: "early",
    cue: cue(2 * 120000, 4 * 120000, "Early"),
  });
  ok({
    type: "cue.add",
    trackId: "C",
    instanceId: "middle",
    cue: cue(5 * 120000, 6 * 120000, "Middle"),
  });
  const texts = () =>
    editor.document.project.timeline.tracks[0].captions.cues.map((c) => c.text);
  assert.deepEqual(texts(), ["Early", "Middle", "Late"]);
  assert.deepEqual(editor.document.metadata.instanceIds["C/cues"], [
    "early",
    "middle",
    late,
  ]);
  ok({
    type: "cue.update",
    trackId: "C",
    instanceId: "early",
    cue: cue(13 * 120000, 14 * 120000, "Early"),
  });
  assert.deepEqual(texts(), ["Middle", "Late", "Early"]);
  assert.deepEqual(editor.document.metadata.instanceIds["C/cues"], [
    "middle",
    late,
    "early",
  ]);
  const overlap = editor.dispatch({
    type: "cue.add",
    trackId: "C",
    instanceId: "over",
    cue: cue(11 * 120000, 12.5 * 120000, "Over"),
  });
  assert.equal(overlap.ok, false);
  assert.equal(overlap.error.code, "invalid_project");
});

/** A caption track holding one cue of timed words, from 1 s to 4 s, each word a second. */
function karaokeEditor() {
  const editor = createEditor({ document: createEmptyDocument() });
  const second = 120000;
  const result = editor.dispatch({
    type: "track.add",
    track: {
      id: "C",
      name: "Captions",
      kind: "caption",
      clips: [],
      captions: {
        cues: [
          {
            timeline_range: { start: second, end: 4 * second },
            text: [
              { text: "one ", time: { start: 0, end: second } },
              { text: "two ", time: { start: second, end: 2 * second } },
              { text: "three", time: { start: 2 * second, end: 3 * second } },
            ],
          },
        ],
      },
    },
  });
  assert.equal(result.ok, true, result.error?.message);
  return {
    editor,
    second,
    id: editor.document.metadata.instanceIds["C/cues"][0],
  };
}

test("splitting a cue rebases its runs", () => {
  const { editor, second, id } = karaokeEditor();
  // Cut half way through "two": it is said in both halves, cut at the split.
  const result = editor.dispatch({
    type: "cue.split",
    trackId: "C",
    instanceId: id,
    newInstanceId: "after",
    at: 2.5 * second,
  });
  assert.equal(result.ok, true, result.error?.message);
  const [first, second_half] =
    editor.document.project.timeline.tracks[0].captions.cues;
  assert.deepEqual(
    first.text.map((run) => [run.text, run.time]),
    [
      ["one ", { start: 0, end: second }],
      ["two ", { start: second, end: 1.5 * second }],
    ],
  );
  assert.deepEqual(second_half.timeline_range, {
    start: 2.5 * second,
    end: 4 * second,
  });
  assert.deepEqual(
    second_half.text.map((run) => [run.text, run.time]),
    [
      ["two ", { start: 0, end: 0.5 * second }],
      ["three", { start: 0.5 * second, end: 1.5 * second }],
    ],
  );
  assert.deepEqual(editor.document.metadata.instanceIds["C/cues"], [
    id,
    "after",
  ]);
});

test("trimming a cue start drops spoken runs", () => {
  const { editor, second, id } = karaokeEditor();
  const result = editor.dispatch({
    type: "cue.trim",
    trackId: "C",
    instanceId: id,
    start: 2 * second,
    end: 4 * second,
  });
  assert.equal(result.ok, true, result.error?.message);
  const [cue] = editor.document.project.timeline.tracks[0].captions.cues;
  // "one" was said before the new start; the rest move a second earlier in the cue.
  assert.deepEqual(
    cue.text.map((run) => [run.text, run.time]),
    [
      ["two ", { start: 0, end: second }],
      ["three", { start: second, end: 2 * second }],
    ],
  );
  // A trim that leaves the cue's range is refused.
  const refused = editor.dispatch({
    type: "cue.trim",
    trackId: "C",
    instanceId: id,
    start: 0,
    end: 4 * second,
  });
  assert.equal(refused.ok, false);
});
