// Edit operations against a real alpha2-host; `npm run test:node-e2e` builds it first.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import test from "node:test";

import { Alpha2NodeClient } from "../../dist/index.js";
import { executable } from "../../../../scripts/lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..", "..");
const hostPath =
  process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "debug");
const client = new Alpha2NodeClient({ hostPath });
test.after(() => client.close());

const corpus = JSON.parse(
  await readFile(resolve(root, "tests/cases/data-edit-corpus.json"), "utf8"),
);

test("the host's edit operation answers the corpus as Rust does", async () => {
  for (const fixture of corpus.cases) {
    const project = corpus.projects[fixture.project];
    const { expected } = fixture;
    if (expected.refusal !== undefined) {
      await assert.rejects(
        client.edit(project, fixture.request),
        (error) =>
          error.code === expected.code && error.refusal === expected.refusal,
        fixture.name,
      );
      continue;
    }
    const edited = await client.edit(project, fixture.request);
    assert.equal(
      createHash("sha256").update(edited.projectJson).digest("hex"),
      expected.project_sha256,
      fixture.name,
    );
    assert.deepEqual(edited.report, expected.report, fixture.name);
    assert.deepEqual(edited.project, JSON.parse(edited.projectJson));
  }
});
