import type {
  Clip,
  ClipComposition,
  ClipEffect,
  Composition,
  CompositionNode,
  Project,
} from "alpha2/project";
import { MAX_COMPOSITION_DEPTH } from "alpha2/project";
import { copyComposition } from "alpha2/edit";
import { EditorError } from "./document.js";

/** One effect of a clip's effect stack: an effect node of its composition. */
export interface StackEntry {
  /** The effect node's id, stable across edits. */
  readonly id: string;
  readonly effect: ClipEffect;
  readonly bypass: boolean;
}

/**
 * A composition of `P`: in the forms the engine writes when `P` is a document's `CanonicalProject`,
 * as written when `P` is a `Project`. The helpers below return parts in the form their input has:
 * they rearrange what they are given, and what they add (ids, inputs, effect and group nodes) holds
 * no color or paint, so a canonical project stays canonical through them.
 */
export type CompositionOf<P extends Project> = NonNullable<
  P["compositions"]
>[number];
/** A clip of `P`, in the form `P` holds it (see {@link CompositionOf}). */
export type ClipOf<P extends Project> = NonNullable<
  P["timeline"]["tracks"][number]["clips"]
>[number];

/** Where each composition of a compositions array is, by id, for {@link findComposition}. */
const compositionIndexes = new WeakMap<
  readonly Composition[],
  Map<string, number>
>();

/**
 * The composition `id` names in `project`. Looked up in an index of the project's compositions
 * array, made once per array, so walks that find many compositions stay linear. An index a changed
 * array no longer matches is made again, and an id it lacks is looked for in the array itself.
 */
export function findComposition<P extends Project>(
  project: P,
  id: string,
): CompositionOf<P> | undefined {
  const compositions: readonly CompositionOf<P>[] = project.compositions ?? [];
  let index = compositionIndexes.get(compositions);
  if (!index) {
    index = new Map(compositions.map((c, i) => [c.id, i]));
    compositionIndexes.set(compositions, index);
  }
  const at = index.get(id);
  if (at !== undefined && compositions[at]?.id === id) return compositions[at];
  if (at !== undefined) compositionIndexes.delete(compositions);
  return compositions.find((c) => c.id === id);
}

/**
 * A clip's effects as a stack: its composition when that is the clip's picture through a chain of
 * effect nodes and nothing else, bottom first. A clip without a composition has an empty stack.
 * `undefined` means the composition is a graph a stack cannot show: it branches, merges, reads
 * other sources, or has loose nodes.
 */
export function effectStack(
  project: Project,
  clip: Clip,
): readonly StackEntry[] | undefined {
  if (clip.composition == null) return [];
  const composition = findComposition(project, clip.composition.id);
  if (!composition) return undefined;
  const nodes = new Map((composition.nodes ?? []).map((n) => [n.id, n]));
  const stack: StackEntry[] = [];
  let at = composition.output ?? undefined;
  while (at !== undefined) {
    const node = nodes.get(at);
    if (!node) return undefined;
    if (node.op.type === "clip") {
      return stack.length + 1 === nodes.size ? stack.reverse() : undefined;
    }
    // An effect reading named inputs reads another branch, which a stack cannot show.
    if (
      node.op.type !== "effect" ||
      node.op.input == null ||
      Object.keys(node.op.inputs ?? {}).length > 0
    )
      return undefined;
    stack.push({ id: node.id, effect: node.op.effect, bypass: !!node.bypass });
    at = node.op.input;
  }
  return undefined;
}

/** An id based on `base` that no composition in `project` has. */
export function freeCompositionId(project: Project, base: string): string {
  const taken = new Set((project.compositions ?? []).map((c) => c.id));
  let id = base;
  for (let n = 2; taken.has(id); n += 1) id = `${base}~${n}`;
  return id;
}

/**
 * `project` with clip `clipId` showing its picture through `stack`, bottom first: its composition
 * rewritten as that chain, or removed with the reference when the stack is empty. The composition
 * keeps its id and the clip its time mapping; a clip without one gets `<clip id>-fx`.
 * Throws when `clipId` names no clip.
 */
export function withEffectStack<P extends Project>(
  project: P,
  clipId: string,
  stack: readonly StackEntry[],
): P {
  const clip = project.timeline.tracks
    .flatMap((t) => t.clips ?? [])
    .find((c) => c.id === clipId);
  if (!clip) throw new Error(`Unknown clip ${clipId}`);
  // The composition no clip shows any more goes when the command ends (`executeCommand`).
  if (stack.length === 0) {
    if (clip.composition == null) return project;
    return setClipComposition(project, clipId, undefined) as P;
  }
  const reference: ClipComposition = clip.composition ?? {
    id: freeCompositionId(project, `${clipId}-fx`),
  };
  const previous = findComposition(project, reference.id);
  const taken = new Set(stack.map((entry) => entry.id));
  const kept = previous?.nodes?.find((n) => n.op.type === "clip")?.id;
  let pictureId = kept && !taken.has(kept) ? kept : "clip";
  for (let n = 2; taken.has(pictureId); n += 1) pictureId = `clip~${n}`;
  const nodes: CompositionNode[] = [{ id: pictureId, op: { type: "clip" } }];
  let input = pictureId;
  for (const entry of stack) {
    nodes.push({
      id: entry.id,
      ...(entry.bypass ? { bypass: true } : {}),
      op: { type: "effect", input, effect: entry.effect },
    });
    input = entry.id;
  }
  const composition: Composition = { id: reference.id, nodes, output: input };
  const compositions = project.compositions ?? [];
  return setClipComposition(
    {
      ...project,
      compositions: previous
        ? compositions.map((c) => (c.id === reference.id ? composition : c))
        : [...compositions, composition],
    },
    clipId,
    reference,
  ) as P;
}

