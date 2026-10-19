import assert from "node:assert/strict";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import {
  engineVideoCodec,
  qualityLevels,
  resolveAudioBitrate,
  resolveVideoRateControl,
} from "../dist/rate-control.js";

const packageRoot = resolve(import.meta.dirname, "..");
const wasmDirectory = join(packageRoot, "wasm");

// The packaged binding, or null before `npm run build` made it. ALPHA2_REQUIRE_WASM=1 loads it
// regardless, so a missing package fails instead of skipping.
async function binding() {
  if (
    process.env.ALPHA2_REQUIRE_WASM === "1" ||
    existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"))
  ) {
    const wasm = await import(
      pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
    );
    wasm.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
    });
    return wasm;
  }
  return null;
}

test("the browser's avc is the engine's h264, and nothing else is renamed", () => {
  assert.equal(engineVideoCodec("avc"), "h264");
  for (const codec of ["hevc", "vp9", "av1"])
    assert.equal(engineVideoCodec(codec), codec);
});

test("an explicit rate is passed through without asking the engine", () => {
  const refuse = {
    resolveRateControl() {
      throw new Error("an explicit rate must not be resolved");
    },
    resolveAudioBitrate() {
      throw new Error("an explicit rate must not be resolved");
    },
    qualityLevels: () => "[]",
  };
  assert.deepEqual(
    resolveVideoRateControl(refuse, {
      codec: "h264",
      width: 1920,
      height: 1080,
      frameRate: { num: 30, den: 1 },
      quality: { bitrate: 8_000_000, mode: "constant" },
    }),
    { bitrate: 8_000_000, bitrateMode: "constant" },
  );
  assert.equal(resolveAudioBitrate(refuse, "aac", { bitrate: 96_000 }), 96_000);
  assert.throws(
    () =>
      resolveVideoRateControl(refuse, {
        codec: "h264",
        width: 1920,
        height: 1080,
        frameRate: { num: 30, den: 1 },
        quality: { bitrate: 0 },
      }),
    /videoQuality\.bitrate/,
  );
});

test("a level resolves through the engine to the pinned table", async (t) => {
  const engine = await binding();
  if (engine === null) {
    t.skip("the packaged wasm is not built");
    return;
  }
  assert.deepEqual(qualityLevels(engine), [
    "very-low",
    "low",
    "medium",
    "high",
    "very-high",
  ]);
  const table = JSON.parse(
    readFileSync(
      resolve(packageRoot, "../../tests/cases/video-rate-table.json"),
      "utf8",
    ),
  );
  for (const rateCase of table.cases) {
    const resolved = resolveVideoRateControl(engine, {
      codec: rateCase.codec,
      width: rateCase.width,
      height: rateCase.height,
      frameRate: rateCase.frameRate,
      quality: rateCase.level,
    });
    assert.deepEqual(resolved, {
      bitrate: rateCase.expected.bitrate,
      bitrateMode: rateCase.expected.mode,
      ...(rateCase.expected.quantizer === null
        ? {}
        : { quantizer: rateCase.expected.quantizer }),
    });
  }
  assert.equal(resolveAudioBitrate(engine, "aac", "high"), 192_000);
  assert.throws(() => resolveAudioBitrate(engine, "aac", "ultra"), /ultra/);
});

test("nothing in the package names a quality level or hands mediabunny one", () => {
  // The engine owns the level names and their cost; a level string here would be a second vocabulary.
  const source = join(packageRoot, "src");
  for (const file of readdirSync(source).filter((name) =>
    name.endsWith(".ts"),
  )) {
    const text = readFileSync(join(source, file), "utf8");
    assert.doesNotMatch(
      text,
      /["'](very-low|low|medium|high|very-high)["']/,
      `${file} names a quality level`,
    );
    assert.doesNotMatch(
      text,
      /new Quality\(\s*["'A-Za-z_]/,
      `${file} constructs a mediabunny Quality from a level`,
    );
  }
});

test("the mediabunny sink refuses a codec without resolved numbers", async () => {
  const { MediabunnyMp4Sink } = await import("../dist/mediabunny.js");
  const memory = { kind: "memory", maxBytes: 1 << 20 };
  assert.throws(
    () => new MediabunnyMp4Sink({ videoCodec: "avc", destination: memory }),
    /videoRateControl is required/,
  );
  assert.throws(
    () => new MediabunnyMp4Sink({ audioCodec: "aac", destination: memory }),
    /audioBitrate is required/,
  );
});
