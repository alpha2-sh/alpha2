import type { FrameRate } from "./generated/project-schema.js";
import { TICKS_PER_SECOND } from "./generated/project-schema.js";
import { secondsToTicks, ticksToSeconds, type Ticks } from "./project.js";

/** The frame grid of one rate, with frame starts floored to ticks as the engine places them. */
export interface Frames {
  readonly rate: FrameRate;
  /** Seconds at which frame `index` starts. */
  start(index: number): number;
  /** Ticks at which frame `index` starts. */
  startTicks(index: number): Ticks;
  /** The frame whose start is nearest `seconds`; a tie takes the later frame. */
  index(seconds: number): number;
  /** `seconds` moved to the nearest frame start. */
  snap(seconds: number): number;
  /** The start of the last frame shown before an exclusive `end` in seconds. */
  last(end: number): number;
}

/**
 * The frame grid of `rate`: whole frames per second, or `{ num, den }` (`{ num: 30000, den: 1001 }`
 * for 29.97).
 *
 * @throws RangeError When the rate is not positive integers.
 */
export function frames(rate: number | FrameRate): Frames {
  const { num, den } = typeof rate === "number" ? { num: rate, den: 1 } : rate;
  if (
    !Number.isSafeInteger(num) ||
    !Number.isSafeInteger(den) ||
    num <= 0 ||
    den <= 0
  )
    throw new RangeError(
      `${JSON.stringify(rate)} is not a frame rate: write fps or { num, den }`,
    );
  const n = BigInt(num);
  const scale = BigInt(TICKS_PER_SECOND) * BigInt(den);
  // Floor division, as BigInt division truncates toward zero.
  const floorDiv = (a: bigint, b: bigint) =>
    a / b - (a % b !== 0n && a < 0n !== b < 0n ? 1n : 0n);
  const startTicks = (index: number): Ticks =>
    Number(floorDiv(BigInt(index) * scale, n));
  const start = (index: number) => ticksToSeconds(startTicks(index));
  const index = (seconds: number) =>
    Number(
      floorDiv(2n * BigInt(secondsToTicks(seconds)) * n + scale, 2n * scale),
    );
  return Object.freeze({
    rate: Object.freeze({ num, den }),
    start,
    startTicks,
    index,
    snap: (seconds: number) => start(index(seconds)),
    last(end: number) {
      const before = BigInt(secondsToTicks(end)) - 1n;
      return start(Number(-floorDiv(-(before + 1n) * n, scale) - 1n));
    },
  });
}
