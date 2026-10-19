// The supplier trims what the engine holds of a source behind the earliest point a mix reads, so
// playing a long source, or playing on after seeks forward, keeps seconds of it rather than every
// window decoded since the project loaded.
import test from "node:test";
import assert from "node:assert/strict";

import { AudioWindowSupplier } from "../dist/browser/runtime/audio.js";

const RATE = 48_000;
const BLOCK = 4_096;
const TICKS_PER_SECOND = 120_000n;
/** What the supplier keeps before the earliest tick a mix reads, and its trim step. */
const KEEP_BEHIND = 2 * RATE;
const TRIM_STEP = 2 * RATE;

const ticks = (sample) => (BigInt(sample) * TICKS_PER_SECOND) / BigInt(RATE);
const sampleAt = (tick) => Number((tick * BigInt(RATE)) / TICKS_PER_SECOND);

/**
 * An engine whose audio store keeps windows as the wasm one does where it matters here: an upload
 * replaces the windows it overlaps, and a trim drops or shortens the ones before its time.
 */
function storeEngine(needsAt) {
  const windows = new Map();
  const trims = [];
  return {
    windows,
    trims,
    audioNeeds: (start, frames) =>
      JSON.stringify(
        needsAt(Number(start), frames).map(({ sourceId, start, end }) => ({
          sourceId,
          startTicks: String(ticks(start)),
          endTicks: String(ticks(end)),
        })),
      ),
    audioRetainedWindows: (sourceId) =>
      JSON.stringify(
        (windows.get(sourceId) ?? []).map(({ start, end }) => ({
          startSample: start,
          endSample: end,
        })),
      ),
    uploadAudioBlock(sourceId, start, rate, layout, samples) {
      assert.equal(rate, RATE);
      const from = Number(start);
      const to = from + samples.length / layout;
      const kept = (windows.get(sourceId) ?? []).filter(
        (window) => window.end <= from || to <= window.start,
      );
      windows.set(sourceId, [{ start: from, end: to }, ...kept]);
    },
    trimAudioBefore(sourceId, tick) {
      trims.push({ sourceId, tick });
      const cut = sampleAt(tick);
      windows.set(
        sourceId,
        (windows.get(sourceId) ?? [])
          .filter((window) => window.end > cut)
          .map((window) => ({ ...window, start: Math.max(window.start, cut) })),
      );
    },
  };
}

/** Generated audio as long as a feature film, which the supplier slices per request. */
function library(...ids) {
  const buffer = {
    sampleRate: RATE,
    numberOfChannels: 1,
    length: RATE * 3_600,
    getChannelData: () => ({
      subarray: (from, to) => new Float32Array(to - from),
    }),
  };
  const assets = new Map(
    ids.map((id) => [id, { id, kind: "audio", audioBuffer: buffer }]),
  );
  return {
    get: (id) => assets.get(id),
    project: {
      settings: { sample_rate: RATE },
      sources: ids.map((id) => ({ id, kind: "audio" })),
    },
  };
}

const held = (engine, sourceId) =>
  (engine.windows.get(sourceId) ?? []).reduce(
    (sum, { start, end }) => sum + end - start,
    0,
  );

test("playing on after seeks forward keeps seconds of a source, not every window", async () => {
  const assets = library("s");
  const engine = storeEngine((start, frames) => [
    { sourceId: "s", start, end: start + frames },
  ]);
  const supplier = new AudioWindowSupplier(assets);
  supplier.configure(engine, assets.project);
  // Ten seconds from each of 20 points 40 s apart, then 700 s straight through.
  const plays = [
    ...Array.from({ length: 20 }, (_, index) => [index * 40 * RATE, 10 * RATE]),
    [800 * RATE, 700 * RATE],
  ];
  for (const [from, length] of plays) {
    for (let start = from; start < from + length; start += BLOCK) {
      await supplier.supply(start, BLOCK);
      for (const { tick } of engine.trims)
        assert.ok(
          sampleAt(tick) <= start - KEEP_BEHIND,
          `a trim at ${tick} cut what the block at ${start} reads`,
        );
      assert.ok(
        (engine.windows.get("s") ?? []).some(
          (window) => window.start <= start && start + BLOCK <= window.end,
        ),
        `the block at ${start} is held`,
      );
      // What is held: the trim step and what is kept behind, the block, and a prefetch ahead.
      assert.ok(
        held(engine, "s") <= KEEP_BEHIND + TRIM_STEP + BLOCK + 3 * RATE,
        `${held(engine, "s") / RATE} s held at ${start / RATE} s`,
      );
    }
  }
  await supplier.close();
});

test("a source two clips read at different times is trimmed below both", async () => {
  const assets = library("s");
  // The second clip reads the same source 30 s further in.
  const engine = storeEngine((start, frames) => [
    { sourceId: "s", start, end: start + frames },
    {
      sourceId: "s",
      start: start + 30 * RATE,
      end: start + 30 * RATE + frames,
    },
  ]);
  const supplier = new AudioWindowSupplier(assets);
  supplier.configure(engine, assets.project);
  for (let start = 0; start < 60 * RATE; start += BLOCK) {
    await supplier.supply(start, BLOCK);
    const windows = engine.windows.get("s");
    for (const at of [start, start + 30 * RATE])
      assert.ok(
        windows.some(
          (window) => window.start <= at && at + BLOCK <= window.end,
        ),
        `${at} is held`,
      );
  }
  assert.ok(engine.trims.length > 0, "the source was trimmed");
  await supplier.close();
});

test("a seek back trims nothing, and playing on from there trims again", async () => {
  const assets = library("s");
  const engine = storeEngine((start, frames) => [
    { sourceId: "s", start, end: start + frames },
  ]);
  const supplier = new AudioWindowSupplier(assets);
  supplier.configure(engine, assets.project);
  await supplier.supply(100 * RATE, BLOCK);
  const trimmed = engine.trims.length;
  assert.ok(trimmed > 0);
  for (let start = 10 * RATE; start < 20 * RATE; start += BLOCK) {
    await supplier.supply(start, BLOCK);
    for (const { tick } of engine.trims.slice(trimmed))
      assert.ok(sampleAt(tick) <= start - KEEP_BEHIND);
  }
  assert.ok(engine.trims.length > trimmed, "playing on from the seek trims");
  await supplier.close();
});

test("a mix still supplying keeps what it reads from another mix's trim", async () => {
  const assets = library("s");
  const engine = storeEngine((start, frames) => [
    { sourceId: "s", start, end: start + frames },
  ]);
  const supplier = new AudioWindowSupplier(assets);
  supplier.configure(engine, assets.project);
  const early = supplier.supply(10 * RATE, BLOCK);
  const late = supplier.supply(100 * RATE, BLOCK);
  await Promise.all([early, late]);
  for (const { tick } of engine.trims)
    assert.ok(sampleAt(tick) <= 10 * RATE - KEEP_BEHIND, `trimmed at ${tick}`);
  assert.ok(
    engine.windows
      .get("s")
      .some(
        (window) =>
          window.start <= 10 * RATE && 10 * RATE + BLOCK <= window.end,
      ),
    "the earlier mix's window is held",
  );
  await supplier.close();
});
