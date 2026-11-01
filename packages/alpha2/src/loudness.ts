/**
 * Loudness measurement and normalization (ITU-R BS.1770-4, EBU R128), computed by the engine.
 *
 * To export at a target loudness:
 * 1. call `setLoudnessMetering(true)` on the engine and render the audio once with `renderAudio`;
 * 2. pass `JSON.parse(engine.loudnessReport())` to {@link planLoudnessNormalization};
 * 3. render the export with `setNormalizationGainDb(plan.gainDb)`.
 *
 * The plan keeps the mix under the master limiter's threshold, so the limiter does not change the
 * planned loudness. Enabling metering resets the gain to zero.
 */

/** Loudness measurement. A field is `null` when nothing was measurable. */
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
   * Linear threshold of the master limiter after the metering point; `null` without a limiter and
   * absent from {@link measureLoudness} results.
   */
  readonly limiterThreshold?: number | null;
}

/** Static gain that brings a mix to a loudness target. */
export interface LoudnessNormalization {
  /** Gain to apply before the master limiter, dB. */
  readonly gainDb: number;
  /** Integrated loudness after the gain, LUFS. */
  readonly integratedLufs: number;
  /** True peak after the gain, dBTP. */
  readonly truePeakDbtp: number;
  /**
   * Whether the true-peak ceiling or the master limiter capped the gain, leaving the result quieter
   * than the target.
   */
  readonly peakLimited: boolean;
  /**
   * Whether the limiter threshold capped the gain; the sample peak is kept 0.01 dB under it so the
   * limiter does not act.
   */
  readonly limiterLimited: boolean;
}

/** The `alpha2/wasm` items loudness uses. */
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
 * Measures planar audio, one equal-length `Float32Array` per channel. Non-finite samples count as
 * silence. `blockFrames` is the number of frames copied into wasm per call.
 *
 * @throws RangeError When there are no planes, their lengths differ, or `blockFrames` is not a
 *   positive integer.
 * @throws When the sample rate is outside 8 to 96 kHz.
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
 * Plans the gain that brings `report` to `targetLufs` without the true peak exceeding
 * `ceilingDbtp` or the sample peak reaching `report.limiterThreshold`. Returns `null` for audio
 * below the absolute gate.
 *
 * @throws TypeError When `report` is not an object (for example the unparsed JSON string from
 *   `loudnessReport()`, or its `null` while metering is off).
 */
export function planLoudnessNormalization(
  binding: LoudnessBinding,
  report: LoudnessReport,
  targetLufs: number,
  ceilingDbtp = -1,
): LoudnessNormalization | null {
  // Without this check a string or null would silently plan as silence.
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
