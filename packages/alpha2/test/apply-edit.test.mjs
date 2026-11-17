import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import { applyEdit } from "../dist/apply-edit.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
// Skipped before `npm run build`; ALPHA2_REQUIRE=wasm makes that a failure.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

const corpus = JSON.parse(
  readFileSync(
    new URL("../../../tests/cases/surface-conformance.json", import.meta.url),
    "utf8",
  ),
);

async function binding() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
  });
  return wasm;
}

test(
  "edit operations in wasm answer the corpus as Rust does",
  { skip },
  async () => {
    const wasm = await binding();
    for (const fixture of corpus.edit) {
      const project = corpus.projects[fixture.project];
      const before = structuredClone(project);
      const { expected } = fixture;
      if (expected.refusal !== undefined) {
        assert.throws(
          () => applyEdit(wasm, project, fixture.request),
          (error) =>
            error.code === expected.code &&
            error.details?.refusal === expected.refusal,
          fixture.name,
        );
        continue;
      }
      const edited = applyEdit(wasm, project, fixture.request);
      assert.equal(
        createHash("sha256").update(edited.projectJson).digest("hex"),
        expected.project_sha256,
        fixture.name,
      );
      assert.deepEqual(edited.report, expected.report, fixture.name);
      assert.deepEqual(edited.project, JSON.parse(edited.projectJson));
      assert.deepEqual(project, before, `${fixture.name} changed its input`);
    }
  },
);

function pointsSet(id, channels, samples) {
  const names = Array.from({ length: channels }, (_, index) => `"k${index}"`);
  const times = Array.from({ length: samples }, (_, index) => index * 4004);
  const values = new Array(samples * channels * 2);
  for (let index = 0; index < values.length; index += 1) {
    values[index] =
      `0.${String((index * 7919) % 100_000_000).padStart(8, "0")}`;
  }
  return `{"id":"${id}","anchor":{"type":"timeline"},"kind":"points","interpolation":"hold","channels":[${names.join(",")}],"times":[${times.join(",")}],"values":[${values.join(",")}]}`;
}

test(
  "an edit that grows a project past the document cap is refused",
  { skip },
  async () => {
    const wasm = await binding();
    const cap = 64 * 1024 * 1024;
    const follow = '{"mode":"data","data":"track","channel":"k0"}';
    const end = 20_000 * 4004;
    const head = `{"schema_version":1,"id":"p","name":"p","settings":{"width":1920,"height":1080,"frame_rate":{"num":30,"den":1},"sample_rate":48000,"background":{"red":0.0,"green":0.0,"blue":0.0}},"sources":[{"id":"v","locator":"asset://v","kind":"video"}],"data":[`;
    const tail = `],"timeline":{"tracks":[{"id":"t","name":"V","kind":"video","clips":[{"id":"c0","source_id":"v","timeline_range":{"start":0,"end":${end}},"source_range":{"start":0,"end":${end}},"visual":{"position":${follow},"scale":${follow}}}]}]}}`;
    const parts = [pointsSet("track", 1, 20_000)];
    let length = head.length + parts[0].length + tail.length;
    // Bake adds about 3.6 MB of keyframes; the rest fills the document to 1 MB under the cap.
    while (cap - length > 1_000_000) {
      const samples = Math.min(
        Math.floor((cap - length - 1_000_000) / (128 * 11)),
        31_250,
      );
      if (samples === 0) break;
      const set = pointsSet(`fill${parts.length}`, 64, samples);
      parts.push(set);
      length += set.length + 1;
    }
    const project = `${head}${parts.join(",")}${tail}`;
    assert.ok(project.length <= cap, `${project.length} bytes`);
    assert.throws(
      () =>
        wasm.applyEdit(
          project,
          JSON.stringify({
            op: "bake",
            targets: [
              { clip: "c0", property: "visual.position" },
              { clip: "c0", property: "visual.scale" },
            ],
          }),
        ),
      (error) =>
        error.code === "INVALID_INPUT" &&
        error.details?.refusal === "result_too_large",
    );
  },
);
