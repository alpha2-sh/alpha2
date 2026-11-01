// Cross-device frame tolerance. FMA contraction, `fract` and subtexel rounding differ across GPUs
// (up to 7 codes on an edge), so a few isolated outliers are allowed; callers also check the frame
// mean and the share of samples over one code.

export const PEAK_CODES = 4;
export const OUTLIER_PEAK_CODES = 8;
export const OUTLIER_SAMPLES = 4;

export function peakExcess({ max, outliers }) {
  if (max <= PEAK_CODES) return null;
  if (max > OUTLIER_PEAK_CODES)
    return `${max} codes apart at most; allowed ${PEAK_CODES}, or ${OUTLIER_PEAK_CODES} for up to ${OUTLIER_SAMPLES} isolated samples`;
  if (outliers > OUTLIER_SAMPLES)
    return `${outliers} samples more than ${PEAK_CODES} codes apart (at most ${max}); allowed ${OUTLIER_SAMPLES}`;
  return null;
}
