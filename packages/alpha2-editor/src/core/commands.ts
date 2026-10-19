import {
  addTransition,
  resizeTransition,
  removeTransition,
} from "./transitions.js";
import * as edit from "alpha2/edit";
import {
  drawnEffectResources,
  effectStack,
  findComposition,
  freeCompositionId,
  freeNodeId,
  groupNodes,
  ungroupNode,
  inputKeys,
  correspondingCompositions,
  ownComposition,
  withEffectStack,
  withoutUndrawn,
  withInput,
  withoutNodes,
  shownSources,
} from "./compositions.js";
import type { InputKey, StackEntry } from "./compositions.js";
import type {
  Project,
  Clip,
  Track,
  ClipEffect,
  CaptionCue,
  Composition,
  CompositionNode,
  MediaSource,
  EffectResource,
} from "alpha2/project";
import { rememberValid, withValidated } from "./validated.js";
import {
  EditorError,
  validateDocument,
  validateDocumentShape,
  assertProjection,
  indexDocument,
  transitionInstanceId,
  withInstanceIds,
} from "./document.js";
import type {
  EditorDocument,
  TrackMetadata,
  ClipMetadata,
  AssetDescriptor,
  Marker,
  Result,
  GraphLayout,
  GraphPosition,
} from "./document.js";
/**
 * An edit of an {@link EditorDocument}, run by {@link executeCommand} or
 * `EditorSession.dispatch`. Times are timeline ticks unless a variant says otherwise. Ids that
 * name an entity the document lacks are refused, and so is any change to a locked track.
 */
