/**
 * What a title's authoring style (`asset.authoring.text`) means where it leaves a field out: the
 * browser runtime draws the title with these. A UI that shows a title's style reads its fallbacks
 * here, so what it shows is what is drawn.
 */
export const TEXT_DEFAULTS = Object.freeze({
  content: "alpha2",
  /** In output pixels. */
  fontSize: 120,
  fontFamily: "Inter, Segoe UI, system-ui, sans-serif",
  color: "#ffffff",
  align: "center",
  /** Zero leaves the text unwrapped. */
  maxWidth: 0,
  /** Baseline-to-baseline distance in font sizes. */
  lineHeight: 1.15,
} as const);
