import type {
  Composition,
  CompositionNode,
  EffectResource,
  MediaSource,
  Project,
} from "alpha2/project";
import type { Command } from "./commands.js";
import { findComposition, freeCompositionId } from "./compositions.js";
import type { EditorDocument } from "./document.js";

/**
 * A reusable group: the composition a group node draws, the compositions it uses in turn, and what
 * it needs from the project it is applied to. Saved outside any project, as JSON.
 */
export interface GraphPreset {
  readonly format: "alpha2-graph-preset";
  readonly version: 1;
  readonly name: string;
  /** The group's composition first, then those its group nodes and composition sources use. */
  readonly compositions: readonly Composition[];
  /** Media, effect resources, and effects its nodes read. */
  readonly requires: {
    readonly sources: readonly MediaSource[];
    readonly resources: readonly EffectResource[];
    readonly effects: readonly {
      readonly id: string;
      readonly version: number;
    }[];
  };
}

/** What a preset needs that a project lacks. */
export interface PresetShortfall {
  /** Sources its media nodes read that the project has not got, by id. */
  readonly sources: readonly string[];
  /** Effect resources (LUTs) its effects use that the project has not got, by id. */
  readonly resources: readonly string[];
  /** Effects the engine does not run, as `id@version`. */
  readonly effects: readonly string[];
}

/**
 * The preset of composition `id`, as a group node would draw it: it, every composition it uses, and
 * the sources, resources, and effects they read.
 */
export function graphPreset(
  project: Project,
  id: string,
  name: string,
): GraphPreset {
  const compositions: Composition[] = [];
  const sources = new Map<string, MediaSource>();
  const resources = new Map<string, EffectResource>();
  const effects = new Map<string, { id: string; version: number }>();
  const pending = [id];
  while (pending.length) {
    const current = findComposition(project, pending.shift()!);
    if (!current || compositions.some((c) => c.id === current.id)) continue;
    compositions.push(current);
    for (const node of current.nodes ?? []) {
      const op = node.op;
      if (op.type === "group") pending.push(op.composition);
      if (op.type === "media") {
        const source = project.sources.find((s) => s.id === op.source_id);
        if (source?.composition) pending.push(source.composition);
        else if (source) sources.set(source.id, source);
      }
      if (op.type === "effect" || op.type === "transition") {
        effects.set(`${op.effect.id}@${op.effect.version}`, {
          id: op.effect.id,
          version: op.effect.version,
        });
        for (const resource of Object.values(op.effect.resources ?? {})) {
          const found = (project.effect_resources ?? []).find(
            (r) => r.id === resource,
          );
          if (found) resources.set(found.id, found);
        }
      }
    }
  }
  // Composition sources the preset's compositions read travel with it.
  for (const composition of compositions)
    for (const node of composition.nodes ?? [])
      if (node.op.type === "media") {
        const source = project.sources.find(
          (s) => s.id === (node.op as { source_id: string }).source_id,
        );
        if (source?.composition) sources.set(source.id, source);
      }
  return {
    format: "alpha2-graph-preset",
    version: 1,
    name,
    compositions,
    requires: {
      sources: [...sources.values()],
      resources: [...resources.values()],
      effects: [...effects.values()],
    },
  };
}

/**
 * What `preset` needs that `document`'s project lacks: file-backed sources and effect resources by
 * id (a composition source it brings along is not missing), and effects `runs` says the engine does
 * not have.
 */
export function presetShortfall(
  document: EditorDocument,
  preset: GraphPreset,
  runs: (id: string, version: number) => boolean,
): PresetShortfall {
  const project = document.project;
  return {
    sources: preset.requires.sources
      .filter(
        (source) =>
          source.kind !== "composition" &&
          !project.sources.some((s) => s.id === source.id),
      )
      .map((source) => source.id),
    resources: preset.requires.resources
      .filter(
        (resource) =>
          !(project.effect_resources ?? []).some((r) => r.id === resource.id),
      )
      .map((resource) => resource.id),
    effects: preset.requires.effects
      .filter((effect) => !runs(effect.id, effect.version))
      .map((effect) => `${effect.id}@${effect.version}`),
  };
}

/**
 * Commands that add `preset`'s compositions (and the composition sources they read) to `document`
 * under free ids, and the id its group composition got.
 */
