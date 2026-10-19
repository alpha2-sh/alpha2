// Editor colors (`#rrggbb` strings) in the engine's form.

/** A straight sRGB `#rrggbb` color with `alpha` as an `alpha2_model::Rgba`. */
export function rgba(hex, alpha = 1) {
  const value = /^#?([0-9a-f]{6})$/i.exec(hex ?? "")?.[1] ?? "ffffff";
  const channel = (index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16) / 255;
  return {
    red: channel(0),
    green: channel(1),
    blue: channel(2),
    alpha: Math.min(1, Math.max(0, Number.isFinite(alpha) ? alpha : 1)),
  };
}