function setClipComposition(
  project: Project,
  clipId: string,
  reference: ClipComposition | undefined,
): Project {
  return {
    ...project,
    timeline: {
      ...project.timeline,
      tracks: project.timeline.tracks.map((track) =>
        (track.clips ?? []).some((c) => c.id === clipId)
          ? {
              ...track,
              clips: (track.clips ?? []).map((c) => {
                if (c.id !== clipId) return c;
                if (reference) return { ...c, composition: reference };
                const { composition: _removed, ...rest } = c;
                return rest;
              }),
            }
          : track,
      ),
    },
  };
}

/**
 * The compositions `id` draws: itself and, through group nodes, the group definitions it uses, each
 * once.
 */
export function compositionTree<P extends Project>(
  project: P,
  id: string,
): CompositionOf<P>[] {
  const tree: CompositionOf<P>[] = [];
  const seen = new Set<string>();
  const pending = [id];
  while (pending.length) {
    const next = pending.pop()!;
    if (seen.has(next)) continue;
    seen.add(next);
    const composition = findComposition(project, next);
    if (!composition) continue;
    tree.push(composition);
    for (const node of composition.nodes ?? [])
      if (node.op.type === "group") pending.push(node.op.composition);
  }
  return tree;
}

/**
 * Ids of the compositions `project` draws: those clips show and composition sources show (Library
 * items), and those these use through group nodes and composition sources, as far as they go.
 */
export function drawnCompositions(project: Project): Set<string> {
  const sources = new Map(project.sources.map((s) => [s.id, s]));
  const pending: string[] = project.sources.flatMap((s) =>
    s.composition ? [s.composition] : [],
  );
  for (const track of project.timeline.tracks)
    for (const clip of track.clips ?? [])
      if (clip.composition != null) pending.push(clip.composition.id);
  const drawn = new Set<string>();
  while (pending.length) {
    const id = pending.pop()!;
    if (drawn.has(id)) continue;
    drawn.add(id);
    for (const node of findComposition(project, id)?.nodes ?? []) {
      if (node.op.type === "group") pending.push(node.op.composition);
      if (node.op.type === "media") {
        const shown = sources.get(node.op.source_id)?.composition;
        if (shown) pending.push(shown);
      }
    }
  }
  return drawn;
}

/**
 * `after` without the compositions `before` drew and `after` no longer does: a clip owns the
 * composition it shows, so the edit that stops the last drawing of one (deleting its clip or track,
 * removing its Library source or the group node that drew it, emptying an effect stack) removes it,
 * and the group definitions only it drew with it. A composition `before` did not draw stays, so one
 * added for a later edit (a macro's definition) waits for it. Returns the ids it removed.
 */
export function withoutUndrawn<P extends Project>(
  before: Project,
  after: P,
): { readonly project: P; readonly removed: ReadonlySet<string> } {
  const compositions = after.compositions ?? [];
  if (!compositions.length) return { project: after, removed: new Set() };
  const was = drawnCompositions(before);
  const is = drawnCompositions(after);
  const removed = new Set(
    compositions.filter((c) => was.has(c.id) && !is.has(c.id)).map((c) => c.id),
  );
  if (!removed.size) return { project: after, removed };
  const kept = compositions.filter((c) => !removed.has(c.id));
  if (kept.length)
    return { project: { ...after, compositions: kept }, removed };
  const { compositions: _removed, ...rest } = after;
  return { project: rest as P, removed };
}

/**
 * `project` with `clip` given compositions no other clip draws, for a clip a command puts down (a
 * paste, a duplicate). With `compositions`, those the clip drew when it was copied, it shows a copy
 * of them whatever the project has now. Otherwise it shows a copy of its composition when anything
 * else draws that composition or a group definition in it, and the composition itself when nothing
 * does. A copy takes free ids based on `<root>~<clip id>` ({@link copyComposition}), so the clip is
 * edited independently of its original. Returns the copies' ids by their originals'.
 */
