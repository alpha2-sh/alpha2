/**
 * Waveform summaries for timeline display, computed by the engine.
 *
 * A timeline draws a clip's audio as columns: the extremes and RMS of the
 * samples under each. {@link summarizeWaveform} computes them at a finest
 * bucket size and at coarser zoom levels, each four times the last, with the
 * same code a native host uses (`alpha2-audio`), so the numbers are identical
 * wherever they are computed.
 */

/** The wasm-bindgen classes `alpha2/wasm` exports for waveforms. */
export interface WaveformBinding {
  readonly WaveformBuilder: new (
    channels: number,
    bucketSamples: number,
    levels: number,
  ) => {
    push(planar: Float32Array): void;
    finish(): {
      readonly frames: number;
      readonly levelCount: number;
      bucketSamples(level: number): number;
      buckets(level: number, channel: number): Float32Array;
      free?(): void;
    };
    free?(): void;
  };
}

/** One zoom level of a {@link Waveform}. */
export interface WaveformLevel {
  /** Samples per bucket. The last bucket may cover fewer. */
  readonly bucketSamples: number;
  /** One array per channel of `[min, max, rms]` triples, in time order. */
  readonly channels: readonly Float32Array[];
}

/** A waveform summary: its levels, finest first. */
export interface Waveform {
  /** Samples summarized per channel. */
  readonly frames: number;
  readonly levels: readonly WaveformLevel[];
}

/** How {@link summarizeWaveform} buckets audio. */
export interface WaveformOptions {
  /** Samples per bucket at the finest level; the engine refuses zero. */
  readonly bucketSamples: number;
  /** Zoom levels, each four times coarser; 1 to 16, default 1. */
  readonly levels?: number;
  /** Frames copied into wasm per call; default 65 536. */
  readonly blockFrames?: number;
}

/**
 * Summarizes planar audio: one `Float32Array` per channel, all one length.
 * The planes are copied into wasm in `blockFrames` chunks and not modified.
 *
 * @throws RangeError When there are no planes, their lengths differ, or
 *   `blockFrames` is not a positive integer.
 * @throws When the engine refuses the bucket size or level count.
 */
export function summarizeWaveform(
  binding: WaveformBinding,
  planes: readonly Float32Array[],
  options: WaveformOptions,
): Waveform {
  const channels = planes.length;
  const frames = planes[0]?.length ?? 0;
  if (channels === 0 || planes.some((plane) => plane.length !== frames)) {
    throw new RangeError("waveform planes must be non-empty and of one length");
  }
  const blockFrames = options.blockFrames ?? 65_536;
  if (!Number.isSafeInteger(blockFrames) || blockFrames <= 0) {
    throw new RangeError("waveform blockFrames must be a positive integer");
  }
  const builder = new binding.WaveformBuilder(
    channels,
    options.bucketSamples,
    options.levels ?? 1,
  );
  try {
    const block = new Float32Array(Math.min(blockFrames, frames) * channels);
    for (let start = 0; start < frames; start += blockFrames) {
      const length = Math.min(blockFrames, frames - start);
      planes.forEach((plane, channel) =>
        block.set(plane.subarray(start, start + length), channel * length),
      );
      builder.push(block.subarray(0, length * channels));
    }
    const result = builder.finish();
    try {
      const levels: WaveformLevel[] = [];
      for (let level = 0; level < result.levelCount; level += 1) {
        levels.push({
          bucketSamples: result.bucketSamples(level),
          channels: planes.map((_, channel) => result.buckets(level, channel)),
        });
      }
      return { frames: result.frames, levels };
    } finally {
      result.free?.();
    }
  } finally {
    builder.free?.();
  }
}
