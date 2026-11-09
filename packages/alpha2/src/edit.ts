/**
 * Document editing operations (split, trim, ripple, roll, slip, slide, retime, move, insert,
 * overwrite, append) as pure functions from one valid project to another.
 *
 * Each returns a new project that passes {@link validateProject}, or a typed error, and never
 * modifies its input. Undo history, selection and snapping are the host's job.
 *
 * Times, deltas and durations are whole ticks (`invalid_time` otherwise). A clip keeps its speed
 * across an edit, so a reversed clip trimmed at its start loses the end of its source.
 *
 * A clip's composition keeps its own time: moving a clip moves its animation with it, trimming the
 * start by `delta` adds `delta * scale` to the offset, and splitting gives the right half a copy
 * ({@link copyComposition}).
 *
 * Edits may not read before a source's start or past its fingerprint's `duration_ticks`
 * (`outside_source`).
 *
 * Ripple edits move later content on the edited track and on `syncTracks` by the same amount. A
 * sync track with content across the edit point, or where pulled content would land on content
 * that stays, refuses the whole edit (`sync_conflict`).
 *
 * Use {@link deferValidation} for a batch of edits that is valid only as a whole.
 */

import type {
  Clip,
  ClipComposition,
  ClipTransition,
  Composition,
  Project,
  TimeRange,
  Track,
} from "./generated/project-schema.js";
import { validateProject } from "./project.js";

/** Why an edit was refused. */
export type EditErrorCode =
  /** No clip or track has the given id. */
  | "unknown_clip"
  | "unknown_track"
  /** The new id is already a clip's. */
  | "duplicate_id"
  /** The time is not strictly inside the clip, or the edit empties it. */
  | "outside_clip"
  /** A time, delta, or duration is not a whole number of ticks. */
  | "invalid_time"
  /** The edit would change a transition's overlap. */
  | "in_transition"
  /** A split would start or end a half part way through a fade. */
  | "in_fade"
  /** Another clip uses this one as its matte. */
  | "matte_source"
  /** A loop cannot start part way through a pass. */
  | "unsupported_time_map"
  /** The clip's source kind does not belong on that track. */
  | "incompatible_track"
  /** Two clips do not meet at one cut, so there is nothing to roll. */
  | "not_adjacent"
  /** A sync track has content across the edit point, or would collide after the edit. */
  | "sync_conflict"
  /** The edit splits a clip without an id for the new part, or splits more than one clip. */
  | "needs_id"
  /** The edit would read the source before its start or past its recorded end. */
  | "outside_source"
  /** The edited document breaks a model rule; `problems` says which. */
  | "invalid_result";

/** A refused edit. Branch on `code`; `message` is human-readable. */
export interface EditError {
  readonly code: EditErrorCode;
  readonly message: string;
  /** For `invalid_result`, what {@link validateProject} reported. */
  readonly problems?: readonly string[];
}

/** The new project, or why the edit was refused. The input is never modified. */
export type EditResult =
  | { readonly ok: true; readonly project: Project }
  | { readonly ok: false; readonly error: EditError };

export type ClipEdge = "start" | "end";

/** How a ripple edit reaches other tracks. */
export interface RippleOptions {
  /** Sync-locked tracks whose content after the edit point moves with the edited track. */
  readonly syncTracks?: readonly string[];
}

/** Options for placing a clip where another must be split. */
export interface PlaceOptions extends RippleOptions {
  /** Id for the split-off part of an existing clip. */
  readonly splitId?: string;
}

/**
 * Splits a clip at `at`. The left part keeps the id, incoming transitions and fade in; the right
 * part becomes `newId` with outgoing transitions, fade out, and its own copy of the composition
 * ({@link copyComposition}). Refused inside a transition or fade, for a matte source, and for a
 * loop.
 */
export function split(
  project: Project,
  clipId: string,
  at: number,
  newId: string,
): EditResult {
  const time = notTicks(at, "the split time");
  if (time) return time;
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  if (clipIds(project).has(newId)) {
    return failure("duplicate_id", `clip ${newId} already exists`);
  }
  const range = clip.timeline_range;
  if (!(at > range.start && at < range.end)) {
    return failure("outside_clip", `${at} is not inside clip ${clipId}`);
  }
  if (
    transitionsOf(track, clipId).some(
      (transition) =>
        at > transition.timeline_range.start &&
        at < transition.timeline_range.end,
    )
  ) {
    return failure("in_transition", `${at} is inside a transition`);
  }
  if (isMatteSource(project, clipId)) {
    return failure("matte_source", `clip ${clipId} is another clip's matte`);
  }
  if (
    (clip.audio?.fade_in ?? 0) > at - range.start ||
    (clip.audio?.fade_out ?? 0) > range.end - at
  ) {
    return failure("in_fade", `${at} is inside a fade of clip ${clipId}`);
  }
  const sources = splitSource(clip, at);
  if (sources === undefined) {
    return failure("unsupported_time_map", `a loop cannot be split`);
  }
  const audio = clip.audio;
  const left: Clip = {
    ...clip,
    timeline_range: { start: range.start, end: at },
    source_range: sources.left,
    ...(audio?.fade_out === undefined
      ? {}
      : { audio: { ...audio, fade_out: 0 } }),
  };
  let edited = project;
  let rightComposition: ClipComposition | undefined;
  if (clip.composition != null) {
    const copy = copyComposition(project, clip.composition.id, newId);
    edited = copy.project;
    rightComposition = {
      ...advance(clip.composition, at - range.start),
      id: copy.id,
    };
  }
  const right: Clip = {
    ...clip,
    id: newId,
    timeline_range: { start: at, end: range.end },
    source_range: sources.right,
    ...(audio?.fade_in === undefined
      ? {}
      : { audio: { ...audio, fade_in: 0 } }),
    ...(rightComposition === undefined
      ? {}
      : { composition: rightComposition }),
  };
  const transitions = (track.transitions ?? []).map((transition) =>
    transition.from_clip === clipId
      ? { ...transition, from_clip: newId }
      : transition,
  );
  return finish(
    replaceTrack(edited, {
      ...track,
      clips: (track.clips ?? []).flatMap((candidate) =>
        candidate.id === clipId ? [left, right] : [candidate],
      ),
      transitions,
    }),
  );
}

