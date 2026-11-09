// Data sets and the places that bind to them, mirroring `alpha2-model`'s `data` and `data_places`.

import {
  MAX_DATA_CHANNELS,
  MAX_DATA_COORDINATE,
  MAX_DATA_ID_BYTES,
  MAX_DATA_INTERVALS,
  MAX_DATA_LABEL_BYTES,
  MAX_DATA_ROTATION_DEGREES,
  MAX_DATA_SCALAR,
  MAX_DATA_SCALE,
  MAX_DATA_SETS,
  MAX_DATA_VALUES,
  MAX_DATA_WORDS,
} from "./generated/project-schema.js";

type Json = Record<string, unknown>;

/** Which clock a place's keyframes are on. */
type Clock =
  | { readonly kind: "timeline" }
  | { readonly kind: "composition"; readonly composition: Json }
  | { readonly kind: "picture" };

/** The value type a binding sits on; `param` carries a built-in parameter's type, if known. */
type Slot =
  | { readonly kind: "point" | "vec2" | "scalar" | "other" }
  | { readonly kind: "param"; readonly type: string | undefined };

/** The type of a built-in effect's parameter, or `undefined` for a host effect. */
export type ParamType = (effectId: string, name: string) => string | undefined;

const encoder = new TextEncoder();

function isRecord(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function bytes(text: string): number {
  return encoder.encode(text).length;
}

function finite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isData(value: unknown): value is Json {
  return isRecord(value) && value.mode === "data";
}

function arityOf(kind: unknown): number | undefined {
  return kind === "points"
    ? 2
    : kind === "scalars"
      ? 1
      : kind === "similarity"
        ? 4
        : undefined;
}

/** Mirrors `value_in_bounds`. */
function inBounds(kind: unknown, component: number, value: unknown): boolean {
  if (!finite(value)) return false;
  const v = Math.fround(value);
  if (kind === "points") return Math.abs(v) <= MAX_DATA_COORDINATE;
  if (kind === "similarity") {
    if (component < 2) return Math.abs(v) <= MAX_DATA_COORDINATE;
    if (component === 2) return Math.abs(v) <= MAX_DATA_ROTATION_DEGREES;
    return v > 0 && v <= MAX_DATA_SCALE;
  }
  return Math.abs(v) <= MAX_DATA_SCALAR;
}

function label(text: unknown): boolean {
  return typeof text === "string" && bytes(text) <= MAX_DATA_LABEL_BYTES;
}

/** Mirrors `DataSet::check`. */
function setProblem(
  set: Json,
  sourceDuration: number | undefined,
  validFingerprint: (fingerprint: unknown) => boolean,
): string | undefined {
  const id = String(set.id);
  if (bytes(id) > MAX_DATA_ID_BYTES)
    return `has an id over ${MAX_DATA_ID_BYTES} bytes`;
  const madeBy = set.made_by;
  if (isRecord(madeBy)) {
    const plugin = madeBy.plugin;
    const pluginOk =
      plugin == null ||
      (isRecord(plugin) &&
        typeof plugin.version === "string" &&
        plugin.version.length > 0 &&
        typeof plugin.artifact_sha256 === "string" &&
        /^[0-9a-f]{64}$/.test(plugin.artifact_sha256));
    if (madeBy.extractor === "" || !pluginOk) return "has an invalid made_by";
  }
  const anchor = isRecord(set.anchor) ? set.anchor : {};
  if (set.made_from != null) {
    if (anchor.type === "timeline") return "has an invalid made_from.anchor";
    if (!validFingerprint(set.made_from)) return "has an invalid made_from";
  }
  const timeProblem = (time: unknown): string | undefined => {
    if (typeof time !== "number" || time < 0) return "has a negative time";
    if (sourceDuration !== undefined && time > sourceDuration)
      return "has a time past its source's duration";
    return undefined;
  };
  const kind = set.kind;
  const arity = arityOf(kind);
  if (arity !== undefined) {
    const channels =
      kind === "similarity"
        ? [""]
        : Array.isArray(set.channels)
          ? set.channels
          : [];
    if (kind !== "similarity") {
      if (channels.length === 0 || channels.length > MAX_DATA_CHANNELS)
        return `needs 1 to ${MAX_DATA_CHANNELS} channels`;
      const seen = new Set<unknown>();
      for (const name of channels) {
        if (
          typeof name !== "string" ||
          name.length === 0 ||
          !label(name) ||
          seen.has(name)
        )
          return "has an empty, overlong or repeated channel name";
        seen.add(name);
      }
    }
    const times = Array.isArray(set.times) ? (set.times as unknown[]) : [];
    const values = Array.isArray(set.values) ? (set.values as unknown[]) : [];
    if (values.length > MAX_DATA_VALUES)
      return `has more than ${MAX_DATA_VALUES} values`;
    if (values.length !== times.length * channels.length * arity)
      return "does not have times * channels * arity values";
    for (const [index, time] of times.entries()) {
      if (index > 0 && (times[index - 1] as number) >= (time as number))
        return "has times that are not strictly ascending";
      const problem = timeProblem(time);
      if (problem) return problem;
    }
    for (let group = 0; group * arity < values.length; group += 1) {
      const lost = values[group * arity] === null;
      for (let component = 0; component < arity; component += 1) {
        const index = group * arity + component;
        const value = values[index];
        const ok =
          value === null ? lost : !lost && inBounds(kind, component, value);
        if (!ok) return `has an invalid value at values[${index}]`;
      }
    }
    return undefined;
  }
  const words = kind === "words";
  const items = (words ? set.words : set.intervals) as unknown[] | undefined;
  const list = Array.isArray(items) ? items : [];
  if (list.length > (words ? MAX_DATA_WORDS : MAX_DATA_INTERVALS))
    return `has more than ${MAX_DATA_WORDS} words or ${MAX_DATA_INTERVALS} intervals`;
  for (const [index, item] of list.entries()) {
    if (!isRecord(item)) return `has an invalid item at [${index}]`;
    const start = item.start as number;
    const end = item.end as number;
    const before = index > 0 ? (list[index - 1] as Json) : undefined;
    const ordered =
      before === undefined ||
      (before.start as number) < start ||
      ((before.start as number) === start && (before.end as number) <= end);
    const own = words
      ? start >= 0 &&
        start < end &&
        typeof item.text === "string" &&
        item.text.length > 0 &&
        label(item.text) &&
        (item.speaker == null || label(item.speaker))
      : start >= 0 &&
        start <= end &&
        (item.label == null || label(item.label)) &&
        (item.score == null || inBounds("scalars", 0, item.score));
    if (!own || !ordered) return `has an invalid item at [${index}]`;
    const problem = timeProblem(start) ?? timeProblem(end);
    if (problem) return problem;
  }
  return undefined;
}

/** Every `{ mode: "data" }` value under `node`, with its path, for places that refuse data. */
function bindingsUnder(
  node: unknown,
  at: string,
  found: [string, Json][],
): void {
  if (Array.isArray(node)) {
    node.forEach((item, index) =>
      bindingsUnder(item, `${at}[${index}]`, found),
    );
    return;
  }
  if (!isRecord(node)) return;
  if (isData(node)) {
    found.push([at, node]);
    return;
  }
  for (const [key, value] of Object.entries(node)) {
    bindingsUnder(value, at ? `${at}.${key}` : key, found);
  }
}

function slotOf(kind: "point" | "vec2" | "scalar"): Slot {
  return { kind };
}

/** Each data binding of a clip mask or composition shape mask's geometry and feather. */
function maskBindings(
  masks: unknown,
  prefix: string,
  found: [string, Json, Slot][],
): void {
  if (!Array.isArray(masks)) return;
  masks.forEach((mask: unknown, index) => {
    if (!isRecord(mask)) return;
    const at = `${prefix}masks[${index}]`;
    const shape = isRecord(mask.shape) ? mask.shape : {};
    for (const [field, value] of Object.entries(shape)) {
      if (!isData(value)) continue;
      const kind =
        field === "center" ? "point" : field === "size" ? "vec2" : "scalar";
      found.push([`${at}.shape.${field}`, value, slotOf(kind)]);
    }
    if (isData(mask.feather))
      found.push([`${at}.feather`, mask.feather, slotOf("scalar")]);
  });
}

/** Each data binding of a visual (a clip's, or a transform node's). */
function visualBindings(
  visual: unknown,
  prefix: string,
  found: [string, Json, Slot][],
): void {
  if (!isRecord(visual)) return;
  const fields: [string, "point" | "vec2" | "scalar"][] = [
    ["position", "point"],
    ["scale", "vec2"],
    ["rotation_degrees", "scalar"],
    ["opacity", "scalar"],
  ];
  for (const [field, kind] of fields) {
    const value = visual[field];
    if (isData(value)) found.push([`${prefix}.${field}`, value, slotOf(kind)]);
  }
}

interface Follow {
  readonly follower: string;
  readonly via: string;
  readonly at: string;
}

/** What holds a data reference. */
export type DataOwner =
  | { readonly kind: "clip"; readonly clip: string }
  | { readonly kind: "track"; readonly track: string }
  | {
      readonly kind: "transition";
      readonly track: string;
      readonly from: string;
      readonly to: string;
    }
  | {
      readonly kind: "node";
      readonly composition: string;
      readonly node: string;
    }
  | { readonly kind: "master" }
  | { readonly kind: "source"; readonly source: string };

/** One place of a project that reads a data set. */
export interface DataReference {
  /** The data set's id. */
  readonly data: string;
  readonly owner: DataOwner;
  /**
   * The property path in the syntax `bake` targets use: `visual.position`, `masks[0].feather`,
   * `effect.params.radius`, `visual.correction`, or `captions` for a track's caption words.
   */
  readonly property: string;
  /** The clip or composition node a source-anchored binding reads through. */
  readonly via?: { readonly clip: string } | { readonly node: string };
}

/** A reference as the walker finds it, with what checking it needs. */
type Place =
  | {
      readonly use: "binding";
      readonly owner: DataOwner;
      readonly at: string;
      readonly binding: Json;
      readonly clock: Clock;
      readonly slot: Slot;
      readonly follower: string | undefined;
    }
  | {
      readonly use: "correction" | "words";
      readonly owner: DataOwner;
      readonly at: string;
      readonly reference: Json;
    }
  | {
      readonly use: "refused";
      readonly owner: DataOwner;
      readonly at: string;
      readonly problem: string;
    };

function ownerText(owner: DataOwner): string {
  switch (owner.kind) {
    case "clip":
      return `clip ${owner.clip}`;
    case "track":
      return `track ${owner.track}`;
    case "transition":
      return `track ${owner.track} transition ${owner.from} to ${owner.to}`;
    case "node":
      return `composition ${owner.composition} node ${owner.node}`;
    case "master":
      return "the master bus";
    case "source":
      return `source ${owner.source}`;
  }
}

/** `reference` as text in the lint path syntax, such as `clip c1 visual.position`. */
export function describeDataReference(reference: DataReference): string {
  return `${ownerText(reference.owner)} ${reference.property}`;
}

/** Visits every binding, correction and caption words reference in `data_places` order. */
function walkPlaces(
  project: Json,
  paramType: ParamType,
  visit: (place: Place) => void,
): void {
  const timeline = isRecord(project.timeline) ? project.timeline : {};
  const tracks = (Array.isArray(timeline.tracks) ? timeline.tracks : []).filter(
    isRecord,
  );
  const timelineClock: Clock = { kind: "timeline" };
  const pictureClock: Clock = { kind: "picture" };
  const scalar: Slot = { kind: "scalar" };
  for (const track of tracks) {
    const trackId = String(track.id);
    for (const clip of Array.isArray(track.clips) ? track.clips : []) {
      if (!isRecord(clip)) continue;
      const clipId = String(clip.id);
      const owner: DataOwner = { kind: "clip", clip: clipId };
      const found: [string, Json, Slot][] = [];
      visualBindings(clip.visual, "visual", found);
      maskBindings(clip.masks, "", found);
      const audio = isRecord(clip.audio) ? clip.audio : {};
      for (const field of ["gain", "pan"]) {
        if (isData(audio[field]))
          found.push([`audio.${field}`, audio[field], slotOf("scalar")]);
      }
      for (const [at, binding, slot] of found)
        visit({
          use: "binding",
          owner,
          at,
          binding,
          clock: timelineClock,
          slot,
          follower: clipId,
        });
      const visual = isRecord(clip.visual) ? clip.visual : {};
      if (isRecord(visual.correction))
        visit({
          use: "correction",
          owner,
          at: "visual.correction",
          reference: visual.correction,
        });
    }
    for (const transition of Array.isArray(track.transitions)
      ? track.transitions
      : []) {
      if (!isRecord(transition) || !isRecord(transition.effect)) continue;
      const owner: DataOwner = {
        kind: "transition",
        track: trackId,
        from: String(transition.from_clip),
        to: String(transition.to_clip),
      };
      const params = isRecord(transition.effect.params)
        ? transition.effect.params
        : {};
      for (const [name, value] of Object.entries(params)) {
        if (!isData(value)) continue;
        visit({
          use: "binding",
          owner,
          at: `effect.params.${name}`,
          binding: value,
          clock: timelineClock,
          slot: {
            kind: "param",
            type: paramType(String(transition.effect.id), name),
          },
          follower: undefined,
        });
      }
    }
    const owner: DataOwner = { kind: "track", track: trackId };
    const audio = isRecord(track.audio) ? track.audio : {};
    for (const field of ["gain", "pan"]) {
      const value = audio[field];
      if (isData(value))
        visit({
          use: "binding",
          owner,
          at: `audio.${field}`,
          binding: value,
          clock: timelineClock,
          slot: scalar,
          follower: undefined,
        });
    }
    if (isRecord(track.captions)) {
      const captions = track.captions;
      const found: [string, Json][] = [];
      bindingsUnder(captions.style, "captions.style", found);
      bindingsUnder(captions.styles, "captions.styles", found);
      for (const [at, binding] of found)
        visit({
          use: "binding",
          owner,
          at,
          binding,
          clock: pictureClock,
          slot: scalar,
          follower: undefined,
        });
      if (isRecord(captions.from_words))
        visit({
          use: "words",
          owner,
          at: "captions.from_words",
          reference: captions.from_words,
        });
    }
  }
  const master = isRecord(project.audio) ? project.audio : {};
  if (isData(master.gain))
    visit({
      use: "binding",
      owner: { kind: "master" },
      at: "audio.gain",
      binding: master.gain,
      clock: timelineClock,
      slot: slotOf("scalar"),
      follower: undefined,
    });

  const compositions = new Map<string, Json>();
  for (const composition of Array.isArray(project.compositions)
    ? project.compositions
    : []) {
    if (isRecord(composition))
      compositions.set(String(composition.id), composition);
  }
  for (const composition of compositions.values()) {
    for (const node of Array.isArray(composition.nodes)
      ? composition.nodes
      : []) {
      if (!isRecord(node) || !isRecord(node.op)) continue;
      const op = node.op;
      const owner: DataOwner = {
        kind: "node",
        composition: String(composition.id),
        node: String(node.id),
      };
      const clock: Clock = { kind: "composition", composition };
      const found: [string, Json, Slot][] = [];
      const effectParams = (effect: unknown) => {
        if (!isRecord(effect) || !isRecord(effect.params)) return;
        for (const [name, value] of Object.entries(effect.params)) {
          if (isData(value))
            found.push([
              `effect.params.${name}`,
              value,
              { kind: "param", type: paramType(String(effect.id), name) },
            ]);
        }
      };
      switch (op.type) {
        case "effect":
          effectParams(op.effect);
          break;
        case "transition":
          effectParams(op.effect);
          if (isData(op.progress))
            found.push(["progress", op.progress, slotOf("scalar")]);
          break;
        case "transform":
          if (isRecord(op.transform) && op.transform.correction != null)
            visit({
              use: "refused",
              owner,
              at: "transform.correction",
              problem: "has `correction` where it does not apply",
            });
          visualBindings(op.transform, "transform", found);
          break;
        case "merge":
          (Array.isArray(op.layers) ? op.layers : []).forEach(
            (layer: unknown, index) => {
              if (isRecord(layer) && isData(layer.opacity))
                found.push([
                  `layers[${index}].opacity`,
                  layer.opacity,
                  slotOf("scalar"),
                ]);
            },
          );
          break;
        case "shape_mask":
          maskBindings(op.masks, "", found);
          break;
        case "group": {
          const controls = isRecord(op.controls) ? op.controls : {};
          const group = compositions.get(String(op.composition));
          for (const [name, value] of Object.entries(controls)) {
            if (!isData(value)) continue;
            const control = (
              Array.isArray(group?.controls) ? group.controls : []
            ).find(
              (candidate: unknown) =>
                isRecord(candidate) && candidate.name === name,
            ) as Json | undefined;
            const target = (
              Array.isArray(group?.nodes) ? group.nodes : []
            ).find(
              (candidate: unknown) =>
                isRecord(candidate) && candidate.id === control?.node,
            ) as Json | undefined;
            const targetOp = isRecord(target?.op) ? target.op : undefined;
            const type =
              (targetOp?.type === "effect" ||
                targetOp?.type === "transition") &&
              isRecord(targetOp.effect)
                ? paramType(String(targetOp.effect.id), String(control?.param))
                : undefined;
            found.push([`controls.${name}`, value, { kind: "param", type }]);
          }
          break;
        }
        case "generator": {
          const nested: [string, Json][] = [];
          bindingsUnder(op.generator, "generator", nested);
          for (const [at, binding] of nested) found.push([at, binding, scalar]);
          break;
        }
        default:
          break;
      }
      for (const [at, binding, slot] of found)
        visit({
          use: "binding",
          owner,
          at,
          binding,
          clock: op.type === "generator" ? pictureClock : clock,
          slot,
          follower: undefined,
        });
    }
  }
  for (const source of Array.isArray(project.sources) ? project.sources : []) {
    if (!isRecord(source) || !isRecord(source.generator)) continue;
    const found: [string, Json][] = [];
    bindingsUnder(source.generator, "", found);
    for (const [at, binding] of found)
      visit({
        use: "binding",
        owner: { kind: "source", source: String(source.id) },
        at,
        binding,
        clock: pictureClock,
        slot: scalar,
        follower: undefined,
      });
  }
}

/**
 * Every place of `project` that reads a data set, in document order: bindings, corrections and
 * caption words. References to sets the project lacks are listed too.
 */
export function dataReferences(project: object): DataReference[] {
  const references: DataReference[] = [];
  walkPlaces(
    project as Json,
    () => undefined,
    (place) => {
      if (place.use === "refused") return;
      const reference =
        place.use === "binding" ? place.binding : place.reference;
      const via = place.use === "binding" ? place.binding.via : undefined;
      references.push({
        data: String(reference.data),
        owner: place.owner,
        property: place.use === "words" ? "captions" : place.at,
        ...(isRecord(via) && typeof via.clip === "string"
          ? { via: { clip: via.clip } }
          : isRecord(via) && typeof via.node === "string"
            ? { via: { node: via.node } }
            : {}),
      });
    },
  );
  return references;
}

/** Data sets whose own checks passed, by the source duration they passed with. */
const passedSets = new WeakMap<object, number | undefined>();

/**
 * Problems of the project's data sets and of every binding, correction and caption words
 * reference. `validFingerprint` is the validator's own fingerprint check. A set `sealed` reports
 * as unchangeable skips its own checks once it has passed them with the same source duration.
 */
export function dataProblems(
  project: Json,
  paramType: ParamType,
  validFingerprint: (fingerprint: unknown) => boolean,
  sealed: (value: unknown) => boolean = () => false,
): string[] {
  const problems: string[] = [];
  const sets = Array.isArray(project.data) ? (project.data as unknown[]) : [];
  if (sets.length > MAX_DATA_SETS) {
    return [
      `project has ${sets.length} data sets; the maximum is ${MAX_DATA_SETS}`,
    ];
  }
  const sources = new Map<string, Json>();
  for (const source of Array.isArray(project.sources) ? project.sources : []) {
    if (isRecord(source)) sources.set(String(source.id), source);
  }
  const data = new Map<string, Json>();
  for (const set of sets) {
    if (!isRecord(set)) continue;
    const id = String(set.id);
    if (data.has(id)) {
      problems.push(`duplicate data set id ${id}`);
      continue;
    }
    data.set(id, set);
    const anchor = isRecord(set.anchor) ? set.anchor : {};
    let duration: number | undefined;
    if (anchor.type === "source") {
      const source = sources.get(String(anchor.source_id));
      if (source === undefined) {
        problems.push(
          `data set ${id} is anchored to unknown source ${String(anchor.source_id)}`,
        );
        continue;
      }
      if (!["video", "audio", "image"].includes(String(source.kind))) {
        problems.push(
          `data set ${id} is anchored to source ${String(source.id)}, which has no media`,
        );
        continue;
      }
      const fingerprint = isRecord(source.fingerprint)
        ? source.fingerprint
        : {};
      duration =
        typeof fingerprint.duration_ticks === "number"
          ? fingerprint.duration_ticks
          : undefined;
    }
    if (passedSets.has(set) && passedSets.get(set) === duration) continue;
    const problem = setProblem(set, duration, validFingerprint);
    if (problem) problems.push(`data set ${id} ${problem}`);
    else if (sealed(set)) passedSets.set(set, duration);
  }

  const timeline = isRecord(project.timeline) ? project.timeline : {};
  const tracks = (Array.isArray(timeline.tracks) ? timeline.tracks : []).filter(
    isRecord,
  );
  const clips = new Map<string, { clip: Json; trackKind: unknown }>();
  for (const track of tracks) {
    for (const clip of Array.isArray(track.clips) ? track.clips : []) {
      if (isRecord(clip))
        clips.set(String(clip.id), { clip, trackKind: track.kind });
    }
  }
  const follows: Follow[] = [];

  /** Mirrors `Places::check`; `undefined` when the binding is usable. */
  const check = (
    binding: Json,
    clock: Clock,
    slot: Slot,
    follower: string | undefined,
    at: string,
  ): string | undefined => {
    const set = data.get(String(binding.data));
    if (set === undefined) return "names an unknown data set";
    const kind = set.kind;
    const vec2Slot =
      slot.kind === "point" ||
      slot.kind === "vec2" ||
      (slot.kind === "param" &&
        (slot.type === "vec2" || slot.type === undefined));
    const scalarSlot =
      slot.kind === "scalar" ||
      (slot.kind === "param" && slot.type === "float");
    if (!vec2Slot && !scalarSlot) return "is a value whose type takes no data";
    const accepted = vec2Slot ? ["points", "scalars"] : ["scalars"];
    if (!accepted.includes(String(kind)))
      return `cannot follow ${String(kind)} data`;
    const channels = Array.isArray(set.channels) ? set.channels : [];
    if (!channels.includes(binding.channel)) return "names an unknown channel";
    const offset = binding.offset;
    const remap = binding.remap;
    const offsetOk =
      offset == null || (Array.isArray(offset) && offset.every(finite));
    const remapOk =
      remap == null ||
      (isRecord(remap) &&
        Array.isArray(remap.input) &&
        Array.isArray(remap.output) &&
        [...(remap.input as unknown[]), ...(remap.output as unknown[])].every(
          finite,
        ) &&
        Math.fround(remap.input[0] as number) !==
          Math.fround(remap.input[1] as number));
    if (!offsetOk || !remapOk)
      return "has a non-finite offset or remap, or a remap with equal inputs";
    const points = kind === "points";
    if (points && remap != null) return "has `remap` where it does not apply";
    if (!points && offset != null)
      return "has `offset` where it does not apply";
    if (!points && binding.space != null)
      return "has `space` where it does not apply";
    const anchor = isRecord(set.anchor) ? set.anchor : {};
    if (
      clock.kind === "picture" ||
      (anchor.type === "timeline" && clock.kind === "composition")
    )
      return "is keyed on a clock that refuses this anchor";
    if (anchor.type === "timeline") {
      if (binding.via != null)
        return "takes no `via` for a timeline-anchored data set";
      if (binding.space != null) return "has `space` where it does not apply";
      return undefined;
    }
    const via = binding.via;
    if (!isRecord(via)) return "needs `via` for a source-anchored data set";
    if (points && binding.space == null)
      return "needs `space` for a point read through `via`";
    const source = String(anchor.source_id);
    if (typeof via.clip === "string" && clock.kind === "timeline") {
      const entry = clips.get(via.clip);
      if (entry === undefined) return "reads through an unknown clip";
      if (String(entry.clip.source_id) !== source)
        return "reads through a reader of another source";
      if (binding.space === "canvas") {
        if (entry.trackKind !== "video")
          return "names a reader of the wrong kind for its clock";
        if (follower === via.clip)
          return "follows clips in canvas space in a cycle";
        if (follower !== undefined)
          follows.push({ follower, via: via.clip, at });
      }
      return undefined;
    }
    if (typeof via.node === "string" && clock.kind === "composition") {
      const nodes: unknown[] = Array.isArray(clock.composition.nodes)
        ? (clock.composition.nodes as unknown[])
        : [];
      const node = nodes.find(
        (candidate: unknown) =>
          isRecord(candidate) && candidate.id === via.node,
      );
      const op = isRecord(node) && isRecord(node.op) ? node.op : undefined;
      if (op?.type !== "media")
        return "reads through a node that is not a media node of its composition";
      if (String(op.source_id) !== source)
        return "reads through a reader of another source";
      if (binding.space === "canvas")
        return "has `space` where it does not apply";
      return undefined;
    }
    return "names a reader of the wrong kind for its clock";
  };
  const report = (owner: string, at: string, problem: string | undefined) => {
    if (problem) problems.push(`${owner} ${at} ${problem}`);
  };
  const usable = (id: unknown, at: string, owner: string): Json | undefined => {
    const set = data.get(String(id));
    if (set === undefined) report(owner, at, "names an unknown data set");
    return set;
  };

  walkPlaces(project, paramType, (place) => {
    const owner = ownerText(place.owner);
    if (place.use === "refused") {
      report(owner, place.at, place.problem);
      return;
    }
    if (place.use === "binding") {
      report(
        owner,
        place.at,
        check(place.binding, place.clock, place.slot, place.follower, place.at),
      );
      return;
    }
    const set = usable(place.reference.data, place.at, owner);
    if (set === undefined) return;
    const anchor = isRecord(set.anchor) ? set.anchor : {};
    if (place.use === "correction") {
      const clip = clips.get((place.owner as { clip: string }).clip)?.clip;
      if (set.kind !== "similarity")
        report(owner, place.at, `cannot follow ${String(set.kind)} data`);
      else if (anchor.type !== "source" || anchor.source_id !== clip?.source_id)
        report(
          owner,
          place.at,
          "corrects with data anchored to another source",
        );
      return;
    }
    const words = place.reference;
    const trackOk =
      anchor.type === "source"
        ? typeof words.track === "string" &&
          tracks.some(
            (other) => other.id === words.track && other.kind !== "caption",
          )
        : words.track == null;
    if (set.kind !== "words")
      report(owner, place.at, `cannot follow ${String(set.kind)} data`);
    else if (!trackOk || words.max_chars === 0)
      report(owner, place.at, "has an invalid words track or max_chars");
  });
  const cycle = followCycle(follows);
  if (cycle)
    problems.push(
      `clip ${cycle.follower} ${cycle.at} follows clips in canvas space in a cycle`,
    );
  return problems;
}

/** The first canvas follow, in document order, whose two clips lie on a cycle. */
function followCycle(follows: readonly Follow[]): Follow | undefined {
  const next = new Map<string, string[]>();
  for (const follow of follows) {
    const list = next.get(follow.follower) ?? [];
    list.push(follow.via);
    next.set(follow.follower, list);
  }
  const reaches = (from: string, target: string): boolean => {
    const seen = new Set<string>();
    const stack = [from];
    while (stack.length > 0) {
      const clip = stack.pop() as string;
      if (clip === target) return true;
      if (seen.has(clip)) continue;
      seen.add(clip);
      stack.push(...(next.get(clip) ?? []));
    }
    return false;
  };
  return follows.find((follow) => reaches(follow.via, follow.follower));
}
