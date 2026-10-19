// Proxies are written to their origin-private file as they are encoded and read back from it, never
// held in memory; without a writable one they stay in memory within a limit.
//
// OPFS is a directory on disk here, so memory measured while a proxy is made is the store's own.
// Fake coders: the decoder makes a blank picture per packet, the encoder a large packet each.
import test from "node:test";
import assert from "node:assert/strict";
import {
  closeSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openAsBlob,
  openSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import v8 from "node:v8";
import { FakeLocks } from "./support/fake-locks.mjs";
import vm from "node:vm";
import {
  ALL_FORMATS,
  BlobSource,
  BufferTarget,
  CustomVideoDecoder,
  CustomVideoEncoder,
  EncodedAudioPacketSource,
  EncodedPacket,
  EncodedVideoPacketSource,
  Input,
  Mp4OutputFormat,
  Output,
  VideoSample,
  registerDecoder,
  registerEncoder,
} from "mediabunny";

v8.setFlagsFromString("--expose-gc");
const gc = vm.runInNewContext("gc");

const MiB = 1024 * 1024;
const WIDTH = 64;
const HEIGHT = 36;
const FRAMES = 480;
/** Each encoded picture: 480 of them make a 120 MiB proxy. */
const PACKET_BYTES = 256 * 1024;
const VP9 = { codec: "vp09.00.31.08", codedWidth: WIDTH, codedHeight: HEIGHT };

/**
 * What the fake encoder saw: pictures encoded, and the most memory held, beyond the start, while it
 * ran.
 */
const encoder = { encoded: 0, peak: 0, base: 0, onEncode: () => {} };
const measure = () => {
  gc();
  encoder.peak = Math.max(
    encoder.peak,
    process.memoryUsage().arrayBuffers - encoder.base,
  );
};

class BlankDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return codec === "vp9";
  }
  init() {}
  decode(packet) {
    this.onSample(
      new VideoSample(new Uint8Array(WIDTH * HEIGHT * 4), {
        format: "RGBX",
        codedWidth: WIDTH,
        codedHeight: HEIGHT,
        timestamp: packet.timestamp,
        duration: packet.duration,
      }),
    );
  }
  flush() {}
  close() {}
}
class LargeEncoder extends CustomVideoEncoder {
  static supports(codec) {
    return codec === "vp9";
  }
  init() {
    this.first = true;
  }
  encode(sample, { keyFrame }) {
    encoder.encoded++;
    if (encoder.encoded % 16 === 0) measure();
    encoder.onEncode();
    this.onPacket(
      new EncodedPacket(
        new Uint8Array(PACKET_BYTES).fill(encoder.encoded & 255),
        keyFrame ? "key" : "delta",
        sample.timestamp,
        sample.duration,
      ),
      this.first ? { decoderConfig: VP9 } : undefined,
    );
    this.first = false;
  }
  flush() {}
  close() {}
}
registerDecoder(BlankDecoder);
registerEncoder(LargeEncoder);

/**
 * A small original: `frames` tiny VP9 packets at 30 fps from `start` seconds, as an MP4; with
 * `opus`, an Opus track beside them, which no decoder here reads (as a browser without an
 * `AudioDecoder` for it).
 */
async function original(
  frames = FRAMES,
  name = "clip.mp4",
  start = 0,
  { opus = false } = {},
) {
  const target = new BufferTarget();
  const output = new Output({
    format: new Mp4OutputFormat({ fastStart: "in-memory" }),
    target,
  });
  const source = new EncodedVideoPacketSource("vp9");
  output.addVideoTrack(source, { frameRate: 30 });
  const sound = opus ? new EncodedAudioPacketSource("opus") : null;
  if (sound) output.addAudioTrack(sound);
  await output.start();
  for (let i = 0; sound && i < frames / 30 / 0.02; i++)
    await sound.add(
      new EncodedPacket(new Uint8Array(8), "key", start + i * 0.02, 0.02),
      i === 0
        ? {
            decoderConfig: {
              codec: "opus",
              numberOfChannels: 2,
              sampleRate: 48_000,
            },
          }
        : undefined,
    );
  for (let i = 0; i < frames; i++)
    await source.add(
      new EncodedPacket(
        new Uint8Array(16).fill(i & 255),
        i % 30 === 0 ? "key" : "delta",
        start + i / 30,
        1 / 30,
      ),
      i === 0 ? { decoderConfig: VP9 } : undefined,
    );
  await output.finalize();
  return new File([target.buffer], name, { type: "video/mp4" });
}