/**
 * Moves one edge of a clip to `to`, keeping its speed. Refused for an edge in a transition, before
 * the timeline start, past the source (`outside_source`), or at a loop's start.
 */
export function trim(
  project: Project,
  clipId: string,
  edge: ClipEdge,
  to: number,
): EditResult {
  const time = notTicks(to, "the trim time");
  if (time) return time;
  const trimmed = trimmedClip(project, clipId, edge, to);
  if (!trimmed.ok) return trimmed;
  return finish(replaceClip(project, trimmed.track, trimmed.clip));
}

/**
 * Trims like {@link trim} and shifts later clips on the track by the same amount. A start trim
 * keeps the clip's start in place, so `to` may be before the timeline start.
 */
export function rippleTrim(
  project: Project,
  clipId: string,
  edge: ClipEdge,
  to: number,
  options: RippleOptions = {},
): EditResult {
  const time = notTicks(to, "the trim time");
  if (time) return time;
  const trimmed = trimmedClip(project, clipId, edge, to, true);
  if (!trimmed.ok) return trimmed;
  const { track, clip } = trimmed;
  const before = trimmed.original.timeline_range;
  let edited = clip;
  let delta: number;
  if (edge === "end") {
    delta = to - before.end;
  } else {
    // Trim the head, then slide the clip back to its original start.
    delta = before.start - to;
    edited = shift(clip, delta);
  }
  if (pullsOnto(track, before.end, delta, clipId)) return pulledOnto(track);
  const shifted = (track.clips ?? []).map((candidate) => {
    if (candidate.id === clipId) return edited;
    return candidate.timeline_range.start >= before.end
      ? shift(candidate, delta)
      : candidate;
  });
  const synced = rippleSyncTracks(
    project,
    options.syncTracks,
    track.id,
    before.end,
    delta,
  );
  if (!synced.ok) return synced;
  return finish(
    replaceTrack(synced.project, {
      ...track,
      clips: shifted,
      transitions: shiftTransitions(track, before.end, delta),
    }),
  );
}

/**
 * Removes a clip and its transitions and closes the gap ({@link rippleDeleteSpan}). Refused for a
 * matte source, and when a sync track has content in that span.
 */
export function rippleDelete(
  project: Project,
  clipId: string,
  options: RippleOptions = {},
): EditResult {
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  if (isMatteSource(project, clipId)) {
    return failure("matte_source", `clip ${clipId} is another clip's matte`);
  }
  const range = clip.timeline_range;
  const gap = aloneSpan(track, clip);
  const delta = gap.start - gap.end;
  const clips = (track.clips ?? [])
    .filter((candidate) => candidate.id !== clipId)
    .map((candidate) =>
      candidate.timeline_range.start >= range.start
        ? shift(candidate, delta)
        : candidate,
    );
  const transitions = (track.transitions ?? [])
    .filter(
      (transition) =>
        transition.from_clip !== clipId && transition.to_clip !== clipId,
    )
    .map((transition) =>
      transition.timeline_range.start >= range.start
        ? shiftTransition(transition, delta)
        : transition,
    );
  for (const other of syncTracksOf(project, options.syncTracks, track.id))
    if (occupies(other, gap))
      return failure(
        "sync_conflict",
        `track ${other.id} has content in the time the delete closes`,
      );
  const synced = rippleSyncTracks(
    project,
    options.syncTracks,
    track.id,
    gap.start,
    delta,
  );
  if (!synced.ok) return synced;
  return finish(replaceTrack(synced.project, { ...track, clips, transitions }));
}

/**
 * The span {@link rippleDelete} closes: the part of the clip's span no other clip on its track
 * shares. Empty when fully covered; `undefined` for an unknown clip. Hosts can use it to move their
 * own timeline items, such as markers.
 */
export function rippleDeleteSpan(
  project: Project,
  clipId: string,
): TimeRange | undefined {
  const found = locate(project, clipId);
  return found.ok ? aloneSpan(found.track, found.clip) : undefined;
}

function aloneSpan(track: Track, clip: Clip): TimeRange {
  const range = clip.timeline_range;
  let start = range.start;
  let end = range.end;
  for (const other of track.clips ?? []) {
    if (other.id === clip.id) continue;
    const span = other.timeline_range;
    if (span.start < range.start)
      start = Math.max(start, Math.min(span.end, range.end));
    else end = Math.min(end, span.start);
  }
  return { start, end: Math.max(start, end) };
}

/**
 * Moves the cut between adjacent clips `leftId` and `rightId` to `at`, each keeping its speed.
 * Refused unless they meet, when a transition joins them, or when either would become empty.
 */
export function rollTrim(
  project: Project,
  leftId: string,
  rightId: string,
  at: number,
): EditResult {
  const time = notTicks(at, "the cut");
  if (time) return time;
  const left = locate(project, leftId);
  if (!left.ok) return left;
  const right = locate(project, rightId);
  if (!right.ok) return right;
  const cut = left.clip.timeline_range.end;
  if (
    left.track.id !== right.track.id ||
    right.clip.timeline_range.start !== cut
  )
    return failure(
      "not_adjacent",
      `clips ${leftId} and ${rightId} do not meet at a cut`,
    );
  if (!(
    at > left.clip.timeline_range.start && at < right.clip.timeline_range.end
  ))
    return failure("outside_clip", `${at} would empty a clip`);
  if (at === cut) return finish(project);
  const ended = trimmedClip(project, leftId, "end", at);
  if (!ended.ok) return ended;
  const started = trimmedClip(project, rightId, "start", at);
  if (!started.ok) return started;
  return finish(
    replaceTrack(project, {
      ...left.track,
      clips: (left.track.clips ?? []).map((candidate) =>
        candidate.id === leftId
          ? ended.clip
          : candidate.id === rightId
            ? started.clip
            : candidate,
      ),
    }),
  );
}

/** Allowed deltas, `min` to `max` inclusive, or why there are none. */
export type EditLimits =
  | { readonly ok: true; readonly min: number; readonly max: number }
  | { readonly ok: false; readonly error: EditError };