export function ownComposition<P extends Project>(
  project: P,
  clip: ClipOf<P>,
  compositions?: readonly CompositionOf<P>[],
): {
  readonly project: P;
  readonly clip: ClipOf<P>;
  readonly copies: ReadonlyMap<string, string>;
} {
  const reference = clip.composition;
  const none = { project, clip, copies: new Map<string, string>() };
  if (reference == null) return none;
  if (!compositions) {
    const tree = compositionTree(project, reference.id);
    if (!tree.length) return none;
    const elsewhere = drawnCompositions(project);
    if (!tree.some((c) => elsewhere.has(c.id))) return none;
  } else if (!compositions.some((c) => c.id === reference.id)) return none;
  const copy = copyComposition(project, reference.id, clip.id, compositions);
  return {
    project: copy.project as P,
    clip: { ...clip, composition: { ...reference, id: copy.id } },
    copies: copy.copies,
  };
}

/**
 * The compositions `copy` draws for those `original` draws, by id: the two themselves, and the
 * definitions of their group nodes of the same id, as far as they go. A composition copied for a
 * split or a paste corresponds to its original this way.
 */
export function correspondingCompositions(
  project: Project,
  original: string,
  copy: string,
): Map<string, string> {
  const pairs = new Map<string, string>();
  const pending: [string, string][] = [[original, copy]];
  while (pending.length) {
    const [from, to] = pending.pop()!;
    if (pairs.has(from)) continue;
    pairs.set(from, to);
    const copied = new Map(
      (findComposition(project, to)?.nodes ?? []).map((n) => [n.id, n]),
    );
    for (const node of findComposition(project, from)?.nodes ?? []) {
      const other = copied.get(node.id);
      if (node.op.type === "group" && other?.op.type === "group")
        pending.push([node.op.composition, other.op.composition]);
    }
  }
  return pairs;
}

/**
 * Composition time at timeline time `tick` for `clip`: `offset + (tick - start) * scale`, rounded
 * down, as the engine maps it. Without a composition, timeline time. Time is whole ticks; a
 * fraction (a curve sampled between ticks) reads as the tick it falls in.
 */
export function compositionTime(clip: Clip, tick: number): number {
  tick = Math.floor(finiteTime(tick));
  const reference = clip.composition;
  if (reference == null) return tick;
  const { num, den } = reference.scale ?? { num: 1, den: 1 };
  return (
    (reference.offset ?? 0) +
    divide(tick - clip.timeline_range.start, num, den, "floor")
  );
}

/**
 * The first timeline time at which `clip` shows composition time `time`: the inverse of
 * {@link compositionTime}, rounded up to a tick. A fractional `time` is first rounded up to the
 * next whole tick.
 */
export function timelineTime(clip: Clip, time: number): number {
  time = Math.ceil(finiteTime(time));
  const reference = clip.composition;
  if (reference == null) return time;
  const { num, den } = reference.scale ?? { num: 1, den: 1 };
  return (
    clip.timeline_range.start +
    divide(time - (reference.offset ?? 0), den, num, "ceil")
  );
}

function finiteTime(time: number): number {
  if (!Number.isFinite(time))
    throw new EditorError("invalid_time", `Time ${time} is not finite`, "time");
  return time;
}

/** `value * num / den` rounded as asked, exact at any size. */
function divide(
  value: number,
  num: number,
  den: number,
  rounding: "floor" | "ceil",
): number {
  const product = BigInt(value) * BigInt(num);
  const divisor = BigInt(den);
  const quotient = product / divisor;
  const remainder = product % divisor;
  if (remainder === 0n) return Number(quotient);
  // BigInt division truncates toward zero.
  if (rounding === "floor")
    return Number(product < 0n ? quotient - 1n : quotient);
  return Number(product > 0n ? quotient + 1n : quotient);
}

/** The node ids an operation takes its inputs from. */
function inputsOf(op: CompositionNode["op"]): string[] {
  switch (op.type) {
    case "effect":
      return [
        op.input,
        ...effectInputNames(op).map((name) => op.inputs![name]),
      ].filter((id): id is string => id != null);
    case "transform":
    case "shape_mask":
      return op.input != null ? [op.input] : [];
    case "transition":
      return [op.from, op.to].filter((id): id is string => id != null);
    case "matte":
      return [op.input, op.matte].filter((id): id is string => id != null);
    case "merge":
      return (op.layers ?? [])
        .map((layer) => layer.input)
        .filter((id): id is string => id != null);
    case "group":
      return (op.inputs ?? []).filter((id): id is string => id != null);
    default:
      return [];
  }
}

/**
 * Ids of the sources `tracks`' clips draw: each clip's own, and those the media nodes its
 * compositions' outputs need read (the inputs {@link CompositionReads.neededInputs} names: of a
 * group node only the ports its composition reads), through group nodes and composition sources
 * too. A clip's composition source counts only where the clip's own composition draws the clip. The
 * engine's `Project::used_sources` is the same set; `tests/cases/used-sources-corpus.json` holds both to
 * it.
 */
export function shownSources(
  project: Project,
  tracks: Project["timeline"]["tracks"] = project.timeline.tracks,
): Set<string> {
  return drawn(project, tracks).sources;
}

