// Subtitle import through the engine's parser: the rows it returns are caption tracks a project
// accepts.
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import { captionTrack, parseCaptions } from "../dist/captions.js";
import {
  MAX_CLIPS_PER_TRACK,
  MAX_TRACKS,
  validateProject,
} from "../dist/project.js";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// Skipped before `npm run build` packages the wasm; ALPHA2_REQUIRE_WASM=1 (set by CI after the
// build) makes a missing package fail them.
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

async function binding() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const timestamp = (ms) => {
  const pad = (value, width) => String(value).padStart(width, "0");
  return `${pad(Math.floor(ms / 3_600_000), 2)}:${pad(Math.floor(ms / 60_000) % 60, 2)}:${pad(Math.floor(ms / 1000) % 60, 2)},${pad(ms % 1000, 3)}`;
};
/** An SRT file of `count` cues, `spacing` ms apart and 2 s long. */
const srt = (count, spacing) =>
  Array.from(
    { length: count },
    (_, i) =>
      `${i + 1}\n${timestamp(i * spacing)} --> ${timestamp(i * spacing + 2000)}\nCue ${i + 1}\n`,
  ).join("\n");

const project = (tracks) => ({
  schema_version: 1,
  id: "captions",
  name: "Captions",
  settings: {
    width: 1920,
    height: 1080,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48000,
  },
  sources: [],
  timeline: { tracks },
});

test(
  "a long subtitle file splits into caption tracks the engine accepts",
  { skip },
  async () => {
    const wasm = await binding();
    // A three-hour talk: 4500 cues one after another.
    const imported = parseCaptions(wasm, srt(4500, 2400), "srt");
    assert.deepEqual(
      imported.rows.map((row) => row.length),
      [MAX_CLIPS_PER_TRACK, 4500 - MAX_CLIPS_PER_TRACK],
    );
    const document = project(
      imported.rows.map((cues, i) =>
        captionTrack(`captions-${i}`, "Talk", cues),
      ),
    );
    assert.deepEqual(validateProject(document), []);
    assert.deepEqual(
      JSON.parse(wasm.lintProject(JSON.stringify(document))),
      [],
    );
  },
);

test(
  "cues that need more caption tracks than a project holds are refused, saying so",
  { skip },
  async () => {
    const wasm = await binding();
    const overlapping = srt(MAX_TRACKS + 1, 0);
    assert.throws(
      () => parseCaptions(wasm, overlapping, "srt"),
      new RegExp(
        `the captions need ${MAX_TRACKS + 1} caption tracks; a project holds at most ${MAX_TRACKS} tracks`,
      ),
    );
  },
);
