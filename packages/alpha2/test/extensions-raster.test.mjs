// Raster images whose headers declare more than a provider opens: in wasm32 such an allocation, or
// its sizing arithmetic, traps and breaks the page's one registry, so sizes are checked from the
// header first. These run the packaged module, one registry for every case.
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

const provider = (format) => JSON.stringify({ format });

/** A GIF89a of a `width` x `height` screen whose two 1x1 frames show 100 ms each. */
function gif(width, height) {
  const bytes = [...new TextEncoder().encode("GIF89a")];
  bytes.push(width & 255, width >> 8, height & 255, height >> 8);
  bytes.push(0x80, 0, 0, 0, 0, 0, 255, 255, 255);
  for (let frame = 0; frame < 2; frame += 1) {
    bytes.push(0x21, 0xf9, 0x04, 0x00, 10, 0, 0, 0);
    bytes.push(
      0x2c,
      0,
      0,
      0,
      0,
      1,
      0,
      1,
      0,
      0x00,
      0x02,
      0x02,
      0x44,
      0x01,
      0x00,
    );
  }
  bytes.push(0x3b);
  return Uint8Array.from(bytes);
}

/** An animated WebP header whose canvas is 2^24 pixels a side. */
function webpCanvas() {
  const bytes = [...new TextEncoder().encode("RIFF\0\0\0\0WEBPVP8X")];
  bytes.push(10, 0, 0, 0, 0x02, 0, 0, 0, 255, 255, 255, 255, 255, 255);
  bytes.push(...new TextEncoder().encode("ANIM"), 6, 0, 0, 0, 0, 0, 0, 0, 0, 0);
  const file = Uint8Array.from(bytes);
  new DataView(file.buffer).setUint32(4, file.length - 8, true);
  return file;
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

/**
 * A PNG whose header declares a `width` x `height` RGBA8 image and whose data is an empty zlib
 * stream.
 */
function declaredPng(width, height) {
  const parts = [[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]];
  const chunk = (kind, data) => {
    const body = [...new TextEncoder().encode(kind), ...data];
    const length = new Uint8Array(4);
    new DataView(length.buffer).setUint32(0, data.length);
    const crc = new Uint8Array(4);
    new DataView(crc.buffer).setUint32(0, crc32(body));
    parts.push([...length], body, [...crc]);
  };
  const header = new Uint8Array(13);
  new DataView(header.buffer).setUint32(0, width);
  new DataView(header.buffer).setUint32(4, height);
  header.set([8, 6, 0, 0, 0], 8);
  chunk("IHDR", [...header]);
  chunk("IDAT", [0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
  chunk("IEND", []);
  return Uint8Array.from(parts.flat());
}

const refused = (prefix) => (error) =>
  typeof error === "string" && error.startsWith(prefix);

test(
  "raster sizes are checked from their headers, and the module keeps working",
  { skip },
  async () => {
    const extensions = await registry();
    // Canvases that ask for more than wasm32 can address, or size it with an overflowing product.
    for (const file of [
      gif(65_535, 65_535),
      gif(23_171, 23_171),
      webpCanvas(),
    ]) {
      assert.throws(() => extensions.probe(file), refused("too large:"));
      assert.throws(
        () => extensions.open(provider("animated_image"), file),
        refused("too large:"),
      );
    }
    // A WebP frame whose header and picture disagree: the compositor would index past the picture.
    const mismatched = new Uint8Array(
      readFileSync(join(corpus, "animation.webp")),
    );
    mismatched[118 + 6] = 28;
    assert.throws(
      () => extensions.open(provider("animated_image"), mismatched),
      refused("malformed:"),
    );
    // An SVG's 23000 x 23000 embedded PNG is 2.1 GB of RGBA; it is reported and never decoded.
    const svg = new TextEncoder().encode(
      `<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><image width="16" height="16" href="data:image/png;base64,${Buffer.from(declaredPng(23_000, 23_000)).toString("base64")}"/></svg>`,
    );
    const { report } = JSON.parse(extensions.probe(svg));
    assert.deepEqual(report.diagnostics, [
      { kind: "missing_image", href: "data:image/png" },
    ]);
    const handle = extensions.open(provider("svg"), svg);
    extensions.render(handle, 0n, 16, 16).free();
    extensions.release(handle);
    // What the corpus always drew still draws, from the same registry.
    const animation = extensions.open(
      provider("animated_image"),
      readFileSync(join(corpus, "animation.gif")),
    );
    const frame = extensions.render(animation, 0n, 0, 0);
    assert.equal(frame.width, 48);
    frame.free();
    extensions.release(animation);
  },
);

/** A PNG signature and one chunk whose length field says `length`, padded to 33 bytes. */
function pngChunkOfLength(length) {
  const file = new Uint8Array(33);
  file.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  new DataView(file.buffer).setUint32(8, length);
  file.set(new TextEncoder().encode("IHDR"), 12);
  return file;
}

test(
  "a PNG chunk length near 2^32 ends the animation sniff, as it does natively",
  { skip },
  async () => {
    const extensions = await registry();
    // wasm32 adds chunk lengths in 32 bits: walking past one this long would overflow and trap.
    for (const length of [
      0xd, 0x7fff_ffff, 0xffff_ffe3, 0xffff_ffe4, 0xffff_fff0, 0xffff_ffff,
    ]) {
      const file = pngChunkOfLength(length);
      assert.equal(extensions.probe(file), undefined, `length ${length}`);
      assert.throws(
        () => extensions.open(provider("animated_image"), file),
        (error) => typeof error === "string" && !error.includes("unreachable"),
        `length ${length}`,
      );
    }
    const animation = extensions.open(
      provider("animated_image"),
      readFileSync(join(corpus, "animation.png")),
    );
    extensions.release(animation);
  },
);