/**
 * Ids of the effect resources (`effect_resources`) what `tracks` draw reads: those the effect and
 * transition nodes drawing {@link shownSources} evaluates name (not a bypassed node's), and those
 * the tracks' transitions name. Only these must be registered for a render: the engine asks for a
 * resource when a node it evaluates needs one.
 */
export function drawnEffectResources(
  project: Project,
  tracks: Project["timeline"]["tracks"] = project.timeline.tracks,
): Set<string> {
  return drawn(project, tracks).resources;
}

/** What {@link shownSources} and {@link drawnEffectResources} walk once. */
function drawn(
  project: Project,
  tracks: Project["timeline"]["tracks"],
): { sources: Set<string>; resources: Set<string> } {
  const used = new Set<string>();
  const resources = new Set<string>();
  const named = (
    effect: { resources?: Record<string, string> } | undefined,
  ) => {
    for (const id of Object.values(effect?.resources ?? {})) resources.add(id);
  };
  const walked = new Set<string>();
  const sources = new Map(project.sources.map((s) => [s.id, s]));
  const reads = compositionReads(project.compositions ?? []);
  const walk = (id: string | null | undefined) => {
    if (id == null || walked.has(id)) return;
    walked.add(id);
    const composition = findComposition(project, id);
    const nodes = new Map((composition?.nodes ?? []).map((n) => [n.id, n]));
    const pending = composition?.output != null ? [composition.output] : [];
    const seen = new Set<string>();
    while (pending.length) {
      const node = nodes.get(pending.pop()!);
      if (!node || seen.has(node.id)) continue;
      seen.add(node.id);
      if (node.op.type === "media") {
        used.add(node.op.source_id);
        walk(sources.get(node.op.source_id)?.composition);
      }
      if (node.op.type === "group") walk(node.op.composition);
      if (
        (node.op.type === "effect" || node.op.type === "transition") &&
        !node.bypass
      )
        named(node.op.effect);
      for (const key of reads.neededInputs(node)) {
        const input = inputOf(node.op, key);
        if (input !== undefined) pending.push(input);
      }
    }
  };
  for (const track of tracks) {
    for (const transition of track.transitions ?? []) named(transition.effect);
    for (const clip of track.clips ?? []) {
      used.add(clip.source_id);
      // A composition source is the clip's picture, drawn only where the clip's own composition
      // draws the clip.
      if (clip.composition == null || reads.drawsClip(clip.composition.id))
        walk(sources.get(clip.source_id)?.composition);
      walk(clip.composition?.id);
    }
  }
  return { sources: used, resources };
}

/**
 * A node input as graph commands name it: `input`, `from`, `to`, `matte`, an effect's named input
 * as `inputs.<name>`, or a merge layer's or group port's index.
 */
export type InputKey =
  "input" | "from" | "to" | "matte" | `inputs.${string}` | number;

/** A name an effect node may give a named input, as the model requires. */
const EFFECT_INPUT_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** The name of the effect input `key` names, or `undefined` for any other key. */
function effectInputName(key: InputKey): string | undefined {
  return typeof key === "string" && key.startsWith("inputs.")
    ? key.slice("inputs.".length)
    : undefined;
}

/** An effect node's named inputs, in name order, as the model keeps them. */
function effectInputNames(
  op: Extract<CompositionNode["op"], { type: "effect" }>,
): string[] {
  return Object.keys(op.inputs ?? {}).sort();
}

/** The input keys of a node's operation, in order. */
export function inputKeys(op: CompositionNode["op"]): InputKey[] {
  switch (op.type) {
    case "effect":
      return [
        "input",
        ...effectInputNames(op).map((name) => `inputs.${name}` as const),
      ];
    case "transform":
    case "shape_mask":
      return ["input"];
    case "transition":
      return ["from", "to"];
    case "matte":
      return ["input", "matte"];
    case "merge":
      return (op.layers ?? []).map((_, index) => index);
    case "group":
      return (op.inputs ?? []).map((_, index) => index);
    default:
      return [];
  }
}

/**
 * The inputs drawing `node` reads: only its pass-through input (the first) when it is bypassed, and
 * every input otherwise.
 */
export function neededInputs(node: CompositionNode): InputKey[] {
  const keys = inputKeys(node.op);
  return node.bypass ? keys.slice(0, 1) : keys;
}

/** What each composition's output reads, as alpha2-model's `CompositionReads`. */
export interface CompositionReads {
  /**
   * Whether composition `id`'s output reads its group input `port`; true for a composition it does
   * not know.
   */
  readsPort(id: string, port: number): boolean;
  /**
   * Whether composition `id`, shown by a clip, draws the clip's picture: its output reaches a clip
   * node, directly or in a group. True for a composition it does not know.
   */
  drawsClip(id: string): boolean;
  /**
   * The inputs drawing `node` reads: its {@link neededInputs}, of a group node only the ports its
   * composition reads.
   */
  neededInputs(node: CompositionNode): InputKey[];
}

