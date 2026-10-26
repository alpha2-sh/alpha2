// The CPU renderer must draw the same bytes everywhere: renders fixed frames natively and as wasm
// under Node and compares the hashes. The fixed graph is the renderer's own determinism test (its
// native hash is pinned in crates/alpha2-render/src/tests/cpu_determinism.rs); the parity project
// (every built-in effect and transition) is drawn natively, by the same crate built for wasm32,
// and by the packaged browser binding's CPU renderer (`createCpu`) as a page exports it. Run
// `npm run build` first for the packaged binding.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const root = resolve(import.meta.dirname, "..");
const PINNED_GRAPH_HASH = "39b4fab61033bdd0";
const cargo = (args) =>
  execFileSync("cargo", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  });

const [nativeGraph, nativeProject, times] = cargo([
  "run",
  "--quiet",
  "--release",
  "-p",
  "alpha2-render-determinism",
])
  .trim()
  .split("\n");
assert.equal(
  nativeGraph,
  PINNED_GRAPH_HASH,
  `native fixed graph ${nativeGraph} != pinned ${PINNED_GRAPH_HASH}`,
);

cargo([
  "build",
  "--quiet",
  "--release",
  "-p",
  "alpha2-render-determinism",
  "--target",
  "wasm32-unknown-unknown",
]);
// The renderer reads the browser's clock, so the module runs with its wasm-bindgen glue.
const glue = await mkdtemp(join(tmpdir(), "alpha2-render-determinism-"));
let determinism;
try {
  execFileSync(
    "wasm-bindgen",
    [
      "--target",
      "web",
      "--out-dir",
      glue,
      join(
        cargoTargetDirectory(),
        "wasm32-unknown-unknown",
        "release",
        "alpha2_render_determinism.wasm",
      ),
    ],
    { cwd: root, stdio: ["ignore", "ignore", "inherit"] },
  );
  determinism = await import(
    pathToFileURL(join(glue, "alpha2_render_determinism.js")).href
  );
  await determinism.default({
    module_or_path: await readFile(
      join(glue, "alpha2_render_determinism_bg.wasm"),
    ),
  });
} finally {
  await rm(glue, { recursive: true, force: true });
}
const hex = (value) => BigInt.asUintN(64, value).toString(16).padStart(16, "0");
const wasmGraph = hex(determinism.cpuGraphHash());
assert.equal(wasmGraph, nativeGraph, `wasm graph ${wasmGraph} != native`);
console.log(
  `verified the fixed graph hashes to ${nativeGraph} natively and as wasm`,
);
const wasmProject = hex(determinism.cpuProjectHash());
assert.equal(
  wasmProject,
  nativeProject,
  `wasm project ${wasmProject} != native`,
);
console.log(
  `verified the parity project's frames hash to ${nativeProject} natively and as wasm`,
);

// The packaged binding, as a page without a GPU exports the same project.
const packageRoot = join(root, "packages", "alpha2");
const wasm = await import(
  pathToFileURL(join(packageRoot, "wasm", "alpha2_wasm.js")).href
);
await wasm.default({
  module_or_path: await readFile(
    join(packageRoot, "wasm", "alpha2_wasm_bg.wasm"),
  ),
});
const projectJson = await readFile(
  join(root, "tests", "cases", "host-parity-effects-project.json"),
  "utf8",
);
const { width, height } = JSON.parse(projectJson).settings;
const engine = wasm.WebEffectRegistry.createCpu();
assert.equal(engine.renderer, "cpu");
engine.loadProject(projectJson);
engine.registerLut3d(
  "grade",
  await readFile(join(root, "tests", "corpus", "parity-grade.cube"), "utf8"),
);
const cards = {
  video: testCard(width, height, false),
  still: testCard(width, height, true),
};
engine.uploadRgbaImage("still", width, height, cards.still);
let hash = 0xcbf29ce484222325n;
const started = performance.now();
const frameTimes = times.split(",").map(BigInt);
for (const ticks of frameTimes) {
  for (const need of JSON.parse(engine.frameNeeds(ticks)))
    if (need.sourceId === "video")
      engine.uploadRgbaFrame(
        "video",
        BigInt(need.mediaTimeTicks),
        width,
        height,
        cards.video,
      );
  const ticket = engine.beginExportAt(ticks, "h264");
  assert.equal(
    ticket.kind,
    "rendered",
    `frame ${ticks} waits for ${ticket.missingSources}`,
  );
  const frame = await ticket.readback;
  assert.equal(frame.format, "I420");
  for (const byte of frame.pixels)
    hash = ((hash ^ BigInt(byte)) * 0x100000001b3n) & 0xffffffffffffffffn;
}
const elapsed = performance.now() - started;
engine.free();
const binding = hash.toString(16).padStart(16, "0");
assert.equal(
  binding,
  nativeProject,
  `packaged binding ${binding} != native ${nativeProject}`,
);
console.log(
  `verified the packaged binding's CPU renderer draws the parity project as the native one does (${frameTimes.length} frames, ${(elapsed / frameTimes.length).toFixed(1)} ms per ${width}x${height} frame)`,
);

/** Where cargo writes, which CARGO_TARGET_DIR or a cargo config can move. */
function cargoTargetDirectory() {
  return JSON.parse(cargo(["metadata", "--format-version", "1", "--no-deps"]))
    .target_directory;
}

/**
 * The premultiplied RGBA8 test card of the determinism crate (`test_card`): ramps, a checker,
 * noise, and a translucent corner unless `opaque`.
 */
function testCard(width, height, opaque) {
  const pixels = new Uint8Array(width * height * 4);
  let state = 0x12345678;
  let at = 0;
  for (let y = 0; y < height; y++)
    for (let x = 0; x < width; x++) {
      state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0;
      const noise = state >>> 24;
      const checker =
        (Math.floor(x / 8) + Math.floor(y / 8)) % 2 === 0 ? 230 : 40;
      let pixel = [
        Math.floor((x * 255) / width),
        Math.floor((y * 255) / height),
        x < Math.floor(width / 2) ? checker : noise,
        255,
      ];
      if (
        !opaque &&
        x > Math.floor((width * 3) / 4) &&
        y > Math.floor((height * 3) / 4)
      )
        pixel = [
          ...pixel.slice(0, 3).map((value) => Math.floor((value * 96) / 255)),
          96,
        ];
      pixels.set(pixel, at);
      at += 4;
    }
  return pixels;
}
