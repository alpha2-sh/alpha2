import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";

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
 * A plugin whose module is the ABI's exports with WAT bodies for `begin`, `video`, `end` (each
 * `(result i32)`), `alloc` and `describe`, the descriptor at offset 0 and 8 MiB of zeroed memory.
 */
async function hostedPlugin({
  begin = "i32.const 0",
  video = "i32.const 0",
  end = "i32.const 0",
  alloc = "i32.const 65536",
  describe = null,
  resources = undefined,
  about = undefined,
  serve = (body) => new Response(body),
  options = {},
  fields = "",
} = {}) {
  const binaryen = (await import("binaryen")).default;
  const json = JSON.stringify(DESCRIPTOR);
  const packed = (BigInt(json.length) << 32n).toString();
  const module = binaryen.parseText(`(module
    (import "alpha2" "fail" (func $fail (param i32 i32)))
    (import "alpha2" "log" (func $log (param i32 i32 i32)))
    (import "alpha2" "emit" (func $emit (param i32 i32) (result i32)))
    (import "alpha2" "resource_size" (func $size (param i32 i32) (result i64)))
    (import "alpha2" "resource_read" (func $read (param i32 i32 i32 i32) (result i32)))
    (memory (export "memory") 128)
    ${fields}
    (data (i32.const 0) "${json.replaceAll('"', "\\22")}")
    (func (export "alpha2_abi") (result i32) i32.const 1)
    (func (export "alpha2_describe") (result i64) ${describe ?? `i64.const ${packed}`})
    (func (export "alpha2_alloc") (param i32 i32) (result i32) ${alloc})
    (func (export "alpha2_free") (param i32 i32 i32))
    (func (export "alpha2_begin") (param i32 i32) (result i32) (local $i i32) ${begin})
    (func (export "alpha2_pass") (param i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_video") (param i64 i32 i32 i32 i32 i32) (result i32) (local $i i32) ${video})
    (func (export "alpha2_audio") (param i64 i64 i32 i32 i32 i32 i32) (result i32) i32.const 0)
    (func (export "alpha2_end") (result i32) (local $i i32) (local $at i32) ${end}))`);
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
      resources,
      about,
      descriptor: DESCRIPTOR,
    }),
  );
  const files = { "manifest.json": manifest, "m.wasm": wasm };
  const fetch = async (url) => {
    const name = new URL(url).pathname.slice(1);
    return name === "manifest.json"
      ? new Response(files[name])
      : serve(files[name]);
  };
  const loaded = await loadPlugin(
    "https://plugins.test/manifest.json",
    await hex(manifest),
    { fetch, ...options },
  );
  assert.equal(loaded.manifestSha256, await hex(manifest));
  return loaded;
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

test("a batch of 150,000 samples is taken whole, past the engine's argument limit", async () => {
  const count = 150_000;
  // Each record: tag 1, output 0, a 20-byte body of time (i64), one channel, no nulls, value.
  const plugin = await hostedPlugin({
    end: `(loop $l
      (local.set $at (i32.add (i32.const 65536) (i32.mul (local.get $i) (i32.const 28))))
      (i32.store (local.get $at) (i32.const 1))
      (i32.store (i32.add (local.get $at) (i32.const 4)) (i32.const 20))
      (i64.store (i32.add (local.get $at) (i32.const 8)) (i64.extend_i32_u (local.get $i)))
      (i32.store (i32.add (local.get $at) (i32.const 16)) (i32.const 1))
      (i32.store (i32.add (local.get $at) (i32.const 20)) (i32.const 0))
      (f32.store (i32.add (local.get $at) (i32.const 24)) (f32.const 0.5))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const ${count}))))
      (drop (call $emit (i32.const 65536) (i32.const ${count * 28})))
      i32.const 0`,
  });
  const extractor = plugin.extractor();
  await extractor.begin(CONTEXT);
  let samples = 0;
  extractor.end({
    sample: () => {
      samples += 1;
    },
  });
  assert.equal(samples, count);
});

