// The browser's extension module on Lottie that velato cannot read as written. A panic in wasm32 is
// a trap that leaves the page's one registry unusable, so each document is rewritten or refused
// before velato reads it: these run the packaged module itself, one registry for every case.
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "../../..");
const wasmDirectory = join(root, "packages/alpha2/wasm");
const corpus = join(root, "tests/corpus/extensions");
const packaged = existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm"));
// Skipped before `npm run build` packages the wasm; ALPHA2_REQUIRE=wasm (set by CI after the
// build) makes a missing package fail them.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the extension wasm package is not built",
  );

async function registry() {
  const wasm = await import(
    pathToFileURL(join(wasmDirectory, "alpha2_extensions.js")).href
  );
  wasm.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
  });
  return new wasm.ExtensionRegistry();
}

const lottie = JSON.stringify({ format: "lottie" });
const encode = (document) => new TextEncoder().encode(JSON.stringify(document));
const prefix = {
  unsupported: "unsupported:",
  malformed: "malformed:",
  budget: "too large:",
};

function centre(registry, handle) {
  const frame = registry.render(handle, 0n, 0, 0);
  try {
    const at = (24 * frame.width + 32) * 4;
    return [...frame.pixels.slice(at, at + 4)];
  } finally {
    frame.free();
  }
}

test(
  "Lottie velato cannot read safely is rewritten or refused, and the module keeps working",
  { skip },
  async () => {
    const extensions = await registry();
    const cases = JSON.parse(
      readFileSync(join(corpus, "normalize-cases.json"), "utf8"),
    );
    for (const { name, outcome, message, animation, diagnostics } of cases) {
      const bytes = encode(animation);
      if (outcome === "opens") {
        const { report } = JSON.parse(extensions.probe(bytes));
        if (diagnostics !== undefined) {
          assert.deepEqual(
            report.diagnostics.map(({ feature, path, count }) => [
              feature,
              path,
              count,
            ]),
            diagnostics,
            name,
          );
        }
        const handle = extensions.open(lottie, bytes);
        assert.deepEqual(centre(extensions, handle), [255, 0, 0, 255], name);
        extensions.release(handle);
      } else {
        for (const attempt of [
          () => extensions.probe(bytes),
          () => extensions.open(lottie, bytes),
        ]) {
          assert.throws(
            attempt,
            // The module throws its error's text; a trap would be a RuntimeError.
            (error) =>
              typeof error === "string" &&
              error.startsWith(prefix[outcome]) &&
              error.includes(message),
            name,
          );
        }
      }
    }
    const handle = extensions.open(
      lottie,
      readFileSync(join(corpus, "moving.json")),
    );
    const frame = extensions.render(handle, 0n, 0, 0);
    assert.deepEqual(
      [...frame.pixels.slice((24 * 64 + 16) * 4, (24 * 64 + 16) * 4 + 4)],
      [255, 0, 0, 255],
    );
    frame.free();
    extensions.release(handle);
  },
);

test(
  "a dotLottie package whose first animation is refused imports by one that opens",
  { skip },
  async () => {
    const extensions = await registry();
    const bytes = readFileSync(join(corpus, "first-refused.lottie"));
    const { report } = JSON.parse(extensions.probe(bytes));
    assert.deepEqual(report.items, ["split", "moving"]);
    assert.equal(report.item, "moving");
    assert.equal(report.diagnostics[0].kind, "unreadable_item");
    assert.equal(report.diagnostics[0].item, "split");
    assert.match(report.diagnostics[0].reason, /split positions/);
    const handle = extensions.open(
      JSON.stringify({ format: "lottie", animation: report.item }),
      bytes,
    );
    extensions.release(handle);
  },
);