/**
 * What each of `compositions` reads: the group inputs and clip nodes a walk from its output
 * reaches, along the inputs drawing each node reads, into the compositions its group nodes draw. A
 * group draws what its nodes would inline, so a group node needs only the ports its composition
 * reads: a port it never reads may be left unconnected, and what a connected one reads is not
 * drawn. A composition that uses itself (which validation refuses) reads every port and draws the
 * clip.
 */
export function compositionReads(
  compositions: readonly Composition[],
): CompositionReads {
  const byId = new Map(compositions.map((c) => [c.id, c]));
  const known = new Map<string, { ports: boolean[]; clip: boolean }>();
  const reads: CompositionReads = {
    readsPort: (id, port) => {
      const read = known.get(id);
      return read === undefined || (read.ports[port] ?? false);
    },
    drawsClip: (id) => known.get(id)?.clip ?? true,
    neededInputs: (node) =>
      !node.bypass && node.op.type === "group"
        ? neededInputs(node).filter(
            (key) =>
              typeof key !== "number" ||
              reads.readsPort(
                (node.op as { composition: string }).composition,
                key,
              ),
          )
        : neededInputs(node),
  };
  const walk = (composition: Composition) => {
    const nodes = new Map((composition.nodes ?? []).map((n) => [n.id, n]));
    const read = {
      ports: (composition.inputs ?? []).map(() => false),
      clip: false,
    };
    const reached = new Set<string>();
    const pending = composition.output != null ? [composition.output] : [];
    while (pending.length) {
      const node = nodes.get(pending.pop()!);
      if (!node || reached.has(node.id)) continue;
      reached.add(node.id);
      if (!node.bypass) {
        if (node.op.type === "group_input" && node.op.index < read.ports.length)
          read.ports[node.op.index] = true;
        if (node.op.type === "clip") read.clip = true;
        if (node.op.type === "group" && reads.drawsClip(node.op.composition))
          read.clip = true;
      }
      for (const key of reads.neededInputs(node)) {
        const input = inputOf(node.op, key);
        if (input !== undefined) pending.push(input);
      }
    }
    return read;
  };
  // In dependency order, without recursion: a composition is walked once the ones its group nodes
  // draw are known.
  const walking = new Set<string>();
  for (const root of compositions) {
    const stack: [Composition, boolean][] = [[root, false]];
    while (stack.length) {
      const [composition, expanded] = stack.pop()!;
      if (known.has(composition.id)) continue;
      if (!expanded) {
        if (walking.has(composition.id)) continue;
        walking.add(composition.id);
        stack.push([composition, true]);
        for (const node of composition.nodes ?? []) {
          const inner =
            node.op.type === "group"
              ? byId.get(node.op.composition)
              : undefined;
          if (inner && !known.has(inner.id) && !walking.has(inner.id))
            stack.push([inner, false]);
        }
        continue;
      }
      walking.delete(composition.id);
      known.set(composition.id, walk(composition));
    }
  }
  return reads;
}

/** The node an input reads, or `undefined` when it is unconnected or absent. */
export function inputOf(
  op: CompositionNode["op"],
  key: InputKey,
): string | undefined {
  if (typeof key === "number")
    return op.type === "merge"
      ? (op.layers?.[key]?.input ?? undefined)
      : op.type === "group"
        ? (op.inputs?.[key] ?? undefined)
        : undefined;
  const name = effectInputName(key);
  if (name !== undefined)
    return op.type === "effect"
      ? Object.hasOwn(op.inputs ?? {}, name)
        ? (op.inputs![name] ?? undefined)
        : undefined
      : undefined;
  const value = (op as Record<string, unknown>)[key];
  return typeof value === "string" ? value : undefined;
}

/**
 * Whether `op` has input `key`, or can take it: an effect node takes any well-named
 * `inputs.<name>`, whether or not its definition declares it.
 */
export function acceptsInput(
  op: CompositionNode["op"],
  key: InputKey,
): boolean {
  const name = effectInputName(key);
  if (name !== undefined)
    return op.type === "effect" && EFFECT_INPUT_NAME.test(name);
  return inputKeys(op).includes(key);
}

/**
 * `op` with input `key` reading `node` (`null` disconnects it, keeping an effect's named input).
 * An effect's named input is added when it has none of the name. Throws when `op` cannot take
 * input `key`.
 */
export function withInput<O extends CompositionNode["op"]>(
  op: O,
  key: InputKey,
  node: string | null,
): O {
  return opWithInput(op, key, node) as O;
}

