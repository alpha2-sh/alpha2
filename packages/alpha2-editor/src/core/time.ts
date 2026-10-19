import { TICKS_PER_SECOND } from "alpha2/project";
export { TICKS_PER_SECOND };
/** A frame or sample rate as `num / den` per second; both positive safe integers. */
export interface Rational {
  readonly num: number;
  readonly den: number;
}
function integer(n: number): bigint {
  if (!Number.isSafeInteger(n) || n < 0)
    throw new RangeError("Expected a non-negative safe integer");
  return BigInt(n);
}
function rate(r: Rational): [bigint, bigint] {
  const n = integer(r.num),
    d = integer(r.den);
  if (!n || !d) throw new RangeError("Rate must be positive");
  return [n, d];
}
function safe(n: bigint): number {
  const v = Number(n);
  if (!Number.isSafeInteger(v)) throw new RangeError("Tick overflow");
  return v;
}
/**
 * The first tick of frame `frame` at `fps`, rounded down. Throws a `RangeError` for a negative or
 * unsafe frame, a non-positive rate, or a tick past `Number.MAX_SAFE_INTEGER`.
 */
export function frameStart(frame: number, fps: Rational): number {
  const [n, d] = rate(fps);
  return safe((integer(frame) * BigInt(TICKS_PER_SECOND) * d) / n);
}
/**
 * The frame containing `tick` at `fps`: the inverse of {@link frameStart}, whose frames are
 * half-open `[start, next start)`, including at fractional rates. Throws like `frameStart`.
 */
export function frameAt(tick: number, fps: Rational): number {
  const [n, d] = rate(fps);
  return safe(((integer(tick) + 1n) * n - 1n) / (BigInt(TICKS_PER_SECOND) * d));
}
/** The first tick of audio sample `sample` at `sampleRate` Hz, rounded down. */
export function sampleStart(sample: number, sampleRate: number): number {
  return frameStart(sample, { num: sampleRate, den: 1 });
}