/** Source-tick range {@link slip} allows, bounded by the source's start and recorded end. */
export function slipLimits(project: Project, clipId: string): EditLimits {
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { clip } = found;
  const length = sourceLength(project, clip.source_id);
  return {
    ok: true,
    min: -clip.source_range.start,
    max:
      length === undefined
        ? Number.MAX_SAFE_INTEGER
        : length - clip.source_range.end,
  };
}

/**
 * Moves a clip's source window (or a frozen clip's instant) by `delta` source ticks, leaving its
 * timeline placement unchanged. Refused past the source's start or recorded end.
 */
export function slip(
  project: Project,
  clipId: string,
  delta: number,
): EditResult {
  const time = notTicks(delta, "the slip");
  if (time) return time;
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  if (delta === 0) return finish(project);
  const source = offset(clip.source_range, delta);
  const outside = outsideSource(project, clip.source_id, source);
  if (outside) return outside;
  const map = clip.time_map ?? { type: "forward" };
  return finish(
    replaceClip(project, track, {
      ...clip,
      source_range: source,
      ...(map.type === "freeze"
        ? { time_map: { ...map, source_time: map.source_time + delta } }
        : {}),
    }),
  );
}

/**
 * Timeline-tick range {@link slide} allows, limited by neighbours becoming empty, reading outside
 * their source or uncovering a transition, and by content across a gap. Only 0 when a neighbour's
 * edge cannot move; refused (`in_transition`) for a clip in a transition.
 */
export function slideLimits(project: Project, clipId: string): EditLimits {
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  if (transitionsOf(track, clipId).length > 0)
    return failure("in_transition", `clip ${clipId} is in a transition`);
  const { left, right, before, after } = neighbours(track, clip);
  const range = clip.timeline_range;
  let min = before - range.start;
  let max = after - range.end;
  if (left) {
    const edges = transitionsOf(track, left.id).map((t) => t.timeline_range);
    if (edges.some((t) => t.end >= left.timeline_range.end))
      return { ok: true, min: 0, max: 0 };
    // The left neighbour's end may not pass back over its transitions.
    min = Math.max(
      min,
      left.timeline_range.start + 1 - range.start,
      ...edges.map((t) => t.end - range.start),
    );
    const room = extensionRoom(project, left, "end");
    if (room !== undefined) max = Math.min(max, room);
  }
  if (right) {
    const edges = transitionsOf(track, right.id).map((t) => t.timeline_range);
    if (
      edges.some((t) => t.start <= right.timeline_range.start) ||
      right.time_map?.type === "loop"
    )
      return { ok: true, min: 0, max: 0 };
    max = Math.min(
      max,
      right.timeline_range.end - 1 - range.end,
      ...edges.map((t) => t.start - range.end),
    );
    const room = extensionRoom(project, right, "start");
    if (room !== undefined) min = Math.max(min, -room);
  }
  return { ok: true, min, max };
}

/**
 * Moves a clip by `delta`, trimming the adjacent neighbours so the total span is unchanged; a side
 * without an adjacent clip is a gap. The clip keeps its source window and composition. Refused
 * within the limits described by {@link slideLimits}.
 */
export function slide(
  project: Project,
  clipId: string,
  delta: number,
): EditResult {
  const time = notTicks(delta, "the slide");
  if (time) return time;
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  if (transitionsOf(track, clipId).length > 0)
    return failure("in_transition", `clip ${clipId} is in a transition`);
  if (delta === 0) return finish(project);
  const { left, right, before, after } = neighbours(track, clip);
  const range = clip.timeline_range;
  if (range.start + delta < before || range.end + delta > after)
    return failure(
      "outside_clip",
      `clip ${clipId} would overlap the content beside it`,
    );
  const replaced = new Map<string, Clip>([[clipId, shift(clip, delta)]]);
  for (const [neighbour, edge, to] of [
    [left, "end", range.start + delta],
    [right, "start", range.end + delta],
  ] as const) {
    if (!neighbour) continue;
    const trimmed = trimmedClip(project, neighbour.id, edge, to);
    if (!trimmed.ok) return trimmed;
    replaced.set(neighbour.id, trimmed.clip);
  }
  return finish(
    replaceTrack(project, {
      ...track,
      clips: (track.clips ?? []).map(
        (candidate) => replaced.get(candidate.id) ?? candidate,
      ),
    }),
  );
}

/** Options for {@link retime}. */
export interface RetimeOptions extends RippleOptions {
  /** Whether later content on the track and `syncTracks` moves with the clip's end. */
  readonly ripple?: boolean;
  /**
   * `keep` leaves keyframes, fades and composition time unchanged; `stretch` scales them with the
   * clip from its start.
   */
  readonly keyframes?: "keep" | "stretch";
  /** Sets playback direction; unchanged when absent. Only for forward or reversed clips. */
  readonly reverse?: boolean;
}

/**
 * Sets a clip's timeline length to `duration`. Forward and reversed clips change speed to fit the
 * same source window; frozen clips hold longer or shorter; loops play more or fewer passes.
 * Refused when the clip's end is in a transition.
 */
