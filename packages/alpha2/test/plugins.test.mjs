import test from "node:test";
import assert from "node:assert/strict";

import { decodeRecords } from "../dist/plugins.js";

/** One record: an 8-byte header and `body`. */
function record(tag, output, body) {
  const header = new DataView(new ArrayBuffer(8));
  header.setUint8(0, tag);
  header.setUint16(2, output, true);
  header.setUint32(4, body.byteLength, true);
  return [...new Uint8Array(header.buffer), ...body];
}

function fields(write) {
  const parts = [];
  const add = (size, set) => {
    const view = new DataView(new ArrayBuffer(size));
    set(view);
    parts.push(...new Uint8Array(view.buffer));
  };
  write({
    i64: (value) => add(8, (view) => view.setBigInt64(0, BigInt(value), true)),
    u32: (value) => add(4, (view) => view.setUint32(0, value, true)),
    f32: (value) => add(4, (view) => view.setFloat32(0, value, true)),
    text: (value) => parts.push(...new TextEncoder().encode(value)),
  });
  return new Uint8Array(parts);
}

test("emit records read as docs/plugin-abi.md lays them out", () => {
  const sample = fields((w) => {
    w.i64(7);
    w.u32(2);
    w.u32(0b10);
    w.f32(1);
    w.f32(0);
  });
  const word = fields((w) => {
    w.i64(100);
    w.i64(200);
    w.u32(1);
    w.u32(0xffffffff);
    w.text("a");
  });
  const interval = fields((w) => {
    w.i64(0);
    w.i64(8000);
    w.u32(1);
    w.f32(0.5);
    w.u32(7);
    w.text("silence");
  });
  const batch = new Uint8Array([
    ...record(1, 2, sample),
    ...record(2, 0, word),
    ...record(3, 1, interval),
  ]);
  assert.deepEqual(decodeRecords(batch), [
    { kind: "sample", output: 2, time: 7, values: [1, null] },
    { kind: "word", output: 0, start: 100, end: 200, text: "a" },
    {
      kind: "interval",
      output: 1,
      start: 0,
      end: 8000,
      label: "silence",
      score: 0.5,
    },
  ]);
  assert.throws(
    () => decodeRecords(new Uint8Array([...record(9, 0, new Uint8Array())])),
    { code: "INVALID_EMIT", message: "record 0: unknown record tag" },
  );
  assert.throws(() => decodeRecords(batch.subarray(0, 5)), {
    message: "record 0: the header ends early",
  });
});
