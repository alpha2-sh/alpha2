/**
 * Fallbacks for fields a title's authoring style (`asset.authoring.text`) leaves out, as the
 * browser runtime draws it. UIs showing a title's style should read them here.
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
