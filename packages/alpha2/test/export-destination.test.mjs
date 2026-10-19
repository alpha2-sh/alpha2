import assert from "node:assert/strict";
import test from "node:test";
import { setFlagsFromString } from "node:v8";
import { runInNewContext } from "node:vm";

import {
  ALL_FORMATS,
  BufferSource,
  BufferTarget,
  EncodedPacket,
  EncodedPacketSink,
  EncodedVideoPacketSource,
  Input,
  Mp4OutputFormat,
  Output,
  StreamTarget,
} from "mediabunny";

import {
  estimatedOutputBytes,
  memoryOutputPeakBytes,
  WritableFileDestination,
} from "../dist/export-destination.js";

setFlagsFromString("--expose-gc");
const gc = runInNewContext("gc");

const FPS = 30;

/** A file that keeps its bytes, or only its length when `keep` is false. */
function memoryFile({ keep = true, delayMs = 0, fail } = {}) {
  const file = {
    bytes: new Uint8Array(0),
    size: 0,
    closed: false,
    aborted: false,
    async write({ data, position }) {
      if (delayMs) await new Promise((resolve) => setTimeout(resolve, delayMs));
      if (fail?.(position)) throw fail.error;
      const end = position + data.byteLength;
      file.size = Math.max(file.size, end);
      if (!keep) return;
      if (end > file.bytes.byteLength) {
        const grown = new Uint8Array(Math.max(end, file.bytes.byteLength * 2));
        grown.set(file.bytes);
        file.bytes = grown;
      }
      file.bytes.set(data, position);
    },
    async close() {
      file.closed = true;
    },
    async abort() {
      file.aborted = true;
    },
    contents() {
      return file.bytes.slice(0, file.size);
    },
  };
  return file;
}

/** Muxes `frames` packets of `packetBytes` each, a fresh buffer per packet, into `target`. */
async function mux(target, frames, packetBytes = 16 * 1024) {
  const output = new Output({
    format: new Mp4OutputFormat({ fastStart: false }),
    target,
  });
  const source = new EncodedVideoPacketSource("vp9");
  output.addVideoTrack(source, { frameRate: FPS });
  await output.start();
  for (let index = 0; index < frames; index += 1) {
    const data = new Uint8Array(packetBytes);
    data[0] = index & 255;
    data[1] = (index >> 8) & 255;
    await source.add(
      new EncodedPacket(
        data,
        index % FPS === 0 ? "key" : "delta",
        index / FPS,
        1 / FPS,
      ),
      index === 0
        ? {
            decoderConfig: {
              codec: "vp09.00.10.08",
              codedWidth: 64,
              codedHeight: 64,
            },
          }
        : undefined,
    );
  }
  await output.finalize();
}

test("a file destination writes the same MP4 a memory buffer holds", async (t) => {
  // An MP4 records when it was made, to the second: two muxes on either side of a second's end
  // differ in those bytes alone. Both are made at the same time.
  t.mock.method(Date, "now", () => Date.UTC(2026, 0, 1));
  const file = memoryFile();
  const destination = new WritableFileDestination(file);
  await mux(new StreamTarget(destination.writable), 90);
  const buffered = new BufferTarget();
  await mux(buffered, 90);
  assert.ok(file.closed, "finishing closes the file");
  assert.deepEqual(file.contents(), new Uint8Array(buffered.buffer));
  assert.equal(destination.stats.size, buffered.buffer.byteLength);

  const input = new Input({
    source: new BufferSource(file.contents()),
    formats: ALL_FORMATS,
  });
  const track = await input.getPrimaryVideoTrack();
  const packets = [];
  for await (const packet of new EncodedPacketSink(track).packets())
    packets.push(packet);
  assert.equal(packets.length, 90);
  assert.equal(packets[45].timestamp, 45 / FPS);
  assert.equal(packets[45].data[0], 45);
  assert.equal(await track.computeDuration(), 90 / FPS);
  input.dispose();
});

