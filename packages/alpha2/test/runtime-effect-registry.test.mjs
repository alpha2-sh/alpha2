// RuntimeEffectRegistry keeps JSON inside it and holds nothing of the binding while the browser
// compiles a shader; driven here against a recording binding.
import assert from "node:assert/strict";
import test from "node:test";

import { RuntimeEffectRegistry } from "../dist/effects.js";

/** A binding whose compiles resolve when the test says, recording every call. */
function recordingBinding() {
  const calls = [];
  const compiles = [];
  const registry = {
    compileEffect(json) {
      calls.push(["compileEffect", JSON.parse(json).id]);
      return new Promise((resolve, reject) =>
        compiles.push({ json, resolve, reject }),
      );
    },
    registerCompiledEffect(compiled) {
      calls.push(["registerCompiledEffect", compiled.id]);
      return `${compiled.id}@${compiled.version}`;
    },
    unregisterEffect: (id, version) => {
      calls.push(["unregisterEffect", id, version]);
      return id === "known";
    },
    clearEffects: () => calls.push(["clearEffects"]),
    builtinCatalog: () => JSON.stringify({ effects: [], transitions: [] }),
    registerLut3d: (id, cube) => calls.push(["registerLut3d", id, cube]),
    unregisterEffectResource: (id) => {
      calls.push(["unregisterEffectResource", id]);
      return false;
    },
    clearEffectResources: () => calls.push(["clearEffectResources"]),
  };
  return { binding: { create: async () => registry }, calls, compiles };
}

const definition = (id) => ({ id, version: 1, kind: "filter", wgsl: "" });

test("registrations overlap: each compiles, then registers, without waiting on another", async () => {
  const { binding, calls, compiles } = recordingBinding();
  const registry = await RuntimeEffectRegistry.create(binding);
  const first = registry.register(definition("first"));
  const second = registry.register(definition("second"));
  await new Promise(setImmediate);
  // Both compile at once; neither has registered.
  assert.deepEqual(calls, [
    ["compileEffect", "first"],
    ["compileEffect", "second"],
  ]);
  compiles[1].resolve({ id: "second", version: 1 });
  assert.equal(await second, "second@1");
  compiles[0].resolve({ id: "first", version: 1 });
  assert.equal(await first, "first@1");
  assert.deepEqual(calls.slice(2), [
    ["registerCompiledEffect", "second"],
    ["registerCompiledEffect", "first"],
  ]);
});

test("a definition that does not compile rejects with the binding's error and registers nothing", async () => {
  const { binding, calls, compiles } = recordingBinding();
  const registry = await RuntimeEffectRegistry.create(binding);
  const failing = registry.register(definition("broken"));
  await new Promise(setImmediate);
  const error = Object.assign(new Error("shader error"), {
    code: "EFFECT_ERROR",
  });
  compiles[0].reject(error);
  await assert.rejects(failing, (thrown) => thrown === error);
  assert.deepEqual(calls, [["compileEffect", "broken"]]);
});

test("the rest of the registry passes straight through to the binding", async () => {
  const { binding, calls } = recordingBinding();
  const registry = await RuntimeEffectRegistry.create(binding);
  assert.equal(registry.unregister("known", 2), true);
  assert.equal(registry.unregister("other", 1), false);
  registry.clearHostEffects();
  assert.deepEqual(registry.builtinCatalog(), { effects: [], transitions: [] });
  registry.registerResource({
    id: "lut",
    kind: "lut3d",
    data: "LUT_3D_SIZE 2",
  });
  assert.equal(registry.unregisterResource("lut"), false);
  registry.clearResources();
  assert.deepEqual(calls, [
    ["unregisterEffect", "known", 2],
    ["unregisterEffect", "other", 1],
    ["clearEffects"],
    ["registerLut3d", "lut", "LUT_3D_SIZE 2"],
    ["unregisterEffectResource", "lut"],
    ["clearEffectResources"],
  ]);
});

test("a binding that cannot be created rejects create", async () => {
  const failure = new Error("no adapter");
  await assert.rejects(
    RuntimeEffectRegistry.create({
      create: async () => {
        throw failure;
      },
    }),
    (thrown) => thrown === failure,
  );
});
