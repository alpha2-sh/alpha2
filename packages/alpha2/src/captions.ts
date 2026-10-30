/**
 * Captions: SRT and WebVTT import into caption tracks, read by the engine's own parser in wasm.
 *
 * Caption tracks draw with fonts the renderer is given (`WasmRenderWorkerHostOptions.fonts`
 * in the browser, `fonts` on a native export request); the same font files on both sides
 * give the same caption pixels.
 */

import type {
  CaptionCue,
  TextStyle,
  TextStylePatch,
  Track,
} from "./generated/project-schema.js";

/** The part of `alpha2/wasm` caption import uses. */
export interface CaptionBinding {
  parseCaptions(text: string, format: "srt" | "webvtt"): string;
}

/** Cues read from a subtitle file. */
export interface ImportedCaptions {
  /**
   * The cues, split so that none overlaps another in its row and no row holds more than
   * `MAX_CLIPS_PER_TRACK`: one caption track's cues per row.
   */
  readonly rows: readonly (readonly CaptionCue[])[];
  /**
   * Blocks that were not cues and could not be read as one, including a cue later than a project
   * holds (a time past `Number.MAX_SAFE_INTEGER` ticks).
   */
  readonly skipped: number;
  /** Markup read but not kept, counted. */
  readonly notes: CaptionImportNotes;
}

/** What a subtitle import read but could not keep. */
export interface CaptionImportNotes {
  /** Voices after a cue's first, whose names are not its speaker. */
  readonly extraVoices: number;
  /** Inline timestamps out of order or outside their cue. */
  readonly droppedTimestamps: number;
  /** Class names a run cannot carry. */
  readonly droppedClasses: number;
  /** Cues with more runs than a text holds, imported as plain text. */
  readonly flattened: number;
}

/**
 * Reads SubRip (`.srt`) or WebVTT (`.vtt`) text. Timestamps are exact (a
 * millisecond is 120 ticks). A WebVTT voice names a cue's `speaker`; voices,
 * `<c.class>` spans, `<b>`, `<i>` and `<u>` give the cue's runs classes, and
 * inline timestamps split it into runs timed from its start. Other markup is
 * dropped; a cue without such markup has plain text.
 *
 * Reads in time linear in the file: a file larger than
 * `MAX_CAPTION_FILE_BYTES` is refused before it is parsed, and parsing stops
 * at the cue past `MAX_CAPTION_FILE_CUES`.
 *
 * @throws For an unknown format, a WebVTT file without its header, a file
 * past those limits, or cues that need more caption tracks than a project
 * holds (`MAX_TRACKS`).
 */
export function parseCaptions(
  binding: CaptionBinding,
  text: string,
  format: "srt" | "webvtt",
): ImportedCaptions {
  return JSON.parse(binding.parseCaptions(text, format)) as ImportedCaptions;
}

/**
 * A caption track holding `cues` in `style` (the defaults where absent), with
 * named `styles` its cues take by `style` or by `speaker`.
 */
export function captionTrack(
  id: string,
  name: string,
  cues: readonly CaptionCue[],
  style: TextStyle = {},
  styles: Readonly<Record<string, TextStylePatch>> = {},
): Track {
  return {
    id,
    name,
    kind: "caption",
    clips: [],
    captions: {
      style,
      ...(Object.keys(styles).length > 0 ? { styles: { ...styles } } : {}),
      cues: [...cues],
    },
  };
}

/**
 * `cue` cut in two at timeline tick `at`, which lies strictly inside it. A plain cue's halves both
 * hold its text. A cue with runs keeps in each half the runs said in it: an untimed run in both, a
 * timed run that straddles the cut in both, its time cut there, and every time in the second half
 * rebased to that half's start.
 *
 * @throws RangeError when `at` does not lie strictly inside the cue.
 */
export function splitCue(
  cue: CaptionCue,
  at: number,
): readonly [CaptionCue, CaptionCue] {
  const { start, end } = cue.timeline_range;
  if (!(at > start && at < end)) {
    throw new RangeError(
      `a cue from ${start} to ${end} cannot be split at ${at}`,
    );
  }
  return [trimCue(cue, start, at), trimCue(cue, at, end)];
}

/**
 * `cue` shown from `start` to `end` instead, inside its range: run times are rebased to the new
 * start, runs said wholly before it or after the new end are dropped, and a run that straddles an
 * end has its time cut there. A plain cue keeps its text.
 *
 * @throws RangeError when the new range is empty or leaves the cue's, or when no run is left.
 */
export function trimCue(
  cue: CaptionCue,
  start: number,
  end: number,
): CaptionCue {
  const range = cue.timeline_range;
  if (!(start >= range.start && end <= range.end && start < end)) {
    throw new RangeError(
      `a cue from ${range.start} to ${range.end} cannot be trimmed to ${start}..${end}`,
    );
  }
  const trimmed: CaptionCue = { ...cue, timeline_range: { start, end } };
  if (typeof cue.text === "string") return trimmed;
  const shift = start - range.start;
  const duration = end - start;
  const runs = cue.text.flatMap((run) => {
    if (run.time == null) return [run];
    const from = Math.max(run.time.start - shift, 0);
    const to = Math.min(run.time.end - shift, duration);
    return from < to ? [{ ...run, time: { start: from, end: to } }] : [];
  });
  if (runs.length === 0) {
    throw new RangeError("no run of the cue is said in the trimmed range");
  }
  return { ...trimmed, text: runs };
}