/** Files read from disk, as the origin-private file system hands them out. */
const diskFiles = new WeakSet();
/** Every write a file took: its name, its bytes, and the pictures encoded by then. */
const writes = [];

let swaps = 0;

/**
 * An origin-private directory on disk; `createWritable` writes aside, to a file of its own, and
 * swaps the file in on close, as browsers do. With `move`, a file handle moves its file within the
 * directory, as Chrome's and Firefox's do.
 */
function diskDirectory(path, { writable = true, move = true } = {}) {
  const fileHandle = (name) => {
    let file = join(path, name);
    const handle = {
      kind: "file",
      name,
      async getFile() {
        const read = new File([await openAsBlob(file)], name);
        diskFiles.add(read);
        return read;
      },
    };
    if (writable)
      handle.createWritable = async () => {
        const aside = `${file}.${++swaps}.crswap`;
        const fd = openSync(aside, "w");
        let open = true;
        const finish = () => {
          if (open) closeSync(fd);
          open = false;
        };
        return {
          async write(chunk) {
            let data;
            let position = null;
            if (typeof chunk === "string") data = Buffer.from(chunk);
            else if (chunk?.type === "write") {
              data = chunk.data;
              position = chunk.position;
            } else if (chunk instanceof Blob)
              data = new Uint8Array(await chunk.arrayBuffer());
            else data = chunk;
            writes.push({
              name,
              bytes: data.byteLength,
              encoded: encoder.encoded,
            });
            writeSync(fd, data, 0, data.byteLength, position);
          },
          async close() {
            finish();
            renameSync(aside, file);
          },
          async abort() {
            finish();
            unlinkSync(aside);
          },
        };
      };
    if (move)
      handle.move = async (_directory, to) => {
        renameSync(file, join(path, to));
        file = join(path, to);
      };
    return handle;
  };
  return {
    kind: "directory",
    async getFileHandle(name, { create = false } = {}) {
      const file = join(path, name);
      if (!existsSync(file)) {
        if (!create)
          throw Object.assign(Error(`${name} not found`), {
            name: "NotFoundError",
          });
        closeSync(openSync(file, "w"));
      }
      return fileHandle(name);
    },
    async removeEntry(name) {
      const file = join(path, name);
      if (!existsSync(file))
        throw Object.assign(Error(`${name} not found`), {
          name: "NotFoundError",
        });
      unlinkSync(file);
    },
    async *entries() {
      for (const name of readdirSync(path))
        if (!name.endsWith(".crswap")) yield [name, fileHandle(name)];
    },
  };
}

/** A fresh origin: its private file system on disk, and its locks. */
function origin(options) {
  const path = mkdtempSync(join(tmpdir(), "alpha2-opfs-"));
  const proxies = join(path, "alpha2-proxies");
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: {
      locks: new FakeLocks(),
      storage: {
        async getDirectory() {
          return {
            async getDirectoryHandle(name, { create = false } = {}) {
              assert.equal(name, "alpha2-proxies");
              if (!existsSync(proxies)) {
                if (!create)
                  throw Object.assign(Error("no directory"), {
                    name: "NotFoundError",
                  });
                mkdirSync(proxies);
              }
              return diskDirectory(proxies, options);
            },
          };
        },
      },
    },
  });
  return {
    proxies,
    files: () => (existsSync(proxies) ? readdirSync(proxies).sort() : []),
    close: () => rmSync(path, { recursive: true, force: true }),
  };
}

const hashA = "a".repeat(64);
const hashB = "b".repeat(64);
const made_from = (hash) => ({ content_sha256: hash, byte_length: 10 });

const storeModule = () => import("../dist/browser/runtime/proxies.js");
const storageModule = () => import("../dist/browser/runtime/proxy-storage.js");

