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

test("parsePluginManifest reads every corpus manifest as the engine's loaders do", async () => {
  const { parsePluginManifest } = await import("../dist/plugins.js");
  for (const { name, valid, error, manifest } of corpus) {
    const text = JSON.stringify(manifest);
    if (valid) {
      assert.deepEqual(parsePluginManifest(text), manifest, name);
      continue;
    }
    assert.throws(
      () => parsePluginManifest(text),
      {
        name: "PluginError",
        code: error === "abi" ? "PLUGIN_ABI_UNSUPPORTED" : "PLUGIN_LOAD_FAILED",
      },
      name,
    );
  }
});

test("parsePluginManifest refuses text past the manifest limit and text that is not JSON", async () => {
  const { MAX_MANIFEST_BYTES, parsePluginManifest } =
    await import("../dist/plugins.js");
  const complete = corpus.find((entry) => entry.name === "complete").manifest;
  const padded = JSON.stringify(complete).replace(
    "{",
    `{"x":"${"é".repeat(MAX_MANIFEST_BYTES / 2)}",`,
  );
  assert.throws(() => parsePluginManifest(padded), {
    code: "PLUGIN_LOAD_FAILED",
    message: /longer than 1048576 bytes/,
  });
  assert.throws(() => parsePluginManifest("{"), {
    code: "PLUGIN_LOAD_FAILED",
  });
});

test("pluginProblem says why a manifest's plugin does not run with given capabilities", async () => {
  const { parsePluginManifest, pluginProblem } =
    await import("../dist/plugins.js");
  const read = (name) =>
    parsePluginManifest(
      JSON.stringify(corpus.find((entry) => entry.name === name).manifest),
    );
  const browser = {
    inputs: { video: ["source", "timeline"], audio: ["source"] },
    artifacts: ["wasm"],
    slots: false,
  };
  const complete = read("complete");
  assert.equal(pluginProblem(complete, browser), undefined);
  const nativeOnly = {
    ...complete,
    artifacts: complete.artifacts.filter((entry) => entry.type !== "wasm"),
  };
  assert.equal(pluginProblem(nativeOnly, browser)?.reason, "artifact");
  assert.equal(
    pluginProblem(nativeOnly, { ...browser, artifacts: ["wasm", "native"] }),
    undefined,
  );
  const slots = read("supplied-slots");
  assert.equal(pluginProblem(slots, browser)?.reason, "slots");
  assert.equal(pluginProblem(slots, { ...browser, slots: true }), undefined);
  const timelineAudio = {
    ...complete,
    descriptor: {
      ...complete.descriptor,
      input: { ...complete.descriptor.input, from: ["timeline"] },
    },
  };
  assert.equal(pluginProblem(timelineAudio, browser)?.reason, "input");
});
