// Timeline-anchored data and `via` under timeline edits, mirroring `alpha2_eval::edit`.

import { shortestFloat32 } from "./float32.js";
import type { DataSet, Project } from "./generated/project-schema.js";

/** Sorted, disjoint, non-empty `[start, end)` tick ranges. */
type Removal = readonly (readonly [number, number])[];

type SampleSet = Extract<DataSet, { readonly times: readonly number[] }>;

const F32_MAX = 3.4028234663852886e38;

/** The removed length before `tick` (mirrors `shift_of`). */
function shiftOf(tick: number, removal: Removal): number {
  let shift = 0;
  for (const [from, to] of removal)
    shift += to <= tick ? to - from : from < tick ? tick - from : 0;
  return shift;
}

/** A kept tick's new position, `undefined` for a removed one (mirrors `new_time`). */
function newTime(tick: number, removal: Removal): number | undefined {
  if (removal.some(([from, to]) => from <= tick && tick < to)) return undefined;
  return tick - shiftOf(tick, removal);
}

/** Mirrors `kept_windows_ticks`. */
function keptWindows(
  start: number,
  end: number,
  removal: Removal,
): [number, number][] {
  const windows: [number, number][] = [];
  let cursor = start;
  for (const [from, to] of removal) {
    if (to <= cursor || from >= end) continue;
    if (from > cursor) windows.push([cursor, from]);
    cursor = Math.max(cursor, to);
  }
  if (cursor < end) windows.push([cursor, end]);
  return windows;
}

function arityOf(set: SampleSet): number {
  return set.kind === "points" ? 2 : set.kind === "similarity" ? 4 : 1;
}

function channelCount(set: SampleSet): number {
  return Math.max("channels" in set ? set.channels.length : 0, 1);
}

/**
 * The 32-bit value of `channel` at `time`, or `undefined` when the channel has no known value
 * (mirrors `sample_channel`).
 */
function sampleChannel(
  set: SampleSet,
  channel: number,
  arity: number,
  time: number,
): number[] | undefined {
  const width = channelCount(set) * arity;
  const { times, values } = set;
  const known = (sample: number) => {
    const start = sample * width + channel * arity;
    return sample < times.length && values[start] !== null
      ? values.slice(start, start + arity).map((v) => Math.fround(v ?? 0))
      : undefined;
  };
  let next = 0;
  for (let high = times.length; next < high;) {
    const middle = (next + high) >> 1;
    if (times[middle] <= time) next = middle + 1;
    else high = middle;
  }
  const first = (from: number, to: number, step: number) => {
    for (let sample = from; sample !== to; sample += step) {
      const value = known(sample);
      if (value) return value;
    }
    return undefined;
  };
  const current = next - 1;
  if (current < 0) return first(0, times.length, 1);
  const left = known(current);
  if (left) {
    const right =
      set.interpolation === "linear" && times[current] < time
        ? known(next)
        : undefined;
    if (!right) return left;
    const amount = (time - times[current]) / (times[next] - times[current]);
    return left.map((from, index) =>
      Math.fround(
        Math.min(
          F32_MAX,
          Math.max(-F32_MAX, from + (right[index] - from) * amount),
        ),
      ),
    );
  }
  return first(current - 1, -1, -1) ?? first(next, times.length, 1);
}

/** The values each channel reads at `time`, written as the engine writes 32-bit floats. */
function heldAt(set: SampleSet, time: number): (number | null)[] {
  const arity = arityOf(set);
  const held: (number | null)[] = [];
  for (let channel = 0; channel < channelCount(set); channel++) {
    const value = sampleChannel(set, channel, arity, time);
    held.push(
      ...(value
        ? value.map(shortestFloat32)
        : new Array<null>(arity).fill(null)),
    );
  }
  return held;
}

/** `set` with samples `entries` as `[tick, values]`, sorted by tick. */
function withSamples(
  set: SampleSet,
  entries: [number, readonly (number | null)[]][],
): SampleSet {
  entries.sort((a, b) => a[0] - b[0]);
  return {
    ...set,
    times: entries.map(([tick]) => tick),
    values: entries.flatMap(([, values]) => values),
  };
}

function samplesOf(
  set: SampleSet,
  time: (tick: number) => number | undefined,
): [number, readonly (number | null)[]][] {
  const width = channelCount(set) * arityOf(set);
  const entries: [number, readonly (number | null)[]][] = [];
  set.times.forEach((tick, index) => {
    const moved = time(tick);
    if (moved !== undefined)
      entries.push([
        moved,
        set.values.slice(index * width, (index + 1) * width),
      ]);
  });
  return entries;
}