export type Command =
  /**
   * Adds a transition of `duration` ticks between two clips of a track; the incoming clip and
   * those chained after it move earlier to overlap the outgoing one.
   */
  | {
      readonly type: "transition.add";
      readonly trackId: string;
      readonly fromId: string;
      readonly toId: string;
      readonly duration: number;
      readonly effect: ClipEffect;
    }
  /**
   * Gives a transition, named by its instance id, a new duration; the incoming clips move to match.
   */
  | {
      readonly type: "transition.resize";
      readonly trackId: string;
      readonly instanceId: string;
      readonly duration: number;
    }
  /**
   * Removes a transition; the incoming clip and its chain move later to where the outgoing clip
   * ends.
   */
  | {
      readonly type: "transition.remove";
      readonly trackId: string;
      readonly instanceId: string;
    }
  /** Runs `commands` in order as one edit: all apply, or none does. */
  | { readonly type: "transaction"; readonly commands: readonly Command[] }
  /** Replaces the project's name, settings or master audio with the patch's fields. */
  | {
      readonly type: "project.update";
      readonly patch: Partial<Pick<Project, "name" | "settings" | "audio">>;
    }
  /**
   * Adds a track on top of the others. Like `clip.insert`, each of its clips whose composition
   * something else draws shows its own copy of it.
   */
  | { readonly type: "track.add"; readonly track: Track }
  | { readonly type: "track.remove"; readonly id: string }
  /** Puts the tracks in the order `ids` lists them; `ids` must name every track once. */
  | { readonly type: "track.reorder"; readonly ids: readonly string[] }
  /** Replaces the track's fields the patch has. */
  | {
      readonly type: "track.update";
      readonly id: string;
      readonly patch: Partial<
        Pick<Track, "name" | "audio" | "captions" | "transitions">
      >;
    }
  /** Merges `patch` into the track's editor metadata. Unlocking is allowed on a locked track. */
  | {
      readonly type: "track.metadata";
      readonly id: string;
      readonly patch: TrackMetadata;
    }
  /** Merges `patch` into the clip's editor metadata. */
  | {
      readonly type: "clip.metadata";
      readonly id: string;
      readonly patch: ClipMetadata;
    }
  /**
   * Adds a clip to a track as it is: nothing makes room for it. A clip whose composition something
   * else draws shows its own copy of it (and of the group definitions in it), so a duplicate edits
   * independently of its original. `compositions` are those the clip drew when it was copied: it
   * shows a copy of them, whether or not the document still has them.
   */
  | {
      readonly type: "clip.insert";
      readonly trackId: string;
      readonly clip: Clip;
      readonly name?: string;
      readonly compositions?: readonly Composition[];
    }
  /**
   * Puts a clip down at `at` on a track: `insert` makes room by moving later content (on
   * sync-locked tracks too), `overwrite` replaces what is there, `append` goes after the track's
   * last clip. A clip the new one cuts in two keeps its id on the left; the right part is
   * `splitId`. Like `clip.insert`, a clip whose composition something else draws shows its own copy
   * of it.
   */
  | {
      readonly type: "clip.place";
      readonly mode: "insert" | "overwrite" | "append";
      readonly trackId: string;
      readonly clip: Clip;
      readonly at: number;
      readonly splitId?: string;
      readonly name?: string;
    }
  /** Shows another part of a clip's source in the same place, `delta` source ticks later. */
  | { readonly type: "clip.slip"; readonly id: string; readonly delta: number }
  /** Moves a clip `delta` ticks between its neighbours, which give and take the time. */
  | { readonly type: "clip.slide"; readonly id: string; readonly delta: number }
  /**
   * Gives a clip a new length from where it starts: its speed, for a clip that plays forward or
   * reversed. `ripple` moves what follows (on sync-locked tracks too); `keyframes` keeps or
   * stretches its keyframes, fades, and composition time.
   */
  | {
      readonly type: "clip.retime";
      readonly id: string;
      readonly duration: number;
      readonly reverse?: boolean;
      readonly ripple?: boolean;
      readonly keyframes?: "keep" | "stretch";
    }
  /** Adds a marker; its id must be new. */
  | { readonly type: "marker.add"; readonly marker: Marker }
  /** Applies `patch` to a marker; a field patched to `undefined` is removed. */
  | {
      readonly type: "marker.update";
      readonly id: string;
      readonly patch: Partial<Omit<Marker, "id">>;
    }
  | { readonly type: "marker.remove"; readonly id: string }
  /** Moves the cut between two clips that meet, keeping their total length. */
  | {
      readonly type: "clip.roll";
      readonly leftId: string;
      readonly rightId: string;
      readonly at: number;
    }
  /** Moves a clip to start at `at`, onto track `trackId` when given. */
  | {
      readonly type: "clip.move";
      readonly id: string;
      readonly at: number;
      readonly trackId?: string;
    }
  /**
   * Moves several clips as one: only where they all land must be valid, whatever order `moves`
   * lists them in.
   */
  | {
      readonly type: "clips.move";
      readonly moves: readonly {
        readonly id: string;
        readonly at: number;
        readonly trackId?: string;
      }[];
    }
  /**
   * Moves a clip's `edge` to `at`, showing more or less of its source. `ripple` moves what follows
   * by the same amount (on sync-locked tracks too).
   */
  | {
      readonly type: "clip.trim";
      readonly id: string;
      readonly edge: "start" | "end";
      readonly at: number;
      readonly ripple?: boolean;
    }
  /** Cuts a clip in two at `at`: the left part keeps its id, the right part is `newId`. */
  | {
      readonly type: "clip.split";
      readonly id: string;
      readonly at: number;
      readonly newId: string;
    }
  /**
   * Removes a clip and its transitions. `ripple` closes the gap by moving what follows (on
   * sync-locked tracks too).
   */
  | {
      readonly type: "clip.delete";
      readonly id: string;
      readonly ripple?: boolean;
    }
  /** Replaces the clip's fields the patch has. A `cache` of `auto` removes the field. */
  | {
      readonly type: "clip.update";
      readonly id: string;
      readonly patch: Partial<
        Pick<
          Clip,
          | "visual"
          | "audio"
          | "blend_mode"
          | "masks"
          | "matte"
          | "time_map"
          | "source_range"
          | "cache"
        >
      >;
    }
  /**
   * Adds an effect on top of clip `id`'s effect stack as `instanceId`. The `effect.*` commands are
   * refused for a clip whose composition is a graph rather than a stack (see `effectStack`).
   */
  | {
      readonly type: "effect.add";
      readonly id: string;
      readonly instanceId: string;
      readonly effect: ClipEffect;
    }
  /** Replaces an effect of clip `id`'s stack. */
  | {
      readonly type: "effect.update";
      readonly id: string;
      readonly instanceId: string;
      readonly effect: ClipEffect;
      /** Whether the effect is switched off; unchanged when omitted. */
      readonly bypass?: boolean;
    }
  /** Removes an effect from clip `id`'s stack; an empty stack removes the clip's composition. */
  | {
      readonly type: "effect.remove";
      readonly id: string;
      readonly instanceId: string;
    }
  /**
   * Puts clip `id`'s effects in the order `ids` lists them, bottom first; `ids` names each once.
   */
  | {
      readonly type: "effect.reorder";
      readonly id: string;
      readonly ids: readonly string[];
    }
  /** Gives a clip without one a composition that shows its picture. */
  | {
      readonly type: "graph.create";
      readonly clipId: string;
      /** The new composition's id; a free one based on `<clip>-fx` when omitted. */
      readonly compositionId?: string;
    }
  /** Adds a node to a composition; its id must be new there. */
  | {
      readonly type: "graph.node.add";
      readonly compositionId: string;
      readonly node: CompositionNode;
      /** Where the node editor draws it. */
      readonly position?: GraphPosition;
    }
  /** Replaces a node's operation and bypass; its id stays. */
  | {
      readonly type: "graph.node.update";
      readonly compositionId: string;
      readonly node: CompositionNode;
    }
  /** Removes nodes, disconnecting every input that read them. */
  | {
      readonly type: "graph.node.remove";
      readonly compositionId: string;
      readonly ids: readonly string[];
    }
  /**
   * Connects node `from`'s picture to input `input` of node `to`; `"layer"` adds a merge layer on
   * top.
   */
  | {
      readonly type: "graph.connect";
      readonly compositionId: string;
      readonly from: string;
      readonly to: string;
      readonly input: InputKey | "layer";
    }
  /** Disconnects input `input` of node `to`; a merge layer's slot is removed with its wire. */
  | {
      readonly type: "graph.disconnect";
      readonly compositionId: string;
      readonly to: string;
      readonly input: InputKey;
    }
  /** Makes node `nodeId` the composition's output; `null` leaves it without one. */
  | {
      readonly type: "graph.output";
      readonly compositionId: string;
      readonly nodeId: string | null;
    }
  /**
   * Folds nodes into a group node that draws a new composition of them; the picture does not
   * change. Ids are free ones based on `group` and `<composition>-group` when omitted.
   */
  | {
      readonly type: "graph.group";
      readonly compositionId: string;
      readonly ids: readonly string[];
      readonly groupId?: string;
      readonly definitionId?: string;
      /** What the group is called in the node editor. */
      readonly name?: string;
    }
  /** Adds a composition, such as a group definition a macro brings. */
  | { readonly type: "composition.add"; readonly composition: Composition }
  /** Replaces a group node with the nodes it draws; the picture does not change. */
  | {
      readonly type: "graph.ungroup";
      readonly compositionId: string;
      readonly nodeId: string;
    }
  /** Gives a composition another group input, unconnected on every group node that uses it. */
  | {
      readonly type: "graph.port.add";
      readonly compositionId: string;
      readonly name: string;
    }
  /** Exposes a node's effect parameter to the group nodes that use its composition. */
  | {
      readonly type: "graph.control.add";
      readonly compositionId: string;
      readonly control: {
        readonly name: string;
        readonly node: string;
        readonly param: string;
      };
    }
  /** Stops exposing a control, dropping the values group nodes set for it. */
  | {
      readonly type: "graph.control.remove";
      readonly compositionId: string;
      readonly name: string;
    }
  /**
   * Makes a composition a Library item: a composition source that shows it, which clips place like
   * media. It starts with a background to build on.
   */
  | {
      readonly type: "composition.create";
      readonly compositionId: string;
      readonly sourceId: string;
      readonly name: string;
    }
  /** Moves nodes in the node editor; the picture does not change. */
  | {
      readonly type: "graph.layout";
      readonly compositionId: string;
      readonly positions: Readonly<Record<string, GraphPosition>>;
    }
  /** Adds a cue to a caption track under a new `instanceId`, in its place by time. */
  | {
      readonly type: "cue.add";
      readonly trackId: string;
      readonly instanceId: string;
      readonly cue: CaptionCue;
    }
  /** Replaces the cue `instanceId` names; a cue given another time moves to its place by it. */
  | {
      readonly type: "cue.update";
      readonly trackId: string;
      readonly instanceId: string;
      readonly cue: CaptionCue;
    }
  | {
      readonly type: "cue.remove";
      readonly trackId: string;
      readonly instanceId: string;
    }
  /**
   * Adds or replaces a bin asset, with the project source or effect resource of the same id when
   * given. `contentRevision` may not go below the replaced asset's.
   */
  | {
      readonly type: "asset.put";
      readonly asset: AssetDescriptor;
      readonly source?: MediaSource;
      readonly resource?: EffectResource;
    }
  /**
   * Removes a bin asset and its project source or effect resource; a clip still reading it is
   * refused.
   */
  | { readonly type: "asset.remove"; readonly id: string }
  /** Replaces the document metadata fields the patch has. */
  | {
      readonly type: "metadata.update";
      readonly patch: Partial<
        Pick<
          EditorDocument["metadata"],
          "markers" | "exportRange" | "graphLayout" | "graphNames"
        >
      >;
    };