function opWithInput(
  op: CompositionNode["op"],
  key: InputKey,
  node: string | null,
): CompositionNode["op"] {
  if (!acceptsInput(op, key))
    throw new Error(`A ${op.type} node has no input ${String(key)}`);
  const name = effectInputName(key);
  if (name !== undefined && op.type === "effect")
    return { ...op, inputs: { ...(op.inputs ?? {}), [name]: node } };
  if (typeof key === "number" && op.type === "merge")
    return {
      ...op,
      layers: (op.layers ?? []).map((layer, index) =>
        index === key ? { ...layer, input: node } : layer,
      ),
    };
  if (typeof key === "number" && op.type === "group")
    return {
      ...op,
      inputs: (op.inputs ?? []).map((input, index) =>
        index === key ? node : input,
      ),
    };
  return { ...op, [key]: node };
}

/**
 * `op` without its effect input `name`; an effect left with none has no `inputs`, as the model
 * writes it. Any other operation is returned as it is.
 */
export function withoutEffectInput<O extends CompositionNode["op"]>(
  op: O,
  name: string,
): O {
  if (op.type !== "effect" || !Object.hasOwn(op.inputs ?? {}, name)) return op;
  const { [name]: _removed, ...rest } = op.inputs!;
  const { inputs: _inputs, ...without } = op;
  return (
    Object.keys(rest).length > 0 ? { ...without, inputs: rest } : without
  ) as O;
}

/**
 * How a composition is drawn, which decides what its clip nodes and group inputs read, as the
 * engine plans it.
 *
 * - `clip`: a clip's own composition. Its clip nodes draw the clip; no group node feeds its group
 *   inputs.
 * - `source`: a composition source, drawn by a clip or a media node. Nothing feeds its clip nodes
 *   or its group inputs.
 * - `group`: a group's definition, opened on its own. Its group inputs read the group node's
 *   inputs, and whether its clip nodes draw anything depends on where the group is drawn, so
 *   neither is an issue here.
 */
export type GraphUse = "clip" | "source" | "group";

/**
 * Why a composition's output cannot be drawn: the engine's four reasons for refusing it as an
 * incomplete composition (`INCOMPLETE_COMPOSITION`). An issue inside a composition that a group or
 * media node draws names its node as the engine does, prefixed by the drawing node's id and `/`
 * (`group/blur`); a `no_output` there names the drawing node.
 */
export type GraphIssue =
  | { readonly kind: "no_output"; readonly node?: string }
  | {
      readonly kind: "missing_input";
      readonly node: string;
      readonly input: InputKey;
    }
  /** A group input no group node feeds: in a clip's composition or a composition source. */
  | { readonly kind: "unbound_group_input"; readonly node: string }
  /** A clip node in a composition source, which shows no clip. */
  | { readonly kind: "clip_without_clip"; readonly node: string };

/**
 * What {@link graphIssues} reads to follow group and media nodes into the compositions they draw.
 */
export interface GraphProject {
  readonly compositions?: readonly Composition[] | null;
  readonly sources: readonly {
    readonly id: string;
    readonly composition?: string | null;
  }[];
}

/**
 * What stops `composition`'s output from rendering when it is drawn as `use`: no output, an
 * unconnected input a node the output reaches uses (only the pass-through input of a bypassed node,
 * and of a group node only the ports its composition reads), a group input nothing feeds, or a clip
 * node where there is no clip. The engine refuses the same frames with `INCOMPLETE_COMPOSITION`;
 * this lets an editor mark the nodes before it asks.
 *
 * With `project`, the issues of the compositions the output's group and media nodes draw are
 * included, as the engine plans them inline; without it those nodes are taken as drawn.
 */
