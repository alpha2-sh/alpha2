// The browser runtime's stills and titles stay within a byte budget: an imported still keeps its
// file and thumbnail, not its decoded picture, and `SourcePixels` holds still and text rasters in
// one LRU bounded by bytes, never evicting what the frame it draws reads.
import test from "node:test";
import assert from "node:assert/strict";

const bitmaps = [];
const decodes = new Map();

/** A decoded picture: its size, and a seed its pixels are made of. */
class FakeBitmap {
  constructor(width, height, seed) {
    Object.assign(this, { width, height, seed, closed: false });
    bitmaps.push(this);
  }
  close() {
    this.closed = true;
  }
}

/** Reads back what was drawn as the drawn picture's seed, and refuses to read a closed picture. */
class FakeCanvas {
  constructor(width, height) {
    Object.assign(this, { width, height, drawn: null });
  }
  getContext() {
    const canvas = this;
    return {
      drawImage(image) {
        assert.equal(image.closed, false, "a closed picture is drawn");
        canvas.drawn = image;
      },
      getImageData(x, y, width, height) {
        return {
          data: new Uint8ClampedArray(width * height * 4).fill(
            canvas.drawn.seed,
          ),
        };
      },
      clearRect() {},
    };
  }
}

globalThis.OffscreenCanvas = FakeCanvas;
globalThis.createImageBitmap = async (file) => {
  decodes.set(file, (decodes.get(file) ?? 0) + 1);
  const [size, seed] = (await file.text()).split(":");
  const [width, height] = size.split("x").map(Number);
  return new FakeBitmap(width, height, Number(seed));
};
// The import's thumbnail draws the picture once.
globalThis.document = {
  createElement: () => ({
    getContext: () => ({
      drawImage(image) {
        assert.equal(image.closed, false);
      },
    }),
    toDataURL: () => "data:",
  }),
};

const { AssetLibrary } = await import("../dist/browser/runtime/assets.js");
const { SourcePixels } = await import("../dist/browser/runtime/sources.js");

const MiB = 1024 * 1024;
const photo = (width, height, seed, name = `photo-${seed}.jpg`) =>
  new File([`${width}x${height}:${seed}`], name, { type: "image/jpeg" });

/** A title as `AssetLibrary.createText` leaves it: a premultiplied raster at its own size. */
const title = (id, seed) => ({
  id,
  kind: "text",
  fit: "native",
  raster: {
    texture: { width: 4, height: 2 },
    rgba8: new Uint8Array(4 * 2 * 4).fill(seed),
  },
});

/**
 * Imports `count` stills of `width`x`height`, and makes `titles` titles, in one library
 * `SourcePixels` reads.
 */
async function sources({ stills = 0, titles = 0, width = 512, height = 512 }) {
  const library = new AssetLibrary();
  const { imported } = await library.importFiles(
    Array.from({ length: stills }, (_, index) =>
      photo(width, height, (index % 250) + 1),
    ),
  );
  assert.equal(imported.length, stills);
  // Counted from here: the decodes `SourcePixels` makes, not the import's.
  for (const asset of imported) decodes.delete(asset.file);
  const texts = Array.from({ length: titles }, (_, index) =>
    title(`text-${index}`, (index % 250) + 1),
  );
  const lookup = {
    get: (id) => library.get(id) ?? texts.find((text) => text.id === id),
  };
  return {
    library: lookup,
    stills: imported,
    titles: texts,
    live: new Set([...imported, ...texts].map((asset) => asset.id)),
  };
}

/** Reads `assets` as one frame reads them: the engine releases first, then asks for each source. */
async function frame(pixels, live, assets) {
  await pixels.release(live);
  return Promise.all(assets.map((asset) => pixels.pixelsFor(asset, 0)));
}

test("an imported still keeps its file and thumbnail, not its decoded picture", async () => {
  const before = bitmaps.length;
  const library = new AssetLibrary();
  const file = photo(6000, 4000, 7);
  const { imported } = await library.importFiles([file]);
  const [asset] = imported;
  assert.equal(asset.file, file);
  assert.deepEqual([asset.width, asset.height], [6000, 4000]);
  assert.ok(asset.thumbnail);
  assert.equal(asset.bitmap, undefined, "the asset holds the decoded picture");
  const decoded = bitmaps.slice(before);
  assert.equal(decoded.length, 1);
  assert.ok(
    decoded.every((bitmap) => bitmap.closed),
    "the decoded picture is not closed after import",
  );
});

test("the raster budget is the frame supply's image budget unless set", () => {
  assert.equal(new SourcePixels({ get() {} }).rasterBudgetBytes, 256 * MiB);
  assert.equal(
    new SourcePixels({ get() {} }, { rasterBudgetBytes: 8 * MiB })
      .rasterBudgetBytes,
    8 * MiB,
  );
});

