import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "../../..");
const wasmDirectory = join(root, "packages/alpha2/wasm");
const corpus = join(root, "tests/corpus/extensions");
const packaged = existsSync(join(wasmDirectory, "alpha2_extensions_bg.wasm"));
// Skipped before `npm run build`; ALPHA2_REQUIRE=wasm makes that a failure.
const skip =
  !packaged &&
  missingCapability(
    "wasm",
    suiteOf(import.meta.url),
    "the extension wasm package is not built",
  );

async function extensions() {
  const { loadExtensions } = await import("alpha2/extensions");
  const glue = await import("alpha2/extensions-wasm");
  glue.initSync({
    module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
  });
  return loadExtensions();
}

function pixel(frame, x, y) {
  const at = (y * frame.width + x) * 4;
  return [...frame.pixels.slice(at, at + 4)];
}

test(
  "a LaTeX formula opens without bytes and draws in its color",
  { skip },
  async () => {
    const module = await extensions();
    const source = module.open({
      format: "latex",
      latex: String.raw`\blacksquare`,
      font_size: 64,
      color: { red: 0, green: 1, blue: 0, alpha: 0.5 },
    });
    try {
      assert.equal(source.report.format, "latex");
      const frame = source.render(0n, { width: 0, height: 0 });
      const centre = pixel(frame, frame.width >> 1, frame.height >> 1);
      assert.deepEqual(centre, [0, 128, 0, 128]);
    } finally {
      source.release();
    }
    assert.throws(
      () =>
        module.open({
          format: "latex",
          latex: String.raw`\frac{1}`,
          font_size: 64,
        }),
      /malformed: .*missing argument/,
    );
  },
);

test(
  "a PDF is probed as its pages and opens the page it names",
  { skip },
  async () => {
    const module = await extensions();
    const bytes = new Uint8Array(readFileSync(join(corpus, "pages.pdf")));
    const found = module.probe(bytes);
    assert.equal(found.provider, "pdf");
    assert.deepEqual(
      [
        found.report.format,
        found.report.width,
        found.report.height,
        found.report.items,
      ],
      ["pdf", 793, 1123, ["1", "2"]],
    );
    const first = module.open({ format: "pdf", page: 1 }, bytes);
    try {
      const frame = first.render(0n, { width: 0, height: 0 });
      assert.deepEqual(pixel(frame, 100, 100), [255, 0, 0, 255]);
      assert.deepEqual(pixel(frame, 700, 100), [255, 255, 255, 255]);
    } finally {
      first.release();
    }
    const second = module.open({ format: "pdf", page: 2 }, bytes);
    try {
      assert.deepEqual(
        [second.report.width, second.report.height, second.report.item],
        [1056, 816, "2"],
      );
    } finally {
      second.release();
    }
    assert.throws(
      () =>
        module.open(
          { format: "pdf", page: 1 },
          new Uint8Array(readFileSync(join(corpus, "user-password.pdf"))),
        ),
      /unsupported: .*password/,
    );
  },
);