const NO_NAMES: Readonly<Record<string, string>> = Object.freeze({});
/** The names the node editor shows for group compositions, by composition id. */
export function graphNames(
  document: EditorDocument,
): Readonly<Record<string, string>> {
  return document.metadata.graphNames ?? NO_NAMES;
}
function withGraphName(
  document: EditorDocument,
  id: string,
  name: string | undefined,
): EditorDocument {
  if (!name) return document;
  return {
    ...document,
    metadata: {
      ...document.metadata,
      graphNames: {
        ...graphNames(document),
        [id]: name,
      },
    },
  };
}
/** The node editor positions a document stores. */
export function graphLayout(document: EditorDocument): GraphLayout {
  return document.metadata.graphLayout ?? {};
}
function withLayout(
  document: EditorDocument,
  compositionId: string,
  positions: Readonly<Record<string, GraphPosition>>,
): EditorDocument {
  for (const position of Object.values(positions))
    if (!Number.isFinite(position.x) || !Number.isFinite(position.y))
      fail("Node positions must be finite");
  const layout = graphLayout(document);
  return {
    ...document,
    metadata: {
      ...document.metadata,
      graphLayout: {
        ...layout,
        [compositionId]: { ...layout[compositionId], ...positions },
      },
    },
  };
}
function changeComposition(
  document: EditorDocument,
  id: string,
  update: (composition: Composition) => Composition,
): EditorDocument {
  const composition = findComposition(document.project, id);
  if (!composition) fail("Unknown composition", id);
  const next = update(composition);
  return {
    ...document,
    project: {
      ...document.project,
      compositions: (document.project.compositions ?? []).map((c) =>
        c.id === id ? next : c,
      ),
    },
  };
}
/**
 * `document` with `update` applied to every group node that draws composition `id`. A composition
 * without one stays the same object: the track lock tells what an edit changed by identity.
 */
function mapGroupNodes(
  document: EditorDocument,
  id: string,
  update: (
    op: Extract<CompositionNode["op"], { type: "group" }>,
  ) => CompositionNode["op"],
): EditorDocument {
  const draws = (node: CompositionNode) =>
    node.op.type === "group" && node.op.composition === id;
  return {
    ...document,
    project: {
      ...document.project,
      compositions: (document.project.compositions ?? []).map((c) =>
        (c.nodes ?? []).some(draws)
          ? {
              ...c,
              nodes: (c.nodes ?? []).map((node) =>
                node.op.type === "group" && node.op.composition === id
                  ? { ...node, op: update(node.op) }
                  : node,
              ),
            }
          : c,
      ),
    },
  };
}
function nodeIn(composition: Composition, id: string): CompositionNode {
  const node = (composition.nodes ?? []).find((n) => n.id === id);
  if (!node) fail("Unknown node", id);
  return node;
}
/**
 * The tracks a ripple edit on `trackId` moves too: every other sync-locked track. A locked one
 * among them still refuses the edit, as any change to a locked track does.
 */
function syncTrackIds(document: EditorDocument, trackId: string): string[] {
  return document.project.timeline.tracks
    .filter((t) => t.id !== trackId && document.metadata.tracks[t.id]?.syncLock)
    .map((t) => t.id);
}
/** `marker` with `patch` applied; a field patched to `undefined` is removed. */
function patchedMarker(
  marker: Marker,
  patch: Partial<Omit<Marker, "id">>,
): Marker {
  const next: Record<string, unknown> = { ...marker, ...patch };
  for (const [key, value] of Object.entries(next))
    if (value === undefined) delete next[key];
  return next as unknown as Marker;
}
/**
 * `document` with every timeline marker (and range marker end) at or after `pivot` moved by
 * `delta`. A negative `delta` closes the time `[pivot + delta, pivot)`: a marker with any time
 * there refuses the edit (`sync_conflict`), as content there on a sync-locked track does, rather
 * than land on what the edit pulls in.
 */
function shiftMarkers(
  document: EditorDocument,
  pivot: number,
  delta: number,
): EditorDocument {
  if (!delta) return document;
  if (delta < 0) {
    const closed = { start: pivot + delta, end: pivot };
    const inside = document.metadata.markers.find((m) =>
      m.end === undefined
        ? m.time >= closed.start && m.time < closed.end
        : m.time < closed.end && m.end > closed.start,
    );
    if (inside)
      throw new EditorError(
        "sync_conflict",
        `Marker ${inside.name || inside.id} is in the time the edit closes`,
        "dispatch",
        inside.id,
      );
  }
  const markers = document.metadata.markers.map((m) => {
    const time = m.time >= pivot ? m.time + delta : m.time;
    if (m.end === undefined) return time === m.time ? m : { ...m, time };
    const end = m.end >= pivot ? m.end + delta : m.end;
    return time === m.time && end === m.end ? m : { ...m, time, end };
  });
  if (markers.some((m) => m.time < 0))
    fail("The edit would move a marker before the timeline start");
  return { ...document, metadata: { ...document.metadata, markers } };
}
/** The metadata the right part `newId` of a clip `leftId` cut in two inherits. */
function afterSplit(
  document: EditorDocument,
  newId: string,
  leftId: string,
): EditorDocument {
  if (!indexDocument(document).clips.has(newId)) return document;
  let next: EditorDocument = {
    ...document,
    metadata: {
      ...document.metadata,
      clips: {
        ...document.metadata.clips,
        [newId]: document.metadata.clips[leftId] ?? {},
      },
    },
  };
  next = ids(
    next,
    `${newId}/masks`,
    (next.metadata.instanceIds[`${leftId}/masks`] ?? []).map(
      (_, i) => `${newId}/masks/${i}`,
    ),
  );
  const index = indexDocument(next);
  const left = index.clips.get(leftId)?.clip.composition?.id;
  const right = index.clips.get(newId)?.clip.composition?.id;
  return left && right && left !== right
    ? withCopiedGraphs(
        next,
        correspondingCompositions(next.project, left, right),
      )
    : next;
}
/**
 * `document` with each copy in `copies` (by original id) drawn in the node editor where its
 * original is, under its name.
 */
