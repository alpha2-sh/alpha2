// Node has no WebCodecs `AudioDecoder` (like Safari before 26): the stream is still recorded and
// this session supplies silence for it.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { probeVideo } from "../dist/browser/runtime/assets.js";
import { AudioWindowSupplier } from "../dist/browser/runtime/audio.js";

const corpus = new URL("../../../tests/corpus/", import.meta.url);

test("a video's audio stream is recorded whether or not this browser decodes it", async () => {
  assert.equal(typeof AudioDecoder, "undefined", "Node has no AudioDecoder");
  const file = new File(
    [await readFile(new URL("h264-bt709-copy.mp4", corpus))],
    "clip.mp4",
  );
  const probe = await probeVideo(file);
  assert.equal(probe.hasAudio, true, "the file has an AAC stream");
  assert.equal(probe.audioDecodable, false, "this realm cannot decode it");
  const silent = await probeVideo(
    new File([await readFile(new URL("video-only.mp4", corpus))], "silent.mp4"),
  );
  assert.equal(silent.hasAudio, false);
});

test("a source this browser cannot decode is supplied as silence, not left missing", async () => {
  const uploads = [];
  const engine = {
    audioNeeds: () =>
      JSON.stringify([
        { sourceId: "clip", startTicks: "0", endTicks: "12000" },
      ]),
    audioRetainedWindows: () => "[]",
    uploadAudioBlock: (...args) => uploads.push(args),
  };
  const asset = { id: "clip", kind: "video", file: {}, audioDecodable: false };
  const library = { get: (id) => (id === "clip" ? asset : undefined) };
  const supplier = new AudioWindowSupplier(library);
  supplier.configure(engine, {
    settings: { sample_rate: 48_000 },
    sources: [{ id: "clip", kind: "video", has_audio: true }],
  });
  await supplier.supply(0, 4_800);
  assert.equal(uploads.length, 1);
  const [source, start, rate, layout, samples] = uploads[0];
  assert.deepEqual([source, start, rate, layout], ["clip", 0n, 48_000, 1]);
  assert.ok(samples.length >= 4_800, "it covers what the mix reads");
  assert.ok(samples.every((sample) => sample === 0));
  await supplier.close();
});

/** A supplier over one source `asset` whose uploads land in the returned list. */
function supplierFor(
  asset,
  needs = [{ sourceId: "clip", startTicks: "0", endTicks: "12000" }],
) {
  const uploads = [];
  const engine = {
    audioNeeds: () => JSON.stringify(needs),
    audioRetainedWindows: () => "[]",
    uploadAudioBlock: (...args) => uploads.push(args),
  };
  const library = { get: (id) => (id === "clip" ? asset : undefined) };
  const supplier = new AudioWindowSupplier(library);
  supplier.configure(engine, {
    settings: { sample_rate: 48_000 },
    sources: [{ id: "clip", kind: "video", has_audio: true }],
  });
  return { supplier, uploads };
}

test("silence for an undecodable source has as many planes as the stream it stands for", async () => {
  const asset = {
    id: "clip",
    kind: "video",
    file: {},
    hasAudio: true,
    audioDecodable: false,
    audioStreams: [{ channels: 2 }, { channels: 6, default: true }],
  };
  const { supplier, uploads } = supplierFor(asset, [
    { sourceId: "clip", startTicks: "0", endTicks: "12000" },
    { sourceId: "clip", startTicks: "0", endTicks: "12000", stream: 0 },
  ]);
  await supplier.supply(0, 4_800);
  const byStream = new Map(uploads.map((upload) => [upload[6], upload]));
  const [, , , layout, samples] = byStream.get(undefined);
  assert.equal(layout, 6);
  assert.equal(samples.length % 6, 0);
  assert.ok(samples.length / 6 >= 4_800);
  assert.equal(byStream.get(0)[3], 2);
  await supplier.close();
});