/** An item's new edges past removed ranges, `undefined` when none stays (mirrors `cut_item`). */
function cutItem(
  start: number,
  end: number,
  removal: Removal,
  instants: boolean,
): [number, number] | undefined {
  const windows = keptWindows(start, Math.max(end, start + 1), removal);
  if (!windows.length) return undefined;
  const from = windows[0][0] - shiftOf(windows[0][0], removal);
  const last = windows[windows.length - 1][1] - 1;
  const to = end === start ? from : last - shiftOf(last, removal) + 1;
  return instants || from < to ? [from, to] : undefined;
}

function byEdges<T extends { readonly start: number; readonly end: number }>(
  items: T[],
): T[] {
  return items.sort((a, b) => a.start - b.start || a.end - b.end);
}

/**
 * A timeline-anchored set with `removal` cut out and later times moved left; a sample set keeps
 * the value read just after each cut (mirrors `split::cut_data`).
 */
export function cutData(set: DataSet, removal: Removal): DataSet {
  if (set.kind === "words" || set.kind === "intervals") {
    const instants = set.kind === "intervals";
    const cut = <T extends { readonly start: number; readonly end: number }>(
      items: readonly T[],
    ) =>
      byEdges(
        items.flatMap((item) => {
          const edges = cutItem(item.start, item.end, removal, instants);
          return edges ? [{ ...item, start: edges[0], end: edges[1] }] : [];
        }),
      );
    return set.kind === "words"
      ? { ...set, words: cut(set.words) }
      : { ...set, intervals: cut(set.intervals) };
  }
  if (!set.times.length) return set;
  const entries = samplesOf(set, (tick) => newTime(tick, removal));
  const ticks = new Set(entries.map(([tick]) => tick));
  for (const [, to] of removal) {
    const at = newTime(to, removal);
    if (at === undefined || ticks.has(at)) continue;
    ticks.add(at);
    entries.push([at, heldAt(set, to)]);
  }
  return withSamples(set, entries);
}

/**
 * Opens `delta > 0` ticks at `pivot` in a timeline-anchored set. Later times move right, an item
 * across `pivot` lengthens, and a sample set holds the value read at `pivot` on both sides of the
 * opening, so every moment outside it reads as before.
 */
export function openData(set: DataSet, pivot: number, delta: number): DataSet {
  if (set.kind === "words" || set.kind === "intervals") {
    const open = <T extends { readonly start: number; readonly end: number }>(
      items: readonly T[],
    ) =>
      byEdges(
        items.map((item) =>
          item.start >= pivot
            ? { ...item, start: item.start + delta, end: item.end + delta }
            : item.end > pivot
              ? { ...item, end: item.end + delta }
              : item,
        ),
      );
    return set.kind === "words"
      ? { ...set, words: open(set.words) }
      : { ...set, intervals: open(set.intervals) };
  }
  const entries = samplesOf(set, (tick) =>
    tick >= pivot ? tick + delta : tick,
  );
  if (set.times.some((t) => t < pivot) && set.times.some((t) => t >= pivot)) {
    const held = heldAt(set, pivot);
    const ticks = new Set(entries.map(([tick]) => tick));
    for (const at of [pivot, pivot + delta])
      if (!ticks.has(at)) entries.push([at, held]);
  }
  return withSamples(set, entries);
}

/**
 * Edits `project`'s timeline-anchored data sets as a ripple of `delta` at `pivot` edits the
 * timeline. A negative `delta` removes `[pivot + delta, pivot)` and a positive one opens that much
 * time at `pivot`. Source-anchored sets never change.
 */
export function rippleTimelineData(
  project: Project,
  pivot: number,
  delta: number,
): Project {
  if (!delta || !project.data?.some((set) => set.anchor.type === "timeline"))
    return project;
  const removal: Removal = [[pivot + delta, pivot]];
  return {
    ...project,
    data: project.data.map((set) =>
      set.anchor.type !== "timeline"
        ? set
        : delta < 0
          ? cutData(set, removal)
          : openData(set, pivot, delta),
    ),
  };
}

/** `value` with every data binding that reads through clip `from` reading through `to`. */
export function renameVia<T>(value: T, from: string, to: string): T {
  if (Array.isArray(value))
    return value.map((item: unknown) => renameVia(item, from, to)) as T;
  if (typeof value !== "object" || value === null) return value;
  const record = value as Record<string, unknown>;
  if (record.mode === "data") {
    const via = record.via as { clip?: unknown } | null | undefined;
    return via?.clip === from ? ({ ...record, via: { clip: to } } as T) : value;
  }
  let changed = false;
  const next: Record<string, unknown> = {};
  for (const [key, field] of Object.entries(record)) {
    next[key] = renameVia(field, from, to);
    changed ||= next[key] !== field;
  }
  return changed ? (next as T) : value;
}
