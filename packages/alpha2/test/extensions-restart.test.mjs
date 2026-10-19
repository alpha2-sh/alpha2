// A trap (provider panic, failed allocation) would leave the page's one extension registry
// borrowed and every later call throwing until a reload; the module restarts instead. These run
// the packaged module, trapping with a real out-of-bounds read inside a registry call.
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "../../..");
const wasmDirectory = join(root, "packages/alpha2/wasm");
const packaged = existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm"));
// Skipped before `npm run build` packages the wasm; ALPHA2_REQUIRE_WASM=1 (set by CI after the
// build) makes a missing package fail them.
const skip =
  !packaged &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the extension wasm package is not built";

const svg = new TextEncoder().encode(
  '<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="#f00"/></svg>',
);
const fit = { width: 8, height: 8 };

test(
  "a failed load is tried again, and a call that aborts the module restarts it",
  { skip },
  async () => {
    const { loadExtensions, ExtensionModuleError, StaleExtensionSourceError } =
      await import("alpha2/extensions");
    const glue = await import("alpha2/extensions-wasm");

    // Node cannot fetch a file URL, as a page cannot fetch while offline: the load fails, and that
    // failure is not kept.
    await assert.rejects(
      loadExtensions(new URL("file:///alpha2/missing.wasm")),
    );
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const extensions = await loadExtensions();
    assert.equal(await loadExtensions(), extensions, "one module per page");

    const font = readFileSync(join(root, "tests/fonts/Alpha2Fixture.ttf"));
    extensions.setFonts([new Uint8Array(font)]);
    const first = extensions.open({ format: "svg" }, svg);
    const second = extensions.open({ format: "svg" }, svg);
    assert.equal(second.render(0n, fit).width, 8);

    // The next registry call reads past the end of the instance's memory while it holds the
    // registry borrowed, as a panic inside a provider leaves it.
    const render = glue.ExtensionRegistry.prototype.render;
    glue.ExtensionRegistry.prototype.render = function (...args) {
      glue.ExtensionRegistry.prototype.render = render;
      glue
        .initSync()
        .extensionregistry_open(this.__wbg_ptr, 0xffffff00, 200, 0, 0);
      return render.apply(this, args);
    };
    try {
      assert.throws(
        () => first.render(0n, fit),
        (error) =>
          error instanceof ExtensionModuleError &&
          error.cause instanceof WebAssembly.RuntimeError,
      );
    } finally {
      glue.ExtensionRegistry.prototype.render = render;
    }
    // The source whose call stopped the module is not run again.
    assert.throws(() => first.render(0n, fit), ExtensionModuleError);
    // The other one belonged to the old instance: it says so, and opens again.
    assert.throws(() => second.render(0n, fit), StaleExtensionSourceError);
    second.release();
    const reopened = extensions.open({ format: "svg" }, svg);
    const frame = reopened.render(0n, fit);
    assert.equal(frame.width, 8);
    assert.deepEqual([...frame.pixels.subarray(0, 4)], [255, 0, 0, 255]);
    assert.equal(extensions.probe(svg)?.provider, "svg");
    const text = extensions.open(
      { format: "svg" },
      new TextEncoder().encode(
        '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="16"><text x="0" y="12" font-family="DejaVu Sans" font-size="12">Ab</text></svg>',
      ),
    );
    assert.deepEqual(text.report.diagnostics, []);
    reopened.release();
    text.release();
  },
);

test(
  "fonts replace those set before, and a source keeps the fonts it opened with",
  { skip },
  async () => {
    const { loadExtensions } = await import("alpha2/extensions");
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const extensions = await loadExtensions();
    const font = new Uint8Array(
      readFileSync(join(root, "tests/fonts/Alpha2Fixture.ttf")),
    );
    const text = new TextEncoder().encode(
      '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="16"><text x="0" y="12" font-family="DejaVu Sans" font-size="12">Ab</text></svg>',
    );
    const missing = (source) =>
      source.report.diagnostics.some(({ kind }) => kind === "missing_font");
    extensions.setFonts([font]);
    const drawn = extensions.open({ format: "svg" }, text);
    assert.equal(missing(drawn), false);
    // An empty stack is no stack: the text is reported missing, as the native host reports it
    // without caption fonts.
    extensions.setFonts([]);
    const left = extensions.open({ format: "svg" }, text);
    assert.equal(missing(left), true);
    assert.equal(missing(drawn), false, "opened with the font, keeps it");
    const pixels = (source) => source.render(0n, { width: 64, height: 16 });
    assert.ok(pixels(drawn).pixels.some((value) => value !== 0));
    assert.ok(pixels(left).pixels.every((value) => value === 0));
    extensions.setFonts([font]);
    extensions.setFonts([font]);
    const again = extensions.open({ format: "svg" }, text);
    assert.deepEqual(pixels(again).pixels, pixels(drawn).pixels);
    for (const source of [drawn, left, again]) source.release();
    extensions.setFonts([]);
  },
);