export function graphIssues(
  composition: Composition,
  use: GraphUse,
  project?: GraphProject,
): GraphIssue[] {
  const issues: GraphIssue[] = [];
  const compositions = new Map(
    (project?.compositions ?? []).map((c) => [c.id, c]),
  );
  // Without the project every group port counts as read.
  const reads = compositionReads(project?.compositions ?? []);
  const sourceCompositions = new Map(
    (project?.sources ?? []).flatMap((s) =>
      s.composition != null ? [[s.id, s.composition] as const] : [],
    ),
  );
  const visit = (
    drawn: Composition,
    scope: {
      readonly prefix: string;
      /**
       * How many group inputs the group node drawing it feeds; `Infinity` when unknown, `0` when
       * none does.
       */
      readonly bound: number;
      readonly clipAllowed: boolean;
      /** The compositions being drawn around it, outermost first. */
      readonly within: readonly string[];
    },
  ) => {
    const named = (id: string) => scope.prefix + id;
    if (drawn.output == null) {
      issues.push(
        scope.prefix
          ? { kind: "no_output", node: scope.prefix.slice(0, -1) }
          : { kind: "no_output" },
      );
      return;
    }
    const nodes = new Map((drawn.nodes ?? []).map((n) => [n.id, n]));
    const seen = new Set<string>();
    const pending = [drawn.output];
    while (pending.length) {
      const node = nodes.get(pending.pop()!);
      if (!node || seen.has(node.id)) continue;
      seen.add(node.id);
      const needed = reads.neededInputs(node);
      if (node.bypass && needed.length === 0 && node.op.type === "merge")
        issues.push({ kind: "missing_input", node: named(node.id), input: 0 });
      for (const key of needed) {
        const input = inputOf(node.op, key);
        if (input === undefined)
          issues.push({
            kind: "missing_input",
            node: named(node.id),
            input: key,
          });
        else pending.push(input);
      }
      if (node.bypass) continue;
      const within = [...scope.within, drawn.id];
      // Validation bounds the nesting and forbids cycles; an unvalidated draft stops here rather
      // than recursing forever.
      const nest = (id: string | undefined) => {
        const inner = id === undefined ? undefined : compositions.get(id);
        return inner &&
          within.length < MAX_COMPOSITION_DEPTH &&
          !within.includes(inner.id)
          ? inner
          : undefined;
      };
      switch (node.op.type) {
        case "clip":
          if (!scope.clipAllowed)
            issues.push({ kind: "clip_without_clip", node: named(node.id) });
          break;
        case "group_input":
          if (!(node.op.index < scope.bound))
            issues.push({ kind: "unbound_group_input", node: named(node.id) });
          break;
        case "group": {
          const inner = nest(node.op.composition);
          if (inner)
            visit(inner, {
              prefix: `${named(node.id)}/`,
              bound: (node.op.inputs ?? []).length,
              clipAllowed: scope.clipAllowed,
              within,
            });
          break;
        }
        case "media": {
          const inner = nest(sourceCompositions.get(node.op.source_id));
          if (inner)
            visit(inner, {
              prefix: `${named(node.id)}/`,
              bound: 0,
              clipAllowed: false,
              within,
            });
          break;
        }
        default:
          break;
      }
    }
  };
  visit(composition, {
    prefix: "",
    bound: use === "group" ? Infinity : 0,
    clipAllowed: use !== "source",
    within: [],
  });
  return issues;
}

/** An id based on `base` that no node of `composition` has. */
export function freeNodeId(composition: Composition, base: string): string {
  const taken = new Set((composition.nodes ?? []).map((n) => n.id));
  let id = base;
  for (let n = 2; taken.has(id); n += 1) id = `${base}-${n}`;
  return id;
}

/**
 * `composition` without the nodes `ids`, every input that read one of them disconnected, no output
 * if it was one of them, and no control that exposed one of them.
 */
export function withoutNodes<C extends Composition>(
  composition: C,
  ids: readonly string[],
): C {
  const drop = new Set(ids);
  const nodes = (composition.nodes ?? [])
    .filter((node) => !drop.has(node.id))
    .map((node) => {
      let op = node.op;
      for (const key of inputKeys(op)) {
        const input = inputOf(op, key);
        if (input !== undefined && drop.has(input))
          op = withInput(op, key, null);
      }
      return op === node.op ? node : { ...node, op };
    });
  const output =
    composition.output != null && drop.has(composition.output)
      ? null
      : (composition.output ?? null);
  const controls = composition.controls?.filter((c) => !drop.has(c.node));
  return {
    ...composition,
    nodes,
    output,
    ...(controls && controls.length !== composition.controls?.length
      ? { controls }
      : {}),
  };
}

/**
 * `composition` with nodes `ids` folded into a group node `groupId` that draws a new composition
 * `definitionId` of them: each node outside the selection that a selected node reads becomes one of
 * its inputs, in the order first read, and the one selected node read from outside (or the output)
 * becomes its output. A selected group input stays in `composition`: it reads `composition`'s own
 * input, which a node inside the group reads through a port like any node outside. The picture does
 * not change.
 *
 * @throws When the selection is empty or only group inputs, names an absent node, `groupId` names
 * an unselected node, or other than one selected node's picture leaves the selection.
 */
