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
}

/**
 * Reads SubRip (`.srt`) or WebVTT (`.vtt`) text. Timestamps are exact (a
 * millisecond is 120 ticks); markup is dropped.
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

/** A caption track holding `cues` in `style` (the defaults where absent). */
export function captionTrack(
  id: string,
  name: string,
  cues: readonly CaptionCue[],
  style: TextStyle = {},
): Track {
  return {
    id,
    name,
    kind: "caption",
    clips: [],
    captions: { style, cues: [...cues] },
  };
}
