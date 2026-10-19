// A clip's waveform peaks read every channel: which plane holds which speaker is the decoder's
// choice (a browser decoder may deliver a 5.1 stream's centre first), and a centre channel's
// dialogue is in neither plane of the front pair.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { AudioSample, CustomAudioDecoder, registerDecoder } from "mediabunny";

import { streamPeaks } from "../dist/browser/runtime/assets.js";

const RATE = 48_000;

/** An AAC decoder whose every packet decodes into silence but in one plane, the third. */
class CentreOnlyDecoder extends CustomAudioDecoder {
  static supports(codec) {
    return codec === "aac";
  }
  init() {}
  decode(packet) {
    const channels = this.config.numberOfChannels;
    const data = new Float32Array(channels * 1_024);
    data.fill(0.5, 2 * 1_024, 3 * 1_024);
    this.onSample(
      new AudioSample({
        data,
        format: "f32-planar",
        numberOfChannels: channels,
        sampleRate: RATE,
        timestamp: packet.timestamp,
      }),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(CentreOnlyDecoder);

test("a waveform peak is the loudest sample in any channel", async () => {
  const file = new Blob([
    await readFile(
      new URL("../../../tests/corpus/audio-aac-5.1.mp4", import.meta.url),
    ),
  ]);
  const peaks = await streamPeaks(file, { bucketsPerSecond: 10 });
  assert.ok(peaks.length > 0);
  assert.ok(
    peaks.every((peak) => peak === 0.5),
    `peaks ${Array.from(peaks)}`,
  );
});
