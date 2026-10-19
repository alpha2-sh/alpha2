import type { Clip } from "alpha2/project";
import { TICKS_PER_SECOND } from "alpha2/project";

/** A frame rate as the document holds it. */
export interface FrameRateValue {
  readonly num: number;
  readonly den: number;
}

/**
 * The source instant `clip`'s picture shows at timeline tick `tick`, as the engine draws it
 * (`picture_source_time` in alpha2-eval). With `e` the ticks since the clip starts, `T` its
 * timeline length, `S` its source length, and `F` one frame at `frameRate`:
 *
 * - forward: `floor(e*S/T)` past the source start;
 * - reverse: the forward instant at the start of the mirrored project frame, less the time into the
 *   frame, so a reversed clip shows the forward clip's frames in the opposite order, each at the
 *   instant the forward clip shows it, at any rate and wherever the clip starts and ends (the
 *   source start where that falls before it);
 * - freeze: its instant;
 * - a repeating loop: `e mod S` past the source start;
 * - a ping-pong loop: the same on even passes, and on odd ones the pass mirrored by frame in the
 *   same way.
 *
 * A clip with an empty range reads its source start.
 */
export function clipSourceTime(
  clip: Clip,
  tick: number,
  frameRate: FrameRateValue,
): number {
  const { timeline, source, span, length, per, frame } = parts(clip, frameRate);
  const map = clip.time_map ?? { type: "forward" };
  if (map.type === "freeze") return map.source_time;
  if (span <= 0n || length <= 0n) return Number(source);
  const elapsed = BigInt(Math.floor(tick)) - timeline;
  let offset: bigint;
  if (map.type === "reverse")
    offset = mirrored(timeline, length, elapsed, span, frame, per);
  else if (map.type === "loop") {
    const within = euclid(elapsed, span);
    const pass = floorDiv(elapsed, span);
    offset =
      map.mode === "ping_pong" && euclid(pass, 2n) === 1n
        ? mirrored(timeline + pass * span, span, within, span, frame, per)
        : within;
  } else offset = floorDiv(elapsed * span, length);
  return Number(source + offset);
}

/**
 * The first timeline tick at which `clip`'s picture shows source time `time` (rounded up to a
 * tick), or where it never shows exactly that, the first that shows it passed in the direction of
 * play: the inverse of {@link clipSourceTime} on a clip's first pass. A freeze shows its instant
 * from its start.
 */
export function clipTimelineTime(
  clip: Clip,
  time: number,
  frameRate: FrameRateValue,
): number {
  const { timeline, source, span, length, per, frame } = parts(clip, frameRate);
  const map = clip.time_map ?? { type: "forward" };
  if (map.type === "freeze" || span <= 0n || length <= 0n)
    return Number(timeline);
  const k = BigInt(Math.ceil(time)) - source;
  let elapsed: bigint;
  if (map.type === "loop") elapsed = k;
  else if (map.type === "reverse") {
    // The mirrored instant never rises with `e`: binary-search the first `e` showing `k` or earlier.
    let low = 0n;
    let high = length;
    while (low < high) {
      const middle = (low + high) / 2n;
      if (mirrored(timeline, length, middle, span, frame, per) <= k)
        high = middle;
      else low = middle + 1n;
    }
    elapsed = low;
  } else elapsed = ceilDiv(k * length, span);
  return Number(timeline + elapsed);
}

function parts(clip: Clip, frameRate: FrameRateValue) {
  const timeline = BigInt(clip.timeline_range.start);
  const source = BigInt(clip.source_range.start);
  return {
    timeline,
    source,
    length: BigInt(clip.timeline_range.end) - timeline,
    span: BigInt(clip.source_range.end) - source,
    frame: BigInt(TICKS_PER_SECOND) * BigInt(frameRate.den),
    per: BigInt(frameRate.num),
  };
}

/**
 * The forward offset `floor(e' * scale / length)` the mirror of `elapsed` shows within `length`
 * ticks from timeline tick `start`, by project frames of `frame / per` ticks (alpha2-eval's
 * `Mirror::offset`): with `k0` and `k1` the first and last frames starting inside and `k` the one
 * containing the instant (`k0` before it, with no time into it), `e'` is the start of `k0 + k1 - k`
 * less the time into `k`, from `start`, or zero where that falls before it. A window no frame
 * starts in shows its start.
 */
function mirrored(
  start: bigint,
  length: bigint,
  elapsed: bigint,
  scale: bigint,
  frame: bigint,
  per: bigint,
): bigint {
  const startOf = (k: bigint) => floorDiv(k * frame, per);
  const frameOf = (tick: bigint) => floorDiv((tick + 1n) * per - 1n, frame);
  const first = frameOf(start - 1n) + 1n;
  const last = frameOf(start + length - 1n);
  if (first > last) return 0n;
  const tick = start + elapsed;
  let k = frameOf(tick);
  let into = 0n;
  if (k < first) k = first;
  else into = tick - startOf(k);
  const position = startOf(first + last - k) - into - start;
  if (position <= 0n) return 0n;
  return floorDiv(position * scale, length);
}

function floorDiv(value: bigint, divisor: bigint): bigint {
  const quotient = value / divisor;
  return value % divisor !== 0n && value < 0n !== divisor < 0n
    ? quotient - 1n
    : quotient;
}

function ceilDiv(value: bigint, divisor: bigint): bigint {
  return -floorDiv(-value, divisor);
}

function euclid(value: bigint, divisor: bigint): bigint {
  const remainder = value % divisor;
  return remainder < 0n ? remainder + divisor : remainder;
}
