// The published project schema against the shared validation corpus, the repository's project
// fixtures, and `validateProject`'s shape layer.

import test from "node:test";
import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import Ajv2020 from "ajv/dist/2020.js";

import { validateProject, validateProjectJson } from "../dist/project.js";
import { readValidationCorpus } from "../../../scripts/lib/validation-corpus.mjs";

const repo = new URL("../../../", import.meta.url);
const readJson = async (path) =>
  JSON.parse(await readFile(new URL(path, repo), "utf8"));

const schema = await readJson("schema/project.v1.schema.json");
// Strict mode rejects any keyword Ajv does not know. The `format` values schemars emits (`uint32`,
// `float`, ...) describe the Rust type and are not checked; the range keywords next to them are.
const ajv = new Ajv2020({
  strict: true,
  validateFormats: false,
  allErrors: true,
});
const validate = ajv.compile(schema);
const corpus = await readValidationCorpus();
// A `json` case is the text of a document: its parsed value is all a schema can see.
const documentOf = (fixture) =>
  fixture.json === undefined ? fixture.project : JSON.parse(fixture.json);
const textOf = (fixture) => fixture.json ?? JSON.stringify(fixture.project);

test("the schema accepts every valid corpus document", () => {
  for (const fixture of corpus.filter((fixture) => fixture.valid)) {
    assert.ok(
      validate(documentOf(fixture)),
      `${fixture.name}: ${ajv.errorsText(validate.errors)}`,
    );
  }
});

test("the schema rejects exactly the corpus cases marked as shape errors", () => {
  for (const fixture of corpus.filter((fixture) => !fixture.valid)) {
    assert.ok(
      ["schema", "parser", "model"].includes(fixture.rejected_by),
      `${fixture.name}: rejected_by must be schema, parser, or model`,
    );
    assert.equal(
      !validate(documentOf(fixture)),
      fixture.rejected_by === "schema",
      `${fixture.name} is rejected by the ${fixture.rejected_by}`,
    );
  }
});

test("validateProject reports shape problems alone, as the engine's parser does", () => {
  for (const fixture of corpus.filter((fixture) =>
    ["schema", "parser"].includes(fixture.rejected_by),
  )) {
    const problems = validateProjectJson(textOf(fixture));
    assert.ok(problems.length > 0, fixture.name);
    assert.ok(
      problems.every((problem) => problem.startsWith("project")),
      `${fixture.name}: ${problems.join("; ")}`,
    );
  }
});

test("shape problems name the offending field", () => {
  const byName = Object.fromEntries(
    corpus.map((fixture) => [fixture.name, fixture.project]),
  );
  assert.deepEqual(validateProject(byName["unknown-clip-field"]), [
    'project.timeline.tracks[0].clips[0] has unknown field "blend_mod"',
  ]);
  assert.deepEqual(validateProject(byName["effect-value-mistyped"]), [
    "project.compositions[0].nodes[1].op.effect.params.radius.value.value must be number",
  ]);
  assert.deepEqual(validateProject(byName["keyframe-missing-interpolation"]), [
    "project.timeline.tracks[0].clips[0].visual.rotation_degrees.keyframes[0].interpolation is required",
  ]);
  assert.deepEqual(validateProject(byName["schema-2"]), [
    "project.schema_version must be 1",
  ]);
  assert.match(
    validateProject(byName["unknown-blend-mode"])[0],
    /^project\.timeline\.tracks\[0\]\.clips\[0\]\.blend_mode must be one of "normal", /,
  );
});

test("problems only the text shows name the offending field", () => {
  const byName = Object.fromEntries(
    corpus.map((fixture) => [fixture.name, fixture]),
  );
  const problems = (name) => validateProjectJson(textOf(byName[name]));
  assert.deepEqual(problems("fps-over-resolution"), [
    "project.settings.frame_rate must be at most 120000 frames per second, one frame per tick",
  ]);
  assert.deepEqual(problems("fingerprint-fps-over-resolution"), [
    "project.sources[0].fingerprint.streams.video.frame_rate must be at most 120000 frames per second, one frame per tick",
  ]);
  assert.deepEqual(problems("fps-num-over-u32"), [
    "project.settings.frame_rate.num must be at most 4294967295",
  ]);
  assert.deepEqual(problems("text-integer-with-fraction"), [
    "project.settings.width must be an integer written without a fraction or exponent",
  ]);
  assert.deepEqual(problems("text-integer-negative-zero"), [
    "project.settings.height must be an integer written without a fraction or exponent",
  ]);
  assert.deepEqual(problems("text-duplicate-field"), [
    'project has duplicate field "name"',
  ]);
  assert.deepEqual(problems("text-duplicate-tag"), [
    'project.compositions[0].nodes[1].op.effect.params.radius has duplicate field "mode"',
  ]);
  assert.deepEqual(problems("text-lone-surrogate"), [
    "project.name must be well-formed Unicode",
  ]);
  assert.deepEqual(problems("text-map-key-lone-surrogate"), [
    "project.compositions[0].nodes[1].op.effect.params has a key that is not well-formed Unicode",
  ]);
  // The engine's parser stops at 127 levels of nesting.
  assert.deepEqual(validateProjectJson("[".repeat(128) + "]".repeat(128)), [
    "project JSON nests deeper than 127 levels",
  ]);
});

test("every project fixture in the repository matches the schema", async () => {
  const names = (await readdir(new URL("tests/cases/", repo))).filter((name) =>
    name.endsWith("-project.json"),
  );
  assert.ok(names.length > 0);
  for (const name of names) {
    const document = await readJson(`tests/cases/${name}`);
    assert.ok(
      validate(document),
      `${name}: ${ajv.errorsText(validate.errors)}`,
    );
    assert.deepEqual(validateProject(document), [], name);
  }
});