export function groupNodes<C extends Composition>(
  composition: C,
  ids: readonly string[],
  groupId: string,
  definitionId: string,
): { readonly parent: C; readonly definition: C } {
  const nodes = composition.nodes ?? [];
  if (ids.length === 0) throw new Error("Choose the nodes to group");
  for (const id of ids)
    if (!nodes.some((node) => node.id === id))
      throw new Error(`Unknown node ${id}`);
  const selected = new Set(
    ids.filter(
      (id) => nodes.find((node) => node.id === id)?.op.type !== "group_input",
    ),
  );
  if (selected.size === 0)
    throw new Error(
      "A group input stays where it is: choose the nodes that read it",
    );
  if (nodes.some((node) => node.id === groupId && !selected.has(groupId)))
    throw new Error(`A node is already called ${groupId}`);
  const leaving = new Set<string>();
  for (const node of nodes)
    if (!selected.has(node.id))
      for (const input of inputsOf(node.op))
        if (selected.has(input)) leaving.add(input);
  if (composition.output != null && selected.has(composition.output))
    leaving.add(composition.output);
  if (leaving.size !== 1)
    throw new Error(
      leaving.size === 0
        ? "Nothing outside the group reads its picture"
        : "A group has one output: only one of its nodes may be read from outside it",
    );
  const [output] = [...leaving];
  // A control exposes a node of this composition to the group nodes drawing it; a node in a nested
  // group cannot be one.
  const exposed = composition.controls?.find((c) => selected.has(c.node));
  if (exposed)
    throw new Error(
      `Node ${exposed.node} is exposed as control ${exposed.name}: remove the control before grouping it`,
    );
  const ports: string[] = [];
  for (const node of nodes)
    if (selected.has(node.id))
      for (const input of inputsOf(node.op))
        if (!selected.has(input) && !ports.includes(input)) ports.push(input);
  const taken = new Set(selected);
  const portNode = ports.map((_, index) => {
    let id = `input-${index + 1}`;
    for (let n = 2; taken.has(id); n += 1) id = `input-${index + 1}~${n}`;
    taken.add(id);
    return id;
  });
  const inside = (op: CompositionNode["op"]) => {
    let next = op;
    for (const key of inputKeys(op)) {
      const input = inputOf(op, key);
      const port = input === undefined ? -1 : ports.indexOf(input);
      if (port >= 0) next = withInput(next, key, portNode[port]);
    }
    return next;
  };
  const definition: Composition = {
    id: definitionId,
    nodes: [
      ...portNode.map((id, index): CompositionNode => ({
        id,
        op: { type: "group_input", index },
      })),
      ...nodes
        .filter((node) => selected.has(node.id))
        .map((node) => ({ ...node, op: inside(node.op) })),
    ],
    output,
    ...(ports.length ? { inputs: ports.map((_, i) => `Input ${i + 1}`) } : {}),
  };
  const outside = (op: CompositionNode["op"]) => {
    let next = op;
    for (const key of inputKeys(op))
      if (inputOf(op, key) === output) next = withInput(next, key, groupId);
    return next;
  };
  const parent: Composition = {
    ...composition,
    nodes: [
      ...nodes
        .filter((node) => !selected.has(node.id))
        .map((node) => ({ ...node, op: outside(node.op) })),
      {
        id: groupId,
        op: { type: "group", composition: definitionId, inputs: ports },
      },
    ],
    output:
      composition.output === output ? groupId : (composition.output ?? null),
  };
  return { parent: parent as C, definition: definition as C };
}

/**
 * `composition` with group node `groupId` replaced by the nodes of the composition it draws, under
 * ids based on `<group>-<node>`: its group inputs read what the group node's inputs read, its
 * controls keep the values the group node set, and what read the group reads its output. Returns
 * the new ids by inner id. The picture does not change.
 * Throws when `groupId` is not a group node of `composition`.
 */
export function ungroupNode<C extends Composition>(
  composition: C,
  groupId: string,
  definition: C,
): { readonly parent: C; readonly ids: ReadonlyMap<string, string> } {
  const nodes = composition.nodes ?? [];
  const group = nodes.find((node) => node.id === groupId);
  if (!group || group.op.type !== "group")
    throw new Error(`${groupId} is not a group node`);
  const op = group.op;
  const taken = new Set(nodes.map((node) => node.id));
  const ids = new Map<string, string>();
  // Group inputs are not inlined: they stand for what the group node reads.
  const bound = new Map<string, string | null>();
  for (const node of definition.nodes ?? []) {
    if (node.op.type === "group_input") {
      bound.set(node.id, op.inputs?.[node.op.index] ?? null);
      continue;
    }
    let id = `${groupId}-${node.id}`;
    for (let n = 2; taken.has(id); n += 1) id = `${groupId}-${node.id}~${n}`;
    taken.add(id);
    ids.set(node.id, id);
  }
  const resolve = (inner: string | undefined): string | null =>
    inner === undefined
      ? null
      : bound.has(inner)
        ? (bound.get(inner) ?? null)
        : (ids.get(inner) ?? null);
  const values = op.controls ?? {};
  const inlined = (definition.nodes ?? [])
    .filter((node) => ids.has(node.id))
    .map((node): CompositionNode => {
      let next = node.op;
      for (const key of inputKeys(next))
        next = withInput(next, key, resolve(inputOf(node.op, key)));
      for (const control of definition.controls ?? []) {
        const value = values[control.name];
        if (control.node !== node.id || value === undefined) continue;
        if (next.type === "effect" || next.type === "transition")
          next = {
            ...next,
            effect: {
              ...next.effect,
              params: { ...next.effect.params, [control.param]: value },
            },
          };
      }
      return { ...node, id: ids.get(node.id)!, op: next };
    });
  const output = resolve(definition.output ?? undefined);
  const reroute = (inner: CompositionNode["op"]) => {
    let next = inner;
    for (const key of inputKeys(inner))
      if (inputOf(inner, key) === groupId) next = withInput(next, key, output);
    return next;
  };
  return {
    parent: {
      ...composition,
      nodes: [
        ...nodes
          .filter((node) => node.id !== groupId)
          .map((node) => ({ ...node, op: reroute(node.op) })),
        ...inlined,
      ],
      output:
        composition.output === groupId ? output : (composition.output ?? null),
    },
    ids,
  };
}