export function retime(
  project: Project,
  clipId: string,
  duration: number,
  options: RetimeOptions = {},
): EditResult {
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  const range = clip.timeline_range;
  const length = range.end - range.start;
  const time = notTicks(duration, "the duration");
  if (time) return time;
  if (duration <= 0)
    return failure("outside_clip", `a clip cannot last ${duration} ticks`);
  const map = clip.time_map ?? { type: "forward" };
  let timeMap = clip.time_map;
  if (options.reverse !== undefined) {
    if (map.type !== "forward" && map.type !== "reverse")
      return failure(
        "unsupported_time_map",
        `a ${map.type} clip cannot play in reverse`,
      );
    timeMap = { type: options.reverse ? "reverse" : "forward" };
  }
  const end = range.start + duration;
  const delta = end - range.end;
  if (
    delta !== 0 &&
    transitionsOf(track, clipId).some(
      (transition) =>
        transition.timeline_range.end >= range.end &&
        transition.timeline_range.start < range.end,
    )
  )
    return failure(
      "in_transition",
      `the end of clip ${clipId} is in a transition`,
    );
  const stretch = options.keyframes === "stretch" && duration !== length;
  let edited: Clip = {
    ...(stretch ? stretchKeyframes(clip, range.start, duration, length) : clip),
    timeline_range: { start: range.start, end },
    ...(timeMap === undefined ? {} : { time_map: timeMap }),
  };
  if (stretch) {
    if (clip.audio)
      edited = {
        ...edited,
        audio: {
          ...edited.audio,
          ...(clip.audio.fade_in == null
            ? {}
            : { fade_in: stretched(clip.audio.fade_in, duration, length) }),
          ...(clip.audio.fade_out == null
            ? {}
            : { fade_out: stretched(clip.audio.fade_out, duration, length) }),
        },
      };
    if (clip.composition)
      edited = {
        ...edited,
        composition: {
          ...clip.composition,
          scale: rescaled(
            clip.composition.scale ?? { num: 1, den: 1 },
            length,
            duration,
          ),
        },
      };
  }
  if (!options.ripple || delta === 0)
    return finish(replaceClip(project, track, edited));
  if (pullsOnto(track, range.end, delta, clipId)) return pulledOnto(track);
  const clips = (track.clips ?? []).map((candidate) =>
    candidate.id === clipId
      ? edited
      : candidate.timeline_range.start >= range.end
        ? shift(candidate, delta)
        : candidate,
  );
  const synced = rippleSyncTracks(
    project,
    options.syncTracks,
    track.id,
    range.end,
    delta,
  );
  if (!synced.ok) return synced;
  return finish(
    replaceTrack(synced.project, {
      ...track,
      clips,
      transitions: shiftTransitions(track, range.end, delta),
    }),
  );
}

/**
 * Inserts `clip` at `at`, pushing later content on the track and `syncTracks` by its length. A clip
 * across `at` is split first, its right part becoming `splitId`; `needs_id` when more than one is.
 */
export function insert(
  project: Project,
  trackId: string,
  clip: Clip,
  at: number,
  options: PlaceOptions = {},
): EditResult {
  const placed = placeAt(project, trackId, clip, at);
  if (!placed.ok) return placed;
  let edited = project;
  let track = placed.track;
  const crossing = (track.clips ?? []).filter(
    (c) => c.timeline_range.start < at && c.timeline_range.end > at,
  );
  if (crossing.length > 1)
    return failure(
      "needs_id",
      `inserting at ${at} splits clips ${crossing.map((c) => c.id).join(", ")}; one split id names one part`,
    );
  const across = crossing[0];
  if (across) {
    if (options.splitId === undefined)
      return failure("needs_id", `inserting at ${at} splits clip ${across.id}`);
    const cut = split(edited, across.id, at, options.splitId);
    if (!cut.ok) return cut;
    edited = cut.project;
    track = edited.timeline.tracks.find((t) => t.id === trackId)!;
  }
  const length = placed.clip.timeline_range.end - at;
  const synced = rippleSyncTracks(
    edited,
    options.syncTracks,
    trackId,
    at,
    length,
  );
  if (!synced.ok) return synced;
  return finish(
    replaceTrack(synced.project, {
      ...track,
      clips: [
        ...(track.clips ?? []).map((c) =>
          c.timeline_range.start >= at ? shift(c, length) : c,
        ),
        placed.clip,
      ],
      transitions: shiftTransitions(track, at, length),
    }),
  );
}

/**
 * Places `clip` at `at`, replacing whatever the track shows there. Clips inside are removed, clips
 * across an edge are trimmed, and a clip across both is split (right part `splitId`). Refused where
 * a transition or matte source would be cut, and (`needs_id`) when more than one clip spans both
 * edges.
 */
export function overwrite(
  project: Project,
  trackId: string,
  clip: Clip,
  at: number,
  options: PlaceOptions = {},
): EditResult {
  const placed = placeAt(project, trackId, clip, at);
  if (!placed.ok) return placed;
  const range = placed.clip.timeline_range;
  let edited = project;
  let track = placed.track;
  if (
    (track.transitions ?? []).some(
      (t) =>
        t.timeline_range.start < range.end &&
        t.timeline_range.end > range.start,
    )
  )
    return failure("in_transition", `the range meets a transition`);
  const covering = (track.clips ?? []).filter(
    (c) =>
      c.timeline_range.start < range.start && c.timeline_range.end > range.end,
  );
  if (covering.length > 1)
    return failure(
      "needs_id",
      `overwriting splits clips ${covering.map((c) => c.id).join(", ")}; one split id names one part`,
    );
  const across = covering[0];
  if (across) {
    if (options.splitId === undefined)
      return failure(
        "needs_id",
        `overwriting inside clip ${across.id} splits it`,
      );
    const cut = split(edited, across.id, range.end, options.splitId);
    if (!cut.ok) return cut;
    edited = cut.project;
    track = edited.timeline.tracks.find((t) => t.id === trackId)!;
  }
  const clips: Clip[] = [];
  for (const candidate of track.clips ?? []) {
    const { start, end } = candidate.timeline_range;
    if (end <= range.start || start >= range.end) {
      clips.push(candidate);
      continue;
    }
    if (start >= range.start && end <= range.end) {
      if (isMatteSource(edited, candidate.id))
        return failure(
          "matte_source",
          `clip ${candidate.id} is another clip's matte`,
        );
      continue;
    }
    const edge: ClipEdge = start < range.start ? "end" : "start";
    const trimmed = trimmedClip(
      edited,
      candidate.id,
      edge,
      edge === "end" ? range.start : range.end,
    );
    if (!trimmed.ok) return trimmed;
    clips.push(trimmed.clip);
  }
  clips.push(placed.clip);
  return finish(replaceTrack(edited, { ...track, clips }));
}

/** Places `clip` after the track's last clip, or at 0. */
export function append(
  project: Project,
  trackId: string,
  clip: Clip,
): EditResult {
  const track = project.timeline.tracks.find((t) => t.id === trackId);
  if (!track) return failure("unknown_track", `no track ${trackId}`);
  const end = Math.max(
    0,
    ...(track.clips ?? []).map((c) => c.timeline_range.end),
  );
  return overwrite(project, trackId, clip, end);
}

