// What a browser export may write and with which audio codec, as WebKit behaves. Safari before 26
// has no AudioEncoder; Safari 26's storage estimate never drops after a delete, so `quota - usage`
// can be near zero with the disk far from full.
import test from "node:test";
import assert from "node:assert/strict";

import { isRetryableFailure } from "alpha2/export-retry";

const { openExportOutput, sweepStaleExports } =
  await import("../dist/browser/runtime/output.js");
import { FakeLocks } from "./support/fake-locks.mjs";
const { chooseAudioCodec, exportTimeline } =
  await import("../dist/browser/runtime/exporter.js");

const GB = 1e9;

/**
 * An origin-private directory in memory; `createWritable` writes aside and swaps the file in on
 * close, as browsers do. `refuseWrites` makes every write fail as a full origin does.
 */
function memoryDirectory({ refuseWrites = false } = {}) {
  const files = new Map();
  const handle = (name) => ({
    kind: "file",
    name,
    async getFile() {
      return new File([files.get(name) ?? new Uint8Array()], name);
    },
    async createWritable() {
      let aside = new Uint8Array();
      return {
        async write(chunk) {
          if (refuseWrites)
            throw new DOMException(
              "The quota has been exceeded.",
              "QuotaExceededError",
            );
          const end = chunk.position + chunk.data.byteLength;
          if (end > aside.byteLength) {
            const grown = new Uint8Array(end);
            grown.set(aside);
            aside = grown;
          }
          aside.set(chunk.data, chunk.position);
        },
        async close() {
          files.set(name, aside);
        },
        async abort() {},
      };
    },
  });
  return {
    files,
    async getFileHandle(name, { create = false } = {}) {
      if (!files.has(name)) {
        if (!create)
          throw new DOMException(`${name} not found`, "NotFoundError");
        files.set(name, new Uint8Array());
      }
      return handle(name);
    },
    async removeEntry(name) {
      if (!files.delete(name))
        throw new DOMException(`${name} not found`, "NotFoundError");
    },
    async *entries() {
      for (const name of [...files.keys()]) yield [name, handle(name)];
    },
  };
}

/**
 * A page whose origin-private exports directory is `exports`, and whose storage estimate is
 * `estimate`.
 */
function page(exports, estimate, locks) {
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: {
      ...(locks ? { locks } : {}),
      storage: {
        async estimate() {
          return estimate;
        },
        async getDirectory() {
          return {
            async getDirectoryHandle(name) {
              assert.equal(name, "alpha2-exports");
              return exports;
            },
          };
        },
      },
    },
  });
}

/** Ten minutes at 20 Mbit/s: about 1.5 GB, 2.25 GB with the margin. */
const LONG_EXPORT = {
  jobId: "job",
  name: "Project.mp4",
  durationSeconds: 600,
  videoBitrate: 20_000_000,
  audioBitrate: 0,
};

/**
 * WebKit after some writes: the quota is the origin's 60 % of a 256 GB disk, and the usage counts
 * every write granted since the last scan.
 */
const WEBKIT_INFLATED = { quota: 150 * GB, usage: 149 * GB };

test("a storage estimate that says the origin is nearly full does not refuse an export", async () => {
  const exports = memoryDirectory();
  page(exports, WEBKIT_INFLATED);
  const output = await openExportOutput(LONG_EXPORT);
  assert.equal(output.mode, "opfs");
  // The estimate is reported, not obeyed.
  assert.deepEqual(output.storageEstimate, {
    quotaBytes: WEBKIT_INFLATED.quota,
    usageBytes: WEBKIT_INFLATED.usage,
    neededBytes: Math.ceil(output.estimatedBytes * 1.5),
    belowEstimate: true,
  });
  const writer = output.destination.writable.getWriter();
  await writer.write({
    type: "write",
    position: 0,
    data: new Uint8Array([1, 2, 3]),
  });
  await writer.close();
  const artifact = await output.finish({ kind: "stream", bytes: 3 });
  assert.equal(artifact.kind, "file");
  assert.equal(artifact.file.size, 3);
  await artifact.release();
  assert.equal(exports.files.size, 0);
});

test("an estimate with room is reported as such, and a page without one reports none", async () => {
  page(memoryDirectory(), { quota: 150 * GB, usage: 1 * GB });
  const roomy = await openExportOutput(LONG_EXPORT);
  assert.equal(roomy.storageEstimate.belowEstimate, false);
  await roomy.abort(new Error("done"));

  page(memoryDirectory(), {});
  const unknown = await openExportOutput(LONG_EXPORT);
  assert.equal(unknown.mode, "opfs");
  assert.equal(unknown.storageEstimate, null);
  await unknown.abort(new Error("done"));
});

test("a write the storage refuses fails the export as STORAGE_QUOTA_EXCEEDED, and its file is removed", async () => {
  const exports = memoryDirectory({ refuseWrites: true });
  page(exports, WEBKIT_INFLATED);
  const output = await openExportOutput(LONG_EXPORT);
  assert.equal(exports.files.size, 1);
  const writer = output.destination.writable.getWriter();
  const refused = await writer
    .write({ type: "write", position: 0, data: new Uint8Array(16) })
    .then(
      () => null,
      (error) => error,
    );
  assert.equal(refused?.failure?.code, "STORAGE_QUOTA_EXCEEDED");
  assert.equal(refused.failure.phase, "publish");
  assert.equal(isRetryableFailure(refused.failure), false);
  await output.abort(refused);
  assert.equal(exports.files.size, 0);
});

/**
 * A page whose `AudioEncoder` takes `codecs` (each a codec string prefix, or `[prefix, rates]` for
 * one that takes only those sample rates), or that has none for `null`.
 */
