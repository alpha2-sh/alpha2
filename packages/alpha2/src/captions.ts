/**
 * SRT and WebVTT import into caption tracks, using the engine's parser in wasm.
 *
 * Captions draw with the fonts given to the renderer (`WasmRenderWorkerHostOptions.fonts` in the
 * browser, `fonts` on a native export request); the same font files give the same pixels.
 */

import type {
  CaptionCue,
  TextStyle,
  TextStylePatch,
  Track,
} from "./generated/project-schema.js";

/** The `alpha2/wasm` function caption import uses. */
export interface CaptionBinding {
  parseCaptions(text: string, format: "srt" | "webvtt"): string;
}

/** Cues read from a subtitle file. */
export interface ImportedCaptions {
  /**
   * One caption track's cues per row. Cues in a row do not overlap, and a row holds at most
   * `MAX_CLIPS_PER_TRACK`.
   */
  readonly rows: readonly (readonly CaptionCue[])[];
  /** Unreadable blocks, including cues past `Number.MAX_SAFE_INTEGER` ticks. */
  readonly skipped: number;
  readonly notes: CaptionImportNotes;
}

/** Counts of markup the import read but could not keep. */
export interface CaptionImportNotes {
  /** Voices after a cue's first; only the first becomes its speaker. */
  readonly extraVoices: number;
  /** Inline timestamps out of order or outside their cue. */
  readonly droppedTimestamps: number;
  /** Class names a run cannot carry. */
  readonly droppedClasses: number;
  /** Cues with more runs than a text holds, imported as plain text. */
  readonly flattened: number;
}

/**
 * Parses SubRip (`.srt`) or WebVTT (`.vtt`) text. Timestamps are exact (1 ms is 120 ticks).
 *
 * A WebVTT voice sets the cue's `speaker`. Voices, `<c.class>`, `<b>`, `<i>` and `<u>` become run
 * classes, and inline timestamps split the cue into runs timed from its start. Other markup is
 * dropped.
 *
 * @throws For an unknown format, a WebVTT file without its header, a file over
 * `MAX_CAPTION_FILE_BYTES` or `MAX_CAPTION_FILE_CUES`, or cues needing more than `MAX_TRACKS`
 * caption tracks.
 */
export function parseCaptions(
  binding: CaptionBinding,
  text: string,
  format: "srt" | "webvtt",
): ImportedCaptions {
  return JSON.parse(binding.parseCaptions(text, format)) as ImportedCaptions;
}

/**
 * Builds a caption track. Cues pick from the named `styles` by their `style` or `speaker`.
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
 * Splits `cue` at timeline tick `at`. Each half keeps the runs timed within it (untimed runs go to
 * both), with run times clipped and rebased as in {@link trimCue}.
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
 * Narrows `cue` to `start..end`. Run times are rebased to the new start and clipped to the range;
 * runs wholly outside it are dropped.
 *
 * @throws RangeError when the new range is empty or outside the cue's, or no run is left.
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