/** Places `clip` at `at` after the checks every placement makes. */
function placeAt(
  project: Project,
  trackId: string,
  clip: Clip,
  at: number,
):
  | { readonly ok: true; readonly track: Track; readonly clip: Clip }
  | { readonly ok: false; readonly error: EditError } {
  const track = project.timeline.tracks.find((t) => t.id === trackId);
  if (!track) return failure("unknown_track", `no track ${trackId}`);
  if (clipIds(project).has(clip.id))
    return failure("duplicate_id", `clip ${clip.id} already exists`);
  const source = project.sources.find((s) => s.id === clip.source_id);
  if (
    track.kind === "caption" ||
    (source !== undefined &&
      (track.kind === "audio") !== (source.kind === "audio"))
  )
    return failure(
      "incompatible_track",
      `clip ${clip.id} cannot sit on ${track.kind} track ${trackId}`,
    );
  const time = notTicks(at, "the time");
  if (time) return time;
  if (at < 0) return failure("outside_clip", `${at} is before the timeline`);
  return {
    ok: true,
    track,
    clip: shift(clip, at - clip.timeline_range.start),
  };
}

function syncTracksOf(
  project: Project,
  ids: readonly string[] | undefined,
  edited: string,
): Track[] {
  const wanted = new Set(ids ?? []);
  wanted.delete(edited);
  return project.timeline.tracks.filter((t) => wanted.has(t.id));
}

/** Whether `track` has a clip, transition, or cue inside `range`. */
function occupies(track: Track, range: TimeRange): boolean {
  if (range.end <= range.start) return false;
  const meets = (r: TimeRange) => r.start < range.end && r.end > range.start;
  return (
    (track.clips ?? []).some((c) => meets(c.timeline_range)) ||
    (track.transitions ?? []).some((t) => meets(t.timeline_range)) ||
    (track.captions?.cues ?? []).some((c) => meets(c.timeline_range))
  );
}

/**
 * Shifts content from `pivot` on by `delta` on tracks `ids` other than `edited`; refuses when one
 * has content across `pivot`.
 */
function rippleSyncTracks(
  project: Project,
  ids: readonly string[] | undefined,
  edited: string,
  pivot: number,
  delta: number,
):
  | { readonly ok: true; readonly project: Project }
  | { readonly ok: false; readonly error: EditError } {
  if (delta === 0) return { ok: true, project };
  let result = project;
  for (const track of syncTracksOf(project, ids, edited)) {
    const across = (r: TimeRange) => r.start < pivot && r.end > pivot;
    if (
      (track.clips ?? []).some((c) => across(c.timeline_range)) ||
      (track.transitions ?? []).some((t) => across(t.timeline_range)) ||
      (track.captions?.cues ?? []).some((c) => across(c.timeline_range))
    )
      return failure(
        "sync_conflict",
        `track ${track.id} has content across ${pivot}`,
      );
    if (pullsOnto(track, pivot, delta)) return pulledOnto(track);
    result = replaceTrack(result, {
      ...track,
      clips: (track.clips ?? []).map((c) =>
        c.timeline_range.start >= pivot ? shift(c, delta) : c,
      ),
      transitions: shiftTransitions(track, pivot, delta),
      ...(track.captions
        ? {
            captions: {
              ...track.captions,
              cues: (track.captions.cues ?? []).map((cue) =>
                cue.timeline_range.start >= pivot
                  ? {
                      ...cue,
                      timeline_range: offset(cue.timeline_range, delta),
                    }
                  : cue,
              ),
            },
          }
        : {}),
    });
  }
  return { ok: true, project: result };
}

/**
 * Whether shifting content from `pivot` on by `delta` would make previously separate content
 * overlap or reorder. Pre-existing overlaps are ignored; `except` is the edited clip.
 */
function pullsOnto(
  track: Track,
  pivot: number,
  delta: number,
  except?: string,
): boolean {
  if (delta >= 0) return false;
  const spans = [
    ...(track.clips ?? [])
      .filter((c) => c.id !== except)
      .map((c) => c.timeline_range),
    ...(track.transitions ?? []).map((t) => t.timeline_range),
    ...(track.captions?.cues ?? []).map((c) => c.timeline_range),
  ];
  const ends = spans
    .filter((r) => r.start < pivot)
    .map((r) => r.end)
    .sort((a, b) => a - b);
  if (ends.length === 0) return false;
  return spans.some((r) => {
    if (r.start < pivot) return false;
    // Find the first end after the landing point; it collides if that end is not after `r.start`.
    let low = 0;
    let high = ends.length;
    while (low < high) {
      const middle = (low + high) >> 1;
      if (ends[middle] <= r.start + delta) low = middle + 1;
      else high = middle;
    }
    return low < ends.length && ends[low] <= r.start;
  });
}

function pulledOnto(track: Track): {
  readonly ok: false;
  readonly error: EditError;
} {
  return failure(
    "sync_conflict",
    `track ${track.id} has content the edit would pull onto content that stays`,
  );
}

/** Moves a clip to start at `to`. Refused while in a transition. */
export function move(project: Project, clipId: string, to: number): EditResult {
  const moved = relocate(project, { clip: clipId, to });
  return moved.ok ? finish(moved.project) : moved;
}

/**
 * Moves a clip to another track, at `to` or its current start. Refused while in a transition or
 * onto an incompatible track.
 */
export function moveToTrack(
  project: Project,
  clipId: string,
  trackId: string,
  to?: number,
): EditResult {
  const moved = relocate(project, { clip: clipId, track: trackId, to });
  return moved.ok ? finish(moved.project) : moved;
}

export interface ClipMove {
  readonly clip: string;
  /** Defaults to the current start. */
  readonly to?: number;
  /** Defaults to the current track. */
  readonly track?: string;
}

/**
 * Moves several clips at once. Only the final result is checked, so the order of moves does not
 * matter. Refused for a clip listed twice (`duplicate_id`) or wherever a single move would be.
 */
export function moveClips(
  project: Project,
  moves: readonly ClipMove[],
): EditResult {
  const moved = new Set<string>();
  let edited = project;
  for (const entry of moves) {
    if (moved.has(entry.clip)) {
      return failure("duplicate_id", `clip ${entry.clip} is moved twice`);
    }
    moved.add(entry.clip);
    const result = relocate(edited, entry);
    if (!result.ok) return result;
    edited = result.project;
  }
  return finish(edited);
}