test("a proxy is written to its file as it is encoded, and read from that file", async () => {
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    const input = await original();
    writes.length = 0;
    encoder.encoded = 0;
    encoder.peak = 0;
    let written = 0;
    let writtenBeforeLast = 0;
    encoder.onEncode = () => {
      written = writes
        .filter((write) => write.name.startsWith(`${hashA}.`))
        .reduce((sum, write) => sum + write.bytes, 0);
      writtenBeforeLast = written;
    };
    gc();
    encoder.base = process.memoryUsage().arrayBuffers;
    const store = new ProxyStore();
    const found = await store.generate(input, made_from(hashA));
    measure();
    encoder.onEncode = () => {};
    const size = found.file.size;
    assert.ok(
      size > FRAMES * PACKET_BYTES,
      `the proxy holds every picture (${size} bytes)`,
    );
    assert.ok(
      writtenBeforeLast > size / 2,
      `the file had taken ${writtenBeforeLast} of ${size} bytes before the last picture was encoded`,
    );
    const largest = Math.max(
      ...writes
        .filter((write) => write.name.startsWith(`${hashA}.`))
        .map((write) => write.bytes),
    );
    assert.ok(
      largest <= 16 * MiB,
      `no write holds much of the proxy (the largest is ${largest} bytes)`,
    );
    assert.ok(
      encoder.peak < size / 4,
      `making a ${Math.round(size / MiB)} MiB proxy held at most ${Math.round(encoder.peak / MiB)} MiB`,
    );
    assert.ok(diskFiles.has(found.file), "the proxy handed out is its file");
    assert.deepEqual(opfs.files(), [`${hashA}.json`, `${hashA}.mp4`]);
    assert.equal(statSync(join(opfs.proxies, `${hashA}.mp4`)).size, size);
    assert.equal(found.record.width, WIDTH);
    assert.equal(found.record.frame_rate.num, 30);

    // Found again, by this page or another: the file on disk.
    const again = await new ProxyStore().find(hashA);
    assert.ok(diskFiles.has(again.file));
    assert.deepEqual(again.record, found.record);
    // Kept already: made from the same bytes, it is not made again.
    const encoded = encoder.encoded;
    const kept = await store.generate(input, made_from(hashA));
    assert.equal(encoder.encoded, encoded);
    assert.ok(diskFiles.has(kept.file));
  } finally {
    opfs.close();
  }
});

test("a cancelled proxy leaves nothing behind", async () => {
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    const input = await original();
    const controller = new AbortController();
    encoder.encoded = 0;
    encoder.onEncode = () => {
      if (encoder.encoded === 100) controller.abort();
    };
    const store = new ProxyStore();
    await assert.rejects(
      store.generate(input, made_from(hashA), { signal: controller.signal }),
    );
    encoder.onEncode = () => {};
    assert.deepEqual(opfs.files(), []);
    assert.equal(await store.find(hashA), null);
  } finally {
    opfs.close();
  }
});

test("without a writable origin-private file system a proxy is kept in memory, within a limit", async () => {
  const opfs = origin({ writable: false });
  try {
    const { ProxyStore } = await storeModule();
    // Small enough: kept in this page's memory, and nothing on disk.
    const small = new ProxyStore({ memoryLimit: 256 * MiB });
    const found = await small.generate(await original(60), made_from(hashA));
    assert.ok(found.file.size > 60 * PACKET_BYTES);
    assert.ok(!diskFiles.has(found.file));
    assert.equal((await small.find(hashA)).file, found.file);
    assert.deepEqual(opfs.files(), []);
    assert.deepEqual(await small.list(), [
      { hash: hashA, bytes: found.file.size },
    ]);

    // Past a quarter of the limit (the copies an in-memory output can take): refused, nothing kept.
    const limited = new ProxyStore({ memoryLimit: 64 * MiB });
    await assert.rejects(
      limited.generate(await original(), made_from(hashB)),
      (error) => error.code === "resource_limit",
    );
    assert.equal(await limited.find(hashB), null);
  } finally {
    opfs.close();
  }
});

test("kept proxies are listed and removed, except one a page holds in use", async () => {
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    const { keptProxies, removeKeptProxies, holdProxy } = await storageModule();
    const store = new ProxyStore();
    const a = await store.generate(await original(30), made_from(hashA));
    const b = await store.generate(await original(30), made_from(hashB));
    const listed = await keptProxies();
    const bytes = (found, hash) =>
      found.file.size + statSync(join(opfs.proxies, `${hash}.json`)).size;
    assert.deepEqual(
      listed.sort((x, y) => x.hash.localeCompare(y.hash)),
      [
        { hash: hashA, bytes: bytes(a, hashA) },
        { hash: hashB, bytes: bytes(b, hashB) },
      ],
    );

    // Another page reads A: removing both removes B only.
    const held = await holdProxy(hashA);
    assert.deepEqual(await removeKeptProxies([hashA, hashB]), {
      removed: [hashB],
      inUse: [hashA],
    });
    assert.deepEqual(opfs.files(), [`${hashA}.json`, `${hashA}.mp4`]);
    assert.equal(await store.find(hashB), null);
    await held.release();
    assert.deepEqual(await removeKeptProxies([hashA]), {
      removed: [hashA],
      inUse: [],
    });
    assert.deepEqual(opfs.files(), []);
    assert.deepEqual(await keptProxies(), []);
  } finally {
    opfs.close();
  }
});

