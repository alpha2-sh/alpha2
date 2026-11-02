import type {
  GradientStop,
  Paint,
  Rgba,
  Spread,
  Vec2,
} from "./generated/project-schema.js";

/** A gradient color: spread evenly with its neighbours, or at `[offset, color]`. */
export type ColorStop = Rgba | readonly [number, Rgba];

export type LinearGradient = Extract<
  Paint,
  { readonly type: "linear_gradient" }
>;
export type RadialGradient = Extract<
  Paint,
  { readonly type: "radial_gradient" }
>;

function stops(colors: readonly ColorStop[]): GradientStop[] {
  if (colors.length < 2)
    throw new RangeError("a gradient has at least two stops");
  return colors.map((stop, index) =>
    Array.isArray(stop)
      ? { offset: stop[0] as number, color: stop[1] as Rgba }
      : { offset: index / (colors.length - 1), color: stop as Rgba },
  );
}

/**
 * A paint along the line from `start` to `end` (normalized). Colors are `"#rrggbb"` strings or
 * `{ red, green, blue, alpha }` objects.
 *
 * @throws RangeError For fewer than two colors.
 */
export function linearGradient(
  colors: readonly ColorStop[],
  start: Vec2 = { x: 0, y: 0 },
  end: Vec2 = { x: 1, y: 1 },
  spread?: Spread,
): LinearGradient {
  return {
    type: "linear_gradient",
    start,
    end,
    stops: stops(colors),
    ...(spread === undefined ? {} : { spread }),
  };
}

/**
 * A paint along the radius of a circle at `center`, `radius` a fraction of the shorter side;
 * colors as {@link linearGradient} takes them.
 *
 * @throws RangeError For fewer than two colors.
 */
export function radialGradient(
  colors: readonly ColorStop[],
  center: Vec2 = { x: 0.5, y: 0.5 },
  radius = 0.5,
  spread?: Spread,
): RadialGradient {
  return {
    type: "radial_gradient",
    center,
    radius,
    stops: stops(colors),
    ...(spread === undefined ? {} : { spread }),
  };
}