test("a trap in describe or in the allocation is a PluginError", async () => {
  await assert.rejects(
    hostedPlugin({ describe: "unreachable" }),
    (error) => error.code === "PLUGIN_DESCRIPTOR_MISMATCH",
  );
  const trapping = await hostedPlugin({ alloc: "unreachable" });
  const extractor = trapping.extractor();
  await assert.rejects(
    extractor.begin(CONTEXT),
    (error) => error.code === "PLUGIN_TRAPPED" && /trapped/.test(error.message),
  );
  const outside = await hostedPlugin({ alloc: "i32.const -16" });
  await assert.rejects(
    outside.extractor().begin(CONTEXT),
    (error) => error.code === "PLUGIN_TRAPPED",
  );
});

const SLOTS = [
  { name: "model", supplied: { shape: "file", max_bytes: 8 } },
  { name: "vocab", supplied: { shape: "directory", required: false } },
];

/** A plugin whose `begin` fails with its context and whose `video` reads resource `name`. */
function readingPlugin(name, supplied, resources = SLOTS) {
  const bytes = new TextEncoder().encode(name);
  return hostedPlugin({
    resources,
    options: { supplied },
    fields: `(data (i32.const 60000) "${name}")`,
    begin: "local.get 0 local.get 1 call $fail i32.const 1",
    video: `(i32.wrap_i64 (call $size (i32.const 60000) (i32.const ${bytes.length})))
      local.set $i
      (call $read (i32.const 60000) (i32.const ${bytes.length}) (i32.const 131072) (local.get $i))`,
  });
}

const picked = (path, text) =>
  Object.defineProperty(
    new File([text], path.split("/").at(-1)),
    "webkitRelativePath",
    {
      value: path,
    },
  );

async function contextOf(plugin) {
  const error = await plugin
    .extractor()
    .begin(CONTEXT)
    .then(
      () => assert.fail("begin fails with its context"),
      (caught) => caught,
    );
  return JSON.parse(error.message);
}

test("supplied files fill a plugin's slots as the native loaders fill them", async () => {
  const sha = (text) => createHash("sha256").update(text).digest("hex");
  const model = new File(["weights"], "model.onnx");
  const plugin = await readingPlugin("model", {
    model,
    vocab: [
      picked("pick/b.txt", "bb"),
      picked("pick/sub/a.txt", "a"),
      picked("pick/.hidden/c.txt", "c"),
      picked("pick/.d.txt", "d"),
    ],
  });
  assert.deepEqual((await contextOf(plugin)).supplied, {
    model: { shape: "file", files: [{ name: "model", bytes: 7 }] },
    vocab: {
      shape: "directory",
      files: [
        { name: "vocab/b.txt", bytes: 2 },
        { name: "vocab/sub/a.txt", bytes: 1 },
      ],
    },
  });
  const reads = await readingPlugin("vocab/sub/a.txt", {
    model,
    vocab: [picked("pick/sub/a.txt", "a")],
  });
  const extractor = reads.extractor();
  await extractor.begin(CONTEXT).catch(() => {});
  extractor.video(FRAME, {});
  assert.deepEqual(extractor.madeByPlugin().supplied, [
    { name: "vocab/sub/a.txt", sha256: sha("a"), bytes: 1 },
  ]);
  assert.equal(extractor.madeByPlugin().manifest_sha256, reads.manifestSha256);
});

test("a directory handle fills a directory slot, walked in name order", async () => {
  const file = (text) => ({
    kind: "file",
    getFile: async () => new File([text], "f"),
  });
  const directory = (entries) => ({
    kind: "directory",
    async *entries() {
      yield* Object.entries(entries);
    },
  });
  const plugin = await readingPlugin("model", {
    model: new File(["w"], "m"),
    vocab: directory({
      z: file("zz"),
      a: directory({ b: file("b") }),
      ".git": file("x"),
    }),
  });
  assert.deepEqual((await contextOf(plugin)).supplied.vocab.files, [
    { name: "vocab/a/b", bytes: 1 },
    { name: "vocab/z", bytes: 2 },
  ]);
});

