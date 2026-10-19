// An image sequence in the browser's extension module. Registering every frame file before opening
// the sequence would hold a whole sequence in a wasm memory that never shrinks and stops at 4 GiB,
// so each call registers only the frames it reads. These run the packaged module.
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

const frame = (number) =>
  new Uint8Array(
    readFileSync(
      join(
        root,
        `tests/corpus/extensions/sequence_${String(number).padStart(4, "0")}.png`,
      ),
    ),
  );

test(
  "a sequence's frames enter the module one call at a time and none stay",
  { skip },
  async () => {
    const { loadExtensions } = await import("alpha2/extensions");
    const glue = await import("alpha2/extensions-wasm");
    glue.initSync({
      module: readFileSync(join(wasmDirectory, "alpha2_extensions_bg.wasm")),
    });
    const extensions = await loadExtensions();
    const provider = {
      format: "image_sequence",
      frames: ["f7", null, "f9", "f10"],
      frame_rate: { num: 10, den: 1 },
      start_number: 7,
      missing: "hold",
    };
    const reads = [];
    const read = async (locator) => {
      reads.push(locator);
      return frame(Number(locator.slice(1)));
    };
    assert.deepEqual(extensions.openNeeds(provider), ["f7"]);
    const source = await extensions.withResources(
      read,
      () => extensions.openNeeds(provider),
      () => extensions.open(provider),
    );
    assert.deepEqual(reads.splice(0), ["f7"], "opening reads the first frame");
    assert.equal(extensions.resourceBytes, 0, "and keeps no file");

    const fit = { width: 48, height: 32 };
    // The held gap shows frame 0, which the source decoded: nothing to read.
    assert.deepEqual(source.needs(15_000n), []);
    // 0.25 s at 10 fps is frame 2.
    assert.deepEqual(source.needs(30_000n), ["f9"]);
    const shown = await extensions.withResources(
      read,
      () => source.needs(30_000n),
      () => source.render(30_000n, fit),
    );
    assert.equal(shown.index, 2);
    assert.deepEqual(reads.splice(0), ["f9"]);
    assert.equal(extensions.resourceBytes, 0);
    assert.deepEqual(source.needs(30_000n), [], "a frame decoded is kept");

    // Two renders of one source at once: each registers what it reads for its own call, and neither
    // releases the other's.
    const [a, b] = await Promise.all([
      extensions.withResources(
        read,
        () => source.needs(36_000n),
        () => source.render(36_000n, fit),
      ),
      extensions.withResources(
        read,
        () => source.needs(36_000n),
        () => source.render(36_000n, fit),
      ),
    ]);
    assert.equal(a.index, 3);
    assert.deepEqual(a.pixels, b.pixels);
    assert.equal(extensions.resourceBytes, 0);

    assert.throws(() => extensions.open(provider), /missing resource f7/);
    source.release();
  },
);
