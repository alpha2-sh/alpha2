import type { CaptionCue, TextStyle, Track } from "alpha2/project";
import type { AssetDescriptor, Json } from "./document.js";
import type { Command } from "./commands.js";

/** The bin asset kind a subtitle file (SRT or WebVTT) imports as. */
export const CAPTIONS_ASSET_KIND = "captions";

/**
 * A subtitle file in the bin: its cues split into rows, none overlapping another in its row, as
 * `parseCaptions` reads them. The cues travel in the asset's authoring data, so the asset needs no
 * file to stay online and saves with the document. Inserting it adds one caption track per row.
 */
export function captionsAsset(
  id: string,
  name: string,
  rows: readonly (readonly CaptionCue[])[],
): AssetDescriptor {
  return {
    id,
    kind: CAPTIONS_ASSET_KIND,
    name,
    contentRevision: 0,
    authoring: { captions: { rows } } as unknown as Json,
  };
}

/**
 * Whether `value` is a cue as a captions asset keeps it: text, and a non-empty range of safe
 * integer ticks from the asset's start.
 */
function isCaptionCue(value: unknown): value is CaptionCue {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    return false;
  const { text, timeline_range: range } = value as Record<string, unknown>;
  if (typeof range !== "object" || range === null) return false;
  const { start, end } = range as Record<string, unknown>;
  return (
    typeof text === "string" &&
    Number.isSafeInteger(start) &&
    Number.isSafeInteger(end) &&
    (start as number) >= 0 &&
    (start as number) < (end as number)
  );
}

/**
 * The cue rows a captions asset's authoring data holds, or `undefined` when it holds anything else:
 * `{ captions: { rows } }`, each row a list of cues.
 */
function rowsOf(
  authoring: Json | undefined,
): readonly (readonly CaptionCue[])[] | undefined {
  const captions =
    typeof authoring === "object" &&
    authoring !== null &&
    !Array.isArray(authoring)
      ? (authoring as { captions?: unknown }).captions
      : undefined;
  const rows =
    typeof captions === "object" && captions !== null
      ? (captions as { rows?: unknown }).rows
      : undefined;
  if (
    !Array.isArray(rows) ||
    !rows.every((row) => Array.isArray(row) && row.every(isCaptionCue))
  )
    return undefined;
  return rows;
}

/**
 * Whether a captions asset's authoring data is the cue rows {@link captionsAsset} writes;
 * `validateDocument` refuses a captions asset with anything else.
 */
export function validCaptionsAuthoring(authoring: Json | undefined): boolean {
  return rowsOf(authoring) !== undefined;
}

/** A captions asset's cue rows; empty for any other asset or malformed authoring data. */
export function captionRows(
  asset: AssetDescriptor | undefined,
): readonly (readonly CaptionCue[])[] {
  if (asset?.kind !== CAPTIONS_ASSET_KIND) return [];
  return rowsOf(asset.authoring) ?? [];
}

/** Where a captions asset's last cue ends, in ticks from its start. */
export function captionsDuration(asset: AssetDescriptor | undefined): number {
  let end = 0;
  for (const row of captionRows(asset))
    for (const cue of row) end = Math.max(end, cue.timeline_range.end);
  return end;
}

/**
 * The commands that put a captions asset on the timeline: a new caption track for each of its rows,
 * with every cue moved `at` ticks later, in `style` when given. Track IDs come from `newId`,
 * `crypto.randomUUID` by default.
 *
 * Throws when the asset has no cues.
 */
export function captionTrackCommands(
  asset: AssetDescriptor,
  at: number,
  options: { newId?: () => string; style?: TextStyle } = {},
): { trackIds: string[]; commands: Command[] } {
  const rows = captionRows(asset).filter((row) => row.length > 0);
  if (!rows.length) throw Error(`${asset.name} has no captions to insert.`);
  const newId = options.newId ?? (() => crypto.randomUUID());
  const base = asset.name.replace(/\.(srt|vtt)$/i, "");
  const trackIds: string[] = [];
  const commands: Command[] = rows.map((row, i) => {
    const track: Track = {
      id: newId(),
      name: rows.length > 1 ? `${base} ${i + 1}` : base,
      kind: "caption",
      clips: [],
      captions: {
        ...(options.style ? { style: options.style } : {}),
        cues: row.map((cue) => ({
          ...cue,
          timeline_range: {
            start: cue.timeline_range.start + at,
            end: cue.timeline_range.end + at,
          },
        })),
      },
    };
    trackIds.push(track.id);
    return { type: "track.add", track };
  });
  return { trackIds, commands };
}
