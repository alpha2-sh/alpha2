import type { Animated } from "alpha2/project";

type Keyframes<T> = Extract<Animated<T>, { mode: "keyframes" }>;
type Keyframe<T> = Keyframes<T>["keyframes"][number];
/** How a keyframe eases to the next one. */
export type Interpolation = Keyframe<unknown>["interpolation"];

/** Common easing curves, as CSS names them. */
export const EASINGS = {
  ease_in: { type: "cubic_bezier", x1: 0.42, y1: 0, x2: 1, y2: 1 },
  ease_out: { type: "cubic_bezier", x1: 0, y1: 0, x2: 0.58, y2: 1 },
  ease_in_out: { type: "cubic_bezier", x1: 0.42, y1: 0, x2: 0.58, y2: 1 },
} as const satisfies Record<string, Interpolation>;

function keyframesOf<T>(value: Animated<T>): Keyframe<T>[] {
  if (value.mode !== "keyframes") throw new Error("the value has no keyframes");
  return [...value.keyframes];
}

function indexAt<T>(keys: readonly Keyframe<T>[], time: number): number {
  const index = keys.findIndex((key) => key.time === time);
  if (index < 0) throw new Error(`no keyframe at ${time}`);
  return index;
}

/**
 * The keyframe at `from` moved to `to`, kept strictly between its neighbours (a tick from each), so
 * the order of keyframes never changes. Returns the value and where the keyframe landed.
 *
 * Throws when `value` is not keyframed or has no keyframe at `from`. The keyframe-editing
 * functions below throw in the same cases.
 */
export function moveKeyframe<T>(
  value: Animated<T>,
  from: number,
  to: number,
): { readonly value: Animated<T>; readonly time: number } {
  const keys = keyframesOf(value);
  const index = indexAt(keys, from);
  const low = index > 0 ? keys[index - 1].time + 1 : -Infinity;
  const high = index + 1 < keys.length ? keys[index + 1].time - 1 : Infinity;
  const time = Math.round(Math.min(high, Math.max(low, to)));
  keys[index] = { ...keys[index], time };
  return { value: { mode: "keyframes", keyframes: keys }, time };
}

/** The keyframe at `time` removed; the last one removed leaves its value, constant. */
export function removeKeyframe<T>(
  value: Animated<T>,
  time: number,
): Animated<T> {
  const keys = keyframesOf(value);
  const [removed] = keys.splice(indexAt(keys, time), 1);
  return keys.length
    ? { mode: "keyframes", keyframes: keys }
    : { mode: "constant", value: removed.value };
}

/** How the keyframe at `time` eases to the next. */
export function setInterpolation<T>(
  value: Animated<T>,
  time: number,
  interpolation: Interpolation,
): Animated<T> {
  const keys = keyframesOf(value);
  const index = indexAt(keys, time);
  keys[index] = { ...keys[index], interpolation };
  return { mode: "keyframes", keyframes: keys };
}

/**
 * The timing curve from the keyframe at `time` to the next, as its two Bezier handles: `x` in
 * `0..=1` of the way in time, `y` any fraction of the way in value (beyond `0..=1` overshoots).
 */
export function setHandles<T>(
  value: Animated<T>,
  time: number,
  handles: { x1: number; y1: number; x2: number; y2: number },
): Animated<T> {
  const clampX = (x: number) => Math.min(1, Math.max(0, x));
  return setInterpolation(value, time, {
    type: "cubic_bezier",
    x1: clampX(handles.x1),
    y1: handles.y1,
    x2: clampX(handles.x2),
    y2: handles.y2,
  });
}

/** The keyframe at `time` given `next` as its value. */
export function setKeyframeValue<T>(
  value: Animated<T>,
  time: number,
  next: T,
): Animated<T> {
  const keys = keyframesOf(value);
  const index = indexAt(keys, time);
  keys[index] = { ...keys[index], value: next };
  return { mode: "keyframes", keyframes: keys };
}

/**
 * The handles a timing curve has in a curve editor: a Bezier's own, and for a linear segment the
 * straight line's (a third and two thirds of the way). A held segment has none.
 */
export function handlesOf(
  interpolation: Interpolation,
): { x1: number; y1: number; x2: number; y2: number } | null {
  if (interpolation.type === "cubic_bezier")
    return {
      x1: interpolation.x1,
      y1: interpolation.y1,
      x2: interpolation.x2,
      y2: interpolation.y2,
    };
  if (interpolation.type === "linear")
    return { x1: 1 / 3, y1: 1 / 3, x2: 2 / 3, y2: 2 / 3 };
  return null;
}

/** Newton steps tried before bisection takes over. */
const NEWTON_STEPS = 8;
/** Bisection halvings; 2^-64 is far below the tolerance. */
const BISECTION_STEPS = 64;
/** Accepted distance between the curve's `x` and the requested progress. */
const TOLERANCE = 1e-12;
/** A slope this flat would send a Newton step far outside the curve. */
const MIN_SLOPE = 1e-9;

/**
 * A CSS `cubic-bezier(x1, y1, x2, y2)` curve's output at linear `progress` (clamped to `0..=1`),
 * bit for bit as the engine evaluates it (`CubicBezier::apply` in alpha2-model): the handles as the
 * 32-bit floats a document holds, Newton from `t = progress` while it stays on the curve, then
 * bisection, which converges where the curve is flat (both `x` handles near one end). A display
 * that shows a keyframed value takes it from here, so it shows what the render draws.
 */
export function cubicBezierAt(
  curve: { x1: number; y1: number; x2: number; y2: number },
  progress: number,
): number {
  const x = Math.min(1, Math.max(0, progress));
  if (x <= 0) return 0;
  if (x >= 1) return 1;
  const x1 = Math.fround(curve.x1);
  const x2 = Math.fround(curve.x2);
  const parameter = newton(x, x1, x2) ?? bisection(x, x1, x2);
  return component(parameter, Math.fround(curve.y1), Math.fround(curve.y2));
}

function newton(x: number, x1: number, x2: number): number | undefined {
  let parameter = x;
  for (let step = 0; step < NEWTON_STEPS; step += 1) {
    const error = component(parameter, x1, x2) - x;
    if (Math.abs(error) < TOLERANCE)
      return parameter >= 0 && parameter <= 1 ? parameter : undefined;
    const slope = slopeAt(parameter, x1, x2);
    if (Math.abs(slope) < MIN_SLOPE) return undefined;
    parameter -= error / slope;
  }
  return undefined;
}

function bisection(x: number, x1: number, x2: number): number {
  let low = 0;
  let high = 1;
  let parameter = x;
  for (let step = 0; step < BISECTION_STEPS; step += 1) {
    const value = component(parameter, x1, x2);
    if (Math.abs(value - x) < TOLERANCE) break;
    if (value < x) low = parameter;
    else high = parameter;
    parameter = low + (high - low) * 0.5;
  }
  return parameter;
}

/** One coordinate of the curve at parameter `t`, in the engine's Horner form. */
function component(t: number, p1: number, p2: number): number {
  const c = 3 * p1;
  const b = 3 * (p2 - p1) - c;
  const a = 1 - c - b;
  return ((a * t + b) * t + c) * t;
}

function slopeAt(t: number, p1: number, p2: number): number {
  const c = 3 * p1;
  const b = 3 * (p2 - p1) - c;
  const a = 1 - c - b;
  return (3 * a * t + 2 * b) * t + c;
}