function withCopiedGraphs(
  document: EditorDocument,
  copies: ReadonlyMap<string, string>,
): EditorDocument {
  const layout = graphLayout(document);
  const names = graphNames(document);
  const positions: Record<string, GraphLayout[string]> = {};
  const named: Record<string, string> = {};
  for (const [original, copy] of copies) {
    if (original === copy) continue;
    if (layout[original]) positions[copy] = layout[original];
    if (names[original]) named[copy] = names[original];
  }
  if (!Object.keys(positions).length && !Object.keys(named).length)
    return document;
  return {
    ...document,
    metadata: {
      ...document.metadata,
      graphLayout: {
        ...layout,
        ...positions,
      },
      graphNames: { ...names, ...named },
    },
  };
}
/** `document` without the node editor's positions and names of compositions `removed`. */
function withoutGraphs(
  document: EditorDocument,
  removed: ReadonlySet<string>,
): EditorDocument {
  const layout = graphLayout(document);
  const names = graphNames(document);
  if (
    ![...removed].some(
      (id) => Object.hasOwn(layout, id) || Object.hasOwn(names, id),
    )
  )
    return document;
  const without = <T>(map: Readonly<Record<string, T>>) =>
    Object.fromEntries(Object.entries(map).filter(([id]) => !removed.has(id)));
  return {
    ...document,
    metadata: {
      ...document.metadata,
      graphLayout: without(layout),
      graphNames: without(names),
    },
  };
}
/**
 * The project an `alpha2/edit` operation made for `command`, or its refusal as an `EditorError`
 * about `id`. Outside a transaction the edit validated what it made, so `executeCommand` does not
 * validate the same project again.
 */
