// The browser text rasterizer, against a stand-in OffscreenCanvas that records what the page's
// canvas would be told: the font list and where each line is anchored.
import assert from "node:assert/strict";
import test from "node:test";

import { OffscreenTextRasterizer } from "../dist/text.js";

const drawn = [];
// What the stand-in canvas holds at every pixel once drawn: straight RGBA, as a canvas reads back.
let straightPixel;
// UTF-16 units passed to measureText, which a canvas shapes each time.
let measured = 0;
// Ink a stand-in glyph reaches past its pen box, in em: overrides per character.
const ink = { ascent: 0.8, descent: 0.2, left: {}, right: {} };
class FakeContext {
  font = "";
  textAlign = "start";
  textBaseline = "alphabetic";
  fillStyle = "";
  constructor(canvas) {
    this.canvas = canvas;
  }
  // Each character is one em wide; the bounding box is measured from the alignment point.
  measureText(text) {
    measured += text.length;
    const size = Number.parseFloat(this.font) || 10;
    const characters = Array.from(text);
    const width = characters.length * size;
    const origin =
      this.textAlign === "center"
        ? width / 2
        : this.textAlign === "end" || this.textAlign === "right"
          ? width
          : 0;
    const first = characters[0];
    const last = characters.at(-1);
    return {
      width,
      actualBoundingBoxLeft: origin + (ink.left[first] ?? 0) * size,
      actualBoundingBoxRight: width - origin + (ink.right[last] ?? 0) * size,
      actualBoundingBoxAscent: ink.ascent * size,
      actualBoundingBoxDescent: ink.descent * size,
    };
  }
  fillText(text, x, y) {
    drawn.push({ text, x, y, font: this.font, align: this.textAlign });
  }
  getImageData(_x, _y, width, height) {
    const data = new Uint8ClampedArray(width * height * 4);
    if (straightPixel !== undefined)
      for (let at = 0; at < data.length; at += 4) data.set(straightPixel, at);
    return { data };
  }
}
globalThis.OffscreenCanvas ??= class {
  constructor(width, height) {
    this.width = width;
    this.height = height;
  }
  getContext() {
    return new FakeContext(this);
  }
};

let version = 0n;
function raster(overrides) {
  drawn.length = 0;
  return new OffscreenTextRasterizer().rasterize({
    text: "Title",
    contentVersion: ++version,
    fontFamilies: ["sans-serif"],
    fontSize: 10,
    maxWidth: 200,
    ...overrides,
  });
}
function rasterize(overrides) {
  raster(overrides);
  return drawn[0];
}

test("generic families reach the canvas as keywords, named ones quoted", () => {
  assert.equal(rasterize().font, "10px sans-serif");
  assert.equal(
    rasterize({
      fontFamilies: ["Noto Sans", 'Odd "Name" \\ X', "system-ui", "Serif"],
    }).font,
    '10px "Noto Sans", "Odd \\"Name\\" \\\\ X", system-ui, serif',
  );
});

test("every alignment draws inside the layout box", () => {
  for (const [align, x] of [
    [undefined, 0],
    ["start", 0],
    ["left", 0],
    ["center", 100],
    ["end", 200],
    ["right", 200],
  ]) {
    const line = rasterize({ align });
    assert.equal(line.x, x, `${align} anchors at ${line.x}`);
    assert.equal(line.align, align ?? "start");
  }
  assert.throws(() => rasterize({ align: "justify" }), RangeError);
});

