import { textOf } from "alpha2/project";
import type { EditorDocument, Marker } from "./document.js";

/** One row of a timeline index: something placed in time the user can find and go to. */
export type IndexEntry =
  | {
      readonly kind: "clip";
      readonly id: string;
      /** The clip's own name, else its source's. */
      readonly name: string;
      readonly trackId: string;
      readonly start: number;
      readonly end: number;
    }
  | {
      readonly kind: "transition";
      /** `from->to`, the clips it joins. */
      readonly id: string;
      /** The transition effect's id. */
      readonly name: string;
      readonly trackId: string;
      readonly start: number;
      readonly end: number;
    }
  | {
      readonly kind: "caption";
      /** `trackId#index`. */
      readonly id: string;
      /** The cue's text. */
      readonly name: string;
      readonly trackId: string;
      readonly start: number;
      readonly end: number;
    }
  | {
      readonly kind: "marker";
      readonly id: string;
      readonly name: string;
      readonly trackId: null;
      readonly start: number;
      /** A range marker's end, or its start for an instant. */
      readonly end: number;
      readonly marker: Marker;
    };

const indexes = new WeakMap<EditorDocument, readonly IndexEntry[]>();

/**
 * Every clip, transition, caption cue, and marker of a document, in timeline order (then by kind,
 * then id). Derived from the document alone and kept per document, so a playing viewer never
 * rebuilds it: only an edit does.
 */
export function timelineIndex(document: EditorDocument): readonly IndexEntry[] {
  const cached = indexes.get(document);
  if (cached) return cached;
  const names = new Map(
    Object.values(document.assets).map((asset) => [asset.id, asset.name]),
  );
  const entries: IndexEntry[] = [];
  for (const track of document.project.timeline.tracks) {
    for (const clip of track.clips ?? [])
      entries.push({
        kind: "clip",
        id: clip.id,
        name:
          document.metadata.clips[clip.id]?.name ??
          names.get(clip.source_id) ??
          clip.source_id,
        trackId: track.id,
        start: clip.timeline_range.start,
        end: clip.timeline_range.end,
      });
    for (const transition of track.transitions ?? [])
      entries.push({
        kind: "transition",
        id: `${transition.from_clip}->${transition.to_clip}`,
        name: transition.effect.id,
        trackId: track.id,
        start: transition.timeline_range.start,
        end: transition.timeline_range.end,
      });
    (track.captions?.cues ?? []).forEach((cue, index) =>
      entries.push({
        kind: "caption",
        id: `${track.id}#${index}`,
        name: textOf(cue.text),
        trackId: track.id,
        start: cue.timeline_range.start,
        end: cue.timeline_range.end,
      }),
    );
  }
  for (const marker of document.metadata.markers)
    entries.push({
      kind: "marker",
      id: marker.id,
      name: marker.name,
      trackId: null,
      start: marker.time,
      end: marker.end ?? marker.time,
      marker,
    });
  const order = { marker: 0, clip: 1, transition: 2, caption: 3 } as const;
  entries.sort(
    (a, b) =>
      a.start - b.start ||
      order[a.kind] - order[b.kind] ||
      (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
  );
  indexes.set(document, entries);
  return entries;
}
