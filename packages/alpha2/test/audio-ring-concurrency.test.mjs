// The ring is single-producer/single-consumer across real threads. These tests run it that way: the
// writer here, the reader in a worker thread. Each runs against both readers: `AudioRingBuffer`,
// and the worklet processor's own copy of it, which is what the browser plays through.

import assert from "node:assert/strict";
import test from "node:test";
import { Worker } from "node:worker_threads";

import {
  AUDIO_RING_CONTROL_BYTES,
  AudioRingBuffer,
} from "../dist/audio-timeline.js";
import { workletRingReader, workletWasm } from "./worklet-ring-reader.mjs";

const moduleUrl = new URL("../dist/audio-timeline.js", import.meta.url).href;
const helperUrl = new URL("./worklet-ring-reader.mjs", import.meta.url).href;

/**
 * Every sample is its own absolute index plus one, so a reader can check each one and tell it from
 * the silence an underrun plays.
 */
const LABEL_MASK = 0xf_ffff;
const label = (index) => (index & LABEL_MASK) + 1;

const READERS = ["AudioRingBuffer", "worklet processor"];

function transfer(capacity, startIndex = 0) {
  const control = new SharedArrayBuffer(AUDIO_RING_CONTROL_BYTES);
  new Int32Array(control).fill(startIndex | 0, 0, 3);
  return {
    control,
    samples: new SharedArrayBuffer(capacity * Float32Array.BYTES_PER_ELEMENT),
    channels: 1,
    capacity,
  };
}

/**
 * A reader that fills `plane` from the ring and returns how many real samples it holds: the run of
 * labels before the silence an underrun leaves.
 */
async function reader(kind, ringTransfer, wasm) {
  let read;
  if (kind === "AudioRingBuffer") {
    const ring = AudioRingBuffer.attach(ringTransfer);
    read = (plane) => ring.read([plane], plane.length);
  } else {
    const worklet = await workletRingReader(ringTransfer, wasm);
    read = (plane) => worklet.read([plane]);
  }
  return (plane) => {
    const returned = read(plane);
    let produced = 0;
    while (produced < plane.length && plane[produced] !== 0) produced += 1;
    if (returned !== undefined) assert.equal(produced, returned);
    return produced;
  };
}

// The reader thread: read in quantum-sized steps until told to stop, saying so after its first
// read, and report the first thing that is wrong.
const readerSource = `
const { workerData, parentPort } = require("node:worker_threads");
(async () => {
  const { AudioRingBuffer } = await import(workerData.moduleUrl);
  const { workletRingReader } = await import(workerData.helperUrl);
  const transfer = workerData.transfer;
  const control = new Int32Array(transfer.control);
  const plane = new Float32Array(128);
  let readPlane;
  if (workerData.kind === "AudioRingBuffer") {
    const ring = AudioRingBuffer.attach(transfer);
    readPlane = () => ring.read([plane], plane.length);
  } else {
    const worklet = await workletRingReader(transfer, workerData.wasm);
    readPlane = () => worklet.read([plane]);
  }
  // [0] is set by the writer to stop; [1] by this thread once it has read.
  const flags = new Int32Array(workerData.flags);
  let reads = 0;
  let problem = null;
  while (Atomics.load(flags, 0) === 0 && problem === null) {
    readPlane();
    let produced = 0;
    while (produced < plane.length && plane[produced] !== 0) produced += 1;
    reads += 1;
    if (reads === 1) {
      Atomics.store(flags, 1, 1);
      Atomics.notify(flags, 1);
    }
    const read = Atomics.load(control, 0) >>> 0;
    const write = Atomics.load(control, 1) >>> 0;
    if (((write - read) >>> 0) > transfer.capacity)
      problem = "write - read = " + ((write - read) >>> 0);
    // The samples just produced end at the published read index; the rest is silence.
    for (let index = 0; index < plane.length && problem === null; index += 1) {
      const expected =
        index < produced ? ((read - produced + index) & ${LABEL_MASK}) + 1 : 0;
      if (plane[index] !== expected)
        problem = "sample " + (read - produced + index) + " was " + plane[index];
    }
  }
  parentPort.postMessage({ reads, problem });
})().catch((error) => parentPort.postMessage({ reads: 0, problem: String(error) }));
`;