test("page-made audio uploads every channel up to 24", async () => {
  const frames = 48_000;
  const channels = Array.from({ length: 6 }, (_, index) =>
    new Float32Array(frames).fill(index / 10),
  );
  const audioBuffer = {
    sampleRate: 48_000,
    length: frames,
    numberOfChannels: 6,
    getChannelData: (channel) => channels[channel],
  };
  const { supplier, uploads } = supplierFor({
    id: "clip",
    kind: "audio",
    audioBuffer,
  });
  await supplier.supply(0, 4_800);
  assert.equal(uploads.length, 1);
  const [, , , layout, samples] = uploads[0];
  assert.equal(layout, 6);
  const plane = samples.length / 6;
  assert.ok(
    Math.abs(samples[5 * plane + 10] - 0.5) < 1e-6,
    "the sixth channel",
  );
  await supplier.close();
});

/** Replaces `MediabunnyAudioDecoder.open` for one test with `open(call)` (`call` counts from 1). */
async function fakeAudioOpen(t, open) {
  const { MediabunnyAudioDecoder } = await import("alpha2/mediabunny");
  const real = MediabunnyAudioDecoder.open;
  let calls = 0;
  MediabunnyAudioDecoder.open = async () => open(++calls);
  t.after(() => {
    MediabunnyAudioDecoder.open = real;
  });
  return () => calls;
}

const decodedBlock = {
  refill: async () => ({
    startSample: 0n,
    sampleRate: 48_000,
    channels: [new Float32Array(9_600).fill(0.5)],
  }),
  close() {},
};

const decodable = (extra = {}) => ({
  id: "clip",
  kind: "audio",
  file: { name: "clip.m4a" },
  hasAudio: true,
  audioDecodable: true,
  ...extra,
});

test("an audio decoder that failed to open is opened again by the next supply", async (t) => {
  const opens = await fakeAudioOpen(t, (call) => {
    if (call === 1) throw new Error("the file could not be read just now");
    return decodedBlock;
  });
  const { supplier, uploads } = supplierFor(decodable(), undefined);
  await assert.rejects(supplier.supply(0, 4_800), /could not be read/);
  await supplier.supply(0, 4_800);
  assert.equal(opens(), 2);
  assert.equal(uploads.length, 1);
  assert.equal(uploads[0][4][100], 0.5);
  await supplier.close();
});

for (const [code, planes] of [
  ["DECODE_UNAVAILABLE", 6],
  ["AUDIO_STREAM_MISSING", 24],
])
  test(`a preview stream that fails to open with ${code} plays silent, reported once`, async (t) => {
    const opens = await fakeAudioOpen(t, () => {
      throw Object.assign(new Error("this stream cannot be read here"), {
        code,
      });
    });
    const errors = [];
    const uploads = [];
    const engine = {
      audioNeeds: () =>
        JSON.stringify([
          { sourceId: "clip", startTicks: "0", endTicks: "12000" },
        ]),
      audioRetainedWindows: () => "[]",
      uploadAudioBlock: (...args) => uploads.push(args),
    };
    const asset = decodable({ audioStreams: [{ channels: 6, default: true }] });
    const supplier = new AudioWindowSupplier(
      { get: () => asset },
      { onError: (error) => errors.push(error) },
    );
    supplier.configure(engine, {
      settings: { sample_rate: 48_000 },
      sources: [{ id: "clip", kind: "audio" }],
    });
    await supplier.supply(0, 4_800);
    await supplier.supply(4_800, 4_800);
    assert.equal(opens(), 1, "not opened again");
    assert.deepEqual(
      uploads.map((upload) => upload[3]),
      [planes, planes],
    );
    assert.ok(uploads.every((upload) => upload[4].every((s) => s === 0)));
    assert.deepEqual(
      errors.map((e) => [e.code, e.entityId, e.cause.code]),
      [["audio_unavailable", "clip", code]],
    );
    await supplier.close();
  });

test("an export's stream that cannot be decoded fails its supply", async (t) => {
  await fakeAudioOpen(t, () => {
    throw Object.assign(new Error("no decoder"), {
      code: "DECODE_UNAVAILABLE",
    });
  });
  const { supplier } = supplierFor(decodable());
  await assert.rejects(supplier.supply(0, 4_800), /no decoder/);
  await supplier.close();
});
