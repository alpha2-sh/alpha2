// A raster provider (OpenEXR, an animated GIF, WebP or APNG) answers at its own size whatever it is
// asked to fit. The page's canvas and file reader are recorded here.
import test from "node:test";
import assert from "node:assert/strict";

const drawn = [];
globalThis.ImageData = class {
  constructor(data, width, height) {
    Object.assign(this, { data, width, height });
  }
};
globalThis.OffscreenCanvas = class {
  constructor(width, height) {
    Object.assign(this, { width, height });
  }
  getContext() {
    return {
      putImageData: (image) => drawn.push(image),
    };
  }
  async convertToBlob() {
    return new Blob([new Uint8Array(this.width * this.height)]);
  }
};
globalThis.FileReader = class {
  readAsDataURL(blob) {
    queueMicrotask(() => {
      this.result = `data:image/png;size=${blob.size}`;
      this.onload();
    });
  }
};

const { thumbnailOf } =
  await import("../dist/browser/runtime/extension-import.js");

/** A half float's bits for `value` in [0, 1]: exact for 0, 0.5 and 1. */
const half = (value) => ({ 0: 0, 0.5: 0x3800, 1: 0x3c00 })[value];

/**
 * A `width`x`height` frame whose left half is opaque red and right half opaque blue, as RGBA8 or
 * as linear half floats.
 */
function frame(width, height, precision) {
  const linear = precision === "linear_f16";
  const pixels = linear
    ? new Uint16Array(width * height * 4)
    : new Uint8Array(width * height * 4);
  const one = linear ? half(1) : 255;
  for (let y = 0; y < height; y += 1)
    for (let x = 0; x < width; x += 1) {
      const at = (y * width + x) * 4;
      pixels[at + (x < width / 2 ? 0 : 2)] = one;
      pixels[at + 3] = one;
    }
  return { width, height, precision, pixels };
}

test("a raster provider's thumbnail is sampled down to 160 pixels wide before it is converted", async () => {
  for (const precision of ["rgba8", "linear_f16"]) {
    drawn.length = 0;
    const thumbnail = await thumbnailOf(frame(4096, 2160, precision));
    assert.deepEqual(
      [thumbnail.width, thumbnail.height],
      [160, 84],
      `${precision}: the thumbnail's size`,
    );
    assert.deepEqual([drawn[0].width, drawn[0].height], [160, 84]);
    assert.equal(drawn[0].data.length, 160 * 84 * 4);
    const pixel = (x, y) => [
      ...drawn[0].data.subarray((y * 160 + x) * 4, (y * 160 + x) * 4 + 4),
    ];
    assert.deepEqual(pixel(10, 40), [255, 0, 0, 255], precision);
    assert.deepEqual(pixel(150, 40), [0, 0, 255, 255], precision);
    assert.match(thumbnail.dataUrl, new RegExp(`size=${160 * 84}$`));
  }
});

test("a frame smaller than a thumbnail keeps its size", async () => {
  drawn.length = 0;
  const thumbnail = await thumbnailOf(frame(64, 36, "rgba8"));
  assert.deepEqual([thumbnail.width, thumbnail.height], [64, 36]);
  assert.deepEqual([drawn[0].width, drawn[0].height], [64, 36]);
});