test("supplied files that do not fit a slot are refused with the native loaders' reasons", async () => {
  const model = new File(["weights"], "model.onnx");
  const refused = async (supplied, reason, slot = "model") =>
    assert.rejects(readingPlugin("model", supplied), (error) => {
      assert.equal(error.code, "PLUGIN_RESOURCE_INVALID", error.message);
      assert.deepEqual(error.details, { slot, reason });
      return true;
    });
  await refused({}, "missing");
  await refused({ model, other: model }, "unknown_slot", "other");
  await refused({ model: [picked("pick/a", "a")] }, "shape");
  await refused({ model, vocab: model }, "shape", "vocab");
  await refused({ model: new File(["123456789"], "m") }, "too_large");
  const deep = `pick/${"d/".repeat(9)}f`;
  await refused({ model, vocab: [picked(deep, "x")] }, "too_deep", "vocab");
  const long = `pick/${"n".repeat(512)}`;
  await refused(
    { model, vocab: [picked(long, "x")] },
    "name_too_long",
    "vocab",
  );
  const many = Array.from({ length: 1025 }, (_, index) =>
    picked(`pick/${index}`, ""),
  );
  await refused({ model, vocab: many }, "too_many_files", "vocab");
});

test("a supplied file past its slot's max_bytes or unreadable fails the call that reads it", async () => {
  const slots = [
    { name: "vocab", supplied: { shape: "directory", max_bytes: 2 } },
  ];
  const plugin = await readingPlugin(
    "vocab/big",
    { vocab: [picked("pick/big", "big")] },
    slots,
  );
  const extractor = plugin.extractor();
  await extractor.begin(CONTEXT).catch(() => {});
  assert.throws(
    () => extractor.video(FRAME, {}),
    (error) => {
      assert.equal(error.code, "PLUGIN_RESOURCE_INVALID");
      assert.deepEqual(error.details, { slot: "vocab", reason: "too_large" });
      return true;
    },
  );
  const unreadable = new File(["x"], "m");
  unreadable.arrayBuffer = async () => {
    throw new DOMException("the file changed", "NotReadableError");
  };
  const failing = await readingPlugin("model", { model: unreadable });
  await assert.rejects(failing.extractor().begin(CONTEXT), {
    code: "PLUGIN_FETCH_FAILED",
  });
});

test("loadPlugin reports each pinned file as it arrives and gives the manifest's about", async () => {
  const progress = [];
  const chunked = (body) =>
    new Response(
      new ReadableStream({
        start(controller) {
          for (let at = 0; at < body.byteLength; at += 64)
            controller.enqueue(body.slice(at, at + 64));
          controller.close();
        },
      }),
    );
  const plugin = await hostedPlugin({
    about: { name: "Bounds", license: "MIT" },
    serve: chunked,
    options: { onProgress: (report) => progress.push(report) },
  });
  assert.deepEqual(plugin.about, { name: "Bounds", license: "MIT" });
  assert.ok(progress.length > 1, `${progress.length} reports`);
  assert.ok(
    progress.every(
      (report) => report.phase === "load" && report.file === "m.wasm",
    ),
  );
  const last = progress.at(-1);
  assert.equal(last.loaded, last.total);
  assert.ok(
    progress.every(
      (report, index) =>
        index === 0 || report.loaded > progress[index - 1].loaded,
    ),
  );
});

test("loadPlugin refuses a file once more bytes arrive than the manifest pins", async () => {
  let sent = 0;
  const endless = () =>
    new Response(
      new ReadableStream({
        pull(controller) {
          sent += 1024;
          controller.enqueue(new Uint8Array(1024));
        },
      }),
    );
  await assert.rejects(hostedPlugin({ serve: endless }), {
    code: "PLUGIN_HASH_MISMATCH",
    message: /m\.wasm is \d+ bytes or more/,
  });
  assert.ok(sent < 1024 * 1024, `${sent} bytes sent`);
});

test("loadPlugin checks the manifest before it fetches a file", async () => {
  let fetched = 0;
  await assert.rejects(
    hostedPlugin({
      about: { name: "Bounds\u0007" },
      serve: (body) => {
        fetched += 1;
        return new Response(body);
      },
    }),
    { code: "PLUGIN_LOAD_FAILED", message: /about\.name/ },
  );
  assert.equal(fetched, 0);
});