test("ink past the line box widens the texture evenly instead of being cut", () => {
  // Tight lines: the last line's descender reaches 0.3 em below its baseline, 0.1 em past the box.
  Object.assign(ink, { descent: 0.3 });
  try {
    const tight = raster({ text: "gypsy\ngypsy", lineHeight: 0.9 });
    assert.equal(tight.layoutHeight, 19);
    assert.equal(tight.layoutY, 1);
    assert.equal(tight.texture.height, 19 + 2 * 1);
    assert.equal(tight.firstBaseline, 8);
    for (const line of drawn)
      assert.ok(
        line.y + 3 <= tight.texture.height,
        `${line.y} cuts its descender`,
      );
    assert.equal(drawn[0].y, 1 + 8, "the first baseline sits below the margin");
  } finally {
    ink.descent = 0.2;
  }
  // A 'j' hooks 0.4 em left of its pen; an 'f' reaches 0.3 em past its advance.
  ink.left.j = 0.4;
  ink.right.f = 0.3;
  try {
    const hooked = raster({ text: "jeff", maxWidth: 0 });
    assert.equal(hooked.layoutWidth, 40);
    assert.equal(hooked.layoutX, 4);
    assert.equal(hooked.texture.width, 40 + 2 * 4);
    assert.equal(drawn[0].x, 4);
  } finally {
    ink.left = {};
    ink.right = {};
  }
  // Text that stays inside its box keeps the texture its box.
  const plain = raster({ text: "Title", maxWidth: 0, lineHeight: 1.2 });
  assert.deepEqual(
    [plain.layoutX, plain.layoutY, plain.texture.width, plain.texture.height],
    [0, 0, 50, 12],
  );
});

test("a run with no spaces wider than the wrap width breaks between characters", () => {
  const japanese = raster({
    text: "これは長い日本語のタイトルです",
    fontSize: 10,
    maxWidth: 40,
  });
  assert.equal(japanese.lineCount, 4);
  assert.deepEqual(
    drawn.map((line) => line.text),
    ["これは長", "い日本語", "のタイト", "ルです"],
  );
  assert.equal(japanese.texture.width, 40);
  // A long word after a short one moves to its own line first, then breaks; spaces hang.
  raster({ text: "ab abcdefghij  ", maxWidth: 40 });
  assert.deepEqual(
    drawn.map((line) => line.text),
    ["ab", "abcd", "efgh", "ij"],
  );
  // Combining marks stay with their base.
  raster({ text: "e\u0301e\u0301e\u0301e\u0301e\u0301", maxWidth: 40 });
  assert.ok(drawn.every((line) => !line.text.startsWith("\u0301")));
});

test("replace takes each emoji grapheme whole and leaves other symbols", () => {
  // As the portable host does: a flag, a skin tone, and a family are one replacement each.
  assert.equal(
    rasterize({
      text: "✓ \u{1f1f0}\u{1f1f7} \u{1f44d}\u{1f3fd} \u{1f468}‍\u{1f469}‍\u{1f467} ⭐",
      emojiPolicy: "replace",
      maxWidth: 0,
    }).text,
    "✓ � � � �",
  );
});

test("a request past the portable host's limits is refused before anything is measured", () => {
  for (const [label, overrides] of [
    ["text over 64 KiB", { text: "a".repeat(64 * 1024 + 1), maxWidth: 0 }],
    ["text over 64 KiB as UTF-8", { text: "\u00e9".repeat(32 * 1024 + 1) }],
    ["line height over 10", { lineHeight: 10.5 }],
    ["font size over 2048", { fontSize: 2049 }],
    ["wrap width over 16384", { maxWidth: 16_385 }],
    ["no font family", { fontFamilies: [] }],
    ["a zero content version", { contentVersion: 0n }],
  ]) {
    measured = 0;
    assert.throws(() => raster(overrides), RangeError, label);
    assert.equal(measured, 0, label);
  }
  // The limits themselves are drawn.
  raster({ text: "a".repeat(64 * 1024), fontSize: 0.25, maxWidth: 0 });
  raster({ lineHeight: 10 });
});

test("wrapping measures each word once, however long the line", () => {
  // Re-measuring the whole line per word or character would shape it quadratically.
  for (const text of ["ab ".repeat(4_000), "\u65e5".repeat(6_000)]) {
    measured = 0;
    const result = raster({ text, fontSize: 1, maxWidth: 5_000 });
    assert.ok(result.lineCount >= 2, "it wrapped");
    assert.ok(
      measured <= 4 * text.length,
      `${measured} units measured for ${text.length}`,
    );
  }
  // What wraps where is unchanged.
  raster({ text: "ab abcdefghij  ", maxWidth: 40 });
  assert.deepEqual(
    drawn.map((line) => line.text),
    ["ab", "abcd", "efgh", "ij"],
  );
});

test("replace follows the emoji corpus both text hosts read", async () => {
  const { readFile } = await import("node:fs/promises");
  const corpus = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/emoji-replace-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  for (const { name, text, replaced } of corpus.cases) {
    assert.equal(
      rasterize({ text, emojiPolicy: "replace", maxWidth: 0 }).text,
      replaced,
      name,
    );
  }
});

