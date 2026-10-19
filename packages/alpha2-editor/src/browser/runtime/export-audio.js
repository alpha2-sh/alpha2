// Export audio covers the export's effective range (widened to the frame grid) as the engine
// decides it on every host; its first sample sits at the output's zero with the first frame.

import { missingAudioSources, mixBlock } from "./audio.js";

/**
 * Audio samples mixed and encoded per block, as the native host mixes them; at most the engine's
 * `audioMaxBlock`.
 */
const AUDIO_BLOCK_FRAMES = 4_096;

/**
 * The project-rate samples the running export's audio covers, as the engine reports them
 * (`exportProgress().audioWindow`): `startSample` is the mix position written at the output's zero,
 * `frames` the sample count. Read after `beginExport`.
 */
export function exportAudioWindow(engine) {
  const { audioWindow } = JSON.parse(engine.exportProgress());
  return {
    startSample: Number(audioWindow.startSample),
    frames: audioWindow.frames,
  };
}

/**
 * Writes the running export's audio track into `sink` as the video advances:
 * `writeThrough(seconds)` mixes and encodes every block up to `seconds` past the output's zero
 * (`Infinity` for the rest). Create it after `beginExport`.
 */
export function exportAudioWriter({
  engine,
  audioSupplier,
  sink,
  sampleRate,
  signal,
  metrics,
  enter,
}) {
  const window = exportAudioWindow(engine);
  const blockFrames = Math.min(AUDIO_BLOCK_FRAMES, engine.audioMaxBlock);
  let written = 0;
  return async function writeThrough(seconds) {
    const until = Math.min(window.frames, Math.ceil(seconds * sampleRate));
    while (written < until) {
      if (signal?.aborted)
        throw (
          signal.reason ?? new DOMException("Export cancelled", "AbortError")
        );
      const offset = written;
      const frames = Math.min(blockFrames, window.frames - offset);
      const startSample = window.startSample + offset;
      const decodeStarted = performance.now();
      enter("decode", undefined);
      await audioSupplier.supply(startSample, frames, { signal });
      metrics.audioDecodeMs += performance.now() - decodeStarted;
      const mixStarted = performance.now();
      let [left, right] = mixBlock(engine, startSample, frames, false);
      metrics.audioMixMs += performance.now() - mixStarted;
      let missing = missingAudioSources(engine);
      if (missing.length > 0) {
        const retryDecodeStarted = performance.now();
        await audioSupplier.supply(startSample, frames, {
          signal,
          forceSources: missing,
        });
        metrics.audioDecodeMs += performance.now() - retryDecodeStarted;
        const retryMixStarted = performance.now();
        [left, right] = mixBlock(engine, startSample, frames, false);
        metrics.audioMixMs += performance.now() - retryMixStarted;
        missing = missingAudioSources(engine);
      }
      if (missing.length > 0) {
        throw new Error(`export is missing audio for ${missing.join(", ")}`);
      }
      const audioEncodeStarted = performance.now();
      enter("encode", undefined);
      await sink.writeAudio({
        sourceId: "alpha2-web-mix",
        startSample: BigInt(offset),
        generation: 0,
        sampleRate,
        channels: [left, right],
      });
      metrics.audioEncodeMs += performance.now() - audioEncodeStarted;
      metrics.audioBlocksInterleaved += 1;
      written += frames;
    }
  };
}
