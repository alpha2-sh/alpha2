import assert from "node:assert/strict";
import { test } from "node:test";

import { checkBody, generate, registryLimits } from "./stress-projects.mjs";

test("every generated body is within the registry limits read from registry.rs", () => {
  const limits = registryLimits();
  assert.equal(limits.maxBodyBytes, 32 * 1024);
  assert.ok(limits.forbidden.includes("while"), limits.forbidden);
  assert.ok(limits.operators.has("+") && limits.operators.has("["));
  const { outputs, checks } = generate();
  for (const [id, result] of Object.entries(checks.bodies))
    assert.deepEqual(result.errors, [], id);
  // The max body is meant to sit just under the limit, not anywhere below it.
  assert.ok(checks.bodies["stress.max"].bytes > limits.maxBodyBytes * 0.9);
  for (const [name, errors] of Object.entries(checks.projects))
    assert.deepEqual(errors, [], name);
  assert.ok(
    outputs["heavy-1920x1080.json"] && outputs["builtin-worst-1920x1080.json"],
  );
});

test("a body over a registry limit is reported", () => {
  const limits = registryLimits();
  const body = (inner) =>
    `fn effect(uv: vec2f, src: vec4f) -> vec4f {\n${inner}\n}\n`;
  assert.deepEqual(checkBody(body("return src;"), limits).errors, []);
  assert.match(
    checkBody(body("loop { } return src;"), limits).errors.join(),
    /forbidden token loop/,
  );
  assert.match(
    checkBody(body(`let alpha2_x = 1.0; return src;`), limits).errors.join(),
    /reserved identifier alpha2_x/,
  );
  assert.match(
    checkBody(
      body(`let x = ${"1.0+".repeat(200)}1.0; return src;`),
      limits,
    ).errors.join(),
    /operators/,
  );
  assert.match(
    checkBody(
      `// ${"x".repeat(limits.maxBodyBytes)}\n${body("return src;")}`,
      limits,
    ).errors.join(),
    /bytes/,
  );
});