/** Moves one clip without validating the whole project. */
function relocate(
  project: Project,
  { clip: clipId, to, track: trackId }: ClipMove,
):
  | { readonly ok: true; readonly project: Project }
  | { readonly ok: false; readonly error: EditError } {
  const time = notTicks(to, "the move target");
  if (time) return time;
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  if (transitionsOf(track, clipId).length > 0) {
    return failure("in_transition", `clip ${clipId} is in a transition`);
  }
  const moved = shift(
    clip,
    (to ?? clip.timeline_range.start) - clip.timeline_range.start,
  );
  if (trackId === undefined || trackId === track.id) {
    return { ok: true, project: replaceClip(project, track, moved) };
  }
  const target = project.timeline.tracks.find(
    (candidate) => candidate.id === trackId,
  );
  if (target === undefined) {
    return failure("unknown_track", `no track ${trackId}`);
  }
  const source = project.sources.find(
    (candidate) => candidate.id === clip.source_id,
  );
  const audioSource = source?.kind === "audio";
  if (target.kind === "caption" || (target.kind === "audio") !== audioSource) {
    return failure(
      "incompatible_track",
      `clip ${clipId} cannot sit on ${target.kind} track ${trackId}`,
    );
  }
  const tracks = project.timeline.tracks.map((candidate) => {
    if (candidate.id === track.id) {
      return {
        ...candidate,
        clips: (candidate.clips ?? []).filter((other) => other.id !== clipId),
      };
    }
    if (candidate.id === trackId) {
      return { ...candidate, clips: [...(candidate.clips ?? []), moved] };
    }
    return candidate;
  });
  return {
    ok: true,
    project: { ...project, timeline: { ...project.timeline, tracks } },
  };
}

type Located =
  | { readonly ok: true; readonly track: Track; readonly clip: Clip }
  | { readonly ok: false; readonly error: EditError };

function locate(project: Project, clipId: string): Located {
  for (const track of project.timeline.tracks) {
    const clip = (track.clips ?? []).find(
      (candidate) => candidate.id === clipId,
    );
    if (clip !== undefined) return { ok: true, track, clip };
  }
  return failure("unknown_clip", `no clip ${clipId}`);
}

function failure(
  code: EditErrorCode,
  message: string,
): { readonly ok: false; readonly error: EditError } {
  return { ok: false, error: { code, message } };
}

/** Refuses a provided time argument that is not whole ticks. */
function notTicks(
  value: number | undefined,
  what: string,
): { readonly ok: false; readonly error: EditError } | undefined {
  if (value === undefined || Number.isSafeInteger(value)) return undefined;
  return failure(
    "invalid_time",
    `${what} ${value} is not a whole number of ticks`,
  );
}

/** Nesting depth of {@link deferValidation}. */
let deferring = 0;

/**
 * Runs `run` with {@link validateProject} skipped after each edit. Edits still apply their own
 * checks (`sync_conflict`, `outside_source`, ...); the caller validates the final project. `run` is
 * synchronous and may nest.
 */
export function deferValidation<T>(run: () => T): T {
  deferring += 1;
  try {
    return run();
  } finally {
    deferring -= 1;
  }
}

function finish(project: Project): EditResult {
  if (deferring > 0) return { ok: true, project };
  const problems = validateProject(project);
  if (problems.length > 0) {
    return {
      ok: false,
      error: {
        code: "invalid_result",
        message: `the edit breaks the document: ${problems.join("; ")}`,
        problems,
      },
    };
  }
  return { ok: true, project };
}

/** Source length in ticks from its fingerprint, if recorded. */
function sourceLength(project: Project, sourceId: string): number | undefined {
  const source = project.sources.find((candidate) => candidate.id === sourceId);
  return source?.fingerprint?.duration_ticks ?? undefined;
}

/**
 * Refuses a `range` outside the source. With `before`, only an edge that moves further outside is
 * refused, so a clip already past a shorter source can still be shortened.
 */
function outsideSource(
  project: Project,
  sourceId: string,
  range: TimeRange,
  before?: TimeRange,
): { readonly ok: false; readonly error: EditError } | undefined {
  const length = sourceLength(project, sourceId);
  const early = range.start < 0 && range.start < (before?.start ?? 0);
  const late =
    length !== undefined &&
    range.end > length &&
    range.end > (before?.end ?? length);
  if (early || late)
    return failure(
      "outside_source",
      `source ${sourceId} has nothing at ${early ? range.start : range.end}`,
    );
  return undefined;
}

/**
 * Adjacent clips (`left`, `right`) and the nearest content edges on either side (`before`,
 * `after`).
 */
function neighbours(
  track: Track,
  clip: Clip,
): {
  readonly left?: Clip;
  readonly right?: Clip;
  readonly before: number;
  readonly after: number;
} {
  const range = clip.timeline_range;
  const others = (track.clips ?? []).filter(
    (candidate) => candidate.id !== clip.id,
  );
  const left = others.find(
    (candidate) => candidate.timeline_range.end === range.start,
  );
  const right = others.find(
    (candidate) => candidate.timeline_range.start === range.end,
  );
  const before = left
    ? left.timeline_range.start
    : Math.max(
        0,
        ...others
          .filter((candidate) => candidate.timeline_range.end <= range.start)
          .map((candidate) => candidate.timeline_range.end),
      );
  const after = right
    ? right.timeline_range.end
    : Math.min(
        Number.MAX_SAFE_INTEGER,
        ...others
          .filter((candidate) => candidate.timeline_range.start >= range.end)
          .map((candidate) => candidate.timeline_range.start),
      );
  return {
    ...(left ? { left } : {}),
    ...(right ? { right } : {}),
    before,
    after,
  };
}

/** Timeline ticks `clip` can grow at `edge` before leaving its source; `undefined` if unbounded. */
function extensionRoom(
  project: Project,
  clip: Clip,
  edge: ClipEdge,
): number | undefined {
  const map = clip.time_map ?? { type: "forward" };
  if (map.type === "freeze" || map.type === "loop") return undefined;
  const source = clip.source_range;
  const timeline = clip.timeline_range;
  // A reversed clip's end reads its source start.
  const readsStart = (edge === "start") === (map.type === "forward");
  const available = readsStart
    ? source.start
    : (() => {
        const length = sourceLength(project, clip.source_id);
        return length === undefined ? undefined : length - source.end;
      })();
  if (available === undefined) return undefined;
  // floor(available * T / S).
  return stretched(
    available,
    timeline.end - timeline.start,
    source.end - source.start,
  );
}

