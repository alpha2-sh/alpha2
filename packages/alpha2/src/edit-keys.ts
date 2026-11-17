// Bus automation under timeline edits, mirroring `shift_keys` in `alpha2_eval::edit` and
// `CubicBezier::between` in `alpha2_model`.

import { shortestFloat32 } from "./float32.js";
import type {
  Animated,
  Interpolation,
  Keyframe,
} from "./generated/project-schema.js";

type Curve = Extract<Interpolation, { readonly type: "cubic_bezier" }>;
type Keys = readonly Keyframe<number>[];

const NEWTON_STEPS = 8;
const BISECTION_STEPS = 64;
const TOLERANCE = 1e-12;
const MIN_SLOPE = 1e-9;
const F32_MAX = 3.4028234663852886e38;

function component(t: number, p1: number, p2: number): number {
  const c = 3 * p1;
  const b = 3 * (p2 - p1) - c;
  const a = 1 - c - b;
  return ((a * t + b) * t + c) * t;
}

function slope(t: number, p1: number, p2: number): number {
  const c = 3 * p1;
  const b = 3 * (p2 - p1) - c;
  const a = 1 - c - b;
  return (3 * a * t + 2 * b) * t + c;
}

/** The Bezier parameter whose `x` is `x` in `(0, 1)` (mirrors `CubicBezier::parameter_at`). */
function parameterAt(x: number, x1: number, x2: number): number {
  let parameter = x;
  for (let step = 0; step < NEWTON_STEPS; step += 1) {
    const error = component(parameter, x1, x2) - x;
    if (Math.abs(error) < TOLERANCE) {
      if (parameter >= 0 && parameter <= 1) return parameter;
      break;
    }
    const steepness = slope(parameter, x1, x2);
    if (Math.abs(steepness) < MIN_SLOPE) break;
    parameter -= error / steepness;
  }
  let low = 0;
  let high = 1;
  parameter = x;
  for (let step = 0; step < BISECTION_STEPS; step += 1) {
    const value = component(parameter, x1, x2);
    if (Math.abs(value - x) < TOLERANCE) break;
    if (value < x) low = parameter;
    else high = parameter;
    parameter = low + (high - low) * 0.5;
  }
  return parameter;
}

/** Mirrors `CubicBezier::apply`. */
function apply(curve: Curve, progress: number): number {
  const x = Math.min(1, Math.max(0, progress));
  if (x <= 0) return 0;
  if (x >= 1) return 1;
  const parameter = parameterAt(
    x,
    Math.fround(curve.x1),
    Math.fround(curve.x2),
  );
  return component(parameter, Math.fround(curve.y1), Math.fround(curve.y2));
}

function blossom(
  points: readonly number[],
  u: number,
  v: number,
  w: number,
): number {
  const lerp = (a: number, b: number, t: number) => a + (b - a) * t;
  const first = [
    lerp(points[0], points[1], u),
    lerp(points[1], points[2], u),
    lerp(points[2], points[3], u),
  ];
  return lerp(lerp(first[0], first[1], v), lerp(first[1], first[2], v), w);
}

/** The part of `curve` between linear progress `from` and `to`, rescaled (mirrors `between`). */
function between(curve: Curve, from: number, to: number): Curve | undefined {
  const x1 = Math.fround(curve.x1);
  const x2 = Math.fround(curve.x2);
  const at = (progress: number) =>
    progress <= 0 ? 0 : progress >= 1 ? 1 : parameterAt(progress, x1, x2);
  const [u, v] = [at(from), at(to)];
  const part = (p1: number, p2: number) => {
    const points = [0, p1, p2, 1];
    return [
      blossom(points, u, u, u),
      blossom(points, u, u, v),
      blossom(points, u, v, v),
      blossom(points, v, v, v),
    ];
  };
  const x = part(x1, x2);
  const y = part(Math.fround(curve.y1), Math.fround(curve.y2));
  const width = x[3] - x[0];
  const height = y[3] - y[0];
  if (!(width > 0) || height === 0) return undefined;
  const unit = (value: number) => Math.min(1, Math.max(0, value));
  const result: Curve = {
    type: "cubic_bezier",
    x1: shortestFloat32(unit((x[1] - x[0]) / width)),
    y1: shortestFloat32((y[1] - y[0]) / height),
    x2: shortestFloat32(unit((x[2] - x[0]) / width)),
    y2: shortestFloat32((y[2] - y[0]) / height),
  };
  return [result.x1, result.y1, result.x2, result.y2].every(Number.isFinite)
    ? result
    : undefined;
}

