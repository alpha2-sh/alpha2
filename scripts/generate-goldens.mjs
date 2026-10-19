// Rewrites every generated file under tests/golden/ from the engine as it is now (PNG goldens,
// time-rules and frame-selection tables, text rasters, the browser audio fixture) by running the
// tests with ALPHA2_UPDATE_GOLDENS=1. Review the diff: a GPU that rounds a tie the other way
// rewrites pixels by one code, so keep only the files an intended change moved.
//
//   npm run generate:goldens
//
// The GPU goldens are drawn on the default test device. A backend variant (`glow_gl_rgba8.png`)
// is rewritten only on that backend, with ALPHA2_GOLDEN_VARIANT=<name> and WGPU_BACKEND=gl.
// alpha2-host links FFmpeg: point LD_LIBRARY_PATH (PATH on Windows) at the FFmpeg the build uses.

import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const update = (args) =>
  execFileSync("cargo", args, {
    cwd: root,
    stdio: "inherit",
    env: { ...process.env, ALPHA2_UPDATE_GOLDENS: "1" },
  });

const steps = [
  [
    "PNG goldens and the time-rules table",
    () =>
      update([
        "test",
        "-p",
        "alpha2-render",
        "-p",
        "alpha2-engine",
        "--",
        "--test-threads=1",
      ]),
  ],
  [
    "text rasters",
    () =>
      update([
        "test",
        "-p",
        "alpha2-text",
        "--lib",
        "fixture_requests_match_the_pinned_rasters",
      ]),
  ],
  [
    "frame-selection table",
    () =>
      update([
        "test",
        "-p",
        "alpha2-host",
        "--bin",
        "alpha2-host",
        "the_frame_selection_table_is_the_native_exports",
      ]),
  ],
  [
    "browser audio fixture",
    () =>
      writeFileSync(
        join(root, "tests", "golden", "audio_mix.json"),
        execFileSync(
          "cargo",
          ["run", "--quiet", "-p", "alpha2-audio-determinism", "--", "fixture"],
          { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] },
        ),
      ),
  ],
];

for (const [name, step] of steps) {
  console.log(`generate:goldens: ${name}`);
  step();
}