/** Maps every keyframe time `t` to `start + floor((t - start) * to / from)`. */
function stretchKeyframes<T>(
  value: T,
  start: number,
  to: number,
  from: number,
): T {
  if (Array.isArray(value))
    return value.map((item: unknown) =>
      stretchKeyframes(item, start, to, from),
    ) as T;
  if (typeof value !== "object" || value === null) return value;
  const record = value as Record<string, unknown>;
  if (record.mode === "keyframes" && Array.isArray(record.keyframes))
    return {
      ...record,
      keyframes: record.keyframes.map((keyframe: unknown) => {
        const frame = keyframe as Record<string, unknown>;
        return {
          ...frame,
          time: start + stretched((frame.time as number) - start, to, from),
        };
      }),
    } as T;
  return Object.fromEntries(
    Object.entries(record).map(([key, field]) => [
      key,
      stretchKeyframes(field, start, to, from),
    ]),
  ) as T;
}

/** Largest term a composition time scale may hold. */
const MAX_SCALE_TERM = 0xffff_ffff;

/**
 * `scale * from / to` in lowest terms, or the closest convergent whose terms fit
 * {@link MAX_SCALE_TERM}.
 */
function rescaled(
  scale: { readonly num: number; readonly den: number },
  from: number,
  to: number,
): { num: number; den: number } {
  let num = BigInt(scale.num) * BigInt(from);
  let den = BigInt(scale.den) * BigInt(to);
  const divisor = gcd(num, den);
  num /= divisor;
  den /= divisor;
  const limit = BigInt(MAX_SCALE_TERM);
  if (num <= limit && den <= limit)
    return { num: Number(num), den: Number(den) };
  // Continued-fraction convergents of num/den, the last whose terms fit.
  let [p0, q0, p1, q1] = [0n, 1n, 1n, 0n];
  let [n, d] = [num, den];
  while (d !== 0n) {
    const a = n / d;
    const [p2, q2] = [a * p1 + p0, a * q1 + q0];
    if (p2 > limit || q2 > limit) break;
    [p0, q0, p1, q1] = [p1, q1, p2, q2];
    [n, d] = [d, n - a * d];
  }
  return { num: Number(p1 === 0n ? 1n : p1), den: Number(q1 === 0n ? 1n : q1) };
}

function gcd(a: bigint, b: bigint): bigint {
  while (b !== 0n) [a, b] = [b, a % b];
  return a;
}

function clipIds(project: Project): Set<string> {
  return new Set(
    project.timeline.tracks.flatMap((track) =>
      (track.clips ?? []).map((clip) => clip.id),
    ),
  );
}

function transitionsOf(track: Track, clipId: string): ClipTransition[] {
  return (track.transitions ?? []).filter(
    (transition) =>
      transition.from_clip === clipId || transition.to_clip === clipId,
  );
}

function isMatteSource(project: Project, clipId: string): boolean {
  return project.timeline.tracks.some((track) =>
    (track.clips ?? []).some((clip) => clip.matte?.source === clipId),
  );
}

function replaceTrack(project: Project, track: Track): Project {
  return {
    ...project,
    timeline: {
      ...project.timeline,
      tracks: project.timeline.tracks.map((candidate) =>
        candidate.id === track.id ? track : candidate,
      ),
    },
  };
}

function replaceClip(project: Project, track: Track, clip: Clip): Project {
  return replaceTrack(project, {
    ...track,
    clips: (track.clips ?? []).map((candidate) =>
      candidate.id === clip.id ? clip : candidate,
    ),
  });
}

function offset(range: TimeRange, delta: number): TimeRange {
  return { start: range.start + delta, end: range.end + delta };
}

/** Moves a clip and its keyframes by `delta`; the composition keeps its own time. */
function shift(clip: Clip, delta: number): Clip {
  return {
    ...shiftKeyframes(clip, delta),
    timeline_range: offset(clip.timeline_range, delta),
  };
}

/** Moves a transition and its keyframes by `delta`. */
function shiftTransition(
  transition: ClipTransition,
  delta: number,
): ClipTransition {
  return {
    ...shiftKeyframes(transition, delta),
    timeline_range: offset(transition.timeline_range, delta),
  };
}

/** Moves every keyframe time in `value` by `delta`, whatever fields are animated. */
function shiftKeyframes<T>(value: T, delta: number): T {
  if (delta === 0) return value;
  if (Array.isArray(value)) {
    return value.map((item: unknown) => shiftKeyframes(item, delta)) as T;
  }
  if (typeof value !== "object" || value === null) return value;
  const record = value as Record<string, unknown>;
  if (record.mode === "keyframes" && Array.isArray(record.keyframes)) {
    return {
      ...record,
      keyframes: record.keyframes.map((keyframe: unknown) => {
        const frame = keyframe as Record<string, unknown>;
        return {
          ...frame,
          time: (frame.time as number) + delta,
          value: shiftKeyframes(frame.value, delta),
        };
      }),
    } as T;
  }
  return Object.fromEntries(
    Object.entries(record).map(([key, field]) => [
      key,
      shiftKeyframes(field, delta),
    ]),
  ) as T;
}

function shiftTransitions(
  track: Track,
  from: number,
  delta: number,
): ClipTransition[] {
  return (track.transitions ?? []).map((transition) =>
    transition.timeline_range.start >= from
      ? shiftTransition(transition, delta)
      : transition,
  );
}

/**
 * `floor(elapsed * S / T)` in exact integer arithmetic, matching the engine. `elapsed` may be
 * negative for a trim that extends the clip.
 */
function stretched(elapsed: number, source: number, timeline: number): number {
  const numerator = BigInt(elapsed) * BigInt(source);
  const denominator = BigInt(timeline);
  const quotient = numerator / denominator;
  // BigInt division truncates toward zero; below zero, floor is one less.
  return Number(numerator % denominator < 0n ? quotient - 1n : quotient);
}

