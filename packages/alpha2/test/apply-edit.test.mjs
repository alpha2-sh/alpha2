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
