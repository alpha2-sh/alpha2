// The editor's renderer module (runtime/engine.js) over a recording fake of the wasm binding: the
// preview's result cache follows the canvas, and a project's LUTs cost only what is drawn with
// them.
import test from "node:test";
import assert from "node:assert/strict";
import { register } from "node:module";

register("./support/registry-hook.mjs", import.meta.url);
globalThis.VideoFrame ??= class VideoFrame {};
globalThis.OffscreenCanvas ??= class OffscreenCanvas {
  constructor(width, height) {
    Object.assign(this, { width, height });
  }
};

const { Engine } = await import("../dist/browser/runtime/engine.js");

/** An engine over the fake binding on a `width`x`height` canvas, started. */
async function started(width, height, library = new Map()) {
  const engine = new Engine(
    new globalThis.OffscreenCanvas(width, height),
    { release: async () => {} },
    { get: (id) => library.get(id) ?? null },
  );
  await engine.start();
  return { engine, registry: globalThis.fakeRegistry.registries.at(-1) };
}

test("the preview's result cache is sized again when the canvas is", async () => {
  const { engine, registry } = await started(1920, 1080);
  const sixFrames = (width, height) => 6 * width * height * 8;
  assert.deepEqual(registry.budgets, [sixFrames(1920, 1080)]);
  // A 4K project opened in the same session, then a 720p one.
  engine.resize(3840, 2160);
  assert.equal(registry.budgets.at(-1), sixFrames(3840, 2160));
  engine.resize(1280, 720);
  assert.equal(registry.budgets.at(-1), sixFrames(1280, 720));
  const count = registry.budgets.length;
  engine.resize(1280, 720);
  assert.equal(registry.budgets.length, count);
});

/**
 * A project with LUT resources `resources`, of which a track transition draws `drawn`: what the
 * renderer module needs to tell drawn from unused.
 */
const withLuts = (resources, drawn) => ({
  settings: { width: 64, height: 36 },
  sources: [],
  effect_resources: resources.map((id) => ({
    id,
    kind: "lut3d",
    locator: `${id}.cube`,
  })),
  timeline: {
    tracks: [
      {
        id: "t",
        clips: [],
        transitions: drawn.map((id) => ({
          effect: { id: "alpha2.effect.lut-3d", resources: { lut: id } },
        })),
      },
    ],
  },
});
const lut = (id, text = `LUT ${id}`) => [
  id,
  { id, kind: "lut3d", name: `${id}.cube`, text },
];

test("a LUT nothing drawn uses is neither required nor registered, and one drawn but missing names itself", async () => {
  const { engine, registry } = await started(64, 36, new Map([lut("used")]));
  // The kept copy of "spare" is gone from this browser; no effect uses it.
  engine.loadProject(withLuts(["used", "spare"], ["used"]));
  assert.deepEqual(
    registry.calls.filter(([call]) => call === "registerLut3d"),
    [["registerLut3d", "used"]],
  );
  assert.throws(
    () => engine.loadProject(withLuts(["used", "spare"], ["used", "spare"])),
    (error) =>
      error.name === "EditorError" &&
      error.code === "missing_asset" &&
      error.entityId === "spare",
  );
});

test("a LUT the engine cannot parse fails the load naming it, and what was registered stays known", async () => {
  globalThis.fakeRegistry.refuseLut = (_id, text) =>
    text.includes("1D")
      ? "one-dimensional .cube tables are not supported"
      : undefined;
  try {
    const library = new Map([
      lut("a"),
      lut("curve", "LUT_1D_SIZE 1D"),
      lut("b"),
    ]);
    const { engine, registry } = await started(64, 36, library);
    const registered = () =>
      registry.calls
        .filter(([call]) => call === "registerLut3d")
        .map(([, id]) => id);
    assert.throws(
      () =>
        engine.loadProject(withLuts(["a", "curve", "b"], ["a", "curve", "b"])),
      (error) =>
        error.code === "invalid_effect_resource" &&
        error.entityId === "curve" &&
        /curve\.cube is not a 3D LUT the engine reads: one-dimensional/.test(
          error.message,
        ),
    );
    assert.deepEqual(registered(), ["a"]);
    // The broken LUT removed: "a" is not parsed again, and "b" is registered.
    engine.loadProject(withLuts(["a", "b"], ["a", "b"]));
    assert.deepEqual(registered(), ["a", "b"]);
    engine.loadProject(withLuts(["a", "b"], ["b"]));
    assert.ok(
      registry.calls.some(
        ([call, id]) => call === "unregisterEffectResource" && id === "a",
      ),
    );
  } finally {
    globalThis.fakeRegistry.refuseLut = () => undefined;
  }
});

const effect = (id, version = 1) => ({ id, version, kind: "effect" });

test("runtime effects register all or none, and one held already is not compiled again", async () => {
  globalThis.fakeRegistry.refuseEffect = (definition) =>
    definition.id === "broken" ? "shader error: unknown identifier" : undefined;
  try {
    const { engine, registry } = await started(64, 36);
    await assert.rejects(
      engine.registerEffects([
        effect("blur"),
        effect("broken"),
        effect("glow"),
      ]),
      (error) =>
        error.name === "EditorError" &&
        error.code === "invalid_effect" &&
        error.entityId === "broken@1" &&
        /broken@1 is not one the engine runs: shader error/.test(error.message),
    );
    assert.deepEqual([...registry.effects], []);
    assert.deepEqual(
      registry.compiled.filter((c) => c.key !== "broken@1").map((c) => c.freed),
      [true, true],
      "what compiled is freed",
    );
    await engine.registerEffects([effect("blur"), effect("glow")]);
    assert.deepEqual([...registry.effects], ["blur@1", "glow@1"]);
    const compiles = registry.compiled.length;
    await engine.registerEffects([effect("blur"), effect("blur", 2)]);
    assert.equal(registry.compiled.length, compiles + 1, "blur@1 is held");
    assert.deepEqual([...registry.effects], ["blur@1", "glow@1", "blur@2"]);
  } finally {
    globalThis.fakeRegistry.refuseEffect = () => undefined;
  }
});

test("a runtime effect refused before compiling, or by a full registry, leaves none of its call registered", async () => {
  globalThis.fakeRegistry.refuseEffect = (definition) =>
    definition.id === "malformed" ? "before" : undefined;
  try {
    const { engine, registry } = await started(64, 36);
    await assert.rejects(
      engine.registerEffects([effect("blur"), effect("malformed")], "export"),
      (error) =>
        error.code === "invalid_effect" &&
        error.operation === "export" &&
        error.entityId === "malformed@1",
    );
    registry.effectLimit = 2;
    await assert.rejects(
      engine.registerEffects([effect("a"), effect("b"), effect("c")]),
      (error) => error.code === "invalid_effect" && error.entityId === "c@1",
    );
    assert.deepEqual([...registry.effects], [], "a and b are taken back");
    // Taken back, they are registered by the next call.
    registry.effectLimit = undefined;
    await engine.registerEffects([effect("a"), effect("b")]);
    assert.deepEqual([...registry.effects], ["a@1", "b@1"]);
  } finally {
    globalThis.fakeRegistry.refuseEffect = () => undefined;
  }
});