function edited(
  result: edit.EditResult,
  command: Command,
  id: string | undefined,
): Project {
  if (!result.ok)
    throw new EditorError(
      result.error.code,
      result.error.message,
      command.type,
      id,
    );
  if (transactions === 0) rememberValid(result.project);
  return result.project;
}
/** How many transactions `apply` is inside: their edits leave validation to `executeCommand`. */
let transactions = 0;
function fail(message: string, id?: string): never {
  throw new EditorError("invalid_command", message, "dispatch", id);
}
function replaceTrack(project: Project, track: Track): Project {
  return {
    ...project,
    timeline: {
      ...project.timeline,
      tracks: project.timeline.tracks.map((t) =>
        t.id === track.id ? track : t,
      ),
    },
  };
}
function changeClip(
  document: EditorDocument,
  id: string,
  update: (clip: Clip) => Clip,
): EditorDocument {
  const found = indexDocument(document).clips.get(id);
  if (!found) fail("Unknown clip", id);
  const track = indexDocument(document).tracks.get(found.trackId)!;
  return {
    ...document,
    project: replaceTrack(document.project, {
      ...track,
      clips: (track.clips ?? []).map((c) => (c.id === id ? update(c) : c)),
    }),
  };
}
function ids(
  document: EditorDocument,
  key: string,
  values: readonly string[],
): EditorDocument {
  return {
    ...document,
    metadata: {
      ...document.metadata,
      instanceIds: { ...document.metadata.instanceIds, [key]: values },
    },
  };
}
function sameSet(a: readonly string[], b: readonly string[]): boolean {
  return (
    a.length === b.length &&
    new Set(b).size === b.length &&
    a.every((x) => b.includes(x))
  );
}
function reduce(
  document: EditorDocument,
  command: Exclude<Command, { type: "transaction" }>,
): EditorDocument {
  const project = document.project;
  const trackId =
    "trackId" in command
      ? command.trackId
      : command.type.startsWith("track.") && "id" in command
        ? command.id
        : undefined;
  const track = trackId
    ? indexDocument(document).tracks.get(trackId)
    : undefined;
  if (trackId && !track) fail("Unknown track", trackId);
  switch (command.type) {
    case "transition.add":
      return {
        ...document,
        project: replaceTrack(
          project,
          addTransition(
            track!,
            command.fromId,
            command.toId,
            command.duration,
            command.effect,
          ),
        ),
      };
    case "transition.resize":
    case "transition.remove": {
      const index = (track!.transitions ?? []).findIndex(
        (t) => transitionInstanceId(track!.id, t) === command.instanceId,
      );
      if (index < 0) fail("Unknown transition", command.instanceId);
      return {
        ...document,
        project: replaceTrack(
          project,
          command.type === "transition.resize"
            ? resizeTransition(track!, index, command.duration)
            : removeTransition(track!, index),
        ),
      };
    }

    case "project.update":
      return { ...document, project: { ...project, ...command.patch } };
    case "metadata.update":
      return {
        ...document,
        metadata: { ...document.metadata, ...command.patch },
      };
    case "track.add": {
      // Each brought clip shows its own copy of a shared composition, as clip.insert does.
      const withTrack = (on: Project, track: Track): Project => ({
        ...on,
        timeline: {
          ...on.timeline,
          tracks: [...project.timeline.tracks, track],
        },
      });
      let next = withTrack(project, command.track);
      if (!command.track.clips?.length) return { ...document, project: next };
      const copies = new Map<string, string>();
      const clips: Clip[] = [];
      for (const clip of command.track.clips) {
        const owned = ownComposition(
          withTrack(next, { ...command.track, clips: [...clips] }),
          clip,
        );
        for (const [original, copy] of owned.copies) copies.set(original, copy);
        clips.push(owned.clip);
        next = owned.project;
      }
      next = withTrack(next, { ...command.track, clips });
      return withCopiedGraphs({ ...document, project: next }, copies);
    }
    case "track.remove":
      return {
        ...document,
        project: {
          ...project,
          timeline: {
            ...project.timeline,
            tracks: project.timeline.tracks.filter((t) => t.id !== command.id),
          },
        },
      };
    case "track.reorder": {
      if (
        !sameSet(
          project.timeline.tracks.map((t) => t.id),
          command.ids,
        )
      )
        fail("Track order must be a permutation");
      return {
        ...document,
        project: {
          ...project,
          timeline: {
            ...project.timeline,
            tracks: command.ids.map((id) =>
              indexDocument(document).tracks.get(id)!,
            ),
          },
        },
      };
    }
    case "track.update":
      return {
        ...document,
        project: replaceTrack(project, { ...track!, ...command.patch }),
      };
    case "track.metadata":
      return {
        ...document,
        metadata: {
          ...document.metadata,
          tracks: {
            ...document.metadata.tracks,
            [command.id]: {
              ...document.metadata.tracks[command.id],
              ...command.patch,
            },
          },
        },
      };
    case "clip.metadata":
      if (!indexDocument(document).clips.has(command.id))
        fail("Unknown clip", command.id);
      return {
        ...document,
        metadata: {
          ...document.metadata,
          clips: {
            ...document.metadata.clips,
            [command.id]: {
              ...document.metadata.clips[command.id],
              ...command.patch,
            },
          },
        },
      };
    case "clip.insert": {
      // A pasted or duplicated clip edits its own copy of a composition.
      const owned = ownComposition(project, command.clip, command.compositions);
      return withCopiedGraphs(
        {
          ...document,
          project: replaceTrack(owned.project, {
            ...track!,
            clips: [...(track!.clips ?? []), owned.clip],
          }),
          metadata: {
            ...document.metadata,
            clips: {
              ...document.metadata.clips,
              // Unnamed clips show their source's name; never invent one.
              [command.clip.id]: command.name ? { name: command.name } : {},
            },
          },
        },
        owned.copies,
      );
    }
    case "clip.place": {
      const syncTracks = syncTrackIds(document, command.trackId);
      const options = { splitId: command.splitId, syncTracks };
      const owned = ownComposition(project, command.clip);
      const result =
        command.mode === "insert"
          ? edit.insert(
              owned.project,
              command.trackId,
              owned.clip,
              command.at,
              options,
            )
          : command.mode === "overwrite"
            ? edit.overwrite(
                owned.project,
                command.trackId,
                owned.clip,
                command.at,
                options,
              )
            : edit.append(owned.project, command.trackId, owned.clip);
      const made = edited(result, command, command.clip.id);
      let next: EditorDocument = {
        ...document,
        project: made,
        metadata: {
          ...document.metadata,
          clips: {
            ...document.metadata.clips,
            [command.clip.id]: command.name ? { name: command.name } : {},
          },
        },
      };
      if (command.splitId) {
        const cut = (
          indexDocument(document).tracks.get(command.trackId)?.clips ?? []
        ).find(
          (c) =>
            c.timeline_range.start < command.at &&
            c.timeline_range.end > command.at,
        );
        if (cut) next = afterSplit(next, command.splitId, cut.id);
      }
      if (command.mode === "insert" && syncTracks.length)
        next = shiftMarkers(
          next,
          command.at,
          command.clip.timeline_range.end - command.clip.timeline_range.start,
        );
      return withCopiedGraphs(next, owned.copies);
    }
    case "clip.slip":
    case "clip.slide": {
      const result =
        command.type === "clip.slip"
          ? edit.slip(project, command.id, command.delta)
          : edit.slide(project, command.id, command.delta);
      const made = edited(result, command, command.id);
      return { ...document, project: made };
    }
    case "clip.retime": {
      const found = indexDocument(document).clips.get(command.id);
      if (!found) fail("Unknown clip", command.id);
      const syncTracks = command.ripple
        ? syncTrackIds(document, found.trackId)
        : [];
      const result = edit.retime(project, command.id, command.duration, {
        ...(command.reverse === undefined ? {} : { reverse: command.reverse }),
        ...(command.ripple ? { ripple: true, syncTracks } : {}),
        ...(command.keyframes ? { keyframes: command.keyframes } : {}),
      });
      const made = edited(result, command, command.id);
      const next = { ...document, project: made };
      // Timeline markers move with a ripple that reaches other tracks.
      const before = found.clip.timeline_range;
      return syncTracks.length
        ? shiftMarkers(
            next,
            before.end,
            before.start + command.duration - before.end,
          )
        : next;
    }
    case "marker.add":
      if (document.metadata.markers.some((m) => m.id === command.marker.id))
        fail("A marker already has this id", command.marker.id);
      return {
        ...document,
        metadata: {
          ...document.metadata,
          markers: [...document.metadata.markers, command.marker],
        },
      };
    case "marker.update":
    case "marker.remove": {
      if (!document.metadata.markers.some((m) => m.id === command.id))
        fail("Unknown marker", command.id);
      return {
        ...document,
        metadata: {
          ...document.metadata,
          markers:
            command.type === "marker.remove"
              ? document.metadata.markers.filter((m) => m.id !== command.id)
              : document.metadata.markers.map((m) =>
                  m.id === command.id ? patchedMarker(m, command.patch) : m,
                ),
        },
      };
    }
    case "clip.roll": {
      const result = edit.rollTrim(
        project,
        command.leftId,
        command.rightId,
        command.at,
      );
      const made = edited(result, command, command.leftId);
      return { ...document, project: made };
    }
    case "clips.move": {
      const result = edit.moveClips(
        project,
        command.moves.map((m) => ({ clip: m.id, to: m.at, track: m.trackId })),
      );
      const made = edited(result, command, command.moves[0]?.id);
      return { ...document, project: made };
    }
    case "clip.move":
    case "clip.trim":
    case "clip.split":
    case "clip.delete": {
      let result: edit.EditResult;
      const found = indexDocument(document).clips.get(command.id);
      const syncTracks =
        "ripple" in command && command.ripple && found
          ? syncTrackIds(document, found.trackId)
          : [];
      if (command.type === "clip.move")
        result = command.trackId
          ? edit.moveToTrack(project, command.id, command.trackId, command.at)
          : edit.move(project, command.id, command.at);
      else if (command.type === "clip.trim")
        result = command.ripple
          ? edit.rippleTrim(project, command.id, command.edge, command.at, {
              syncTracks,
            })
          : edit.trim(project, command.id, command.edge, command.at);
      else if (command.type === "clip.split")
        result = edit.split(project, command.id, command.at, command.newId);
      else if (command.ripple)
        result = edit.rippleDelete(project, command.id, { syncTracks });
      else {
        if (!found) fail("Unknown clip", command.id);
        const old = indexDocument(document).tracks.get(found.trackId)!;
        result = {
          ok: true,
          project: replaceTrack(project, {
            ...old,
            clips: (old.clips ?? []).filter((c) => c.id !== command.id),
            transitions: (old.transitions ?? []).filter(
              (t) => t.from_clip !== command.id && t.to_clip !== command.id,
            ),
          }),
        };
      }
      // A delete that closes nothing is validated with the rest.
      const made =
        command.type === "clip.delete" && !command.ripple && result.ok
          ? result.project
          : edited(result, command, command.id);
      let next = { ...document, project: made };
      // Timeline markers move with a ripple that reaches other tracks.
      if (syncTracks.length && found) {
        const before = found.clip.timeline_range;
        if (command.type === "clip.trim") {
          const delta =
            command.edge === "end"
              ? command.at - before.end
              : before.start - command.at;
          next = shiftMarkers(next, before.end, delta);
        } else if (command.type === "clip.delete") {
          const gap = edit.rippleDeleteSpan(project, command.id)!;
          next = shiftMarkers(next, gap.end, gap.start - gap.end);
        }
      }
      if (command.type === "clip.split")
        next = afterSplit(next, command.newId, command.id);
      return next;
    }
    case "clip.update":
      return changeClip(document, command.id, (c) => {
        const next = { ...c, ...command.patch };
        // `auto` is the default the engine writes by leaving the field out.
        if (next.cache === "auto") delete next.cache;
        return next;
      });
    case "effect.add":
    case "effect.remove":
    case "effect.update":
    case "effect.reorder": {
      const found = indexDocument(document).clips.get(command.id);
      if (!found) fail("Unknown clip", command.id);
      const current = effectStack(project, found.clip);
      if (!current)
        fail(
          "The clip's composition is a graph, not an effect stack; edit it as a graph",
          command.id,
        );
      const order = current.map((entry) => entry.id);
      let stack: StackEntry[] = [...current];
      if (command.type === "effect.add") {
        if (order.includes(command.instanceId)) fail("Duplicate instance ID");
        stack.push({
          id: command.instanceId,
          effect: command.effect,
          bypass: false,
        });
      } else if (command.type === "effect.reorder") {
        if (!sameSet(order, command.ids))
          fail("Effect order must be a permutation");
        stack = command.ids.map((id) => current[order.indexOf(id)]);
      } else {
        const i = order.indexOf(command.instanceId);
        if (i < 0) fail("Unknown effect instance");
        if (command.type === "effect.remove") stack.splice(i, 1);
        else
          stack[i] = {
            id: command.instanceId,
            effect: command.effect,
            bypass: command.bypass ?? stack[i].bypass,
          };
      }
      return {
        ...document,
        project: withEffectStack(project, command.id, stack),
      };
    }
    case "graph.create": {
      const found = indexDocument(document).clips.get(command.clipId);
      if (!found) fail("Unknown clip", command.clipId);
      if (found.clip.composition != null)
        fail("The clip already has a composition", command.clipId);
      const id =
        command.compositionId ??
        freeCompositionId(project, `${command.clipId}-fx`);
      if (findComposition(project, id)) fail("Duplicate composition", id);
      const created: Composition = {
        id,
        nodes: [{ id: "clip", op: { type: "clip" } }],
        output: "clip",
      };
      return changeClip(
        {
          ...document,
          project: {
            ...project,
            compositions: [...(project.compositions ?? []), created],
          },
        },
        command.clipId,
        (c) => ({ ...c, composition: { id } }),
      );
    }
    case "graph.node.add": {
      const next = changeComposition(document, command.compositionId, (c) => {
        if ((c.nodes ?? []).some((n) => n.id === command.node.id))
          fail("Duplicate node", command.node.id);
        return { ...c, nodes: [...(c.nodes ?? []), command.node] };
      });
      return command.position
        ? withLayout(next, command.compositionId, {
            [command.node.id]: command.position,
          })
        : next;
    }
    case "graph.node.update":
      return changeComposition(document, command.compositionId, (c) => {
        nodeIn(c, command.node.id);
        return {
          ...c,
          nodes: (c.nodes ?? []).map((n) =>
            n.id === command.node.id ? command.node : n,
          ),
        };
      });
    case "graph.node.remove": {
      // A control goes with the node it exposes, and so do the values group nodes set for it.
      let dropped: string[] = [];
      const next = changeComposition(document, command.compositionId, (c) => {
        for (const id of command.ids) nodeIn(c, id);
        const kept = withoutNodes(c, command.ids);
        const names = new Set((kept.controls ?? []).map((k) => k.name));
        dropped = (c.controls ?? [])
          .map((k) => k.name)
          .filter((name) => !names.has(name));
        return kept;
      });
      if (!dropped.length) return next;
      return mapGroupNodes(next, command.compositionId, (op) => ({
        ...op,
        controls: Object.fromEntries(
          Object.entries(op.controls ?? {}).filter(
            ([name]) => !dropped.includes(name),
          ),
        ),
      }));
    }
    case "graph.connect":
    case "graph.disconnect":
      return changeComposition(document, command.compositionId, (c) => {
        const target = nodeIn(c, command.to);
        let op = target.op;
        let key = command.input;
        if (command.type === "graph.connect") {
          nodeIn(c, command.from);
          if (command.from === command.to)
            fail("A node cannot read its own picture", command.to);
          if (key === "layer") {
            if (op.type !== "merge")
              fail("Only a merge has layers", command.to);
            op = { ...op, layers: [...(op.layers ?? []), { input: null }] };
            key = (op.layers ?? []).length - 1;
          }
        } else if (op.type === "merge" && typeof key === "number") {
          return {
            ...c,
            nodes: (c.nodes ?? []).map((n) =>
              n.id === command.to && op.type === "merge"
                ? {
                    ...n,
                    op: {
                      ...op,
                      layers: (op.layers ?? []).filter((_, i) => i !== key),
                    },
                  }
                : n,
            ),
          };
        }
        if (!inputKeys(op).includes(key as InputKey))
          fail(`No input ${String(key)} on node ${command.to}`, command.to);
        const connected = withInput(
          op,
          key as InputKey,
          command.type === "graph.connect" ? command.from : null,
        );
        return {
          ...c,
          nodes: (c.nodes ?? []).map((n) =>
            n.id === command.to ? { ...n, op: connected } : n,
          ),
        };
      });
    case "graph.output":
      return changeComposition(document, command.compositionId, (c) => {
        if (command.nodeId !== null) nodeIn(c, command.nodeId);
        return { ...c, output: command.nodeId };
      });
    case "graph.group": {
      const composition = findComposition(project, command.compositionId);
      if (!composition) fail("Unknown composition", command.compositionId);
      const groupId = command.groupId ?? freeNodeId(composition, "group");
      const definitionId =
        command.definitionId ??
        freeCompositionId(project, `${command.compositionId}-group`);
      if (findComposition(project, definitionId))
        fail("A composition already has this id", definitionId);
      let grouped: ReturnType<typeof groupNodes>;
      try {
        grouped = groupNodes(composition, command.ids, groupId, definitionId);
      } catch (error) {
        fail((error as Error).message, command.compositionId);
      }
      const layout = graphLayout(document)[command.compositionId] ?? {};
      // What moved into the definition: a selected group input stays where it is.
      const moved = new Set(
        (grouped.definition.nodes ?? []).map((node) => node.id),
      );
      const grouping = command.ids.filter((id) => moved.has(id));
      const placed = grouping
        .map((id) => layout[id])
        .filter((p): p is GraphPosition => p !== undefined);
      const centre = placed.length
        ? {
            x: placed.reduce((sum, p) => sum + p.x, 0) / placed.length,
            y: placed.reduce((sum, p) => sum + p.y, 0) / placed.length,
          }
        : { x: 0, y: 0 };
      const inner: Record<string, GraphPosition> = {};
      for (const id of grouping) if (layout[id]) inner[id] = layout[id];
      const left = Math.min(...placed.map((p) => p.x), centre.x) - 220;
      (grouped.definition.nodes ?? [])
        .filter((node) => node.op.type === "group_input")
        .forEach((node, index) => {
          inner[node.id] = { x: left, y: centre.y + index * 110 };
        });
      let next: EditorDocument = {
        ...document,
        project: {
          ...project,
          compositions: [
            ...(project.compositions ?? []).map((c) =>
              c.id === command.compositionId ? grouped.parent : c,
            ),
            grouped.definition,
          ],
        },
      };
      next = withLayout(next, command.compositionId, { [groupId]: centre });
      next = withGraphName(next, definitionId, command.name);
      return withLayout(next, definitionId, inner);
    }
    case "graph.ungroup": {
      const composition = findComposition(project, command.compositionId);
      if (!composition) fail("Unknown composition", command.compositionId);
      const group = nodeIn(composition, command.nodeId);
      if (group.op.type !== "group")
        fail("Only a group node ungroups", command.nodeId);
      // A bypassed group passes its first input through and draws nothing of its definition, which
      // inlined nodes cannot say in general.
      if (group.bypass)
        fail(
          `Turn group ${command.nodeId} on before ungrouping it: ungrouped, its nodes would draw`,
          command.nodeId,
        );
      const definition = findComposition(project, group.op.composition);
      if (!definition) fail("Unknown composition", group.op.composition);
      const { parent, ids } = ungroupNode(
        composition,
        command.nodeId,
        definition,
      );
      let next: EditorDocument = {
        ...document,
        project: {
          ...project,
          compositions: (project.compositions ?? []).map((c) =>
            c.id === command.compositionId ? parent : c,
          ),
        },
      };
      const layout = graphLayout(document);
      const at = layout[command.compositionId]?.[command.nodeId] ?? {
        x: 0,
        y: 0,
      };
      const inner = layout[definition.id] ?? {};
      const placed = [...ids].filter(([id]) => inner[id]);
      const cx =
        placed.reduce((sum, [id]) => sum + inner[id].x, 0) /
        Math.max(1, placed.length);
      const cy =
        placed.reduce((sum, [id]) => sum + inner[id].y, 0) /
        Math.max(1, placed.length);
      next = withLayout(
        next,
        command.compositionId,
        Object.fromEntries(
          [...ids].map(([id, outer], index) => [
            outer,
            inner[id]
              ? { x: at.x + inner[id].x - cx, y: at.y + inner[id].y - cy }
              : { x: at.x + index * 40, y: at.y + 120 },
          ]),
        ),
      );
      return next;
    }
    case "composition.add":
      if (findComposition(project, command.composition.id))
        fail("A composition already has this id", command.composition.id);
      return {
        ...document,
        project: {
          ...project,
          compositions: [...(project.compositions ?? []), command.composition],
        },
      };
    case "graph.port.add": {
      const name = command.name.trim();
      if (!name) fail("A group input needs a name", command.compositionId);
      const next = changeComposition(document, command.compositionId, (c) => {
        if ((c.inputs ?? []).includes(name))
          fail("A group input already has this name", command.compositionId);
        return { ...c, inputs: [...(c.inputs ?? []), name] };
      });
      return mapGroupNodes(next, command.compositionId, (op) => ({
        ...op,
        inputs: [...(op.inputs ?? []), null],
      }));
    }
    case "graph.control.add":
      return changeComposition(document, command.compositionId, (c) => {
        const target = nodeIn(c, command.control.node);
        if (target.op.type !== "effect" && target.op.type !== "transition")
          fail("Only an effect's settings are controls", command.control.node);
        const controls = c.controls ?? [];
        if (controls.some((control) => control.name === command.control.name))
          fail("A control already has this name", command.compositionId);
        return { ...c, controls: [...controls, { ...command.control }] };
      });
    case "graph.control.remove": {
      const next = changeComposition(document, command.compositionId, (c) => {
        const controls = (c.controls ?? []).filter(
          (control) => control.name !== command.name,
        );
        if (controls.length === (c.controls ?? []).length)
          fail("Unknown control", command.name);
        return { ...c, controls };
      });
      return mapGroupNodes(next, command.compositionId, (op) => {
        const { [command.name]: _dropped, ...controls } = op.controls ?? {};
        return { ...op, controls };
      });
    }
    case "composition.create": {
      if (findComposition(project, command.compositionId))
        fail("A composition already has this id", command.compositionId);
      if (
        project.sources.some((s) => s.id === command.sourceId) ||
        document.assets[command.sourceId]
      )
        fail("A source already has this id", command.sourceId);
      const composition: Composition = {
        id: command.compositionId,
        nodes: [
          {
            id: "background",
            op: {
              type: "generator",
              generator: {
                type: "fill",
                paint: {
                  type: "solid",
                  color: { red: 0.12, green: 0.12, blue: 0.14, alpha: 1 },
                },
              },
            },
          },
        ],
        output: "background",
      };
      return withLayout(
        {
          ...document,
          assets: {
            ...document.assets,
            [command.sourceId]: {
              id: command.sourceId,
              kind: "composition",
              name: command.name,
              contentRevision: 0,
            },
          },
          project: {
            ...project,
            sources: [
              ...project.sources,
              {
                id: command.sourceId,
                kind: "composition",
                composition: command.compositionId,
              },
            ],
            compositions: [...(project.compositions ?? []), composition],
          },
        },
        command.compositionId,
        { background: { x: 0, y: 0 } },
      );
    }
    case "graph.layout":
      if (!findComposition(project, command.compositionId))
        fail("Unknown composition", command.compositionId);
      return withLayout(document, command.compositionId, command.positions);
    case "cue.add":
    case "cue.update":
    case "cue.remove": {
      if (!track?.captions) fail("Not a caption track", command.trackId);
      const key = `${track.id}/cues`;
      const nextIds = [...(document.metadata.instanceIds[key] ?? [])];
      const cues = [...track.captions.cues];
      // Cues stay in start order; overlaps are for validation to report.
      const place = (instanceId: string, cue: CaptionCue) => {
        const at = cues.findIndex(
          (c) => c.timeline_range.start > cue.timeline_range.start,
        );
        const index = at < 0 ? cues.length : at;
        cues.splice(index, 0, cue);
        nextIds.splice(index, 0, instanceId);
      };
      if (command.type === "cue.add") {
        if (nextIds.includes(command.instanceId)) fail("Duplicate cue");
        place(command.instanceId, command.cue);
      } else {
        const i = nextIds.indexOf(command.instanceId);
        if (i < 0) fail("Unknown cue");
        cues.splice(i, 1);
        nextIds.splice(i, 1);
        if (command.type === "cue.update")
          place(command.instanceId, command.cue);
      }
      return ids(
        {
          ...document,
          project: replaceTrack(project, {
            ...track,
            captions: { ...track.captions, cues },
          }),
        },
        key,
        nextIds,
      );
    }
    case "asset.put": {
      const previous = document.assets[command.asset.id];
      if (previous && command.asset.contentRevision < previous.contentRevision)
        fail("Asset revision cannot go backwards");
      const sources = command.source
        ? [
            ...project.sources.filter((s) => s.id !== command.source!.id),
            command.source,
          ]
        : project.sources;
      const resources = command.resource
        ? [
            ...(project.effect_resources ?? []).filter(
              (r) => r.id !== command.resource!.id,
            ),
            command.resource,
          ]
        : project.effect_resources;
      return {
        ...document,
        assets: { ...document.assets, [command.asset.id]: command.asset },
        project: {
          ...project,
          sources,
          ...(resources ? { effect_resources: resources } : {}),
        },
      };
    }
    case "asset.remove": {
      const assets = { ...document.assets };
      delete assets[command.id];
      return {
        ...document,
        assets,
        project: {
          ...project,
          sources: project.sources.filter((s) => s.id !== command.id),
          effect_resources: (project.effect_resources ?? []).filter(
            (s) => s.id !== command.id,
          ),
        },
      };
    }
  }
}
/**
 * Ids of the compositions `clips` draw: their own and their sources', and those these use through
 * group nodes and composition sources.
 */
