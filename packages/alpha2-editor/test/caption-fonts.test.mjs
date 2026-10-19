// A font the engine cannot parse costs only itself: registration keeps every other font of the
// stack, and import refuses such a file before it reaches a document; a .cube the engine cannot
// apply is refused at import too.
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { registerCaptionFonts } from "../dist/browser/runtime/caption-fonts.js";
import { createMediaAdapter } from "../dist/browser/index.js";

const root = resolve(import.meta.dirname, "..", "..", "..");
const wasmDirectory = join(root, "packages", "alpha2", "wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm"));
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

/** An engine registry that refuses what is not an OpenType font, as the engine's parser does. */
function registry() {
  const fonts = [];
  return {
    fonts,
    clearFonts: () => fonts.splice(0),
    addFont(name, bytes) {
      const magic = new TextDecoder().decode(bytes.subarray(0, 4));
      if (magic === "wOF2" || magic === "wOFF")
        throw new Error("invalid text font: unknown magic");
      fonts.push(name);
    },
  };
}

const font = (name, magic) => ({
  id: `font-${name}`,
  name,
  bytes: new TextEncoder().encode(magic),
  faceIndex: 0,
});

test("a font the engine refuses is skipped and returned; the rest of the stack stays", () => {
  const target = registry();
  const broken = font("Web", "wOF2");
  const refused = registerCaptionFonts(target, [
    font("Mine", "OTTO"),
    broken,
    font("DejaVu Sans", "\0\x01\0\0"),
  ]);
  assert.deepEqual(target.fonts, ["Mine", "DejaVu Sans"]);
  assert.equal(refused.length, 1);
  assert.equal(refused[0].font, broken);
  assert.match(refused[0].error.message, /unknown magic/);
});

test(
  "importing a WOFF2 named .ttf is refused, saying why; an OpenType font imports",
  { skip },
  async () => {
    const wasm = await import(
      pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
    );
    wasm.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
    });
    const adapter = createMediaAdapter();
    const signal = new AbortController().signal;
    // A WOFF2 header: the engine refuses it by its magic before reading further.
    const woff2 = new TextEncoder().encode("wOF2\0\x01\0\0");
    await assert.rejects(
      adapter.stage(new File([woff2], "Web.ttf"), signal),
      (error) =>
        error.code === "unreadable_media" &&
        /Web\.ttf/.test(error.message) &&
        /magic/i.test(error.message),
    );
    const fixture = readFileSync(
      join(root, "tests", "fonts", "Alpha2Fixture.ttf"),
    );
    const staged = await adapter.stage(
      new File([fixture], "Alpha2Fixture.ttf"),
      signal,
    );
    assert.equal(staged.runtime.kind, "font");
    staged.dispose();
  },
);

test(
  "importing a .cube the engine cannot apply is refused naming the file; a 3D LUT imports",
  { skip },
  async () => {
    const wasm = await import(
      pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
    );
    wasm.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
    });
    const adapter = createMediaAdapter();
    const signal = new AbortController().signal;
    // A one-dimensional curve: also a .cube, which the engine does not apply.
    const curve = "LUT_1D_SIZE 2\n0 0 0\n1 1 1\n";
    await assert.rejects(
      adapter.stage(new File([curve], "Curve.cube"), signal),
      (error) =>
        error.code === "unreadable_media" &&
        /Cannot read Curve\.cube as a 3D LUT/.test(error.message) &&
        /one-dimensional/.test(error.message),
    );
    const grade = readFileSync(
      join(root, "tests", "corpus", "parity-grade.cube"),
    );
    const staged = await adapter.stage(new File([grade], "Grade.cube"), signal);
    assert.equal(staged.runtime.kind, "lut3d");
    staged.dispose();
  },
);