/** The value of keyframes `keys` at `tick` (mirrors the scalar keyframe sampler). */
function valueAt(keys: Keys, tick: number): number {
  let next = 0;
  for (let high = keys.length; next < high;) {
    const middle = (next + high) >> 1;
    if (keys[middle].time <= tick) next = middle + 1;
    else high = middle;
  }
  if (next === 0) return Math.fround(keys[0].value);
  const left = keys[next - 1];
  const right = keys[next];
  if (!right || tick === left.time) return Math.fround(left.value);
  const progress = (tick - left.time) / (right.time - left.time);
  const interpolation = left.interpolation;
  const amount =
    interpolation.type === "hold"
      ? undefined
      : interpolation.type === "linear"
        ? Math.min(1, Math.max(0, progress))
        : apply(interpolation, progress);
  if (amount === undefined) return Math.fround(left.value);
  const start = Math.fround(left.value);
  const end = Math.fround(right.value);
  return Math.fround(
    Math.min(F32_MAX, Math.max(-F32_MAX, start + (end - start) * amount)),
  );
}

/** The interpolation showing the old curve from `from` to `to` (mirrors `part_of_curve`). */
function partOfCurve(
  keys: Keys,
  from: number,
  to: number,
): Interpolation | undefined {
  let next = 0;
  while (next < keys.length && keys[next].time <= from) next += 1;
  const start = keys[next - 1];
  const end = keys[next];
  if (!start || !end) return undefined;
  const interpolation = start.interpolation;
  if (interpolation.type !== "cubic_bezier") return interpolation;
  if (from === start.time && to === end.time) return interpolation;
  const span = end.time - start.time;
  return (
    between(
      interpolation,
      (from - start.time) / span,
      (to - start.time) / span,
    ) ?? {
      type: "linear",
    }
  );
}

/** Sorted, disjoint, non-empty `[start, end)` tick ranges. */
type Removal = readonly (readonly [number, number])[];

function shiftOf(tick: number, removal: Removal): number {
  let shift = 0;
  for (const [from, to] of removal)
    shift += to <= tick ? to - from : from < tick ? tick - from : 0;
  return shift;
}

/**
 * Bus keyframes after `removal` leaves a rippled timeline: keys inside go, the rest move, the
 * value at each cut is keyed on both sides, and every key takes the part of the old curve up to
 * the next key (mirrors `shift_keys`).
 */
export function closeKeys(
  animation: Animated<number> | undefined,
  removal: Removal,
): Animated<number> | undefined {
  if (animation?.mode !== "keyframes" || !removal.length) return animation;
  const old = animation.keyframes;
  const removed = (tick: number) =>
    removal.some(([from, to]) => from <= tick && tick < to);
  const at: [number, number | undefined][] = [];
  old.forEach((key, index) => {
    if (!removed(key.time)) at.push([key.time, index]);
  });
  for (const [from, to] of removal) {
    if (from - 1 - shiftOf(from - 1, removal) >= 0)
      at.push([from - 1, undefined]);
    at.push([to, undefined]);
  }
  at.sort(
    (a, b) =>
      a[0] - b[0] || Number(a[1] === undefined) - Number(b[1] === undefined),
  );
  const unique = at.filter(
    (entry, index) => index === 0 || at[index - 1][0] !== entry[0],
  );
  const keyframes = unique.map(([tick, kept], position): Keyframe<number> => {
    const next = unique[position + 1]?.[0];
    const own = kept === undefined ? undefined : old[kept].interpolation;
    const interpolation: Interpolation =
      next === undefined
        ? (own ?? { type: "linear" })
        : removal.some(([from, to]) => tick < from && to <= next)
          ? { type: "hold" }
          : (partOfCurve(old, tick, next) ?? own ?? { type: "linear" });
    return {
      time: tick - shiftOf(tick, removal),
      value:
        kept === undefined
          ? shortestFloat32(valueAt(old, tick))
          : old[kept].value,
      interpolation,
    };
  });
  return { ...animation, keyframes };
}

/**
 * Bus keyframes after `length` ticks open at `pivot`: keys at or after it move right, and when
 * keys lie on both sides the value at `pivot` is held across the opening, so every moment outside
 * it reads as before.
 */
export function openKeys(
  animation: Animated<number> | undefined,
  pivot: number,
  length: number,
): Animated<number> | undefined {
  if (animation?.mode !== "keyframes" || !length) return animation;
  const old = animation.keyframes;
  const before = old.filter((key) => key.time < pivot);
  const after = old.filter((key) => key.time >= pivot);
  const moved = after.map((key) => ({ ...key, time: key.time + length }));
  if (!before.length || !after.length)
    return { ...animation, keyframes: [...before, ...moved] };
  const held = shortestFloat32(valueAt(old, pivot));
  const last = before[before.length - 1];
  const head = before.slice(0, -1);
  const keyframes: Keyframe<number>[] = [
    ...head,
    {
      ...last,
      interpolation: partOfCurve(old, last.time, pivot) ?? last.interpolation,
    },
    { time: pivot, value: held, interpolation: { type: "hold" } },
  ];
  if (after[0].time === pivot) keyframes.push(...moved);
  else
    keyframes.push(
      {
        time: pivot + length,
        value: held,
        interpolation: partOfCurve(old, pivot, after[0].time) ?? {
          type: "linear",
        },
      },
      ...moved,
    );
  return { ...animation, keyframes };
}
