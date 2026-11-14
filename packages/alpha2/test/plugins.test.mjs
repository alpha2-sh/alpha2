import test from "node:test";
import assert from "node:assert/strict";

import { decodeRecords, loadPlugin } from "../dist/plugins.js";

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

const DESCRIPTOR = {
  id: "alpha2.test.bounds",
  version: 1,
  input: { media: "video", from: ["source"], format: "rgba8", max_size: 64 },
  params_schema: { type: "object" },
  outputs: [{ name: "levels", kind: "scalars", channels: ["a"] }],
};

/**
 * A plugin whose module is the ABI's exports around `body`: WAT for `begin`, `video` and `end`
 * (each `(result i32)`), with the descriptor at offset 0 and 2 MiB of zeroed memory.
 */
async function hostedPlugin({
  begin = "i32.const 0",
  video = "i32.const 0",
  alloc = "i32.const 65536",
} = {}) {
  const binaryen = (await import("binaryen")).default;
  const json = JSON.stringify(DESCRIPTOR);
  const packed = (BigInt(json.length) << 32n).toString();
  const module = binaryen.parseText(`(module
    (import "alpha2" "fail" (func $fail (param i32 i32)))
    (import "alpha2" "log" (func $log (param i32 i32 i32)))
    (import "alpha2" "emit" (func $emit (param i32 i32) (result i32)))
    (memory (export "memory") 32)
    (data (i32.const 0) "${json.replaceAll('"', "\\22")}")
    (func (export "alpha2_abi") (result i32) i32.const 1)
    (func (export "alpha2_describe") (result i64) i64.const ${packed})
    (func (export "alpha2_alloc") (param i32 i32) (result i32) ${alloc})
    (func (export "alpha2_free") (param i32 i32 i32))
    (func (export "alpha2_begin") (param i32 i32) (result i32) (local $i i32) ${begin})
    (func (export "alpha2_pass") (param i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_video") (param i64 i32 i32 i32 i32 i32) (result i32) (local $i i32) ${video})
    (func (export "alpha2_audio") (param i64 i64 i32 i32 i32 i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_end") (result i32) i32.const 0))`);
  const wasm = module.emitBinary();
  module.dispose();
  const hex = async (bytes) =>
    Buffer.from(await crypto.subtle.digest("SHA-256", bytes)).toString("hex");
  const manifest = new TextEncoder().encode(
    JSON.stringify({
      id: DESCRIPTOR.id,
      version: "0.1.0",
      abi: "alpha2-plugin/1",
      kind: "extractor",
      artifacts: [
        {
          target: "wasm32",
          type: "wasm",
          url: "m.wasm",
          sha256: await hex(wasm),
          bytes: wasm.byteLength,
        },
      ],
      descriptor: DESCRIPTOR,
    }),
  );
  const files = { "manifest.json": manifest, "m.wasm": wasm };
  const fetch = async (url) =>
    new Response(files[new URL(url).pathname.slice(1)]);
  return loadPlugin("https://plugins.test/manifest.json", await hex(manifest), {
    fetch,
  });
}

const CONTEXT = {
  range: { start: 0, end: 1 },
  passes: [],
  from: "source",
  video: null,
  audio: null,
};
const FRAME = {
  time: 0,
  width: 1,
  height: 1,
  format: "rgba8",
  data: new Uint8Array(4),
};

test("a plugin's log lines stop at the run's budget with one notice", async (t) => {
  const warned = t.mock.method(console, "warn", () => {});
  const plugin = await hostedPlugin({
    begin: `(loop $l
      (call $log (i32.const 1) (i32.const 65536) (i32.const 100))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const 100000))))
      i32.const 0`,
  });
  await plugin.extractor().begin(CONTEXT);
  const lines = warned.mock.calls.map((call) => String(call.arguments[0]));
  const text = lines.reduce(
    (sum, line) => sum + line.length - "plugin: ".length,
    0,
  );
  assert.ok(
    text <= 64 * 1024 + 100,
    `${text} characters in ${lines.length} lines`,
  );
  assert.equal(
    lines.filter((line) => line.includes("further lines are dropped")).length,
    1,
  );
  assert.match(lines.at(-1), /further lines are dropped/);
});

test("a plugin's fail message is cut to 4096 bytes before it is decoded", async () => {
  const plugin = await hostedPlugin({
    video: "(call $fail (i32.const 65536) (i32.const 1048576)) i32.const 1",
  });
  const extractor = plugin.extractor();
  await extractor.begin(CONTEXT);
  assert.throws(
    () => extractor.video(FRAME, {}),
    (error) =>
      error.code === "EXTRACTOR_FAILED" && error.message.length === 4096,
  );
});