/** Source ranges of a clip's two halves at `at`, or `undefined` for a loop. */
function splitSource(
  clip: Clip,
  at: number,
): { left: TimeRange; right: TimeRange } | undefined {
  const timeline = clip.timeline_range;
  const source = clip.source_range;
  const length = timeline.end - timeline.start;
  const sourceLength = source.end - source.start;
  const elapsed = at - timeline.start;
  const map = clip.time_map ?? { type: "forward" };
  switch (map.type) {
    case "forward": {
      const cut = source.start + stretched(elapsed, sourceLength, length);
      return {
        left: { start: source.start, end: cut },
        right: { start: cut, end: source.end },
      };
    }
    case "reverse": {
      // The left half plays the end of the source.
      const cut =
        source.start + stretched(length - elapsed, sourceLength, length);
      return {
        left: { start: cut, end: source.end },
        right: { start: source.start, end: cut },
      };
    }
    case "freeze":
      return { left: source, right: source };
    case "loop":
      return undefined;
  }
}

type Trimmed =
  | {
      readonly ok: true;
      readonly track: Track;
      readonly clip: Clip;
      readonly original: Clip;
    }
  | { readonly ok: false; readonly error: EditError };

/**
 * Clip `clipId` with `edge` moved to `to` at its speed, or a refusal. With `ripple`, a start trim
 * slides back afterwards, so `to` may precede the timeline start.
 */
function trimmedClip(
  project: Project,
  clipId: string,
  edge: ClipEdge,
  to: number,
  ripple = false,
): Trimmed {
  const found = locate(project, clipId);
  if (!found.ok) return found;
  const { track, clip } = found;
  const range = clip.timeline_range;
  const inTransition = transitionsOf(track, clipId).some((transition) =>
    edge === "start"
      ? transition.timeline_range.start <= range.start &&
        transition.timeline_range.end > range.start
      : transition.timeline_range.end >= range.end &&
        transition.timeline_range.start < range.end,
  );
  if (inTransition) {
    return failure(
      "in_transition",
      `the ${edge} of clip ${clipId} is in a transition`,
    );
  }
  const start = edge === "start" ? to : range.start;
  const end = edge === "end" ? to : range.end;
  if (!(end > start)) {
    return failure("outside_clip", `clip ${clipId} would be empty`);
  }
  if (start < 0 && !ripple) {
    return failure("outside_clip", `${start} is before the timeline`);
  }
  const source = trimmedSource(clip, start, end);
  if (source === undefined) {
    return failure("unsupported_time_map", `a loop's start cannot move`);
  }
  const outside = outsideSource(
    project,
    clip.source_id,
    source,
    clip.source_range,
  );
  if (outside) return outside;
  return {
    ok: true,
    track,
    original: clip,
    clip: {
      ...clip,
      timeline_range: { start, end },
      source_range: source,
      ...(clip.composition == null
        ? {}
        : { composition: advance(clip.composition, start - range.start) }),
    },
  };
}

/** Advances a composition offset by `delta * scale`, rounded down as the engine does. */
function advance(reference: ClipComposition, delta: number): ClipComposition {
  if (delta === 0) return reference;
  const scale = reference.scale ?? { num: 1, den: 1 };
  const offset =
    (reference.offset ?? 0) + stretched(delta, scale.num, scale.den);
  return { ...reference, offset };
}

/**
 * Copies composition `id` and every group definition it draws (recursively) so clip `clipId` can
 * be edited independently. Copies take the first free id of `<root>~<clipId>`, `<root>~<clipId>~2`
 * and so on, where `<root>` is the original's id up to its first `~` after the first character, so
 * repeated splits do not grow ids. Composition sources stay shared. `from` supplies the
 * compositions to copy (default: the project's), for pasting a clip whose composition was removed.
 * Returns the copy's id and a map from original to copy ids.
 */
export function copyComposition(
  project: Project,
  id: string,
  clipId: string,
  from: readonly Composition[] = project.compositions ?? [],
): {
  readonly project: Project;
  readonly id: string;
  readonly copies: ReadonlyMap<string, string>;
} {
  const originals = new Map(
    from.map((composition) => [composition.id, composition]),
  );
  if (!originals.has(id)) return { project, id, copies: new Map() };
  const taken = new Set((project.compositions ?? []).map((c) => c.id));
  const copies = new Map<string, string>();
  const pending = [id];
  while (pending.length > 0) {
    const original = pending.pop()!;
    if (copies.has(original) || !originals.has(original)) continue;
    const cut = original.indexOf("~", 1);
    const root = cut < 0 ? original : original.slice(0, cut);
    let copy = `${root}~${clipId}`;
    for (let n = 2; taken.has(copy); n += 1) copy = `${root}~${clipId}~${n}`;
    taken.add(copy);
    copies.set(original, copy);
    for (const node of originals.get(original)!.nodes ?? []) {
      if (node.op.type === "group") pending.push(node.op.composition);
    }
  }
  const copied = [...copies].map(([original, copy]): Composition => {
    const composition = structuredClone(originals.get(original)!);
    if (composition.nodes === undefined) return { ...composition, id: copy };
    return {
      ...composition,
      id: copy,
      nodes: composition.nodes.map((node) =>
        node.op.type === "group" && copies.has(node.op.composition)
          ? {
              ...node,
              op: { ...node.op, composition: copies.get(node.op.composition)! },
            }
          : node,
      ),
    };
  });
  return {
    project: {
      ...project,
      compositions: [...(project.compositions ?? []), ...copied],
    },
    id: copies.get(id)!,
    copies,
  };
}

/**
 * Source range of a clip trimmed to `start..end` at its own speed, or `undefined` when a loop's
 * start would move. Edges are mapped as a split maps them, so the untouched edge stays exact.
 */
function trimmedSource(
  clip: Clip,
  start: number,
  end: number,
): TimeRange | undefined {
  const timeline = clip.timeline_range;
  const source = clip.source_range;
  const length = timeline.end - timeline.start;
  const sourceLength = source.end - source.start;
  const map = clip.time_map ?? { type: "forward" };
  const at = (elapsed: number) =>
    source.start + stretched(elapsed, sourceLength, length);
  switch (map.type) {
    case "forward":
      return {
        start: at(start - timeline.start),
        end: at(end - timeline.start),
      };
    case "reverse":
      // Reversed: the timeline end plays the source start.
      return { start: at(timeline.end - end), end: at(timeline.end - start) };
    case "freeze":
      return source;
    case "loop":
      return start === timeline.start ? source : undefined;
  }
}