function audioEncoder(codecs) {
  if (codecs === null) {
    delete globalThis.AudioEncoder;
    return;
  }
  globalThis.AudioEncoder = class {
    static async isConfigSupported(config) {
      return {
        supported: codecs.some((entry) => {
          const [codec, rates] = Array.isArray(entry) ? entry : [entry];
          return (
            config.codec.startsWith(codec) &&
            (rates === undefined || rates.includes(config.sampleRate))
          );
        }),
        config,
      };
    }
  };
}

// mediabunny caches each support answer by configuration, bitrate included, so each call asks at a
// bitrate of its own.
let nextBitrate = 128_000;
const freshBitrate = () => ({ bitrate: nextBitrate++ });

test("AAC is chosen where the browser encodes it, Opus where it encodes only Opus", async () => {
  try {
    audioEncoder(["mp4a", "opus"]);
    assert.deepEqual(await chooseAudioCodec(48_000, freshBitrate()), {
      codec: "aac",
      sampleRate: 48_000,
    });
    // Chrome on Linux and Firefox: no AAC encoder.
    audioEncoder(["opus"]);
    assert.deepEqual(await chooseAudioCodec(44_100, freshBitrate()), {
      codec: "opus",
      sampleRate: 44_100,
    });
  } finally {
    audioEncoder(null);
  }
});

test("a project rate the browser's encoder does not take is exported at the rate a native host would pick", async () => {
  try {
    // Chrome on macOS: AAC at 44.1 and 48 kHz only. The mix is converted to the next rate up, or
    // to the highest below a rate above them all.
    audioEncoder([["mp4a", [44_100, 48_000]]]);
    assert.deepEqual(await chooseAudioCodec(22_051, freshBitrate()), {
      codec: "aac",
      sampleRate: 44_100,
    });
    assert.deepEqual(await chooseAudioCodec(96_001, freshBitrate()), {
      codec: "aac",
      sampleRate: 48_000,
    });
    // An Opus encoder that takes only Opus's own rates.
    audioEncoder([["opus", [8_000, 12_000, 16_000, 24_000, 48_000]]]);
    assert.deepEqual(await chooseAudioCodec(22_052, freshBitrate()), {
      codec: "opus",
      sampleRate: 24_000,
    });
  } finally {
    audioEncoder(null);
  }
});

test("an export with audio is refused before it renders where the browser can encode no audio", async () => {
  for (const [encoder, sampleRate] of [
    // Safari 17 and 18: VideoEncoder, no AudioEncoder.
    [null, 32_000],
    // An encoder that takes neither codec at this configuration.
    [[], 24_000],
  ]) {
    audioEncoder(encoder);
    try {
      await assert.rejects(
        chooseAudioCodec(sampleRate, freshBitrate()),
        (error) =>
          error.failure?.code === "INVALID_INPUT" &&
          error.failure.phase === "prepare" &&
          !isRetryableFailure(error.failure) &&
          /cannot encode audio/.test(error.failure.message),
      );
      // The export itself: refused before its output is opened, so nothing renders or is written.
      let opened = false;
      await assert.rejects(
        exportTimeline({
          engine: {},
          project: { sources: [{ id: "voice", kind: "audio" }] },
          doc: {
            settings: {
              width: 64,
              height: 36,
              fpsNum: 30,
              fpsDen: 1,
              sampleRate: sampleRate + 1,
            },
          },
          library: { get: () => ({ hasAudio: true }) },
          range: { start: 0, end: 1 },
          options: { includeAudio: true, audioQuality: freshBitrate() },
          openOutput: async () => {
            opened = true;
            throw new Error("opened the output");
          },
        }),
        (error) => error.failure?.code === "INVALID_INPUT",
      );
      assert.equal(opened, false);
    } finally {
      audioEncoder(null);
    }
  }
});

/** Writes three bytes into an opened export and finishes it: the job's artifact. */
async function finished(output) {
  const writer = output.destination.writable.getWriter();
  await writer.write({
    type: "write",
    position: 0,
    data: new Uint8Array([1, 2, 3]),
  });
  await writer.close();
  return output.finish({ kind: "stream", bytes: 3 });
}

const DAY = 24 * 60 * 60 * 1000;

test("a sweep removes only a temporary export no page holds, however old a held one is", async () => {
  const exports = memoryDirectory();
  const locks = new FakeLocks();
  page(exports, {}, locks);
  // Another tab's finished export, its result kept and not yet saved; a page that closed left an
  // older one behind; a name the sweep does not read is never its to remove.
  const kept = await finished(
    await openExportOutput({ ...LONG_EXPORT, jobId: "kept" }),
  );
  const [keptName] = [...exports.files.keys()];
  const closed = `${Date.now() - 3 * DAY}-closed.mp4`;
  exports.files.set(closed, new Uint8Array(4));
  exports.files.set("notes.txt", new Uint8Array(1));
  assert.deepEqual(await sweepStaleExports(Date.now() + 2 * DAY), [closed]);
  assert.deepEqual(
    [...exports.files.keys()].sort(),
    [keptName, "notes.txt"].sort(),
  );
  assert.equal((await kept.file.arrayBuffer()).byteLength, 3);
  await kept.release();
  assert.deepEqual([...exports.files.keys()], ["notes.txt"]);
  assert.deepEqual(await sweepStaleExports(Date.now() + 2 * DAY), []);
});

test("without Web Locks a sweep goes by age alone", async () => {
  const exports = memoryDirectory();
  page(exports, {});
  const old = `${Date.now() - 2 * DAY}-gone.mp4`;
  const recent = `${Date.now()}-recent.mp4`;
  exports.files.set(old, new Uint8Array(1));
  exports.files.set(recent, new Uint8Array(1));
  assert.deepEqual(await sweepStaleExports(), [old]);
  assert.deepEqual([...exports.files.keys()], [recent]);
});
