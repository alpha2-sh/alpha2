import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import { recordSkip, suiteOf } from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");

// Null before `npm run build`; a skip that ALPHA2_REQUIRE=wasm turns into a failure.
async function binding() {
  if (!existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"))) return null;
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

const SECOND = 120_000;
const white = {
  type: "fill",
  paint: {
    type: "solid",
    color: { red: 1, green: 1, blue: 1, alpha: 1 },
  },
};

// The second clip shows a composition with no output, from 9 s on.
const project = {
  schema_version: 1,
  id: "late",
  name: "Late",
  settings: {
    width: 64,
    height: 48,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48000,
  },
  sources: [{ id: "white", kind: "generated", generator: white }],
  compositions: [{ id: "g", nodes: [{ id: "clip", op: { type: "clip" } }] }],
  timeline: {
    tracks: [
      {
        id: "v",
        name: "V",
        kind: "video",
        clips: [
          {
            id: "early",
            source_id: "white",
            timeline_range: { start: 0, end: 9 * SECOND },
            source_range: { start: 0, end: 9 * SECOND },
          },
          {
            id: "late",
            source_id: "white",
            composition: { id: "g" },
            timeline_range: { start: 9 * SECOND, end: 10 * SECOND },
            source_range: { start: 0, end: SECOND },
          },
        ],
      },
    ],
  },
};

test("an export whose late composition cannot draw is refused before its first frame", async (t) => {
  const wasm = await binding();
  if (wasm === null) {
    t.skip(
      recordSkip({
        suite: suiteOf(import.meta.url),
        test: t.name,
        capability: "wasm",
        reason: "the packaged wasm is not built",
      }),
    );
    return;
  }
  const engine = wasm.WebEffectRegistry.createCpu(undefined);
  engine.loadProject(JSON.stringify(project));
  assert.throws(
    () => engine.beginExport(0n, BigInt(10 * SECOND), 30, 1, "h264"),
    (error) => error.code === "INCOMPLETE_COMPOSITION",
  );
  assert.throws(() => engine.beginExportFrame(), /no export is running/);
  engine.beginExport(0n, BigInt(9 * SECOND), 30, 1, "h264");
  assert.equal(JSON.parse(engine.exportProgress()).total, 270);
  engine.free();
});