test("pages making the same proxy take turns, and one that gives up never removes another's", async () => {
  // Two pages of one origin make the same proxy at once; the first keeps and reads it, the second
  // is cancelled. The first page's proxy stays, and reads.
  for (const options of [{}, { move: false }]) {
    const opfs = origin(options);
    try {
      const { ProxyStore } = await storeModule();
      const pageA = new ProxyStore();
      const pageB = new ProxyStore();
      const inputA = await original(120);
      const inputB = await original(480);
      encoder.encoded = 0;
      const cancelB = new AbortController();
      const makingA = pageA.generate(inputA, made_from(hashA));
      const makingB = pageB.generate(inputB, made_from(hashA), {
        signal: cancelB.signal,
      });
      const madeA = await makingA;
      cancelB.abort(new Error("page B gave way"));
      const outcomeB = await makingB.then(
        (found) => (found.file.size === madeA.file.size ? "found A's" : "made"),
        (error) => error.message,
      );
      assert.ok(
        ["found A's", "page B gave way"].includes(outcomeB),
        `page B: ${outcomeB}`,
      );
      // Page A's 120 pictures (and the conversion's last one again); page B's 480 never start.
      assert.ok(
        encoder.encoded <= 121,
        `page B encoded nothing (${encoder.encoded} pictures)`,
      );
      assert.deepEqual(opfs.files(), [`${hashA}.json`, `${hashA}.mp4`]);
      const read = await madeA.file.arrayBuffer();
      assert.equal(read.byteLength, madeA.file.size);
      const found = await new ProxyStore().find(hashA);
      assert.equal(found.file.size, madeA.file.size);
    } finally {
      opfs.close();
    }
  }
});

test("a proxy is written under a name of its own and what a stopped page left is cleared", async () => {
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    // A page stopped mid-make, and one stopped after moving the video in but before writing the
    // record: neither is found, and the next make replaces both.
    mkdirSync(opfs.proxies, { recursive: true });
    writeFileSync(join(opfs.proxies, `${hashA}.0f1e2d.part`), "unfinished");
    writeFileSync(join(opfs.proxies, `${hashA}.mp4`), "unrecorded");
    writeFileSync(join(opfs.proxies, `${hashB}.0f1e2d.part`), "another hash");
    const store = new ProxyStore();
    assert.equal(await store.find(hashA), null);
    const names = [];
    writes.length = 0;
    const made = await store.generate(await original(30), made_from(hashA));
    for (const write of writes) names.push(write.name);
    assert.ok(
      names.some((name) => /^a{64}\.[0-9a-f-]+\.part$/.test(name)),
      `the video was written aside (${[...new Set(names)]})`,
    );
    assert.ok(!names.includes(`${hashA}.mp4`));
    assert.deepEqual(opfs.files(), [
      `${hashA}.json`,
      `${hashA}.mp4`,
      `${hashB}.0f1e2d.part`,
    ]);
    assert.equal((await store.find(hashA)).file.size, made.file.size);
    assert.ok(made.file.size > 30 * PACKET_BYTES);
  } finally {
    opfs.close();
  }
});

const wasmDirectory = join(
  resolve(import.meta.dirname, "..", ".."),
  "alpha2",
  "wasm",
);
const skip =
  !existsSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")) &&
  process.env.ALPHA2_REQUIRE_WASM !== "1" &&
  "the wasm package is not built";

