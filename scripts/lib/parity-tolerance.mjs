// How far apart browser and native frames may be when drawn on different devices. FMA
// contraction, `fract` and sampler subtexel rounding are not bit-exact across GPUs (lavapipe vs a
// discrete NVIDIA GPU swaps 7 codes on one edge), so a few isolated samples may exceed the
// general bound; callers also check the frame mean and the share of samples over one code.

/** The largest code difference any sample may have without counting as an outlier. */
export const PEAK_CODES = 4;
/** The largest code difference an outlier may have. */
export const OUTLIER_PEAK_CODES = 8;
/** How many outliers one compared body (or project) may have. */
export const OUTLIER_SAMPLES = 4;

/**
 * Why `max` (the largest code difference) and `outliers` (samples more than {@link PEAK_CODES}
 * apart) exceed the cross-device bound, or null when they are within it.
 */
export function peakExcess({ max, outliers }) {
  if (max <= PEAK_CODES) return null;
  if (max > OUTLIER_PEAK_CODES)
    return `${max} codes apart at most; allowed ${PEAK_CODES}, or ${OUTLIER_PEAK_CODES} for up to ${OUTLIER_SAMPLES} isolated samples`;
  if (outliers > OUTLIER_SAMPLES)
    return `${outliers} samples more than ${PEAK_CODES} codes apart (at most ${max}); allowed ${OUTLIER_SAMPLES}`;
  return null;
}
