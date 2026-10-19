// Whether a source has audio is a fact of its file, not of the browser that imported it. Node has
// no WebCodecs `AudioDecoder`, as Safari before 26 has none: the stream is still recorded, and this
// session supplies silence for it.
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
