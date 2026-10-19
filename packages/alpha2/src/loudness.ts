/**
 * Loudness measurement and normalization (ITU-R BS.1770-4, EBU R128),
 * computed by the engine with the same code a native host uses.
 *
 * Measure decoded audio with {@link measureLoudness}. To export at a target
 * loudness, measure the export's mix before the master limiter first: turn
 * `setLoudnessMetering(true)` on the engine, render the export's audio once
 * with `renderAudio`, parse its report with `JSON.parse(engine.loudnessReport())`,
 * plan the gain with {@link planLoudnessNormalization}, and render the export with
 * `setNormalizationGainDb(plan.gainDb)`. The engine's report states the master
 * limiter's threshold, and the plan keeps the mix under it, so the limiter
 * leaves the export at the planned loudness. Turning metering on removes the
 * gain, so the cycle can run again; set zero for audio that is not normalized.
 */

/** What a loudness measurement found. `null` where nothing was measurable. */
export interface LoudnessReport {
  /** Gated integrated loudness, LUFS; `null` for audio below -70 LUFS. */
  readonly integratedLufs: number | null;
  /** Loudness range (EBU Tech 3342), LU. */
  readonly loudnessRangeLu: number | null;
  /** Maximum true peak (4x oversampled), dBTP. */
  readonly truePeakDbtp: number | null;
  /** Maximum sample peak, dBFS. */
  readonly samplePeakDbfs: number | null;
  /** Loudest 400 ms window, LUFS. */
  readonly momentaryMaxLufs: number | null;
  /** Loudest 3 s window, LUFS. */
  readonly shortTermMaxLufs: number | null;
  /** Samples measured per channel. */
  readonly frames: number;
  /**
   * The linear threshold of the master limiter the mix was measured in front of: `null` for a mix
   * without one, absent from a {@link measureLoudness} measurement. The engine's
   * `loudnessReport()` states it, and a plan keeps the mix's sample peak under it.
   */
  readonly limiterThreshold?: number | null;
}

/** A static gain that brings a mix to a loudness target. */
export interface LoudnessNormalization {
  /** Gain to apply before the master limiter, dB. */
  readonly gainDb: number;
  /** Integrated loudness after the gain, LUFS. */
  readonly integratedLufs: number;
  /** True peak after the gain, dBTP. */
  readonly truePeakDbtp: number;
  /**
   * Whether a peak bound set the gain, leaving the result quieter than the target: the true-peak
   * ceiling, or the master limiter (`limiterLimited`).
   */
  readonly peakLimited: boolean;
  /**
   * Whether the master limiter's threshold set the gain, below what the ceiling allows: the mix's
   * sample peak is kept 0.01 dB under it, so the limiter does not act.
   */
  readonly limiterLimited: boolean;
}

/** The parts of `alpha2/wasm` loudness uses. */
export interface LoudnessBinding {
  readonly LoudnessMeter: new (
    sampleRate: number,
    channels: number,
  ) => {
    push(planar: Float32Array): void;
    report(): string;
    free?(): void;
  };
  planLoudnessNormalization(
    reportJson: string,
    targetLufs: number,
    ceilingDbtp: number,
  ): string;
}

/**
 * Measures planar audio: one `Float32Array` per channel, all one length. A NaN
 * or infinite sample measures as silence. `blockFrames` is how many frames are
 * copied into wasm per call.
 *
 * @throws RangeError When there are no planes, their lengths differ, or
 *   `blockFrames` is not a positive integer.
 * @throws When the engine refuses the sample rate (outside 8 to 96 kHz).
 */
export function measureLoudness(
  binding: LoudnessBinding,
  planes: readonly Float32Array[],
  sampleRate: number,
  blockFrames = 65_536,
): LoudnessReport {
  const channels = planes.length;
  const frames = planes[0]?.length ?? 0;
  if (channels === 0 || planes.some((plane) => plane.length !== frames)) {
    throw new RangeError("loudness planes must be non-empty and of one length");
  }
  if (!Number.isSafeInteger(blockFrames) || blockFrames <= 0) {
    throw new RangeError("loudness blockFrames must be a positive integer");
  }
  const meter = new binding.LoudnessMeter(sampleRate, channels);
  try {
    const block = new Float32Array(Math.min(blockFrames, frames) * channels);
    for (let start = 0; start < frames; start += blockFrames) {
      const length = Math.min(blockFrames, frames - start);
      planes.forEach((plane, channel) =>
        block.set(plane.subarray(start, start + length), channel * length),
      );
      meter.push(block.subarray(0, length * channels));
    }
    return JSON.parse(meter.report()) as LoudnessReport;
  } finally {
    meter.free?.();
  }
}

/**
 * The gain that brings `report` to `targetLufs` without its true peak passing `ceilingDbtp`
 * (default -1 dBTP), or its sample peak reaching the master limiter `report.limiterThreshold`
 * states; `null` for audio silent below the absolute gate.
 *
 * @throws TypeError When `report` is not an object: the engine's `loudnessReport()` returns JSON
 *   text, which is parsed first, and `null` while metering is off, which measured nothing.
 */
export function planLoudnessNormalization(
  binding: LoudnessBinding,
  report: LoudnessReport,
  targetLufs: number,
  ceilingDbtp = -1,
): LoudnessNormalization | null {
  // An unchecked string or null would plan as silence, the same answer as quiet audio.
  if (typeof report !== "object" || report === null)
    throw new TypeError(
      "planLoudnessNormalization takes a report object: parse loudnessReport() with JSON.parse, " +
        "and measure with metering on",
    );
  return JSON.parse(
    binding.planLoudnessNormalization(
      JSON.stringify(report),
      targetLufs,
      ceilingDbtp,
    ),
  ) as LoudnessNormalization | null;
}