function reachedCompositions(
  project: Project,
  clips: readonly Clip[],
): Set<string> {
  const sources = new Map(project.sources.map((s) => [s.id, s]));
  const reached = new Set<string>();
  const pending: string[] = [];
  for (const clip of clips) {
    if (clip.composition) pending.push(clip.composition.id);
    const source = sources.get(clip.source_id)?.composition;
    if (source) pending.push(source);
  }
  while (pending.length) {
    const id = pending.pop()!;
    if (reached.has(id)) continue;
    reached.add(id);
    for (const node of findComposition(project, id)?.nodes ?? []) {
      if (node.op.type === "group") pending.push(node.op.composition);
      if (node.op.type === "media") {
        const used = sources.get(node.op.source_id)?.composition;
        if (used) pending.push(used);
      }
    }
  }
  return reached;
}
function apply(document: EditorDocument, command: Command): EditorDocument {
  if (command.type === "transaction") {
    // Only the transaction's result must be valid; `executeCommand` validates it once.
    transactions += 1;
    try {
      return edit.deferValidation(() =>
        command.commands.reduce(apply, document),
      );
    } finally {
      transactions -= 1;
    }
  }
  const next = reduce(document, command);
  assertLocksKept(document, next, command);
  return next;
}
/**
 * Refuses `command` when it changed what a locked track renders, directly or through a ripple,
 * matte, graph or asset edit: the track and its clips, their metadata, the compositions its clips
 * reach (with the groups and composition sources they draw), and the sources it draws and the
 * effect resources its drawn nodes and transitions read, with their bin assets. Fonts are the whole
 * project's fallback stack, which no clip names, so they are not among them. A part the edit left
 * as the same object is not looked into.
 */