test("monochrome asks for text presentation without splitting a sequence", () => {
  const monochrome = (text) =>
    rasterize({ text, emojiPolicy: "monochrome", maxWidth: 0 }).text;
  // A single pictograph takes U+FE0E; one that asks for text already keeps its own.
  assert.equal(monochrome("⭐ ☀ ❤️"), "⭐︎ ☀︎ ❤︎");
  assert.equal(monochrome("⌚︎"), "⌚︎");
  // A skin tone, a family, a flag, and a keycap have no text form; they are left whole.
  for (const sequence of [
    "\u{1f44d}\u{1f3fd}",
    "\u{1f468}‍\u{1f469}‍\u{1f467}",
    "\u{1f1f0}\u{1f1f7}",
    "1️⃣",
  ])
    assert.equal(monochrome(sequence), sequence);
  // Letters and the color default are untouched.
  assert.equal(monochrome("Title"), "Title");
  assert.equal(rasterize({ text: "⭐", maxWidth: 0 }).text, "⭐");
});

test("pixels are premultiplied, each channel rounded", () => {
  for (const [straight, premultiplied] of [
    [
      [200, 100, 50, 128],
      [100, 50, 25, 128],
    ],
    [
      [255, 255, 255, 0],
      [0, 0, 0, 0],
    ],
    [
      [255, 1, 3, 255],
      [255, 1, 3, 255],
    ],
    [
      [128, 255, 1, 1],
      [1, 1, 0, 1],
    ],
  ]) {
    straightPixel = straight;
    try {
      const result = raster();
      assert.equal(result.texture.premultiplication, "premultiplied");
      assert.deepEqual(Array.from(result.rgba8.subarray(0, 4)), premultiplied);
      assert.deepEqual(Array.from(result.rgba8.subarray(-4)), premultiplied);
    } finally {
      straightPixel = undefined;
    }
  }
});

test("a content version is drawn once, until it is invalidated or the cache cleared", () => {
  const rasterizer = new OffscreenTextRasterizer();
  const request = {
    text: "Title",
    contentVersion: 7n,
    fontFamilies: ["sans-serif"],
    fontSize: 10,
  };
  drawn.length = 0;
  const first = rasterizer.rasterize(request);
  // The version is the key: a request with a cached version gets the cached texture as it is.
  assert.equal(rasterizer.rasterize({ ...request, text: "Other" }), first);
  assert.equal(drawn.length, 1);
  assert.equal(rasterizer.cacheSize, 1);
  rasterizer.invalidate(7n);
  assert.equal(rasterizer.cacheSize, 0);
  assert.notEqual(rasterizer.rasterize(request), first);
  assert.equal(drawn.length, 2);
  rasterizer.rasterize({ ...request, contentVersion: 8n });
  assert.equal(rasterizer.cacheSize, 2);
  rasterizer.clear();
  assert.equal(rasterizer.cacheSize, 0);
});

test("loaded fonts join the global font set until the rasterizer closes", async () => {
  const set = new Set();
  const faces = [];
  globalThis.fonts = set;
  globalThis.FontFace = class {
    constructor(family, source, descriptors) {
      Object.assign(this, { family, source, descriptors });
      faces.push(this);
    }
    async load() {
      return this;
    }
  };
  try {
    const rasterizer = new OffscreenTextRasterizer();
    const face = await rasterizer.loadFont("Fixture", new ArrayBuffer(4), {
      weight: "700",
    });
    assert.equal(face, faces[0]);
    assert.deepEqual(
      [face.family, face.descriptors],
      ["Fixture", { weight: "700" }],
    );
    assert.ok(set.has(face));
    raster();
    rasterizer.rasterize({
      text: "Title",
      contentVersion: 1n,
      fontFamilies: ["Fixture"],
      fontSize: 10,
    });
    assert.equal(rasterizer.cacheSize, 1);
    rasterizer.close();
    assert.equal(set.size, 0, "the face left the global set");
    assert.equal(rasterizer.cacheSize, 0);
  } finally {
    delete globalThis.fonts;
    delete globalThis.FontFace;
  }
});
