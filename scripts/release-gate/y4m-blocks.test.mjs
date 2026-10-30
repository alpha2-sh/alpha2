import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { compareRun, main, parseY4m } from "./y4m-blocks.mjs";

const WIDTH = 32;
const HEIGHT = 16;

/** A 4:2:0 y4m whose luma is `luma(frame, x, y)` and chroma is 128. */
function y4m(frameCount, luma, colorspace = "420jpeg") {
  const parts = [
    Buffer.from(
      `YUV4MPEG2 W${WIDTH} H${HEIGHT} F30:1 Ip A1:1 C${colorspace}\n`,
    ),
  ];
  for (let frame = 0; frame < frameCount; frame++) {
    parts.push(Buffer.from("FRAME\n"));
    const plane = Buffer.alloc(WIDTH * HEIGHT);
    for (let y = 0; y < HEIGHT; y++)
      for (let x = 0; x < WIDTH; x++) plane[y * WIDTH + x] = luma(frame, x, y);
    parts.push(plane, Buffer.alloc((WIDTH / 2) * (HEIGHT / 2) * 2, 128));
  }
  return Buffer.concat(parts);
}

const gradient = (frame, x, y) => (16 + x * 3 + y * 5 + frame) % 200;

test("a clean run against its reference reports corrupt 0", () => {
  const reference = parseY4m(y4m(4, gradient));
  // Rounding differences within the tolerance are not corruption.
  const run = parseY4m(
    y4m(4, (frame, x, y) => gradient(frame, x, y) + ((x + y) % 2 ? 4 : 0)),
  );
  const result = compareRun(reference, run);
  assert.equal(result.corrupt.length, 0);
  assert.deepEqual(result.firstClean, [0, 1, 2, 3]);

  const dir = mkdtempSync(join(tmpdir(), "y4m-blocks-"));
  writeFileSync(join(dir, "ref.y4m"), y4m(4, gradient));
  writeFileSync(join(dir, "run.y4m"), y4m(4, gradient));
  const lines = [];
  assert.equal(
    main([join(dir, "ref.y4m"), join(dir, "run.y4m")], (line) =>
      lines.push(line),
    ),
    0,
  );
  assert.match(lines[0], /corrupt 0,/);
});

test("an 8x8 white block in frame 3 is one region of 8x8 at origin 0 mod 8", () => {
  const reference = parseY4m(y4m(5, gradient));
  const run = parseY4m(
    y4m(5, (frame, x, y) =>
      frame === 3 && x >= 16 && x < 24 && y >= 8 && y < 16
        ? 235
        : gradient(frame, x, y),
    ),
  );
  const result = compareRun(reference, run);
  assert.equal(result.corrupt.length, 1);
  assert.equal(result.corrupt[0].frame, 3);
  assert.equal(result.corrupt[0].regions, 1);
  assert.deepEqual(result.sizes, [["8x8", 1]]);
  assert.deepEqual(result.origins, [["0,0", 1]]);
  assert.deepEqual(result.values, [[235, 64]]);
  assert.deepEqual(result.firstClean, [0, 1, 2, 4]);
});

test("a 10-bit y4m is refused", () => {
  assert.throws(() => parseY4m(y4m(1, gradient, "420p10")), /high-bit-depth/);
});

test("a run of another length is reported, not compared", () => {
  const result = compareRun(
    parseY4m(y4m(3, gradient)),
    parseY4m(y4m(2, gradient)),
  );
  assert.match(result.mismatch, /frame count differs/);
});
