import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import Ajv2020 from "ajv/dist/2020.js";

const repo = new URL("../../../", import.meta.url);
const readJson = async (path) =>
  JSON.parse(await readFile(new URL(path, repo), "utf8"));

const schema = await readJson("schema/plugin-manifest.v1.json");
const corpus = await readJson("tests/cases/plugin-manifest-corpus.json");
// schemars `format` values (`uint32`, `uint64`) describe the Rust type and are not checked.
const ajv = new Ajv2020({ strict: true, validateFormats: false });
const validate = ajv.compile(schema);

test("the manifest schema accepts every valid corpus manifest", () => {
  for (const { name, manifest } of corpus.filter((entry) => entry.valid))
    assert.ok(
      validate(manifest),
      `${name}: ${ajv.errorsText(validate.errors)}`,
    );
});

test("the manifest schema rejects missing hashes, unknown ABIs and the other shape errors", () => {
  const rejected = corpus.filter((entry) => entry.rejected_by_schema);
  assert.ok(rejected.some((entry) => entry.name === "missing-artifact-hash"));
  assert.ok(rejected.some((entry) => entry.name === "unknown-abi"));
  for (const { name, manifest } of rejected)
    assert.equal(validate(manifest), false, name);
});