test("stills and titles past the budget stay within it, counted in bytes", async () => {
  const budget = 4 * MiB;
  const { library, stills, titles, live } = await sources({
    stills: 80,
    titles: 80,
  });
  const pixels = new SourcePixels(library, { rasterBudgetBytes: budget });
  // A title's raster is the output's size; a still's is its own (512x512, 1 MiB).
  pixels.setOutputSize(640, 360);
  for (let index = 0; index < 80; index += 1) {
    const [still, text] = await frame(pixels, live, [
      stills[index],
      titles[index],
    ]);
    assert.equal(still.pixels.length, 512 * 512 * 4);
    assert.equal(text.length, 640 * 360 * 4);
    const { cachedRasters, cachedRasterBytes } = pixels.diagnostics;
    assert.ok(
      cachedRasterBytes <= budget,
      `frame ${index}: ${cachedRasterBytes} bytes cached over a ${budget} byte budget`,
    );
    // The two the frame reads, and what else fits.
    assert.ok(cachedRasters <= 5, `frame ${index}: ${cachedRasters} rasters`);
  }
});

test("a raster evicted and read again has the same pixels", async () => {
  const { library, stills, titles, live } = await sources({
    stills: 4,
    titles: 4,
  });
  const pixels = new SourcePixels(library, { rasterBudgetBytes: 2 * MiB });
  pixels.setOutputSize(640, 360);
  const [still, text] = await frame(pixels, live, [stills[0], titles[0]]);
  const first = { still: still.pixels.slice(), text: text.slice() };
  for (let index = 1; index < 4; index += 1)
    await frame(pixels, live, [stills[index], titles[index]]);
  assert.equal(decodes.get(stills[0].file), 1);
  const [again, textAgain] = await frame(pixels, live, [stills[0], titles[0]]);
  assert.equal(decodes.get(stills[0].file), 2);
  assert.notEqual(again, still);
  assert.deepEqual(again.pixels, first.still);
  assert.deepEqual([again.width, again.height], [512, 512]);
  assert.notEqual(textAgain, text);
  assert.deepEqual(textAgain, first.text);
  assert.ok(bitmaps.every((bitmap) => bitmap.closed));
});

test("what the frame being drawn reads is not evicted, even past the budget", async () => {
  const { library, stills, live } = await sources({ stills: 3 });
  const pixels = new SourcePixels(library, { rasterBudgetBytes: 1 * MiB });
  const first = await frame(pixels, live, stills);
  assert.deepEqual(
    [pixels.diagnostics.cachedRasters, pixels.diagnostics.cachedRasterBytes],
    [3, 3 * MiB],
  );
  // Read again within the same frame (a second layer of one source): nothing is decoded again.
  for (const [index, still] of stills.entries())
    assert.equal(await pixels.pixelsFor(still, 0), first[index]);
  assert.ok(stills.every((still) => decodes.get(still.file) === 1));
  await frame(pixels, live, [stills[2]]);
  assert.deepEqual(
    [pixels.diagnostics.cachedRasters, pixels.diagnostics.cachedRasterBytes],
    [1, 1 * MiB],
  );
});

test("one still asked for twice at once is decoded once", async () => {
  const { library, stills, live } = await sources({ stills: 1 });
  const pixels = new SourcePixels(library);
  await pixels.release(live);
  const [a, b] = await Promise.all([
    pixels.pixelsFor(stills[0], 0),
    pixels.pixelsFor(stills[0], 0, { consumerId: "other" }),
  ]);
  assert.equal(a, b);
  assert.equal(decodes.get(stills[0].file), 1);
});

test("a relinked still is read from its new file", async () => {
  const { library, stills, live } = await sources({ stills: 1 });
  const pixels = new SourcePixels(library);
  const [before] = await frame(pixels, live, stills);
  const relinked = { ...stills[0], file: photo(512, 512, 99, "moved.jpg") };
  const [after] = await frame(pixels, live, [relinked]);
  assert.notEqual(after, before);
  assert.equal(after.pixels[0], 99);
  assert.equal(pixels.diagnostics.cachedRasters, 1);
});

test("a still whose file cannot be read again fails that frame, and the next read tries again", async () => {
  const { library, stills, live } = await sources({ stills: 1 });
  const pixels = new SourcePixels(library);
  const asset = stills[0];
  const original = globalThis.createImageBitmap;
  globalThis.createImageBitmap = async () => {
    throw new DOMException("the file changed", "NotReadableError");
  };
  try {
    await assert.rejects(frame(pixels, live, [asset]), /the file changed/);
    assert.equal(pixels.diagnostics.cachedRasters, 0);
  } finally {
    globalThis.createImageBitmap = original;
  }
  const [raster] = await frame(pixels, live, [asset]);
  assert.equal(raster.pixels.length, 512 * 512 * 4);
});