for (const kind of READERS) {
  test(
    `a writer racing clears against a reading thread never overruns or tears (${kind})`,
    { timeout: 120_000 },
    async (t) => {
      const wasm = kind === "AudioRingBuffer" ? undefined : await workletWasm();
      if (kind !== "AudioRingBuffer" && wasm === undefined)
        return t.skip("the worklet wasm is not built");
      const capacity = 1_024;
      // Starts short of 2^32 so the indices wrap during the run.
      const ring = AudioRingBuffer.attach(transfer(capacity, -300_000));
      const flags = new Int32Array(
        new SharedArrayBuffer(2 * Int32Array.BYTES_PER_ELEMENT),
      );
      const worker = new Worker(readerSource, {
        eval: true,
        workerData: {
          moduleUrl,
          helperUrl,
          kind,
          wasm,
          transfer: ring.transfer,
          flags: flags.buffer,
        },
      });
      const result = new Promise((resolve, reject) => {
        worker.once("message", resolve);
        worker.once("error", reject);
      });
      // Wait for the first read so worker startup cannot outlast the writer loop.
      assert.notEqual(
        Atomics.wait(flags, 1, 0, 30_000),
        "timed-out",
        "the reader thread started",
      );
      const control = new Int32Array(ring.transfer.control);
      const chunk = new Float32Array(96);
      let operations = 0;
      let clears = 0;
      while (operations < 1_000_000) {
        operations += 1;
        if (operations % 7 === 0) {
          ring.clear();
          clears += 1;
          continue;
        }
        const write = Atomics.load(control, 1) >>> 0;
        const frames = Math.min(chunk.length, ring.writable);
        for (let index = 0; index < frames; index += 1)
          chunk[index] = label(write + index);
        ring.write([chunk], frames);
      }
      Atomics.store(flags, 0, 1);
      const { reads, problem } = await result;
      await worker.terminate();
      assert.equal(problem, null);
      assert.ok(
        reads > 0 && clears > 100_000,
        `${reads} reads, ${clears} clears`,
      );
    },
  );

  test(`a ring keeps every sample across the 2^32 index wrap (${kind})`, async (t) => {
    const wasm = kind === "AudioRingBuffer" ? undefined : await workletWasm();
    if (kind !== "AudioRingBuffer" && wasm === undefined)
      return t.skip("the worklet wasm is not built");
    const isolated = globalThis.crossOriginIsolated;
    globalThis.crossOriginIsolated = true;
    let ring;
    try {
      // 24 000 does not divide 2^32, so a ring of that capacity would alias slots at the wrap.
      ring = AudioRingBuffer.create(1, 24_000);
    } finally {
      globalThis.crossOriginIsolated = isolated;
    }
    assert.ok(ring.capacity >= 24_000);
    assert.equal(
      2 ** 32 % ring.capacity,
      0,
      "capacity divides the index space",
    );
    const control = new Int32Array(ring.transfer.control);
    control.fill(-60_000, 0, 3);
    const read = await reader(kind, ring.transfer, wasm);
    const chunk = new Float32Array(1_000);
    const out = new Float32Array(5_000);
    let corrupted = 0;
    // A full ring is what makes two live indices share a slot at the wrap.
    for (let step = 0; step < 40; step += 1) {
      while (ring.writable > 0) {
        const write = Atomics.load(control, 1) >>> 0;
        for (let index = 0; index < chunk.length; index += 1)
          chunk[index] = label(write + index);
        ring.write([chunk], chunk.length);
      }
      const start = Atomics.load(control, 0) >>> 0;
      assert.equal(read(out), out.length);
      for (let index = 0; index < out.length; index += 1)
        if (out[index] !== label(start + index)) corrupted += 1;
    }
    assert.ok(Atomics.load(control, 0) >>> 0 < 2 ** 31, "the run wrapped");
    assert.equal(corrupted, 0);
  });

  test(`a clear the reader has already read past never moves it back (${kind})`, async (t) => {
    const wasm = kind === "AudioRingBuffer" ? undefined : await workletWasm();
    if (kind !== "AudioRingBuffer" && wasm === undefined)
      return t.skip("the worklet wasm is not built");
    const ring = AudioRingBuffer.attach(transfer(1_024));
    const control = new Int32Array(ring.transfer.control);
    const read = await reader(kind, ring.transfer, wasm);
    const chunk = Float32Array.from({ length: 200 }, (_, index) =>
      label(index),
    );
    ring.write([chunk], 100);
    // The writer asks for a clear at 100, and before the reader sees it, writes more that the
    // reader takes: the request is then behind the read index, as on two threads.
    ring.clear();
    const epoch = Atomics.load(control, 3);
    Atomics.store(control, 4, epoch);
    ring.write([chunk.subarray(100)], 100);
    const out = new Float32Array(150);
    assert.equal(read(out), 150);
    Atomics.store(control, 4, epoch - 1);
    const next = new Float32Array(50);
    assert.equal(read(next), 50);
    assert.equal(next[0], label(150), "the reader keeps its place");
  });
}
