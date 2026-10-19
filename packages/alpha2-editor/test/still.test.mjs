// How the browser runtime reads a still: upright by its Exif, in its stored code values, and at its
// own size, so the engine presents it as it presents a native decode.
import test from "node:test";
import assert from "node:assert/strict";

const decoded = [];

/** A decoded picture whose pixels are its size and the options it was decoded with. */
class FakeBitmap {
  constructor(width, height, options) {
    this.width = width;
    this.height = height;
    this.options = options;
  }
  close() {
    this.closed = true;
  }
}

/** Draws a bitmap at its own size and reads back what was drawn where. */
class FakeCanvas {
  constructor(width, height) {
    this.width = width;
    this.height = height;
    this.draws = [];
  }
  getContext() {
    const canvas = this;
    return {
      drawImage(image, ...rect) {
        canvas.draws.push({ image, rect });
      },
      getImageData(x, y, width, height) {
        assert.deepEqual(canvas.draws.at(-1).rect, [0, 0]);
        return { data: new Uint8ClampedArray(width * height * 4).fill(9) };
      },
      clearRect() {},
    };
  }
}

globalThis.OffscreenCanvas = FakeCanvas;
globalThis.createImageBitmap = async (file, options) => {
  decoded.push(options);
  const [width, height] = (await file.text()).split("x").map(Number);
  return new FakeBitmap(width, height, options);
};
// The import's thumbnail, which this does not look at.
globalThis.document = {
  createElement: () => ({
    getContext: () => ({ drawImage() {} }),
    toDataURL: () => "data:",
  }),
};

const { AssetLibrary } = await import("../dist/browser/runtime/assets.js");
const { SourcePixels } = await import("../dist/browser/runtime/sources.js");

const photo = (width, height) =>
  new File([`${width}x${height}`], "photo.jpg", { type: "image/jpeg" });

test("a still decodes upright and without colour management, at its own size", async () => {
  const library = new AssetLibrary();
  const { imported } = await library.importFiles([photo(48, 64)]);
  assert.equal(imported.length, 1);
  // Exif decides the orientation; the embedded profile does not change the code values.
  assert.deepEqual(decoded.at(-1), {
    imageOrientation: "from-image",
    colorSpaceConversion: "none",
  });
  const pixels = new SourcePixels(library);
  pixels.setOutputSize(1920, 1080);
  const raster = await pixels.pixelsFor(imported[0], 0);
  assert.equal(raster.kind, "raster-frame");
  assert.deepEqual([raster.width, raster.height], [48, 64]);
  assert.equal(raster.alpha, "straight");
  assert.equal(raster.pixels.length, 48 * 64 * 4);
  // The engine scales it into the output, so another output size reads nothing again.
  pixels.setOutputSize(1280, 720);
  assert.equal(await pixels.pixelsFor(imported[0], 0), raster);
});

test("a still larger than the engine decodes is refused at import, as natively", async () => {
  const library = new AssetLibrary();
  for (const [width, height] of [
    [16_385, 2],
    [8_193, 8_192],
  ]) {
    const { imported, rejected } = await library.importFiles([
      photo(width, height),
    ]);
    assert.deepEqual(
      [imported.length, rejected.map((r) => [r.name, r.error.code])],
      [0, [["photo.jpg", "unreadable_media"]]],
    );
  }
});