function presetAdditions(
  document: EditorDocument,
  preset: GraphPreset,
): {
  readonly commands: Command[];
  readonly root: string;
  readonly taken: Project;
} {
  const project = document.project;
  let taken: Project = project;
  const renamed = new Map<string, string>();
  for (const composition of preset.compositions) {
    const id = freeCompositionId(taken, composition.id);
    renamed.set(composition.id, id);
    taken = {
      ...taken,
      compositions: [...(taken.compositions ?? []), { id, nodes: [] }],
    };
  }
  // An id no existing source, bin asset or earlier-added source has: two of the preset's may share
  // a base id (`card`, and `card~2` from an earlier rename).
  const sourceIds = new Map<string, string>();
  const takenSources = new Set([
    ...project.sources.map((s) => s.id),
    ...Object.keys(document.assets),
  ]);
  for (const source of preset.requires.sources) {
    if (source.kind !== "composition" || !source.composition) continue;
    let id = source.id;
    for (let n = 2; takenSources.has(id); n += 1) id = `${source.id}~${n}`;
    takenSources.add(id);
    sourceIds.set(source.id, id);
  }
  const rename = (node: CompositionNode): CompositionNode => {
    const op = node.op;
    if (op.type === "group")
      return {
        ...node,
        op: {
          ...op,
          composition: renamed.get(op.composition) ?? op.composition,
        },
      };
    if (op.type === "media" && sourceIds.has(op.source_id))
      return {
        ...node,
        op: { ...op, source_id: sourceIds.get(op.source_id)! },
      };
    return node;
  };
  const commands: Command[] = preset.compositions.map((composition) => ({
    type: "composition.add",
    composition: {
      ...structuredClone(composition),
      id: renamed.get(composition.id)!,
      nodes: (composition.nodes ?? []).map(rename),
    },
  }));
  for (const source of preset.requires.sources) {
    const id = sourceIds.get(source.id);
    if (!id) continue;
    commands.push({
      type: "asset.put",
      asset: { id, kind: "composition", name: preset.name, contentRevision: 0 },
      source: {
        ...source,
        id,
        composition: renamed.get(source.composition!) ?? source.composition!,
      },
    });
  }
  const root = renamed.get(preset.compositions[0].id)!;
  commands.push({
    type: "metadata.update",
    patch: {
      graphNames: {
        ...document.metadata.graphNames,
        [root]: preset.name,
      },
    },
  });
  return { commands, root, taken };
}

/**
 * The command that places `preset` in composition `compositionId` as one undoable edit: its
 * compositions added under free ids, and an unconnected group node `groupId` drawing the first, at
 * `position` in the node editor.
 * Returns `null` when the preset has no composition or `compositionId` does not exist. The command
 * may still be refused when executed.
 */
export function placeGraphPreset(
  document: EditorDocument,
  preset: GraphPreset,
  compositionId: string,
  groupId: string,
  position?: { readonly x: number; readonly y: number },
): Command | null {
  const root = preset.compositions[0];
  if (!root || !findComposition(document.project, compositionId)) return null;
  const added = presetAdditions(document, preset);
  return {
    type: "transaction",
    commands: [
      ...added.commands,
      {
        type: "graph.node.add",
        compositionId,
        node: {
          id: groupId,
          op: {
            type: "group",
            composition: added.root,
            inputs: (root.inputs ?? []).map(() => null),
          },
        },
        ...(position ? { position } : {}),
      },
    ],
  };
}

/**
 * The command that applies `preset` to clip `clipId` as one undoable edit: its compositions added
 * under free ids, and a group node drawing the first placed after the clip composition's output
 * (reading it, when the group has an input) and made the output. A clip without a composition gets
 * one. Returns `null` for a preset with more than one input, which has no single place to go, and
 * for an unknown clip.
 */
export function applyGraphPreset(
  document: EditorDocument,
  preset: GraphPreset,
  clipId: string,
  ids: { readonly groupId: string },
): Command | null {
  const root = preset.compositions[0];
  if (!root || (root.inputs ?? []).length > 1) return null;
  const project = document.project;
  const clip = project.timeline.tracks
    .flatMap((track) => track.clips ?? [])
    .find((c) => c.id === clipId);
  if (!clip) return null;
  const added = presetAdditions(document, preset);
  const commands = [...added.commands];
  const target =
    clip.composition?.id ?? freeCompositionId(added.taken, `${clipId}-fx`);
  if (!clip.composition)
    commands.push({ type: "graph.create", clipId, compositionId: target });
  const existing = clip.composition
    ? findComposition(project, clip.composition.id)
    : undefined;
  // A new composition's output is its clip node.
  const output = existing ? (existing.output ?? null) : "clip";
  commands.push({
    type: "graph.node.add",
    compositionId: target,
    node: {
      id: ids.groupId,
      op: {
        type: "group",
        composition: added.root,
        inputs: (root.inputs ?? []).length === 1 ? [output] : [],
      },
    },
  });
  commands.push({
    type: "graph.output",
    compositionId: target,
    nodeId: ids.groupId,
  });
  return { type: "transaction", commands };
}