test("a slow file holds the muxer back instead of growing the queue", async () => {
  const maxQueuedBytes = 256 * 1024;
  const file = memoryFile({ keep: false, delayMs: 2 });
  const destination = new WritableFileDestination(file, { maxQueuedBytes });
  await mux(new StreamTarget(destination.writable), 240, 32 * 1024);
  const { peakQueuedBytes, size } = destination.stats;
  assert.ok(size > 7 * 1024 * 1024, `wrote ${size} bytes`);
  // One flush past the bound at most: a stream accepts a chunk while it has room left.
  const oneFlush = 32 * 1024 * FPS;
  assert.ok(
    peakQueuedBytes <= maxQueuedBytes + oneFlush,
    `queued ${peakQueuedBytes} bytes`,
  );
});

test("memory held while muxing does not grow with the file's length", async () => {
  const held = async (frames) => {
    const file = memoryFile({ keep: false });
    const destination = new WritableFileDestination(file);
    // Measured against the lowest point seen, not the start: memory an earlier test left can still
    // be freed while this runs.
    gc();
    let low = process.memoryUsage().arrayBuffers;
    let peak = low;
    let writes = 0;
    const target = new StreamTarget(destination.writable);
    target.onwrite = () => {
      if (++writes % 20 === 0) {
        gc();
        const now = process.memoryUsage().arrayBuffers;
        low = Math.min(low, now);
        peak = Math.max(peak, now);
      }
    };
    await mux(target, frames, 64 * 1024);
    return { grew: peak - low, size: destination.stats.size };
  };
  const short = await held(300);
  const long = await held(3_000);
  assert.ok(long.size > 9 * short.size, `${long.size} vs ${short.size}`);
  // The payload is 10x; what stays in memory is a flush and the queue, not a share of the file.
  assert.ok(
    long.grew < short.grew + 8 * 1024 * 1024,
    `held ${long.grew} bytes for a ${long.size}-byte file, ${short.grew} for ${short.size}`,
  );
  assert.ok(long.grew < long.size / 4, `held ${long.grew} of ${long.size}`);
});

test("a refused write fails the export as storage, and nothing claims success", async () => {
  const quota = Object.assign(new Error("quota"), {
    name: "QuotaExceededError",
  });
  const file = memoryFile({
    fail: (position) => position > 64 * 1024,
    keep: false,
  });
  file.write = ((write) => async (chunk) => {
    if (chunk.position > 64 * 1024) throw quota;
    return write(chunk);
  })(file.write);
  const destination = new WritableFileDestination(file);
  await assert.rejects(
    mux(new StreamTarget(destination.writable), 120),
    (error) => {
      assert.equal(error.failure?.code, "STORAGE_QUOTA_EXCEEDED");
      assert.equal(error.failure?.phase, "publish");
      return true;
    },
  );
  assert.equal(destination.failure?.code, "STORAGE_QUOTA_EXCEEDED");
  assert.equal(file.closed, false);

  const denied = memoryFile({ keep: false });
  denied.close = async () => {
    throw Object.assign(new Error("gone"), { name: "NotFoundError" });
  };
  const closing = new WritableFileDestination(denied);
  await assert.rejects(
    mux(new StreamTarget(closing.writable), 10),
    (error) => error.failure?.code === "OUTPUT_IO_FAILED",
  );
});

test("an aborted destination discards the file even when the muxer closes it", async () => {
  const file = memoryFile({ keep: false });
  const destination = new WritableFileDestination(file);
  const output = new Output({
    format: new Mp4OutputFormat({ fastStart: false }),
    target: new StreamTarget(destination.writable),
  });
  const source = new EncodedVideoPacketSource("vp9");
  output.addVideoTrack(source, { frameRate: FPS });
  await output.start();
  await source.add(new EncodedPacket(new Uint8Array(1024), "key", 0, 1 / FPS), {
    decoderConfig: { codec: "vp09.00.10.08", codedWidth: 64, codedHeight: 64 },
  });
  await destination.abort(new Error("cancelled"));
  await output.cancel();
  assert.equal(file.aborted, true);
  assert.equal(file.closed, false);
});

test("an in-memory output is budgeted for its growth and copies before it starts", () => {
  const estimate = estimatedOutputBytes({
    durationSeconds: 10,
    videoBitrate: 8_000_000,
    audioBitrate: 128_000,
  });
  assert.equal(estimate, 10_160_000 + 1024 * 1024);
  assert.equal(memoryOutputPeakBytes(1_000_000), 6_000_000);
});
