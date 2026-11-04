import assert from "node:assert/strict";
import test from "node:test";

import { WasmModule } from "alpha2/wasm-module";

function fakeGlue({ fails = 0, restarts = true } = {}) {
  const glue = {
    inits: [],
    resets: 0,
    async default(input) {
      glue.inits.push(input?.module_or_path);
      if (glue.inits.length <= fails) throw new Error("offline");
    },
  };
  if (restarts)
    glue.__wbg_reset_state_async = async () => {
      glue.resets += 1;
    };
  return glue;
}

test("a module loads once per realm and a failed load is tried again", async () => {
  const glue = fakeGlue({ fails: 1 });
  let imports = 0;
  const module = new WasmModule(async () => {
    imports += 1;
    return glue;
  });
  await assert.rejects(module.load("first.wasm"), /offline/);
  const [a, b] = await Promise.all([module.load("second.wasm"), module.load()]);
  assert.equal(a, glue);
  assert.equal(b, glue);
  assert.deepEqual(glue.inits, ["first.wasm", "second.wasm"]);
  assert.equal(imports, 2);
});

test("a restart replaces the instance asynchronously, only once loaded and built to", async () => {
  const glue = fakeGlue();
  const module = new WasmModule(async () => glue);
  await assert.rejects(module.restart(), /not loaded/);
  await module.load();
  await module.restart();
  assert.equal(glue.resets, 1);
  const fixed = new WasmModule(async () => fakeGlue({ restarts: false }));
  await fixed.load();
  await assert.rejects(fixed.restart(), /not built to restart/);
});
