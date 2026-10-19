import assert from "node:assert/strict";
import test from "node:test";

import { copyPlanarYuvFrame, PlanarCopyPool } from "../dist/hdr-frames.js";

function frame({ fail = false } = {}) {
  return {
    format: "I420P10",
    codedWidth: 4,
    codedHeight: 2,
    displayWidth: 4,
    displayHeight: 2,
    timestamp: 0,
    duration: null,
    visibleRect: { x: 0, y: 0, width: 4, height: 2 },
    colorSpace: { transfer: "pq" },
    allocationSize: () => 24,
    async copyTo(buffer) {
      if (fail) throw new Error("copy refused");
      buffer.fill(3);
      return [
        { offset: 0, stride: 8 },
        { offset: 16, stride: 4 },
        { offset: 20, stride: 4 },
      ];
    },
    close() {},
  };
}

test("a copy given back is the next copy's buffer; overlapping copies take their own", async () => {
  const pool = new PlanarCopyPool();
  const first = await copyPlanarYuvFrame(frame(), null, {}, pool);
  assert.equal(first.data.byteLength, 24);
  const overlapping = await copyPlanarYuvFrame(frame(), null, {}, pool);
  assert.notEqual(overlapping.data.buffer, first.data.buffer);
  pool.release(first.data);
  pool.release(overlapping.data);
  const next = await copyPlanarYuvFrame(frame(), null, {}, pool);
  assert.ok(
    next.data.buffer === first.data.buffer ||
      next.data.buffer === overlapping.data.buffer,
  );
  assert.deepEqual([...next.data], new Array(24).fill(3));
});

test("a failed copy gives its buffer back, and the pool keeps the largest it may", async () => {
  const pool = new PlanarCopyPool(1);
  await assert.rejects(
    copyPlanarYuvFrame(frame({ fail: true }), null, {}, pool),
    /copy refused/,
  );
  assert.equal(pool.keptBytes, 24);
  const small = pool.acquire(8);
  assert.equal(
    small.byteLength,
    8,
    "a larger kept buffer serves a smaller copy",
  );
  assert.equal(pool.keptBytes, 0);
  pool.release(small);
  pool.release(pool.acquire(100));
  assert.equal(pool.keptBytes, 100, "past its limit the smallest goes");
  pool.clear();
  assert.equal(pool.keptBytes, 0);
});