test(
  "the bridge holds the proxies it reads, lists them, and lets them go to remove them",
  { skip },
  async () => {
    const opfs = origin();
    try {
      const wasm = await import(
        pathToFileURL(join(wasmDirectory, "alpha2_wasm.js")).href
      );
      wasm.initSync({
        module: readFileSync(join(wasmDirectory, "alpha2_wasm_bg.wasm")),
      });
      const { createEditor } = await import("../dist/index.js");
      const { createBrowserBridge } = await import("../dist/browser/index.js");
      const { removeKeptProxies } = await storageModule();
      const editor = createEditor();
      const staged = [];
      const bridge = createBrowserBridge({
        editor,
        audio: false,
        assetDecisions: wasm,
        mediaAdapter: {
          async stage(file) {
            const entry = { file, disposed: false };
            staged.push(entry);
            return {
              runtime: { id: file.name, kind: "video", file, width: 64 },
              description: {},
              fingerprint: file.fingerprint ?? {},
              dispose() {
                entry.disposed = true;
              },
            };
          },
        },
      });
      const clip = await original(30);
      clip.fingerprint = made_from(hashA);
      const { assetIds } = await bridge.importFiles([clip]);
      await bridge.generateProxy(assetIds[0]);
      assert.equal(bridge.proxyStatus(assetIds[0]).available, true);
      const proxy = staged.find((entry) => diskFiles.has(entry.file));
      assert.ok(proxy, "the bridge reads the proxy from its file");

      assert.deepEqual(
        (await bridge.listProxies()).map(({ hash }) => hash),
        [hashA],
      );
      assert.deepEqual(await removeKeptProxies([hashA]), {
        removed: [],
        inUse: [hashA],
      });
      assert.deepEqual(await bridge.removeProxies([hashA]), {
        removed: [hashA],
        inUse: [],
      });
      assert.equal(proxy.disposed, true);
      assert.equal(bridge.proxyStatus(assetIds[0]).available, false);
      assert.deepEqual(opfs.files(), []);
      assert.deepEqual(await bridge.listProxies(), []);
      await bridge.dispose();
      editor.dispose();
    } finally {
      opfs.close();
    }
  },
);

test("a proxy keeps its original's timestamps, so it is read for the original's source times", async () => {
  // First frame at 1.5 s: a proxy starting at zero would show each picture 1.5 s early.
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    const { probeEvidence } =
      await import("../dist/browser/runtime/fingerprint.js");
    const late = await original(60, "late.mp4", 1.5);
    const recorded = await probeEvidence(late);
    assert.equal(recorded.duration_ticks, 3.5 * 120_000);
    const { record, file } = await new ProxyStore().generate(
      late,
      made_from(hashA),
    );
    assert.equal(record.duration_ticks, recorded.duration_ticks);
    const input = new Input({
      source: new BlobSource(file),
      formats: ALL_FORMATS,
    });
    try {
      assert.equal(await input.getFirstTimestamp(), 1.5);
    } finally {
      input.dispose();
    }
  } finally {
    opfs.close();
  }
});

test("a proxy that would lack audio its original records is refused before it is converted", async () => {
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    const { probeEvidence } =
      await import("../dist/browser/runtime/fingerprint.js");
    const clip = await original(60, "voice.mp4", 0, { opus: true });
    const recorded = await probeEvidence(clip);
    assert.ok(recorded.streams.audio, "the fingerprint records the audio");
    encoder.encoded = 0;
    await assert.rejects(
      new ProxyStore().generate(clip, { ...made_from(hashA), ...recorded }),
      (error) =>
        error.code === "unsupported_media" &&
        /cannot convert its audio \(undecodable_source_codec\)/.test(
          error.message,
        ),
    );
    // Setting the conversion up may encode one picture to configure the encoder; it never ran.
    assert.ok(encoder.encoded <= 1, `${encoder.encoded} pictures encoded`);
    assert.deepEqual(opfs.files(), []);
    // Without audio in the fingerprint (a host that never heard it), the video alone is the proxy.
    const found = await new ProxyStore().generate(clip, made_from(hashB));
    assert.equal(found.record.audio_sample_rate, undefined);
  } finally {
    opfs.close();
  }
});

test("a proxy cancelled while it is set up is never converted, and a cancel rejects with its reason", async () => {
  const opfs = origin();
  try {
    const { ProxyStore } = await storeModule();
    const input = await original(120);
    // Aborted after generate starts, during its first await: before the conversion exists.
    const early = new AbortController();
    encoder.encoded = 0;
    const pending = new ProxyStore().generate(input, made_from(hashA), {
      signal: early.signal,
    });
    early.abort(new Error("panel closed"));
    await assert.rejects(pending, { message: "panel closed" });
    assert.equal(encoder.encoded, 0, "nothing was encoded");
    assert.deepEqual(opfs.files(), []);
    // Aborted while it encodes: the caller's reason, not the conversion's own error.
    const late = new AbortController();
    encoder.onEncode = () => {
      if (encoder.encoded === 30) late.abort(new Error("export started"));
    };
    await assert.rejects(
      new ProxyStore().generate(input, made_from(hashA), {
        signal: late.signal,
      }),
      { message: "export started" },
    );
    encoder.onEncode = () => {};
    assert.deepEqual(opfs.files(), []);
  } finally {
    encoder.onEncode = () => {};
    opfs.close();
  }
});
