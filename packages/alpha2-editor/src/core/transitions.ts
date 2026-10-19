import type {
  ClipEffect,
  ClipTransition,
  TimeRange,
  Track,
} from "alpha2/project";
import { EditorError } from "./document.js";
const fail = (message: string): never => {
  throw new EditorError("invalid_transition", message, "transition");
};
/**
 * `value` with every keyframe time moved `delta` ticks. Every `Animated<T>` in a clip, masks
 * included, uses timeline ticks; its composition keeps its own time.
 */
function shifted<T>(value: T, delta: number): T {
  if (!value || typeof value !== "object") return value;
  if (Array.isArray(value))
    return value.map((v: unknown) => shifted(v, delta)) as T;
  const object = value as Record<string, unknown>;
  if (object.mode === "keyframes" && Array.isArray(object.keyframes))
    return {
      ...object,
      keyframes: object.keyframes.map((key: Record<string, unknown>) => ({
        ...key,
        time: (key.time as number) + delta,
      })),
    } as T;
  return Object.fromEntries(
    Object.entries(object).map(([key, v]) => [key, shifted(v, delta)]),
  ) as T;
}
/**
 * `track` with clip `id` starting at `start`, and the clips that follow it through transitions (its
 * outgoing transition's clip, that clip's, and so on) moved as far with their transitions, so each
 * overlap the chain holds stays exact. Collisions with other clips are for validation to refuse.
 */
function moveChain(track: Track, id: string, start: number): Track {
  const clip = track.clips!.find((c) => c.id === id)!;
  const delta = start - clip.timeline_range.start;
  if (delta === 0) return track;
  const chain = new Set<string>();
  for (let at: string | undefined = id; at !== undefined && !chain.has(at);) {
    chain.add(at);
    const from: string = at;
    at = track.transitions?.find((t) => t.from_clip === from)?.to_clip;
  }
  const range = (r: TimeRange): TimeRange => ({
    start: r.start + delta,
    end: r.end + delta,
  });
  return {
    ...track,
    clips: track.clips!.map((c) =>
      chain.has(c.id)
        ? { ...shifted(c, delta), timeline_range: range(c.timeline_range) }
        : c,
    ),
    transitions: track.transitions?.map((t) =>
      chain.has(t.from_clip)
        ? { ...shifted(t, delta), timeline_range: range(t.timeline_range) }
        : t,
    ),
  };
}
/**
 * `track` with a transition of `duration` ticks from clip `fromId` into `toId`, ending where
 * `fromId` ends. `toId` and the clips chained after it move earlier so it overlaps `fromId` by
 * `duration`.
 *
 * Throws an `EditorError` (`invalid_transition`) when the clips are missing or the same, either
 * edge already has a transition, or `duration` is not positive and shorter than both clips.
 */
export function addTransition(
  track: Track,
  fromId: string,
  toId: string,
  duration: number,
  effect: ClipEffect,
): Track {
  const from = track.clips?.find((c) => c.id === fromId),
    to = track.clips?.find((c) => c.id === toId);
  if (!from || !to || fromId === toId)
    fail("Select two different clips on the same track");
  if (
    (track.transitions ?? []).some(
      (t) => t.from_clip === fromId || t.to_clip === toId,
    )
  )
    fail("This clip edge already has a transition");
  const transition: ClipTransition = {
    from_clip: fromId,
    to_clip: toId,
    timeline_range: {
      start: from!.timeline_range.end - duration,
      end: from!.timeline_range.end,
    },
    effect,
    easing: { type: "linear" },
  };
  return resizeTransition(
    { ...track, transitions: [...(track.transitions ?? []), transition] },
    (track.transitions ?? []).length,
    duration,
  );
}
/**
 * `track` with transition `index` given `duration` ticks, keeping its end at the outgoing clip's
 * end and moving the incoming clip's chain to match. Throws like {@link addTransition}.
 */
export function resizeTransition(
  track: Track,
  index: number,
  duration: number,
): Track {
  const transition = track.transitions?.[index];
  if (!transition) fail("Unknown transition");
  const from = track.clips!.find((c) => c.id === transition!.from_clip)!,
    to = track.clips!.find((c) => c.id === transition!.to_clip)!;
  if (
    !Number.isSafeInteger(duration) ||
    duration <= 0 ||
    duration >=
      Math.min(
        from.timeline_range.end - from.timeline_range.start,
        to.timeline_range.end - to.timeline_range.start,
      )
  )
    fail("Transition duration must be positive and shorter than both clips");
  const start = from.timeline_range.end - duration;
  const moved = moveChain(track, to.id, start);
  return {
    ...moved,
    transitions: moved.transitions!.map((t, i) =>
      i === index
        ? { ...t, timeline_range: { start, end: from.timeline_range.end } }
        : t,
    ),
  };
}
/**
 * `track` without transition `index`. The incoming clip and its chain move later so it starts
 * where the outgoing clip ends. Throws an `EditorError` for an unknown index.
 */
export function removeTransition(track: Track, index: number): Track {
  const transition = track.transitions?.[index];
  if (!transition) fail("Unknown transition");
  const from = track.clips!.find((c) => c.id === transition!.from_clip)!;
  const moved = moveChain(track, transition!.to_clip, from.timeline_range.end);
  return {
    ...moved,
    transitions: moved.transitions!.filter((_, i) => i !== index),
  };
}
