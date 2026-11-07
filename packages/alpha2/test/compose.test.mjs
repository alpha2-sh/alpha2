import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";

import {
  bindTemplate,
  builtinTemplates,
  compileCompose,
  composeRequirements,
  lintTemplate,
  templateDataSchema,
} from "../dist/compose.js";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const wasmDirectory = join(resolve(import.meta.dirname, ".."), "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_compose_bg.wasm"));
// Skipped before `npm run build`; ALPHA2_REQUIRE=wasm makes that a failure.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the wasm package is not built",
  );

async function binding() {
  const glue = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_compose.js")).href
  );
  glue.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_compose_bg.wasm")),
  });
  return glue;
}

const template = {
  template: { id: "acme/card", version: 1, kind: "video", title: "Card" },
  params: [
    { name: "title", type: "text", max_chars: 12 },
    { name: "photo", type: "media", accept: ["image"] },
  ],
  canvas: "square",
  duration: 2,
  body: {
    type: "box",
    flex_direction: "column",
    padding: "40px",
    background: { $theme: "surface" },
    children: [
      { type: "shape", shapes: [{ shape: "rect", corner_radius: 0.1 }] },
      { type: "text", text: "{{title}}", style: "title" },
    ],
  },
  overlays: [
    {
      type: "alpha2/circle-reveal",
      with: { content: { type: "media", path: { $param: "photo" } } },
    },
  ],
};

test(
  "a template binds, lints and compiles in the browser module",
  { skip },
  async () => {
    const wasm = await binding();
    const spec = bindTemplate(wasm, template, {
      title: "Hello",
      photo: "a.png",
    });
    assert.equal(spec.body.children[1].text, "Hello");
    assert.deepEqual(
      lintTemplate(wasm, template, { title: "Hello", photo: "a.png" }),
      [],
    );
    const schema = templateDataSchema(wasm, template);
    assert.equal(schema.properties.title.maxLength, 12);
    assert.deepEqual(composeRequirements(wasm, spec).media, [
      { path: "a.png", kind: "image" },
    ]);
    const compiled = compileCompose(wasm, spec, {
      facts: { media: { "a.png": { width: 400, height: 300 } } },
    });
    assert.equal(compiled.project.schema_version, 1);
    assert.equal(compiled.assets[Object.keys(compiled.assets)[0]], "a.png");
    assert.ok(builtinTemplates(wasm).length >= 2);
  },
);

test("compose errors name where the spec or data fails", { skip }, async () => {
  const wasm = await binding();
  assert.throws(
    () =>
      bindTemplate(wasm, template, { title: "x".repeat(13), photo: "a.png" }),
    (error) => error.path === "data.title" && /at most 12/.test(error.problem),
  );
  assert.throws(
    () => compileCompose(wasm, { body: { type: "nope" } }),
    (error) => error.path === "spec.body.type",
  );
});