function assertLocksKept(
  document: EditorDocument,
  next: EditorDocument,
  command: Command,
): void {
  const locked = document.project.timeline.tracks.filter(
    (t) => document.metadata.tracks[t.id]?.locked,
  );
  if (!locked.length) return;
  const before = document.project;
  const after = next.project;
  const assetsSame = next.assets === document.assets;
  const sourcesSame = assetsSame && after.sources === before.sources;
  const resourcesSame =
    assetsSame && after.effect_resources === before.effect_resources;
  const compositionsSame = after.compositions === before.compositions;
  const tracksAfter =
    after.timeline.tracks === before.timeline.tracks
      ? null
      : new Map(after.timeline.tracks.map((t) => [t.id, t]));
  const byId = <T extends { readonly id: string }>(items: readonly T[]) =>
    new Map(items.map((item) => [item.id, item]));
  let sources: [Map<string, MediaSource>, Map<string, MediaSource>] | null =
    null;
  let resources:
    [Map<string, EffectResource>, Map<string, EffectResource>] | null = null;
  const assetChanged = (id: string) => next.assets[id] !== document.assets[id];
  for (const track of locked) {
    const unlock =
      command.type === "track.metadata" &&
      command.id === track.id &&
      Object.keys(command.patch).length === 1 &&
      command.patch.locked === false;
    const metadataChanged =
      next.metadata.tracks[track.id] !== document.metadata.tracks[track.id] ||
      (next.metadata.clips !== document.metadata.clips &&
        (track.clips ?? []).some(
          (c) => next.metadata.clips[c.id] !== document.metadata.clips[c.id],
        ));
    const changed = () => {
      if (tracksAfter && tracksAfter.get(track.id) !== track) return true;
      if (
        !compositionsSame &&
        [...reachedCompositions(before, track.clips ?? [])].some(
          (id) => findComposition(after, id) !== findComposition(before, id),
        )
      )
        return true;
      if (!sourcesSame) {
        sources ??= [byId(before.sources), byId(after.sources)];
        const [was, is] = sources;
        if (
          [...shownSources(before, [track])].some(
            (id) => is.get(id) !== was.get(id) || assetChanged(id),
          )
        )
          return true;
      }
      if (!resourcesSame) {
        resources ??= [
          byId(before.effect_resources ?? []),
          byId(after.effect_resources ?? []),
        ];
        const [was, is] = resources;
        if (
          [...drawnEffectResources(before, [track])].some(
            (id) => is.get(id) !== was.get(id) || assetChanged(id),
          )
        )
          return true;
      }
      return false;
    };
    if ((metadataChanged && !unlock) || changed())
      throw new EditorError(
        "locked",
        "Track is locked",
        command.type,
        track.id,
      );
  }
}
/**
 * Applies `command` to `document` and validates the result. `gesture` checks only what a pointer
 * move can change (see `validateDocumentShape`); the gesture validates in full once, when it
 * commits.
 *
 * Never throws: a refusal is `{ ok: false }` with an `EditorError` (`invalid_command`, `locked`, a
 * validation code, or an edit code from `alpha2/edit`). `document` and `command` are not modified;
 * compositions the edit leaves undrawn are removed from the result.
 */
export function executeCommand(
  document: EditorDocument,
  command: Command,
  { gesture = false }: { gesture?: boolean } = {},
): Result<EditorDocument> {
  try {
    return withValidated(() => {
      const reduced = apply(document, structuredClone(command));
      // A clip owns the composition it shows: what the command stopped drawing goes.
      const undrawn = withoutUndrawn(document.project, reduced.project);
      const next = withInstanceIds(
        withoutGraphs(
          undrawn.removed.size
            ? { ...reduced, project: undrawn.project }
            : reduced,
          undrawn.removed,
        ),
      );
      if (gesture) {
        validateDocumentShape(next);
        assertProjection(next);
      } else validateDocument(next);
      return { ok: true, value: next } as const;
    });
  } catch (cause) {
    return {
      ok: false,
      error:
        cause instanceof EditorError
          ? cause
          : new EditorError(
              "invalid_command",
              String(cause),
              "dispatch",
              undefined,
              true,
              { cause },
            ),
    };
  }
}
