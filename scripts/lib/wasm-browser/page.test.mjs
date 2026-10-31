// The helpers every case page carries, run in Node on fake frames.
import assert from "node:assert/strict";
import test from "node:test";

import { LUMA_AT } from "./page.mjs";

const lumaAt = new Function(`${LUMA_AT}; return lumaAt;`)();

/** A stand-in `VideoFrame` of `format` whose first plane is `bytes` at `stride`. */
function frame(format, bytes, stride) {
  return {
    format,
    allocationSize: () => bytes.length,
    copyTo: async (target) => {
      target.set(bytes);
      return [{ offset: 0, stride }];
    },
  };
}

test("the luma helper reads BT.709 limited-range luma from RGB and YUV layouts", async () => {
  // A 2x1 picture: black, then a mid orange (230, 120, 40).
  const rgba = Uint8Array.of(0, 0, 0, 255, 230, 120, 40, 255);
  const bgra = Uint8Array.of(0, 0, 0, 255, 40, 120, 230, 255);
  // 16 + (65.481 * 230 + 128.553 * 120 + 24.966 * 40) / 255 = 139.47
  const orange = 139;
  for (const format of ["RGBA", "RGBX"])
    assert.equal(await lumaAt(frame(format, rgba, 8), 1, 0), orange, format);
  for (const format of ["BGRA", "BGRX"])
    assert.equal(await lumaAt(frame(format, bgra, 8), 1, 0), orange, format);
  assert.equal(await lumaAt(frame("RGBA", rgba, 8), 0, 0), 16);
  // White is 235 in limited range.
  assert.equal(
    await lumaAt(frame("RGBX", Uint8Array.of(255, 255, 255, 255), 4), 0, 0),
    235,
  );
  // YUV: the first plane's sample, on its own stride.
  const plane = Uint8Array.of(10, 20, 30, 0, 40, 50, 60, 0);
  for (const format of ["I420", "I420A", "NV12", "I444"])
    assert.equal(await lumaAt(frame(format, plane, 4), 2, 1), 60, format);
  await assert.rejects(
    lumaAt(frame("RGBAF16", plane, 4), 0, 0),
    /no luma for a RGBAF16 frame/,
  );
});
