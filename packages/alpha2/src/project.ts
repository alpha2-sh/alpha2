/**
 * The alpha2 project document (schema version 1).
 *
 * Field names are the wire names, so a {@link Project} serializes directly with `JSON.stringify`.
 * Types and limits are generated from `schema/project.v1.schema.json`; {@link validateProject}
 * mirrors the model's invariants beyond the schema.
 *
 * Timeline values are integer ticks at {@link TICKS_PER_SECOND}. Ranges are half-open.
 */

import type {
  Animated,
  Clip,
  ClipTransition,
  Composition,
  Easing,
  EffectResource,
  FrameRate,
  Insets,
  Interpolation,
  LayoutOffset,
  LayoutSize,
  MediaSource,
  MediaTime,
  Paint,
  Project,
  Rgba,
  TextContent,
  Track,
  Vec2,
  VisualProperties,
} from "./generated/project-schema.js";
import { BUILTIN_EFFECT_RULES } from "./generated/builtin-effects.js";
import {
  parseHex,
  parseInsets,
  parseLength,
  parseOffset,
  parsePlacement,
  parseSize,
  parseTrackList,
  type GridEdge,
  type Length,
  type Parsed,
  type TrackBreadth,
  type TrackComponent,
  type TrackSize,
} from "./layout-values.js";
import {
  AUDIO_EFFECT_STATE_MARGIN_MS,
  MAX_CANVAS_DIMENSION,
  MAX_CANVAS_PIXELS,
  MAX_TEXT_CONTENT_BYTES,
  MAX_FONT_FAMILY_BYTES,
  MAX_COMPOSITIONS,
  MAX_COMPOSITION_NODES,
  MAX_COMPOSITION_CONTROLS,
  MAX_COMPOSITION_DEPTH,
  MAX_GROUP_INPUTS,
  MAX_EFFECT_INPUTS,
  MAX_FRAME_INLINED_NODES,
  MAX_INLINED_NODES,
  MAX_MERGE_LAYERS,
  MAX_CLIP_MASKS,
  MAX_DASH_ENTRIES,
  MAX_GRADIENT_STOPS,
  MAX_PATH_SUBPATHS,
  MAX_PATH_VERTICES,
  MAX_POLYSTAR_POINTS,
  MAX_SHAPE_DEPTH,
  MAX_SHAPE_ITEMS,
  MAX_LAYOUT_DEPTH,
  MAX_LAYOUT_FONT_SIZE,
  MAX_LAYOUT_ISOLATED,
  MAX_LAYOUT_NODES,
  MAX_LAYOUT_TEXTS,
  MAX_GRID_TRACKS,
  MIN_POLYSTAR_POINTS,
  MAX_CLIPS,
  MAX_CLIPS_PER_TRACK,
  MAX_AUDIO_DELAY_MS,
  MAX_AUDIO_EFFECTS,
  MAX_CAPTION_FONT_SIZE,
  MAX_CAPTION_NAME_BYTES,
  MAX_CAPTION_STYLES,
  MAX_TEXT_GENERATOR_FONT_SIZE,
  MAX_FONT_WEIGHT,
  MAX_LETTER_SPACING,
  MAX_LINE_HEIGHT,
  MAX_TEXT_PADDING,
  MAX_ANIMATOR_SCALE,
  MAX_CLASS_NAME_BYTES,
  MAX_RANGE_INDEX,
  MAX_RANGE_PERCENT,
  MAX_RUN_CLASSES,
  MAX_TEXT_ANIMATORS,
  MAX_TEXT_RUNS,
  MIN_RANGE_PERCENT,
  MAX_TEXT_SHADOW_BLUR,
  MAX_TEXT_SHADOW_OFFSET,
  MAX_TEXT_SHADOWS,
  MAX_TEXT_STROKE_EXTENT,
  MAX_TEXT_STROKES,
  MAX_CHAIN_EFFECTS,
  MAX_COMPRESSOR_RATIO,
  MAX_ENVELOPE_MS,
  MAX_EQ_BANDS,
  MAX_LOOKAHEAD_MS,
  MIN_FONT_WEIGHT,
  MIN_LETTER_SPACING,
  MIN_LINE_HEIGHT,
  MIN_LEVEL_DB,
  MAX_EQ_FREQUENCY_HZ,
  MAX_EQ_GAIN_DB,
  MAX_EQ_Q,
  MAX_GAIN,
  MAX_PROJECT_JSON_BYTES,
  MAX_SAMPLE_RATE,
  MAX_TRACKS,
  MAX_TRANSITIONS_PER_TRACK,
  MIN_EQ_FREQUENCY_HZ,
  MIN_EQ_Q,
  MIN_SAMPLE_RATE,
  MIN_HDR_PEAK_NITS,
  MAX_HDR_PEAK_NITS,
  MAX_SEQUENCE_FRAMES,
  MAX_SELECTED_CHANNELS,
  MAX_AUDIO_CHANNELS,
  MAX_LATEX_BYTES,
  MAX_LATEX_FONT_SIZE,
  MAX_PDF_PAGES,
  REVERB_DELAY_MS,
  TICKS_PER_SECOND,
} from "./generated/project-schema.js";
import { PROJECT_SHAPE } from "./generated/project-shape.js";
import {
  MAX_JSON_DEPTH,
  hasLoneSurrogate,
  readJsonText,
  type JsonSpelling,
} from "./json-text.js";

export * from "./generated/project-schema.js";
/** The deepest nesting of arrays and objects the engine reads in a project (serde_json's limit). */
export { MAX_JSON_DEPTH };

/** A signed position or duration in timeline ticks. */
export type Ticks = MediaTime;

/** The control points of a CSS `cubic-bezier(x1, y1, x2, y2)` curve. */
export type CubicBezier = Omit<
  Extract<Easing, { readonly type: "cubic_bezier" }>,
  "type"
>;

/**
 * A CSS `cubic-bezier` curve, usable as an {@link Easing} or {@link Interpolation}. `x1` and `x2`
 * must lie in `0..=1`; `y1` and `y2` may overshoot.
 */
export function cubicBezier(
  x1: number,
  y1: number,
  x2: number,
  y2: number,
): Extract<Easing, { readonly type: "cubic_bezier" }> {
  return { type: "cubic_bezier", x1, y1, x2, y2 };
}

/** The CSS Easing Functions Level 1 named curves, as `cubic_bezier` values. */
export const EASING_PRESETS = {
  linear: { type: "linear" },
  ease: cubicBezier(0.25, 0.1, 0.25, 1),
  easeIn: cubicBezier(0.42, 0, 1, 1),
  easeOut: cubicBezier(0, 0, 0.58, 1),
  easeInOut: cubicBezier(0.42, 0, 0.58, 1),
} as const satisfies Readonly<Record<string, Easing>>;

/** Keeps a keyframe's value until the next keyframe. */
export const HOLD: Interpolation = { type: "hold" };

export function constant<T>(value: T): Animated<T> {
  return { mode: "constant", value };
}

/**
 * The tick nearest `seconds`, ties away from zero, as the engine reads seconds. Rounding guarantees
 * `secondsToTicks(ticksToSeconds(t)) === t`.
 */
export function secondsToTicks(seconds: number): Ticks {
  const scaled = seconds * TICKS_PER_SECOND;
  return Math.sign(scaled) * Math.round(Math.abs(scaled));
}

export function ticksToSeconds(ticks: Ticks): number {
  return ticks / TICKS_PER_SECOND;
}

/** The tick at or before sample `samples`, matching the engine's `sample_ticks_floor`. */
export function samplesToTicks(samples: number, sampleRate: number): Ticks {
  if (sampleRate <= 0) return 0;
  return Math.floor((samples * TICKS_PER_SECOND) / sampleRate);
}

/** Converts timeline ticks to the sample index containing them. */
export function ticksToSamples(ticks: Ticks, sampleRate: number): number {
  if (sampleRate <= 0) return 0;
  return Math.floor((ticks * sampleRate) / TICKS_PER_SECOND);
}

/**
 * Structural problems found without opening any media, mirroring the engine's checks so an editor
 * can reject a document before loading it. The engine still re-validates on load.
 *
 * Shape problems (types, required fields, enum values, unknown fields) are reported alone, as the
 * engine's parser stops there. A document over a count limit reports only its counts. Otherwise the
 * model's invariants are checked in time linear in the document.
 *
 * Never throws; returns an empty array for a valid document. JSON spelling (repeated keys, `1920.0`
 * for an integer) is invisible to an object; use {@link validateProjectJson} for text.
 */
export function validateProject(project: Project): string[] {
  return projectProblems(project, undefined);
}

function projectProblems(
  project: Project,
  spelling: JsonSpelling | undefined,
): string[] {
  // Only objects can share parts with a document validated before.
  const reuse = spelling === undefined;
  const { problems: shape, read } = shapeProblems(project, spelling, reuse);
  if (shape.length > 0) return shape;
  // Float fields are read as 32-bit, as the engine does. Problems are always worded by a full walk.
  if (reuse && invariantsHold(read)) return [];
  return invariantProblems(read);
}

function invariantProblems(value: unknown): string[] {
  const project = value as Project;
  if (!isRecord(project)) return ["project must be an object"];
  const problems = headerProblems(project);
  const list = <T>(
    value: unknown,
    what: string,
    required: boolean,
  ): readonly T[] => {
    if (Array.isArray(value)) return value as readonly T[];
    if (required || value !== undefined)
      problems.push(`${what} must be an array`);
    return [];
  };

  const sourceList = list<MediaSource>(project.sources, "sources", true);
  const compositionList = list<Composition>(
    project.compositions,
    "compositions",
    false,
  );
  const timeline = isRecord(project.timeline) ? project.timeline : undefined;
  if (timeline === undefined) problems.push("timeline must be an object");
  const tracks = list<Track>(
    timeline?.tracks,
    "timeline.tracks",
    timeline !== undefined,
  );
  // Like the engine, refuse over-count documents before reading what the counts hold.
  const counts = countProblems(sourceList, compositionList, tracks);
  if (counts.length > 0) {
    for (const problem of counts) problems.push(problem);
    return problems;
  }

  const resourceList = list<EffectResource>(
    project.effect_resources,
    "effect_resources",
    false,
  );
  const catalog = catalogOf(
    sourceList,
    resourceList,
    compositionList,
    canvasOf(project),
  );
  for (const problem of catalog.problems) problems.push(problem);

  const trackIds = new Set<string>();
  const clipIds = new Set<string>();
  for (const track of tracks) {
    if (!isRecord(track)) {
      problems.push("each track must be an object");
      continue;
    }
    const trackId = text(track.id);
    if (trackId.length === 0) problems.push("track id must not be empty");
    if (trackIds.has(trackId)) problems.push(`duplicate track id ${trackId}`);
    trackIds.add(trackId);
    for (const problem of trackProblems(track, catalog, clipIds, false))
      problems.push(problem);
  }
  for (const problem of matteProblems(tracks)) problems.push(problem);
  if (catalog.inlined !== undefined) {
    const frame = frameNodesOver(
      tracks.map((track) =>
        frameEdges(track, catalog.sources, catalog.inlined!),
      ),
    );
    if (frame !== undefined) problems.push(frame);
  }
  for (const problem of audioEffectProblems(
    project.audio,
    tracks.map(trackAudioLoad),
  ))
    problems.push(problem);
  for (const problem of duckingProblems(tracks, trackIds))
    problems.push(problem);
  return problems;
}

function headerProblems(project: Project): string[] {
  const problems: string[] = [];
  if (text(project.id).length === 0)
    problems.push("project id must not be empty");
  if (!isRecord(project.settings)) {
    problems.push("settings must be an object");
  } else {
    const { width, height, frame_rate, sample_rate, background } =
      project.settings;
    if (!positiveInteger(width) || !positiveInteger(height)) {
      problems.push("settings must have a positive integer size");
    } else if (width > MAX_CANVAS_DIMENSION || height > MAX_CANVAS_DIMENSION) {
      problems.push(`canvas edges must not exceed ${MAX_CANVAS_DIMENSION}`);
    } else if (width * height > MAX_CANVAS_PIXELS) {
      problems.push(`canvas must not exceed ${MAX_CANVAS_PIXELS} pixels`);
    }
    if (
      !isRecord(frame_rate) ||
      !positiveInteger(frame_rate.num) ||
      !positiveInteger(frame_rate.den)
    ) {
      problems.push("frame_rate must be a positive rational");
    }
    if (
      !positiveInteger(sample_rate) ||
      sample_rate < MIN_SAMPLE_RATE ||
      sample_rate > MAX_SAMPLE_RATE
    ) {
      problems.push(
        `sample_rate must be within ${MIN_SAMPLE_RATE}..=${MAX_SAMPLE_RATE}`,
      );
    }
    if (background !== undefined && !validRgb(background)) {
      problems.push("background must be an RGB color within 0..=1");
    }
    const peak = project.settings.color?.hdr_peak_nits;
    if (
      peak !== undefined &&
      !(
        Number.isInteger(peak) &&
        peak >= MIN_HDR_PEAK_NITS &&
        peak <= MAX_HDR_PEAK_NITS
      )
    ) {
      problems.push(
        `the HDR peak must be within ${MIN_HDR_PEAK_NITS}..=${MAX_HDR_PEAK_NITS} nits`,
      );
    }
  }
  if (!validMasterAudio(project.audio)) {
    problems.push("the project master bus is invalid");
  }
  return problems;
}

/** Project-wide lookups the track checks read. */
interface Catalog {
  readonly problems: readonly string[];
  readonly sources: ReadonlyMap<string, unknown>;
  readonly effectResources: ReadonlySet<string>;
  readonly compositions: ReadonlySet<string>;
  readonly inlined?: ReadonlyMap<string, number>;
}

function canvasOf(project: Project): Canvas {
  const settings: Record<string, unknown> = isRecord(project.settings)
    ? project.settings
    : {};
  return [Number(settings.width), Number(settings.height)];
}

function catalogOf(
  sourceList: readonly MediaSource[],
  resourceList: readonly EffectResource[],
  compositionList: readonly Composition[],
  canvas: Canvas,
): Catalog {
  const problems: string[] = [];
  const sources = new Map(
    sourceList.map((source) => [
      text(isRecord(source) ? source.id : ""),
      source as unknown,
    ]),
  );
  if (sources.size !== sourceList.length) {
    problems.push("source ids must be unique");
  }
  for (const source of sourceList) {
    if (!isRecord(source)) {
      problems.push("each source must be an object");
      continue;
    }
    const sourceId = text(source.id);
    if (sourceId.length === 0) problems.push("source id must not be empty");
    if (
      source.fingerprint != null &&
      (source.kind === "generated" || !validFingerprint(source.fingerprint))
    ) {
      problems.push(`source ${sourceId} has an invalid fingerprint`);
    }
    if (
      source.color_metadata != null &&
      !consistentColor(source.color_metadata)
    ) {
      problems.push(
        `source ${sourceId} declares a dynamic range its transfer contradicts`,
      );
    }
    if (
      source.provider != null &&
      (source.has_audio === true ||
        !validProvider(source.provider, source.kind))
    ) {
      problems.push(`source ${sourceId} has an invalid provider`);
    }
    if (source.kind === "composition") {
      if (
        text(source.composition).length === 0 ||
        source.generator != null ||
        text(source.locator).length > 0 ||
        source.color_metadata != null ||
        source.fingerprint != null ||
        source.provider != null ||
        source.has_audio === true
      ) {
        problems.push(`source ${sourceId} is not a valid composition source`);
      }
    } else if (source.composition != null) {
      problems.push(`source ${sourceId} is not a valid composition source`);
    } else if (source.kind === "generated") {
      const issue = generatorShapeIssue(source.generator);
      const layout = generatorLayoutIssue(source.generator, canvas);
      if (issue) {
        problems.push(shapeProblem(`source ${sourceId}`, issue));
      } else if (layout) {
        problems.push(layoutProblem(`source ${sourceId}`, layout));
      } else if (
        !validGenerator(source.generator) ||
        text(source.locator).length > 0 ||
        source.color_metadata != null ||
        source.has_audio === true
      ) {
        problems.push(`source ${sourceId} has an invalid generator`);
      }
    } else if (source.generator != null) {
      problems.push(`source ${sourceId} has an invalid generator`);
    } else if (
      isRecord(source.provider) &&
      typeof source.provider.format === "string" &&
      !providerReadsFile({ format: source.provider.format })
    ) {
      // Mirrors `validate_fileless_source`.
      if (text(source.locator).length > 0 || source.fingerprint != null) {
        problems.push(`source ${sourceId} has an invalid provider`);
      }
    } else if (text(source.locator).length === 0) {
      problems.push(`source ${sourceId} has an empty locator`);
    } else if (source.kind === "image" && source.has_audio === true) {
      problems.push(`source ${sourceId} is a still image and has no audio`);
    }
  }

  const effectResources = new Set(
    resourceList.map((resource) => text(isRecord(resource) ? resource.id : "")),
  );
  if (effectResources.size !== resourceList.length) {
    problems.push("effect resource ids must be unique");
  }
  for (const resource of resourceList) {
    if (
      !isRecord(resource) ||
      text(resource.id).length === 0 ||
      resource.kind !== "lut3d" ||
      text(resource.locator).length === 0
    ) {
      problems.push(
        "each effect resource must have an id, supported kind, and locator",
      );
    }
  }

  const compositions = new Set(
    compositionList.map((composition) =>
      text(isRecord(composition) ? composition.id : ""),
    ),
  );
  if (compositions.size !== compositionList.length) {
    problems.push("composition ids must be unique");
  }
  const compositionsById = new Map<string, Record<string, unknown>>();
  for (const composition of compositionList)
    if (isRecord(composition))
      compositionsById.set(text(composition.id), composition);
  const controlNames = new Map<Record<string, unknown>, ReadonlySet<string>>();
  const controlNamesOf = (
    target: Record<string, unknown>,
  ): ReadonlySet<string> => {
    let names = controlNames.get(target);
    if (names === undefined) {
      names = new Set(
        (Array.isArray(target.controls) ? target.controls : []).map(
          (control: unknown) => text(isRecord(control) ? control.name : ""),
        ),
      );
      controlNames.set(target, names);
    }
    return names;
  };
  const builtinControls = new Map<
    Record<string, unknown>,
    ReadonlyMap<string, BuiltinControl>
  >();
  const builtinControlsOf = (
    target: Record<string, unknown>,
  ): ReadonlyMap<string, BuiltinControl> => {
    let found = builtinControls.get(target);
    if (found === undefined) {
      found = builtinControlTargets(target);
      builtinControls.set(target, found);
    }
    return found;
  };
  for (const composition of compositionList) {
    const found = compositionProblems(
      composition,
      sources,
      effectResources,
      compositionsById,
      controlNamesOf,
      builtinControlsOf,
      canvas,
    );
    for (const problem of found) problems.push(problem);
  }
  for (const [sourceId, source] of sources)
    if (
      isRecord(source) &&
      source.composition != null &&
      !compositions.has(text(source.composition))
    )
      problems.push(`source ${sourceId} is not a valid composition source`);
  const looping = compositionUsingItself(compositionsById, sources);
  let inlined: ReadonlyMap<string, number> | undefined;
  if (looping !== undefined)
    problems.push(`composition ${looping} uses itself`);
  else {
    const expansion = compositionExpansion(compositionsById, sources);
    if (expansion.problem !== undefined) problems.push(expansion.problem);
    else inlined = expansion.nodes;
  }
  return { problems, sources, effectResources, compositions, inlined };
}

/**
 * Problems of one track that depend on the rest of the project only through `catalog`. Adds the
 * track's clip ids to `clipIds`. With `reuse`, clips that passed before with the same kind and
 * catalog are skipped.
 */
function trackProblems(
  track: Record<string, unknown>,
  catalog: Catalog,
  clipIds: Set<string>,
  reuse: boolean,
): string[] {
  const problems: string[] = [];
  const trackId = text(track.id);
  const trackKind = track.kind;
  const list = <T>(value: unknown, what: string): readonly T[] => {
    if (Array.isArray(value)) return value as readonly T[];
    if (value !== undefined) problems.push(`${what} must be an array`);
    return [];
  };
  const clips = list<Clip>(track.clips, `track ${trackId} clips`);
  const transitions = list<ClipTransition>(
    track.transitions,
    `track ${trackId} transitions`,
  );
  const captions = captionsProblem(track, clips, transitions, trackId);
  if (captions !== undefined) problems.push(captions);
  const overlap = uncoveredOverlap(trackId, trackKind, clips, transitions);
  if (overlap !== undefined) problems.push(overlap);
  for (const clip of clips) {
    if (!isRecord(clip)) {
      problems.push(`track ${trackId} has a clip that is not an object`);
      continue;
    }
    const clipId = text(clip.id);
    if (clipIds.has(clipId)) problems.push(`duplicate clip id ${clipId}`);
    clipIds.add(clipId);
    if (reuse) {
      const known = checkedClips.get(clip);
      if (
        known !== undefined &&
        known.kind === trackKind &&
        known.catalog === catalog
      )
        continue;
    }
    const found = clipProblems(clip, trackKind, catalog);
    for (const problem of found) problems.push(problem);
    if (reuse && found.length === 0 && immutable.has(clip))
      checkedClips.set(clip, { kind: trackKind, catalog });
  }
  if (!validTrackAudio(track.audio)) {
    problems.push(`track ${trackId} has invalid audio bus settings`);
  }
  for (const problem of transitionProblems(
    trackId,
    trackKind,
    clips,
    transitions,
  ))
    problems.push(problem);
  for (const transition of transitions) {
    if (!isRecord(transition) || !isRecord(transition.effect)) continue;
    const resources = transition.effect.resources;
    if (!isRecord(resources)) continue;
    for (const id of Object.values(resources)) {
      if (!catalog.effectResources.has(text(id))) {
        problems.push(
          `track ${trackId} transition refers to unknown effect resource ${text(id)}`,
        );
      }
    }
  }
  return problems;
}

function clipProblems(
  clip: Clip & Record<string, unknown>,
  trackKind: unknown,
  catalog: Catalog,
): string[] {
  const problems: string[] = [];
  const clipId = text(clip.id);
  if (clipId.length === 0) problems.push("clip id must not be empty");
  if (!validRange(clip.timeline_range)) {
    problems.push(`clip ${clipId} has an empty timeline_range`);
  }
  if (!validRange(clip.source_range)) {
    problems.push(`clip ${clipId} has an empty source_range`);
  }
  if (
    rangeStart(clip.timeline_range) < 0 ||
    rangeStart(clip.source_range) < 0
  ) {
    problems.push(`clip ${clipId} starts before the timeline origin`);
  }
  if (!freezesInside(clip.time_map, clip.source_range)) {
    problems.push(`clip ${clipId} freezes outside its source range`);
  }
  const sourceId = text(clip.source_id);
  const source = catalog.sources.get(sourceId);
  if (source === undefined || !isRecord(source)) {
    problems.push(`clip ${clipId} refers to unknown source ${sourceId}`);
  } else if ((trackKind === "audio") !== (source.kind === "audio")) {
    problems.push(`clip ${clipId} cannot sit on a ${text(trackKind)} track`);
  }
  if (!validVisual(clip.visual)) {
    problems.push(`clip ${clipId} has invalid visual properties`);
  }
  const reference = clip.composition;
  if (reference != null) {
    const scale = isRecord(reference.scale) ? reference.scale : undefined;
    if (
      trackKind !== "video" ||
      (scale !== undefined &&
        !(positiveInteger(scale.num) && positiveInteger(scale.den))) ||
      !compositionTimeHeld(clip, reference, scale)
    ) {
      problems.push(`clip ${clipId} has an invalid composition reference`);
    }
    if (!catalog.compositions.has(text(reference.id))) {
      problems.push(
        `clip ${clipId} refers to unknown composition ${text(reference.id)}`,
      );
    }
  }
  for (const problem of audioProblems(clipId, clip.audio))
    problems.push(problem);
  if (!validMaskCount(clip.masks, trackKind)) {
    problems.push(`clip ${clipId} has an invalid mask`);
  } else {
    const issue = masksIssue(clip.masks);
    if (issue) problems.push(shapeProblem(`clip ${clipId}`, issue));
  }
  if (clip.matte != null && trackKind !== "video") {
    problems.push(`clip ${clipId} has an invalid matte`);
  }
  return problems;
}

function duckingProblems(
  tracks: readonly unknown[],
  trackIds: ReadonlySet<string>,
): string[] {
  const problems: string[] = [];
  const cyclic = duckingCycles(tracks as readonly Track[]);
  for (const track of tracks) {
    if (!isRecord(track)) continue;
    const ducking = isRecord(track.audio) ? track.audio.ducking : undefined;
    if (ducking == null) continue;
    const trackId = text(track.id);
    if (!validDucking(ducking)) {
      problems.push(`track ${trackId} has invalid ducking settings`);
    }
    const sidechain = isRecord(ducking) ? text(ducking.sidechain_track) : "";
    if (sidechain === trackId || cyclic.has(trackId)) {
      problems.push(`track ${trackId} has an unusable ducking sidechain`);
    } else if (!trackIds.has(sidechain)) {
      problems.push(`track ${trackId} ducks from unknown track ${sidechain}`);
    }
  }
  return problems;
}

// Validation caches. Deep-frozen plain data cannot change, so an edit pays only for what it
// changed. Only passes are cached; problems are always rewalked.
const immutable = new WeakSet<object>();
const readShapes = new WeakMap<object, { node: ShapeNode; read: unknown }>();
const checkedClips = new WeakMap<object, { kind: unknown; catalog: Catalog }>();
const catalogs = new WeakMap<
  object,
  {
    resources: unknown;
    compositions: unknown;
    canvas: Canvas;
    catalog: Catalog;
  }
>();
const trackSummaries = new WeakMap<object, TrackSummary>();

// Deep-frozen plain arrays and objects with data properties only (a getter could answer
// differently next time). Results are memoized in `immutable`.
function sealed(value: unknown): boolean {
  if (typeof value !== "object" || value === null) return true;
  if (immutable.has(value)) return true;
  if (!Object.isFrozen(value)) return false;
  const array = Array.isArray(value);
  const prototype: unknown = Object.getPrototypeOf(value);
  if (
    array
      ? prototype !== Array.prototype
      : prototype !== Object.prototype && prototype !== null
  )
    return false;
  const keys = Reflect.ownKeys(value);
  if (
    array
      ? keys.length !== (value as unknown[]).length + 1
      : keys.length !== Object.keys(value).length
  )
    return false;
  for (const key of keys) {
    if (array && key === "length") continue;
    const descriptor = Object.getOwnPropertyDescriptor(value, key)!;
    if (!("value" in descriptor) || !sealed(descriptor.value)) return false;
  }
  immutable.add(value);
  return true;
}

function rememberShape(node: ShapeNode, value: object, read: unknown): void {
  if (!sealed(value)) return;
  if (read !== value) {
    if (typeof read !== "object" || read === null) return;
    Object.freeze(read);
    if (!sealed(read)) return;
  }
  readShapes.set(value, { node, read });
}

function catalogFor(
  sources: unknown,
  resources: unknown,
  compositions: unknown,
  canvas: Canvas,
): Catalog {
  const reusable =
    typeof sources === "object" &&
    sources !== null &&
    immutable.has(sources) &&
    sealed(resources) &&
    sealed(compositions);
  if (reusable) {
    const known = catalogs.get(sources);
    if (
      known !== undefined &&
      known.resources === resources &&
      known.compositions === compositions &&
      known.canvas[0] === canvas[0] &&
      known.canvas[1] === canvas[1]
    )
      return known.catalog;
  }
  const catalog = catalogOf(
    Array.isArray(sources) ? sources : [],
    Array.isArray(resources) ? resources : [],
    Array.isArray(compositions) ? compositions : [],
    canvas,
  );
  if (reusable && catalog.problems.length === 0)
    catalogs.set(sources, { resources, compositions, canvas, catalog });
  return catalog;
}

/** What the cross-track checks read of a track that passed `trackProblems`. */
interface TrackSummary {
  readonly catalog: Catalog;
  readonly kind: unknown;
  readonly clips: readonly Record<string, unknown>[];
  readonly ids: readonly string[];
  readonly mattes: readonly { readonly id: string; readonly source: string }[];
  readonly joined: ReadonlySet<string>;
  readonly frames: readonly FrameEdge[];
  readonly audio: TrackAudioLoad | undefined;
  // Built lazily when a matte is looked up.
  byId?: ReadonlyMap<string, Record<string, unknown>>;
}

function summaryOf(
  track: Record<string, unknown>,
  catalog: Catalog,
): TrackSummary | undefined {
  const known = trackSummaries.get(track);
  if (known?.catalog === catalog) return known;
  const ids = new Set<string>();
  if (trackProblems(track, catalog, ids, true).length > 0) return undefined;
  const clips = (Array.isArray(track.clips) ? track.clips : []) as Record<
    string,
    unknown
  >[];
  const video = track.kind === "video";
  const joined = new Set<string>();
  if (video && Array.isArray(track.transitions))
    for (const transition of track.transitions as unknown[]) {
      if (!isRecord(transition)) continue;
      joined.add(text(transition.from_clip));
      joined.add(text(transition.to_clip));
    }
  const summary: TrackSummary = {
    catalog,
    kind: track.kind,
    clips,
    ids: [...ids],
    mattes: video
      ? clips
          .filter((clip) => clip.matte != null)
          .map((clip) => ({
            id: text(clip.id),
            source: isRecord(clip.matte) ? text(clip.matte.source) : "",
          }))
      : [],
    joined,
    frames: frameEdges(track, catalog.sources, catalog.inlined!),
    audio: trackAudioLoad(track),
  };
  if (immutable.has(track)) trackSummaries.set(track, summary);
  return summary;
}

// Must agree exactly with `invariantProblems` finding none, reusing cached passes.
function invariantsHold(value: unknown): boolean {
  if (!isRecord(value)) return false;
  const project = value as unknown as Project;
  if (headerProblems(project).length > 0) return false;
  const tracks = project.timeline?.tracks;
  if (
    !Array.isArray(project.sources) ||
    !Array.isArray(tracks) ||
    (project.compositions !== undefined &&
      !Array.isArray(project.compositions)) ||
    (project.effect_resources !== undefined &&
      !Array.isArray(project.effect_resources))
  )
    return false;
  if (
    countProblems(project.sources, project.compositions ?? [], tracks).length >
    0
  )
    return false;
  const catalog = catalogFor(
    project.sources,
    project.effect_resources,
    project.compositions,
    canvasOf(project),
  );
  if (catalog.problems.length > 0) return false;
  const trackIds = new Set<string>();
  const summaries: TrackSummary[] = [];
  for (const track of tracks as unknown[]) {
    if (!isRecord(track)) return false;
    const trackId = text(track.id);
    if (trackId.length === 0 || trackIds.has(trackId)) return false;
    trackIds.add(trackId);
    const summary = summaryOf(track, catalog);
    if (summary === undefined) return false;
    summaries.push(summary);
  }
  return (
    clipIdsUnique(summaries) &&
    mattesHold(summaries) &&
    frameNodesOver(summaries.map((summary) => summary.frames)) === undefined &&
    audioEffectProblems(
      project.audio,
      summaries.map((summary) => summary.audio),
    ).length === 0 &&
    duckingProblems(tracks, trackIds).length === 0
  );
}

// The last set of tracks found to share no clip id, so a later call checks only new tracks.
// Successive editor projects share most tracks.
let distinctTracks:
  | {
      readonly members: ReadonlySet<TrackSummary>;
      readonly owners: ReadonlyMap<string, TrackSummary>;
    }
  | undefined;

function clipIdsUnique(summaries: readonly TrackSummary[]): boolean {
  let total = 0;
  let holding = 0;
  for (const summary of summaries) {
    total += summary.ids.length;
    if (summary.ids.length > 0) holding += 1;
  }
  if (holding <= 1) return true;
  const known = distinctTracks;
  const present = new Set(summaries);
  if (known !== undefined && present.size === summaries.length) {
    const added = summaries.filter((summary) => !known.members.has(summary));
    let count = 0;
    for (const summary of added) count += summary.ids.length;
    if (count * 2 <= total) {
      // A track's own ids are distinct, so only two or more new tracks can collide.
      const seen = added.length > 1 ? new Set<string>() : undefined;
      for (const summary of added)
        for (const id of summary.ids) {
          const owner = known.owners.get(id);
          if (owner !== undefined && present.has(owner)) return false;
          if (seen !== undefined) {
            if (seen.has(id)) return false;
            seen.add(id);
          }
        }
      return true;
    }
  }
  const owners = new Map<string, TrackSummary>();
  for (const summary of summaries)
    for (const id of summary.ids) {
      if (owners.has(id)) return false;
      owners.set(id, summary);
    }
  distinctTracks = { members: present, owners };
  return true;
}

// Must agree with `matteProblems` for a project whose clip ids are distinct.
function mattesHold(summaries: readonly TrackSummary[]): boolean {
  const video = summaries.filter((summary) => summary.kind === "video");
  for (const summary of video)
    for (const { id, source } of summary.mattes) {
      if (source === id) return false;
      let target: Record<string, unknown> | undefined;
      for (const other of video) {
        other.byId ??= new Map(
          other.clips.map((clip) => [text(clip.id), clip]),
        );
        target ??= other.byId.get(source);
      }
      if (
        target === undefined ||
        target.matte != null ||
        video.some((other) => other.joined.has(source))
      )
        return false;
    }
  return true;
}

// Nested lists are counted only when the top-level counts are within limits.
function countProblems(
  sources: readonly unknown[],
  compositions: readonly unknown[],
  tracks: readonly unknown[],
): string[] {
  const problems: string[] = [];
  if (sources.length > MAX_CLIPS)
    problems.push(`project must not contain more than ${MAX_CLIPS} sources`);
  if (tracks.length > MAX_TRACKS)
    problems.push(`project must not contain more than ${MAX_TRACKS} tracks`);
  if (compositions.length > MAX_COMPOSITIONS)
    problems.push(
      `project must not contain more than ${MAX_COMPOSITIONS} compositions`,
    );
  if (problems.length > 0) return problems;
  for (const composition of compositions)
    if (
      isRecord(composition) &&
      Array.isArray(composition.nodes) &&
      composition.nodes.length > MAX_COMPOSITION_NODES
    )
      problems.push(`composition ${text(composition.id)} has too many nodes`);
  let clips = 0;
  for (const track of tracks) {
    if (!isRecord(track)) continue;
    const trackId = text(track.id);
    const count = (value: unknown): number =>
      Array.isArray(value) ? value.length : 0;
    clips += count(track.clips);
    if (count(track.clips) > MAX_CLIPS_PER_TRACK)
      problems.push(`track ${trackId} has too many clips`);
    if (count(track.transitions) > MAX_TRANSITIONS_PER_TRACK)
      problems.push(`track ${trackId} has too many transitions`);
    const cues = count(isRecord(track.captions) ? track.captions.cues : 0);
    if (cues > MAX_CLIPS_PER_TRACK)
      problems.push(
        `track ${trackId} has ${cues} caption cues; the maximum is ${MAX_CLIPS_PER_TRACK}`,
      );
  }
  if (clips > MAX_CLIPS)
    problems.push(`project must not contain more than ${MAX_CLIPS} clips`);
  return problems;
}

/**
 * Like {@link validateProject}, but also reports what only the text shows: size over
 * {@link MAX_PROJECT_JSON_BYTES}, repeated fields, integers spelled with a fraction or exponent
 * (`1920.0`, `1.92e3`, `-0`), and nesting deeper than 127 levels. Hosts that pass the engine a
 * string should validate the string. Never throws; malformed JSON is reported.
 */
export function validateProjectJson(json: string): string[] {
  if (exceedsParseBudget(json)) {
    const bytes = new TextEncoder().encode(json).length;
    return [
      `project JSON is ${bytes} bytes, over the ${MAX_PROJECT_JSON_BYTES} byte limit`,
    ];
  }
  // `JSON.parse` supplies the error message; `readJsonText` assumes valid JSON.
  try {
    JSON.parse(json);
  } catch (error) {
    return [
      `project JSON is malformed: ${error instanceof Error ? error.message : String(error)}`,
    ];
  }
  const read = readJsonText(json);
  if (read === undefined) {
    return [`project JSON nests deeper than ${MAX_JSON_DEPTH} levels`];
  }
  return projectProblems(read.value as Project, read.spelling);
}

/**
 * Whether `rate` has positive 32-bit `num` and `den` and at most {@link TICKS_PER_SECOND} frames
 * per second.
 */
export function validFrameRate(rate: unknown): rate is FrameRate {
  return (
    isRecord(rate) &&
    positiveInteger(rate.num) &&
    positiveInteger(rate.den) &&
    rate.num <= MAX_U32 &&
    rate.den <= MAX_U32 &&
    rate.num <= TICKS_PER_SECOND * rate.den
  );
}

const MAX_U32 = 0xffff_ffff;

// The engine measures UTF-8 bytes. Each UTF-16 unit encodes to 1 to 3 bytes, so most documents
// are decided without encoding the whole string.
function exceedsParseBudget(json: string): boolean {
  if (json.length > MAX_PROJECT_JSON_BYTES) return true;
  if (json.length * 3 <= MAX_PROJECT_JSON_BYTES) return false;
  return new TextEncoder().encode(json).length > MAX_PROJECT_JSON_BYTES;
}

type ShapeNode = Readonly<Record<string, unknown>>;

const shapeRoot = PROJECT_SHAPE as ShapeNode;

/**
 * Walks the generated schema and returns shape problems plus the document as the engine reads it,
 * with `format: "float"` numbers rounded to 32 bits. Reports at most one problem per value. The
 * walk must cover exactly the keywords the generator emits.
 */
function shapeProblems(
  value: unknown,
  spelling: JsonSpelling | undefined,
  reuse: boolean,
): { problems: string[]; read: unknown } {
  const problems: string[] = [];
  reusingShapes = reuse;
  try {
    const read = checkShape(shapeRoot, value, "project", problems, spelling);
    return { problems, read };
  } finally {
    reusingShapes = false;
  }
}

let reusingShapes = false;

// Cross-field rules the schema cannot state but the engine's parser enforces.
const FRAME_RATE_SHAPE = (shapeRoot.$defs as Record<string, ShapeNode>)
  .FrameRate;
const TIME_RANGE_SHAPE = (shapeRoot.$defs as Record<string, ShapeNode>)
  .TimeRange;

function resolveShape(node: ShapeNode): ShapeNode {
  let current = node;
  while (typeof current.$ref === "string") {
    const name = current.$ref.slice("#/$defs/".length);
    current = (shapeRoot.$defs as Record<string, ShapeNode>)[name];
  }
  return current;
}

function describeValue(value: unknown): string {
  return JSON.stringify(value) ?? String(value);
}

// Ranges of the Rust integer types behind each schema `format`.
const INTEGER_FORMATS: Readonly<
  Record<string, { readonly min: bigint; readonly max: bigint }>
> = {
  uint16: { min: 0n, max: 2n ** 16n - 1n },
  uint32: { min: 0n, max: 2n ** 32n - 1n },
  uint64: { min: 0n, max: 2n ** 64n - 1n },
  int64: { min: -(2n ** 63n), max: 2n ** 63n - 1n },
};

/**
 * Checks `value` against `node` and returns it as the engine reads it. `texts` are the source
 * spellings of a number its value does not round-trip (`JsonSpelling.numbers`).
 */
function checkShape(
  node: ShapeNode,
  value: unknown,
  path: string,
  problems: string[],
  spelling: JsonSpelling | undefined,
  texts?: readonly string[],
): unknown {
  if (!reusingShapes || typeof value !== "object" || value === null)
    return walkShape(node, value, path, problems, spelling, texts);
  const known = readShapes.get(value);
  if (known?.node === node) return known.read;
  const before = problems.length;
  const read = walkShape(node, value, path, problems, spelling, texts);
  if (problems.length === before && Object.isFrozen(value))
    rememberShape(node, value, read);
  return read;
}

// Defs with CSS-style or alternative spellings are normalized to one form, with problems worded
// as `alpha2_model::layout_values` words them.
const SHAPE_DEFS = shapeRoot.$defs as Record<string, ShapeNode>;

type ShapeReader = (
  value: unknown,
  path: string,
  problems: string[],
  spelling: JsonSpelling | undefined,
) => unknown;

function parsedOr<T>(
  parsed: Parsed<T>,
  path: string,
  problems: string[],
  value: unknown,
): unknown {
  if (parsed.ok) return parsed.value;
  problems.push(`${path}: ${parsed.error}`);
  return value;
}

// A number is a fraction; a string is `"<n>px"` or, with `percent`, `"<n>%"`.
function readLayoutLength(percent: boolean): ShapeReader {
  return (value, path, problems) => {
    if (typeof value === "number")
      return { unit: "fraction", value: Math.fround(value) };
    if (typeof value === "string")
      return parsedOr(
        percent ? parseLength(value) : parseSize(value),
        path,
        problems,
        value,
      );
    problems.push(`${path} must be number or string`);
    return value;
  };
}

function readFields(
  value: Record<string, unknown>,
  path: string,
  problems: string[],
  fields: readonly string[],
  required: boolean,
  read: ShapeReader,
  spelling: JsonSpelling | undefined,
): Record<string, unknown> {
  for (const key of Object.keys(value)) {
    if (!fields.includes(key))
      problems.push(`${path} has unknown field ${describeValue(key)}`);
  }
  const out: Record<string, unknown> = {};
  for (const field of fields) {
    if (value[field] === undefined) {
      if (required) problems.push(`${path}.${field} is required`);
      else out[field] = { unit: "fraction", value: 0 };
      continue;
    }
    out[field] = read(value[field], `${path}.${field}`, problems, spelling);
  }
  return out;
}

const readSize = readLayoutLength(false);
const SIDES = ["top", "right", "bottom", "left"] as const;

const SHAPE_READERS = new Map<ShapeNode, ShapeReader>([
  [
    SHAPE_DEFS.Rgba,
    (value, path, problems, spelling) => {
      if (typeof value === "string")
        return parsedOr(parseHex(value), path, problems, value);
      const object = (SHAPE_DEFS.Rgba.anyOf as ShapeNode[])[0];
      return checkShape(object, value, path, problems, spelling);
    },
  ],
  [
    SHAPE_DEFS.Paint,
    (value, path, problems, spelling) => {
      // An untagged color paints solid.
      if (
        typeof value === "string" ||
        (isRecord(value) && !("type" in value))
      ) {
        const color = checkShape(
          SHAPE_DEFS.Rgba,
          value,
          path,
          problems,
          spelling,
        );
        return { type: "solid", color };
      }
      const tagged = (SHAPE_DEFS.Paint.oneOf as ShapeNode[]).filter(
        (variant) => variant.$ref === undefined,
      );
      return checkVariants(tagged, value, path, problems, spelling, undefined);
    },
  ],
  [SHAPE_DEFS.LayoutLength, readLayoutLength(true)],
  [SHAPE_DEFS.LayoutSize, readSize],
  [
    SHAPE_DEFS.Insets,
    (value, path, problems, spelling) => {
      const sides = (lengths: readonly unknown[]) =>
        Object.fromEntries(SIDES.map((side, index) => [side, lengths[index]]));
      if (typeof value === "number")
        return sides(
          Array(4).fill({ unit: "fraction", value: Math.fround(value) }),
        );
      if (typeof value === "string") {
        const parsed = parseInsets(value);
        return parsed.ok
          ? sides(parsed.value)
          : parsedOr(parsed, path, problems, value);
      }
      if (isRecord(value))
        return readFields(
          value,
          path,
          problems,
          SIDES,
          false,
          readSize,
          spelling,
        );
      problems.push(`${path} must be number, string, or object`);
      return value;
    },
  ],
  [
    SHAPE_DEFS.LayoutOffset,
    (value, path, problems, spelling) => {
      if (typeof value === "string")
        return parsedOr(parseOffset(value), path, problems, value);
      if (isRecord(value))
        return readFields(
          value,
          path,
          problems,
          ["x", "y"],
          true,
          readSize,
          spelling,
        );
      problems.push(`${path} must be string or object`);
      return value;
    },
  ],
  [
    SHAPE_DEFS.TrackList,
    (value, path, problems) => {
      if (typeof value === "string")
        return parsedOr(parseTrackList(value), path, problems, value);
      problems.push(`${path} must be string`);
      return value;
    },
  ],
  [
    SHAPE_DEFS.GridPlacement,
    (value, path, problems) => {
      if (typeof value === "number" || typeof value === "string")
        return parsedOr(parsePlacement(value), path, problems, value);
      problems.push(`${path} must be integer or string`);
      return value;
    },
  ],
]);

/** A color as the engine writes it: the object form, never hex. */
export type RgbaObject = Exclude<Rgba, string>;

/** A paint as the engine writes it: tagged, its colors objects. */
export type CanonicalPaint = CanonicalPart<Exclude<Paint, Rgba>>;

/** Padding or margin as the engine writes it, with all four sides. */
export interface CanonicalInsets {
  readonly top: LayoutSize;
  readonly right: LayoutSize;
  readonly bottom: LayoutSize;
  readonly left: LayoutSize;
}

/** A layout node's `translate` as the engine writes it. */
export type CanonicalOffset = Exclude<LayoutOffset, string>;

// Type identity; mutual assignability would match unrelated all-optional object types.
type Same<A, B> =
  (<G>() => G extends A ? 1 : 2) extends <G>() => G extends B ? 1 : 2
    ? true
    : false;

/**
 * `T` in the forms the engine writes, as {@link canonicalProject} returns them. Lengths and grid
 * values keep their CSS spelling.
 */
export type Canonical<T> =
  CanonicalValue<Exclude<T, null | undefined>> | Extract<T, null | undefined>;

type CanonicalValue<T> = [T] extends [never]
  ? never
  : Same<T, Rgba> extends true
    ? RgbaObject
    : Same<T, Paint> extends true
      ? CanonicalPaint
      : Same<T, Insets> extends true
        ? CanonicalInsets
        : Same<T, LayoutOffset> extends true
          ? CanonicalOffset
          : Same<T, VisualProperties> extends true
            ? CanonicalPart<Required<VisualProperties>>
            : CanonicalPart<T>;

type CanonicalPart<T> = T extends string | number | boolean | bigint
  ? T
  : { readonly [K in keyof T]: Canonical<T[K]> };

export type CanonicalProject = Canonical<Project>;

// The shortest decimal that reads back as the same 32-bit float, as the engine writes it.
function shortestFloat32(value: number): number {
  const target = Math.fround(value);
  for (let digits = 1; digits < 9; digits++) {
    const short = Number(target.toPrecision(digits));
    if (Math.fround(short) === target) return short;
  }
  return Number(target.toPrecision(9));
}

/** The whole text of a caption cue or text generator, with runs joined. */
export function textOf(content: TextContent | Canonical<TextContent>): string {
  return typeof content === "string"
    ? content
    : content.map((run: { readonly text: string }) => run.text).join("");
}

/**
 * Converts a CSS hex color to the object form, with 32-bit components spelled shortest.
 *
 * @throws A `TypeError` for a string that is not CSS hex.
 */
export function canonicalRgba(color: Rgba): RgbaObject {
  if (typeof color !== "string") return color;
  const parsed = parseHex(color);
  if (!parsed.ok) throw new TypeError(parsed.error);
  const { red, green, blue, alpha } = parsed.value;
  return {
    red: shortestFloat32(red),
    green: shortestFloat32(green),
    blue: shortestFloat32(blue),
    alpha: shortestFloat32(alpha),
  };
}

/**
 * Converts a paint to the tagged form the engine writes; a bare color becomes a solid paint.
 *
 * @throws A `TypeError` for a color string that is not CSS hex.
 */
export function canonicalPaint(paint: Paint): CanonicalPaint {
  if (typeof paint === "string" || !("type" in paint)) {
    return { type: "solid", color: canonicalRgba(paint) };
  }
  if (paint.type === "solid") {
    return { ...paint, color: canonicalRgba(paint.color) };
  }
  return {
    ...paint,
    stops: paint.stops.map((stop) => ({
      ...stop,
      color: canonicalRgba(stop.color),
    })),
  };
}

function lengthWritten(length: Length): LayoutSize {
  const value = shortestFloat32(length.value);
  return length.unit === "px" ? `${value}px` : value;
}

// The readers below leave unreadable values for validation to report, and return their input
// itself when nothing changes so unchanged parts are shared.
function rgbaRead(color: unknown): unknown {
  if (typeof color !== "string") return color;
  return parseHex(color).ok ? canonicalRgba(color) : color;
}

function paintRead(paint: unknown): unknown {
  if (typeof paint === "string" || (isRecord(paint) && !("type" in paint))) {
    const color = rgbaRead(paint);
    return isRecord(color) ? { type: "solid", color } : paint;
  }
  if (!isRecord(paint)) return paint;
  if (paint.type === "solid") {
    const color = rgbaRead(paint.color);
    return color === paint.color ? paint : { ...paint, color };
  }
  if (!Array.isArray(paint.stops)) return paint;
  const stops = paint.stops as readonly unknown[];
  const read = stops.map((stop) => {
    if (!isRecord(stop)) return stop;
    const color = rgbaRead(stop.color);
    return color === stop.color ? stop : { ...stop, color };
  });
  return read.every((stop, index) => stop === stops[index])
    ? paint
    : { ...paint, stops: read };
}

const SIDE_NAMES = ["top", "right", "bottom", "left"] as const;

function insetsRead(value: unknown): unknown {
  if (typeof value === "number") {
    return { top: value, right: value, bottom: value, left: value };
  }
  if (typeof value === "string") {
    const parsed = parseInsets(value);
    if (!parsed.ok) return value;
    const [top, right, bottom, left] = parsed.value.map(lengthWritten);
    return { top, right, bottom, left };
  }
  if (!isRecord(value) || SIDE_NAMES.every((side) => value[side] !== undefined))
    return value;
  return Object.fromEntries([
    ...Object.entries(value),
    ...SIDE_NAMES.map((side) => [side, value[side] ?? 0]),
  ]);
}

function offsetRead(value: unknown): unknown {
  if (typeof value !== "string") return value;
  const parsed = parseOffset(value);
  return parsed.ok
    ? { x: lengthWritten(parsed.value.x), y: lengthWritten(parsed.value.y) }
    : value;
}

/**
 * Returns `project` in the forms the engine writes ({@link Canonical}): object colors, tagged
 * paints, four-side insets and `{ x, y }` offsets. Unreadable values are left for
 * {@link validateProject} to report, so pass a valid document to get a fully canonical one.
 *
 * The input is not changed. Already-canonical parts are returned as the same objects, and frozen
 * parts seen before are not walked again.
 */
export function canonicalProject(project: Project): CanonicalProject {
  const reusing = reusingShapes;
  reusingShapes = true;
  try {
    return canonicalAt(shapeRoot, project) as CanonicalProject;
  } finally {
    reusingShapes = reusing;
  }
}

const canonicalShapes = new WeakMap<
  object,
  { node: ShapeNode; canonical: unknown }
>();

// The defs that are canonicalized, plus every def that can contain one.
const CANONICAL_DEFS: ReadonlySet<string> = (() => {
  const holding = new Set([
    "Rgba",
    "Paint",
    "Insets",
    "LayoutOffset",
    "VisualProperties",
  ]);
  const refs = Object.entries(SHAPE_DEFS).map(
    ([name, def]) => [name, refsIn(def)] as const,
  );
  for (let grew = true; grew;) {
    grew = false;
    for (const [name, names] of refs)
      if (!holding.has(name) && names.some((ref) => holding.has(ref))) {
        holding.add(name);
        grew = true;
      }
  }
  return holding;
})();

function refsIn(node: unknown): string[] {
  return [
    ...JSON.stringify(node).matchAll(/"\$ref":"#\/\$defs\/([^"]+)"/g),
  ].map((match) => match[1]);
}

const holdingNodes = new WeakMap<ShapeNode, boolean>();

// Fields that cannot hold a canonicalized value are skipped.
function mayHold(node: ShapeNode): boolean {
  let holds = holdingNodes.get(node);
  if (holds === undefined) {
    holds = refsIn(node).some((ref) => CANONICAL_DEFS.has(ref));
    holdingNodes.set(node, holds);
  }
  return holds;
}

function canonicalAt(node: ShapeNode | boolean, value: unknown): unknown {
  if (typeof node !== "object" || !mayHold(node)) return value;
  if (typeof value !== "object" || value === null)
    return canonicalLeaf(node, value);
  const known = canonicalShapes.get(value);
  if (known?.node === node) return known.canonical;
  const canonical = canonicalWalk(node, value);
  if (Object.isFrozen(value)) canonicalShapes.set(value, { node, canonical });
  return canonical;
}

function canonicalLeaf(node: ShapeNode | boolean, value: unknown): unknown {
  if (typeof node !== "object" || value === null || value === undefined)
    return value;
  return canonicalWalk(node, value);
}

function canonicalWalk(node: ShapeNode, value: unknown): unknown {
  const schema = resolveShape(node);
  if (schema === SHAPE_DEFS.Rgba) return rgbaRead(value);
  if (schema === SHAPE_DEFS.Paint) return paintRead(value);
  if (schema === SHAPE_DEFS.Insets) return insetsRead(value);
  if (schema === SHAPE_DEFS.LayoutOffset) return offsetRead(value);
  const variants = (schema.oneOf ?? schema.anyOf) as ShapeNode[] | undefined;
  if (variants !== undefined) {
    // Pick the first form that fits, as the parser does.
    const fits = variants.find((variant) => {
      const found: string[] = [];
      checkShape(variant, value, "", found, undefined);
      return found.length === 0;
    });
    return fits === undefined ? value : canonicalAt(fits, value);
  }
  if (typeof value !== "object" || value === null) return value;
  if (Array.isArray(value)) {
    const items = value as readonly unknown[];
    const prefix = schema.prefixItems as ShapeNode[] | undefined;
    const rest = (schema.items ?? true) as ShapeNode | boolean;
    const read = items.map((item, index) =>
      canonicalAt(
        prefix !== undefined && index < prefix.length ? prefix[index] : rest,
        item,
      ),
    );
    return read.every((item, index) => item === items[index]) ? value : read;
  }
  if (!isRecord(value)) return value;
  const properties = (schema.properties ?? {}) as Record<string, ShapeNode>;
  const extra = (schema.additionalProperties ?? true) as ShapeNode | boolean;
  let read: Record<string, unknown> | undefined;
  for (const key of Object.keys(value)) {
    const field = Object.hasOwn(properties, key) ? properties[key] : extra;
    if (typeof field !== "object" || !mayHold(field)) continue;
    const before = value[key];
    const canonical = canonicalAt(field, before);
    if (canonical !== before) (read ??= {})[key] = canonical;
  }
  // The engine writes a clip's visual whole, filling omitted fields with schema defaults.
  if (schema === SHAPE_DEFS.VisualProperties) {
    for (const [key, field] of Object.entries(properties)) {
      if (value[key] === undefined && field.default !== undefined)
        (read ??= {})[key] = structuredClone(field.default);
    }
  }
  return read === undefined ? value : { ...value, ...read };
}

function walkShape(
  node: ShapeNode,
  value: unknown,
  path: string,
  problems: string[],
  spelling: JsonSpelling | undefined,
  texts?: readonly string[],
): unknown {
  const schema = resolveShape(node);
  const reader = SHAPE_READERS.get(schema);
  if (reader !== undefined) return reader(value, path, problems, spelling);
  const variants = (schema.oneOf ?? schema.anyOf) as ShapeNode[] | undefined;
  if (variants !== undefined) {
    return checkVariants(variants, value, path, problems, spelling, texts);
  }
  if ("const" in schema && value !== schema.const) {
    problems.push(`${path} must be ${describeValue(schema.const)}`);
    return value;
  }
  if (Array.isArray(schema.enum) && !schema.enum.includes(value)) {
    problems.push(
      `${path} must be one of ${schema.enum.map(describeValue).join(", ")}`,
    );
    return value;
  }
  const types = Array.isArray(schema.type) ? schema.type : [schema.type];
  if (schema.type !== undefined && !types.some((type) => isType(type, value))) {
    problems.push(`${path} must be ${types.join(" or ")}`);
    return value;
  }
  if (
    typeof value === "number" &&
    types.includes("integer") &&
    !types.includes("number")
  ) {
    checkInteger(schema, value, texts, path, problems);
    return value;
  }
  if (typeof value === "number") {
    const read = schema.format === "float" ? Math.fround(value) : value;
    if (typeof schema.minimum === "number" && read < schema.minimum) {
      problems.push(`${path} must be at least ${schema.minimum}`);
    }
    if (typeof schema.maximum === "number" && read > schema.maximum) {
      problems.push(`${path} must be at most ${schema.maximum}`);
    }
    return read;
  } else if (typeof value === "string") {
    if (
      typeof schema.minLength === "number" &&
      value.length < schema.minLength
    ) {
      problems.push(`${path} must not be empty`);
    }
    if (
      typeof schema.pattern === "string" &&
      !new RegExp(schema.pattern, "u").test(value)
    ) {
      problems.push(`${path} must match ${schema.pattern}`);
    }
    if (hasLoneSurrogate(value)) {
      problems.push(`${path} must be well-formed Unicode`);
    }
  } else if (Array.isArray(value)) {
    return checkArrayShape(schema, value, path, problems, spelling);
  } else if (isRecord(value)) {
    const before = problems.length;
    const read = checkObjectShape(schema, value, path, problems, spelling);
    if (
      schema === FRAME_RATE_SHAPE &&
      problems.length === before &&
      !validFrameRate(value)
    ) {
      problems.push(
        `${path} must be at most ${TICKS_PER_SECOND} frames per second, one frame per tick`,
      );
    }
    if (
      schema === TIME_RANGE_SHAPE &&
      problems.length === before &&
      isRecord(read) &&
      (read.end as number) < (read.start as number)
    ) {
      problems.push(`${path}.end must not precede its start`);
    }
    return read;
  }
  return value;
}

interface IntegerBounds {
  readonly min?: bigint;
  readonly max?: bigint;
}
const integerBoundsByShape = new WeakMap<ShapeNode, IntegerBounds>();

function integerBounds(schema: ShapeNode): IntegerBounds {
  let bounds = integerBoundsByShape.get(schema);
  if (bounds) return bounds;
  const range = INTEGER_FORMATS[schema.format as string];
  let min = range?.min;
  let max = range?.max;
  if (typeof schema.minimum === "number") {
    const bound = BigInt(Math.ceil(schema.minimum));
    if (min === undefined || bound > min) min = bound;
  }
  if (typeof schema.maximum === "number") {
    const bound = BigInt(Math.floor(schema.maximum));
    if (max === undefined || bound < max) max = bound;
  }
  bounds = { min, max };
  integerBoundsByShape.set(schema, bounds);
  return bounds;
}

// The engine refuses (never rounds) integers spelled with a fraction, exponent or `-0`, and those
// outside the schema bounds or `format` range.
function checkInteger(
  schema: ShapeNode,
  value: number,
  texts: readonly string[] | undefined,
  path: string,
  problems: string[],
): void {
  const { min, max } = integerBounds(schema);
  // A safe integer's value decides; otherwise the spelling the engine would read decides.
  if (texts === undefined && Number.isSafeInteger(value)) {
    if (min !== undefined && value < Number(min)) {
      problems.push(`${path} must be at least ${min}`);
    } else if (max !== undefined && value > Number(max)) {
      problems.push(`${path} must be at most ${max}`);
    }
    return;
  }
  for (const text of texts ?? [JSON.stringify(value)]) {
    if (/[.eE]/.test(text) || text === "-0") {
      problems.push(
        `${path} must be an integer written without a fraction or exponent`,
      );
      return;
    }
    const integer = BigInt(text);
    if (min !== undefined && integer < min) {
      problems.push(`${path} must be at least ${min}`);
      return;
    }
    if (max !== undefined && integer > max) {
      problems.push(`${path} must be at most ${max}`);
      return;
    }
  }
}

function isType(type: unknown, value: unknown): boolean {
  switch (type) {
    case "null":
      return value === null;
    case "boolean":
      return typeof value === "boolean";
    case "string":
      return typeof value === "string";
    case "integer":
      return Number.isInteger(value);
    case "number":
      return finiteNumber(value);
    case "array":
      return Array.isArray(value);
    case "object":
      return isRecord(value);
    default:
      return false;
  }
}

function checkVariants(
  variants: readonly ShapeNode[],
  value: unknown,
  path: string,
  problems: string[],
  spelling: JsonSpelling | undefined,
  texts: readonly string[] | undefined,
): unknown {
  // The first fitting form wins. When none fits, report against the form the value meant.
  const tried: string[][] = [];
  for (const variant of variants) {
    const found: string[] = [];
    const read = checkShape(variant, value, path, found, spelling, texts);
    if (found.length === 0) return read;
    tried.push(found);
  }
  const resolved = variants.map(resolveShape);
  if (resolved.every((variant) => "const" in variant)) {
    const values = resolved.map((variant) => describeValue(variant.const));
    problems.push(`${path} must be one of ${values.join(", ")}`);
    return value;
  }
  const present = resolved.flatMap((variant, index) =>
    variant.type === "null" ? [] : [index],
  );
  if (value !== null && present.length === 1) {
    for (const problem of tried[present[0]]) problems.push(problem);
    return value;
  }
  const tag = tagName(resolved);
  if (tag !== undefined && isRecord(value)) {
    const index = resolved.findIndex(
      (variant) =>
        (variant.properties as Record<string, ShapeNode>)[tag].const ===
        value[tag],
    );
    if (index >= 0) {
      for (const problem of tried[index]) problems.push(problem);
      return value;
    }
    const tags = resolved.map((variant) =>
      describeValue(
        (variant.properties as Record<string, ShapeNode>)[tag].const,
      ),
    );
    problems.push(`${path}.${tag} must be one of ${tags.join(", ")}`);
    return value;
  }
  problems.push(`${path} matches none of its allowed forms`);
  return value;
}

function tagName(variants: readonly ShapeNode[]): string | undefined {
  const first = variants[0]?.properties;
  if (!isRecord(first)) return undefined;
  return Object.keys(first).find((name) =>
    variants.every((variant) => {
      const properties = variant.properties;
      return (
        isRecord(properties) &&
        isRecord(properties[name]) &&
        "const" in properties[name]
      );
    }),
  );
}

function checkArrayShape(
  schema: ShapeNode,
  value: readonly unknown[],
  path: string,
  problems: string[],
  spelling: JsonSpelling | undefined,
): unknown[] {
  const numbers = spelling?.numbers.get(value);
  if (typeof schema.minItems === "number" && value.length < schema.minItems) {
    problems.push(`${path} must have at least ${schema.minItems} items`);
  }
  if (typeof schema.maxItems === "number" && value.length > schema.maxItems) {
    problems.push(`${path} must have at most ${schema.maxItems} items`);
  }
  const prefix = (schema.prefixItems ?? []) as readonly ShapeNode[];
  let changed = false;
  const read = value.map((item, index) => {
    const itemSchema = prefix[index] ?? (schema.items as ShapeNode | undefined);
    if (itemSchema === undefined) return item;
    const itemRead = checkShape(
      itemSchema,
      item,
      `${path}[${index}]`,
      problems,
      spelling,
      numbers?.get(index),
    );
    if (itemRead !== item) changed = true;
    return itemRead;
  });
  return !changed && reusingShapes && sealed(value)
    ? (value as unknown[])
    : read;
}

function checkObjectShape(
  schema: ShapeNode,
  value: Readonly<Record<string, unknown>>,
  path: string,
  problems: string[],
  spelling: JsonSpelling | undefined,
): Record<string, unknown> {
  const properties = (schema.properties ?? {}) as Record<string, ShapeNode>;
  // A map (`additionalProperties`) keeps the last repeated key, as `JSON.parse` does.
  for (const name of spelling?.repeated.get(value) ?? []) {
    if (Object.hasOwn(properties, name)) {
      problems.push(`${path} has duplicate field ${describeValue(name)}`);
    }
  }
  const numbers = spelling?.numbers.get(value);
  // `JSON.stringify` drops `undefined`, so it counts as omitted.
  for (const name of (schema.required ?? []) as readonly string[]) {
    if (value[name] === undefined) problems.push(`${path}.${name} is required`);
  }
  const extra = schema.additionalProperties;
  const read: Record<string, unknown> = {};
  // Assigning `__proto__` would replace the copy's prototype instead of adding the key.
  const keep = (name: string, item: unknown) => {
    if (name === "__proto__") {
      Object.defineProperty(read, name, {
        value: item,
        enumerable: true,
        writable: true,
        configurable: true,
      });
    } else {
      read[name] = item;
    }
  };
  let changed = false;
  for (const [name, item] of Object.entries(value)) {
    if (item === undefined) {
      changed = true;
      continue;
    }
    keep(name, item);
    const property = Object.hasOwn(properties, name)
      ? properties[name]
      : undefined;
    const texts = numbers?.get(name);
    if (property !== undefined) {
      const itemRead = checkShape(
        property,
        item,
        `${path}.${name}`,
        problems,
        spelling,
        texts,
      );
      if (itemRead !== item) changed = true;
      keep(name, itemRead);
    } else if (extra === false) {
      problems.push(`${path} has unknown field ${describeValue(name)}`);
    } else if (isRecord(extra)) {
      if (hasLoneSurrogate(name)) {
        problems.push(`${path} has a key that is not well-formed Unicode`);
      }
      const itemRead = checkShape(
        extra,
        item,
        `${path}.${name}`,
        problems,
        spelling,
        texts,
      );
      if (itemRead !== item) changed = true;
      keep(name, itemRead);
    }
  }
  return !changed && reusingShapes && sealed(value) ? value : read;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function finiteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function safeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value);
}

// `fallback` when absent or `null`, `undefined` when mistyped.
function optionalFinite(value: unknown, fallback: number): number | undefined {
  if (value === undefined || value === null) return fallback;
  return finiteNumber(value) ? value : undefined;
}

// The first video overlap no transition covers, as the engine reports it. Audio tracks may
// overlap freely because the mixer sums them.
function uncoveredOverlap(
  trackId: string,
  trackKind: unknown,
  clips: readonly Clip[],
  transitions: readonly ClipTransition[],
): string | undefined {
  if (trackKind !== "video") return undefined;
  const transitionKeys = new Set<string>();
  const directed = new Set<string>();
  for (const transition of transitions) {
    if (!isRecord(transition) || !validRange(transition.timeline_range))
      continue;
    const range = transition.timeline_range;
    const from = text(transition.from_clip);
    const to = text(transition.to_clip);
    transitionKeys.add(transitionKey(from, to, range.start, range.end));
    transitionKeys.add(transitionKey(to, from, range.start, range.end));
    directed.add(`${from}\u0000${to}`);
  }
  const byTime = (a: Clip, b: Clip) =>
    a.timeline_range.start - b.timeline_range.start ||
    a.timeline_range.end - b.timeline_range.end;
  const ordered = clips.filter(
    (clip): clip is Clip => isRecord(clip) && validRange(clip.timeline_range),
  );
  if (ordered.some((clip, at) => at > 0 && byTime(ordered[at - 1], clip) > 0))
    ordered.sort(byTime);
  const active: Clip[] = [];
  for (const b of ordered) {
    // Keep only clips still active when `b` starts, in place.
    let kept = 0;
    for (const a of active)
      if (a.timeline_range.end > b.timeline_range.start) active[kept++] = a;
    active.length = kept;
    for (const a of active) {
      const start = Math.max(a.timeline_range.start, b.timeline_range.start);
      const end = Math.min(a.timeline_range.end, b.timeline_range.end);
      if (end <= start) continue;
      if (
        !transitionKeys.has(transitionKey(text(a.id), text(b.id), start, end))
      ) {
        return `clips ${text(a.id)} and ${text(b.id)} overlap without a transition`;
      }
    }
    active.push(b);
    // Transitions among simultaneously active clips must not form a cycle. A tournament is
    // acyclic exactly when its out-degrees are 0, 1, ..., k - 1.
    if (active.length >= 3) {
      const leads = active
        .map(
          (from) =>
            active.filter((to) =>
              directed.has(`${text(from.id)}\u0000${text(to.id)}`),
            ).length,
        )
        .sort((x, y) => x - y);
      if (leads.some((lead, rank) => lead !== rank))
        return `transitions on track ${trackId} active at ${b.timeline_range.start} ticks lead from clip to clip in a circle`;
    }
  }
  return undefined;
}

function transitionKey(
  from: string,
  to: string,
  start: number,
  end: number,
): string {
  return JSON.stringify([from, to, start, end]);
}

function audioProblems(clipId: string, audio: unknown): string[] {
  if (audio === undefined) return [];
  if (!isRecord(audio)) return [`clip ${clipId} has invalid audio properties`];
  const problems: string[] = [];
  const range = (
    animated: unknown,
    low: number,
    high: number,
    name: string,
  ): void => {
    if (animated === undefined) return;
    const values = animationValues<unknown>(animated);
    if (values === undefined) {
      problems.push(`clip ${clipId} has invalid ${name} keyframes`);
      return;
    }
    if (
      values.some(
        (value) => !finiteNumber(value) || value < low || value > high,
      )
    ) {
      problems.push(`clip ${clipId} has ${name} outside ${low}..${high}`);
    }
  };
  range(audio.gain, 0, MAX_GAIN, "gain");
  range(audio.pan, -1, 1, "pan");
  const fadeIn = optionalFinite(audio.fade_in, 0);
  const fadeOut = optionalFinite(audio.fade_out, 0);
  if (
    fadeIn === undefined ||
    fadeIn < 0 ||
    fadeOut === undefined ||
    fadeOut < 0
  ) {
    problems.push(`clip ${clipId} has a negative fade`);
  }
  if (!validEffects(audio.effects)) {
    problems.push(`clip ${clipId} has invalid audio effects`);
  }
  if (audio.channels != null && !validChannelSelection(audio.channels)) {
    problems.push(`clip ${clipId} has invalid audio properties`);
  }
  return problems;
}

/** Mirrors `valid_channel_selection`: one or two distinct stream channels, each in range. */
function validChannelSelection(channels: unknown): boolean {
  return (
    Array.isArray(channels) &&
    channels.length >= 1 &&
    channels.length <= MAX_SELECTED_CHANNELS &&
    channels.every(
      (channel) =>
        Number.isInteger(channel) &&
        (channel as number) >= 0 &&
        (channel as number) < MAX_AUDIO_CHANNELS,
    ) &&
    channels[0] !== channels[1]
  );
}

// Composition time is largest at the clip's last instant: `offset + floor((end - 1 - start) *
// num / den)`, which must fit a safe integer.
function compositionTimeHeld(
  clip: Record<string, unknown>,
  reference: { readonly offset?: unknown },
  scale: Record<string, unknown> | undefined,
): boolean {
  const range = clip.timeline_range as { start: number; end: number };
  const num = scale === undefined ? 1 : scale.num;
  const den = scale === undefined ? 1 : scale.den;
  const offset = reference.offset ?? 0;
  if (
    !validRange(range) ||
    !positiveInteger(num) ||
    !positiveInteger(den) ||
    !Number.isSafeInteger(offset)
  )
    return true;
  // The range is non-empty, so BigInt division here rounds down.
  const elapsed = BigInt(range.end) - 1n - BigInt(range.start);
  const local =
    BigInt(offset as number) + (elapsed * BigInt(num)) / BigInt(den);
  return local <= BigInt(Number.MAX_SAFE_INTEGER);
}

function positiveInteger(value: unknown): value is number {
  return safeInteger(value) && value > 0;
}

function validRange(range: unknown): boolean {
  return (
    isRecord(range) &&
    safeInteger(range.start) &&
    safeInteger(range.end) &&
    range.end > range.start
  );
}

function freezesInside(timeMap: unknown, sourceRange: unknown): boolean {
  if (!isRecord(timeMap) || timeMap.type !== "freeze") return true;
  const instant = timeMap.source_time;
  return (
    isRecord(sourceRange) &&
    safeInteger(instant) &&
    safeInteger(sourceRange.start) &&
    safeInteger(sourceRange.end) &&
    instant >= sourceRange.start &&
    instant < sourceRange.end
  );
}

function rangeStart(range: unknown): number {
  return isRecord(range) && finiteNumber(range.start) ? range.start : 0;
}

function animationValues<T>(animation: unknown): readonly T[] | undefined {
  if (!isRecord(animation)) return undefined;
  if (animation.mode === "constant") return [animation.value as T];
  if (animation.mode !== "keyframes" || !Array.isArray(animation.keyframes)) {
    return undefined;
  }
  const keyframes = animation.keyframes as readonly unknown[];
  if (keyframes.length === 0) return undefined;
  const values: T[] = [];
  let previous: number | undefined;
  for (const keyframe of keyframes) {
    if (!isRecord(keyframe) || !safeInteger(keyframe.time)) return undefined;
    if (previous !== undefined && keyframe.time <= previous) return undefined;
    if (!validCurve(keyframe.interpolation)) return undefined;
    previous = keyframe.time;
    values.push(keyframe.value as T);
  }
  return values;
}

// `x1` and `x2` must lie in `0..=1` so the curve stays a function of time.
function validCurve(curve: unknown): boolean {
  if (!isRecord(curve)) return false;
  if (curve.type !== "cubic_bezier") return true;
  return (
    [curve.x1, curve.x2].every((x) => finiteNumber(x) && x >= 0 && x <= 1) &&
    [curve.y1, curve.y2].every(finiteNumber)
  );
}

function validScalarAnimation(
  animation: unknown,
  low = -Infinity,
  high = Infinity,
): boolean {
  if (animation === undefined) return true;
  const values = animationValues<unknown>(animation);
  return (
    values !== undefined &&
    values.every(
      (value) => finiteNumber(value) && value >= low && value <= high,
    )
  );
}

function validRgb(value: unknown): boolean {
  return (
    isRecord(value) &&
    [value.red, value.green, value.blue].every(
      (component) =>
        finiteNumber(component) && component >= 0 && component <= 1,
    )
  );
}

function validVec2(value: unknown): boolean {
  return isRecord(value) && finiteNumber(value.x) && finiteNumber(value.y);
}

function validVec2Animation(animation: unknown): boolean {
  const values = animationValues<unknown>(animation);
  return values !== undefined && values.every(validVec2);
}

function validVisual(visual: unknown): boolean {
  if (visual === undefined) return true;
  return (
    isRecord(visual) &&
    (visual.anchor === undefined || validVec2(visual.anchor)) &&
    (visual.position === undefined || validPointAnimation(visual.position)) &&
    (visual.scale === undefined || validVec2Animation(visual.scale)) &&
    (visual.rotation_degrees === undefined ||
      validScalarAnimation(visual.rotation_degrees)) &&
    (visual.opacity === undefined || validScalarAnimation(visual.opacity, 0, 1))
  );
}

function validEffectValue(value: unknown): boolean {
  if (!isRecord(value)) return false;
  switch (value.type) {
    case "float":
      return finiteNumber(value.value);
    case "vec2":
      return (
        Array.isArray(value.value) &&
        value.value.length === 2 &&
        value.value.every(finiteNumber)
      );
    case "vec4":
      return (
        Array.isArray(value.value) &&
        value.value.length === 4 &&
        value.value.every(finiteNumber)
      );
    case "color":
      return (
        Array.isArray(value.value) &&
        value.value.length === 4 &&
        value.value.every(
          (component) =>
            finiteNumber(component) && component >= 0 && component <= 1,
        )
      );
    case "enum":
      return (
        typeof value.value === "number" &&
        Number.isInteger(value.value) &&
        value.value >= 0
      );
    case "bool":
      return typeof value.value === "boolean";
    default:
      return false;
  }
}

// All values share one type, and `enum` and `bool` keyframes must hold.
function validEffectParam(param: unknown): boolean {
  const values = animationValues<unknown>(param);
  if (values === undefined || !values.every(validEffectValue)) return false;
  const types = new Set(
    values.map((value) => (value as { readonly type: string }).type),
  );
  if (types.size !== 1) return false;
  if (!isRecord(param) || param.mode !== "keyframes") return true;
  const [type] = types;
  return (
    type === "float" ||
    type === "vec2" ||
    type === "vec4" ||
    type === "color" ||
    (param.keyframes as readonly Record<string, unknown>[]).every(
      (keyframe) =>
        isRecord(keyframe.interpolation) &&
        keyframe.interpolation.type === "hold",
    )
  );
}

const EFFECT_BINDING_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

function validClipEffect(effect: unknown): boolean {
  if (!isRecord(effect)) return false;
  const params = effect.params;
  if (params !== undefined && !isRecord(params)) return false;
  const resources = effect.resources;
  if (resources !== undefined && !isRecord(resources)) return false;
  return (
    text(effect.id).length > 0 &&
    positiveInteger(effect.version) &&
    Object.entries(params ?? {}).every(
      ([name, value]) =>
        EFFECT_BINDING_NAME.test(name) && validEffectParam(value),
    ) &&
    Object.entries(resources ?? {}).every(
      ([name, id]) => EFFECT_BINDING_NAME.test(name) && text(id).length > 0,
    )
  );
}

/** What validation reads of a built-in effect or transition, generated from its definition. */
export interface BuiltinEffectRules {
  readonly version: number;
  readonly kind: "effect" | "transition";
  readonly inputs: readonly string[];
  readonly resources: readonly string[];
  readonly params: Readonly<
    Record<
      string,
      {
        readonly type: string;
        readonly min?: number;
        readonly max?: number;
        readonly choices?: number;
      }
    >
  >;
}

/** Ids under this prefix must name a built-in. Mirrors `alpha2_model::BUILTIN_EFFECT_PREFIX`. */
const BUILTIN_EFFECT_PREFIX = "alpha2.";

function builtinRules(id: string): BuiltinEffectRules | undefined {
  return Object.hasOwn(BUILTIN_EFFECT_RULES, id)
    ? BUILTIN_EFFECT_RULES[id]
    : undefined;
}

/** A group control that sets a built-in effect's parameter. */
interface BuiltinControl {
  readonly effect: string;
  readonly param: string;
  readonly rule: BuiltinEffectRules["params"][string];
}

/** Each control of `composition` that sets a built-in parameter, by name; the first of a name. */
function builtinControlTargets(
  composition: Record<string, unknown>,
): ReadonlyMap<string, BuiltinControl> {
  const nodes = new Map<string, unknown>();
  for (const node of Array.isArray(composition.nodes)
    ? composition.nodes
    : []) {
    if (isRecord(node) && !nodes.has(text(node.id))) {
      nodes.set(text(node.id), node);
    }
  }
  const found = new Map<string, BuiltinControl>();
  const seen = new Set<string>();
  for (const control of Array.isArray(composition.controls)
    ? composition.controls
    : []) {
    if (!isRecord(control) || seen.has(text(control.name))) continue;
    seen.add(text(control.name));
    const node = nodes.get(text(control.node));
    const effect =
      isRecord(node) && isRecord(node.op) ? node.op.effect : undefined;
    if (!isRecord(effect)) continue;
    const param = text(control.param);
    const rule = builtinParamRule(text(effect.id), param);
    if (rule)
      found.set(text(control.name), { effect: text(effect.id), param, rule });
  }
  return found;
}

function builtinParamRule(
  id: string,
  name: string,
): BuiltinEffectRules["params"][string] | undefined {
  const rules = builtinRules(id);
  return rules && Object.hasOwn(rules.params, name)
    ? rules.params[name]
    : undefined;
}

function acceptsBuiltinParam(
  param: BuiltinEffectRules["params"][string],
  animation: unknown,
): boolean {
  const values = animationValues<unknown>(animation) ?? [];
  const scalar = (value: unknown): boolean =>
    finiteNumber(value) &&
    (param.min === undefined || Math.fround(value) >= param.min) &&
    (param.max === undefined || Math.fround(value) <= param.max);
  return values.every((held) => {
    if (!isRecord(held) || held.type !== param.type) return false;
    const value = held.value;
    switch (param.type) {
      case "float":
        return scalar(value);
      case "vec2":
      case "vec4":
        return Array.isArray(value) && value.every(scalar);
      case "color":
        return (
          Array.isArray(value) &&
          value.every(
            (part) =>
              finiteNumber(part) &&
              Math.fround(part) >= 0 &&
              Math.fround(part) <= 1,
          )
        );
      case "enum":
        return (
          Number.isInteger(value) &&
          (value as number) >= 0 &&
          (value as number) < (param.choices ?? 0)
        );
      default:
        return true;
    }
  });
}

/** How `effect`, used as `kind`, fails its built-in definition, worded as the model words it. */
function builtinEffectProblem(
  effect: unknown,
  kind: BuiltinEffectRules["kind"],
  inputs: unknown,
): string | undefined {
  if (!isRecord(effect)) return undefined;
  const id = text(effect.id);
  if (!id.startsWith(BUILTIN_EFFECT_PREFIX)) return undefined;
  const rules = builtinRules(id);
  if (rules === undefined) return "is not a built-in";
  if (effect.version !== rules.version) return `needs version ${rules.version}`;
  if (rules.kind !== kind) return "is used as the wrong kind";
  const params = isRecord(effect.params) ? effect.params : {};
  // The model reads parameters in name order.
  for (const name of Object.keys(params).sort()) {
    const param = Object.hasOwn(rules.params, name)
      ? rules.params[name]
      : undefined;
    if (param === undefined) return `has no parameter ${name}`;
    if (!acceptsBuiltinParam(param, params[name])) {
      return `has a parameter ${name} of the wrong type or out of range`;
    }
  }
  const declared = (names: readonly string[], used: unknown): boolean => {
    const keys = Object.keys(isRecord(used) ? used : {});
    return (
      keys.length === names.length && keys.every((key) => names.includes(key))
    );
  };
  if (!declared(rules.inputs, inputs)) {
    return "does not get exactly its declared inputs";
  }
  if (!declared(rules.resources, effect.resources)) {
    return "does not get exactly its declared resources";
  }
  return undefined;
}

function validEffectInputs(inputs: unknown): boolean {
  if (inputs === undefined) return true;
  if (!isRecord(inputs)) return false;
  const names = Object.keys(inputs);
  return (
    names.length <= MAX_EFFECT_INPUTS &&
    names.every((name) => EFFECT_BINDING_NAME.test(name))
  );
}

function positiveMs(value: unknown): boolean {
  return finiteNumber(value) && value > 0 && value <= MAX_ENVELOPE_MS;
}

// The model stores these as 32-bit floats; its limits are already 32-bit.
function f32Within(value: unknown, min: number, max: number): boolean {
  if (!finiteNumber(value)) return false;
  const single = Math.fround(value);
  return single >= min && single <= max;
}

function validAudioEffect(effect: unknown): boolean {
  if (!isRecord(effect)) return false;
  switch (effect.type) {
    case "eq": {
      const bands = effect.bands;
      return (
        Array.isArray(bands) &&
        bands.length > 0 &&
        bands.length <= MAX_EQ_BANDS &&
        bands.every((band: unknown) => {
          if (!isRecord(band)) return false;
          const gainDb = optionalFinite(band.gain_db, 0);
          return (
            f32Within(
              band.frequency_hz,
              MIN_EQ_FREQUENCY_HZ,
              MAX_EQ_FREQUENCY_HZ,
            ) &&
            f32Within(band.q, MIN_EQ_Q, MAX_EQ_Q) &&
            gainDb !== undefined &&
            f32Within(gainDb, -MAX_EQ_GAIN_DB, MAX_EQ_GAIN_DB)
          );
        })
      );
    }
    case "compressor": {
      const lookahead = optionalFinite(effect.lookahead_ms, 0);
      return (
        finiteNumber(effect.threshold_db) &&
        effect.threshold_db >= MIN_LEVEL_DB &&
        effect.threshold_db <= 0 &&
        f32Within(effect.ratio, 1, MAX_COMPRESSOR_RATIO) &&
        positiveMs(effect.attack_ms) &&
        positiveMs(effect.release_ms) &&
        lookahead !== undefined &&
        lookahead >= 0 &&
        lookahead <= MAX_LOOKAHEAD_MS
      );
    }
    case "reverb":
      return [effect.room_size, effect.damping, effect.mix].every(
        (value) => finiteNumber(value) && value >= 0 && value <= 1,
      );
    default:
      return false;
  }
}

function validEffects(effects: unknown): boolean {
  if (effects === undefined) return true;
  return (
    Array.isArray(effects) &&
    effects.length <= MAX_CHAIN_EFFECTS &&
    effects.every(validAudioEffect)
  );
}

const TICKS_PER_MS = TICKS_PER_SECOND / 1000;

// Delay in whole ticks, rounded up, so sums match the model in any order.
function effectDelayTicks(effect: Record<string, unknown>): number {
  if (effect.type === "reverb") return REVERB_DELAY_MS * TICKS_PER_MS;
  return effectLookaheadTicks(effect);
}

function effectLookaheadTicks(effect: Record<string, unknown>): number {
  if (effect.type !== "compressor" || !finiteNumber(effect.lookahead_ms))
    return 0;
  const lookahead = Math.fround(effect.lookahead_ms);
  return lookahead > 0 ? Math.ceil(lookahead * TICKS_PER_MS) : 0;
}

function chainLoad(effects: unknown): {
  count: number;
  delay: number;
  lookahead: number;
} {
  const load = { count: 0, delay: 0, lookahead: 0 };
  if (!Array.isArray(effects)) return load;
  for (const effect of effects) {
    load.count += 1;
    if (!isRecord(effect)) continue;
    load.delay += effectDelayTicks(effect);
    load.lookahead += effectLookaheadTicks(effect);
  }
  return load;
}

// The bus holds state throughout; `edges` mark where each clip's effects start and stop.
interface TrackAudioLoad {
  readonly count: number;
  readonly delay: number;
  readonly edges: readonly AudioLoadEdge[];
}

interface AudioLoadEdge {
  readonly at: number;
  readonly ends: boolean;
  readonly count: number;
  readonly delay: number;
}

// Mirrors `Clip::stretches_audio`.
function stretchesAudio(clip: Record<string, unknown>): boolean {
  if (!isRecord(clip.audio) || clip.audio.preserve_pitch !== true) return false;
  const map = clip.time_map;
  if (
    map !== undefined &&
    !(isRecord(map) && (map.type === "forward" || map.type === "reverse"))
  )
    return false;
  const timeline = clip.timeline_range;
  const source = clip.source_range;
  if (!validRange(timeline) || !validRange(source)) return false;
  const length = (range: unknown) => {
    const { start, end } = range as { start: number; end: number };
    return end - start;
  };
  return length(timeline) !== length(source);
}

function trackAudioLoad(track: unknown): TrackAudioLoad | undefined {
  if (!isRecord(track)) return undefined;
  const margin = AUDIO_EFFECT_STATE_MARGIN_MS * TICKS_PER_MS;
  const bus = chainLoad(
    isRecord(track.audio) ? track.audio.effects : undefined,
  );
  const edges: AudioLoadEdge[] = [];
  if (Array.isArray(track.clips))
    for (const clip of track.clips) {
      if (!isRecord(clip) || !isRecord(clip.audio)) continue;
      if (!validRange(clip.timeline_range)) continue;
      // A stretched read holds state like one effect, without delay.
      const chain = chainLoad(clip.audio.effects);
      const own = {
        ...chain,
        count: chain.count + Number(stretchesAudio(clip)),
      };
      if (own.count === 0) continue;
      const lead = bus.lookahead + own.lookahead + margin;
      const { start, end } = clip.timeline_range as {
        start: number;
        end: number;
      };
      edges.push({ at: Math.max(0, start - lead), ends: false, ...own });
      edges.push({ at: end, ends: true, ...own });
    }
  return { count: bus.count, delay: bus.delay, edges };
}

/**
 * Mirrors `Project::audio_effect_load`. Master and track effects hold state throughout; a clip's
 * hold from {@link AUDIO_EFFECT_STATE_MARGIN_MS} plus lookahead before its start to its end.
 * Each limit reports its peak and the earliest tick it is reached, in whole ticks, to match the
 * native result.
 */
function audioEffectProblems(
  master: unknown,
  tracks: readonly (TrackAudioLoad | undefined)[],
): string[] {
  const held = chainLoad(isRecord(master) ? master.effects : undefined);
  let effects = held.count;
  let delay = held.delay;
  for (const track of tracks) {
    if (track === undefined) continue;
    effects += track.count;
    delay += track.delay;
  }
  const edges = tracks.flatMap((track) => track?.edges ?? []);
  // Starts sort before ends at one position: a clip ending where another starts overlaps it.
  edges.sort((a, b) => a.at - b.at || Number(a.ends) - Number(b.ends));
  const peak = { effects, effectsAt: 0, delay, delayAt: 0 };
  for (const edge of edges) {
    const sign = edge.ends ? -1 : 1;
    effects += sign * edge.count;
    delay += sign * edge.delay;
    if (edge.ends) continue;
    if (effects > peak.effects) {
      peak.effects = effects;
      peak.effectsAt = edge.at;
    }
    if (delay > peak.delay) {
      peak.delay = delay;
      peak.delayAt = edge.at;
    }
  }
  const problems: string[] = [];
  if (peak.effects > MAX_AUDIO_EFFECTS)
    problems.push(
      `project has ${peak.effects} audio effects holding state at tick ${peak.effectsAt}; the maximum at once is ${MAX_AUDIO_EFFECTS}`,
    );
  const delayMs = Math.ceil(peak.delay / TICKS_PER_MS);
  if (delayMs > MAX_AUDIO_DELAY_MS)
    problems.push(
      `project audio effects hold ${delayMs} ms of delay at tick ${peak.delayAt}; the maximum at once is ${MAX_AUDIO_DELAY_MS} ms`,
    );
  return problems;
}

function validDucking(ducking: unknown): boolean {
  return (
    isRecord(ducking) &&
    finiteNumber(ducking.threshold) &&
    ducking.threshold > 0 &&
    finiteNumber(ducking.depth_db) &&
    ducking.depth_db >= MIN_LEVEL_DB &&
    ducking.depth_db <= 0 &&
    positiveMs(ducking.attack_ms) &&
    positiveMs(ducking.release_ms)
  );
}

function validTrackAudio(audio: unknown): boolean {
  if (audio === undefined) return true;
  return (
    isRecord(audio) &&
    validScalarAnimation(audio.gain, 0, MAX_GAIN) &&
    validScalarAnimation(audio.pan, -1, 1) &&
    validEffects(audio.effects) &&
    (audio.ducking == null || validDucking(audio.ducking))
  );
}

function validMasterAudio(audio: unknown): boolean {
  if (audio === undefined) return true;
  if (!isRecord(audio)) return false;
  const limiter = audio.limiter;
  return (
    validScalarAnimation(audio.gain, 0, MAX_GAIN) &&
    validEffects(audio.effects) &&
    (limiter == null ||
      (isRecord(limiter) &&
        finiteNumber(limiter.threshold) &&
        limiter.threshold > 0 &&
        limiter.threshold <= MAX_GAIN &&
        positiveMs(limiter.release_ms)))
  );
}

function transitionProblems(
  trackId: string,
  trackKind: unknown,
  clips: readonly Clip[],
  transitions: readonly ClipTransition[],
): string[] {
  const problems: string[] = [];
  if (transitions.length === 0) return problems;
  // The engine's track index keeps the first clip of each id.
  const byId = new Map<string, Clip>();
  for (const clip of clips)
    if (isRecord(clip) && !byId.has(text(clip.id)))
      byId.set(text(clip.id), clip);
  const findClip = (id: unknown): Clip | undefined => byId.get(text(id));
  const pairs = new Set<string>();
  for (const transition of transitions) {
    if (!isRecord(transition)) {
      problems.push(`track ${trackId} has an invalid clip transition`);
      continue;
    }
    const from = findClip(transition.from_clip);
    const to = findClip(transition.to_clip);
    const range = transition.timeline_range;
    const coveredBy = (clip: Clip | undefined): boolean =>
      clip !== undefined &&
      validRange(clip.timeline_range) &&
      isRecord(range) &&
      clip.timeline_range.start <= range.start &&
      clip.timeline_range.end >= range.end;
    if (
      trackKind !== "video" ||
      text(transition.from_clip) === text(transition.to_clip) ||
      !validRange(range) ||
      !validCurve(transition.easing) ||
      !validClipEffect(transition.effect) ||
      !coveredBy(from) ||
      !coveredBy(to)
    ) {
      problems.push(`track ${trackId} has an invalid clip transition`);
      continue;
    }
    const builtin = builtinEffectProblem(
      transition.effect,
      "transition",
      undefined,
    );
    if (builtin !== undefined) {
      problems.push(
        `effect ${text(transition.effect.id)} in the transition from ${text(transition.from_clip)} to ${text(transition.to_clip)} on track ${trackId} ${builtin}`,
      );
    }
    // At most one transition per pair of clips, in either direction.
    const ends = [text(transition.from_clip), text(transition.to_clip)].sort();
    const pair = JSON.stringify(ends);
    if (pairs.has(pair)) {
      problems.push(
        `clips ${text(transition.from_clip)} and ${text(transition.to_clip)} have more than one transition on track ${trackId}`,
      );
    }
    pairs.add(pair);
  }
  return problems;
}

/** Mirrors `validate_captions`, worded as the engine words it. */
function captionsProblem(
  track: Record<string, unknown>,
  clips: readonly unknown[],
  transitions: readonly unknown[],
  trackId: string,
): string | undefined {
  const invalid = `track ${trackId} has invalid captions`;
  if (track.kind !== "caption")
    return track.captions == null ? undefined : invalid;
  const captions = track.captions;
  if (!isRecord(captions) || clips.length > 0 || transitions.length > 0) {
    return invalid;
  }
  const cues = captions.cues;
  const styles = captions.styles ?? {};
  const encoder = new TextEncoder();
  const name = (value: unknown) =>
    typeof value === "string" &&
    value.length > 0 &&
    encoder.encode(value).length <= MAX_CAPTION_NAME_BYTES;
  if (
    !Array.isArray(cues) ||
    !validTextStyle(captions.style ?? {}, MAX_CAPTION_FONT_SIZE) ||
    !isRecord(styles) ||
    Object.keys(styles).length > MAX_CAPTION_STYLES ||
    !Object.keys(styles).every(name)
  ) {
    return invalid;
  }
  let previousEnd = 0;
  const cuesValid = cues.every((cue: unknown) => {
    if (!isRecord(cue) || !validRange(cue.timeline_range)) return false;
    const range = cue.timeline_range as { start: number; end: number };
    const valid =
      range.start >= previousEnd &&
      validTextContent(cue.text, encoder) &&
      (cue.speaker == null || name(cue.speaker)) &&
      runTimesLieIn(cue.text, range.end - range.start);
    previousEnd = range.end;
    return valid;
  });
  if (!cuesValid) return invalid;
  for (const [index, cue] of (cues as Record<string, unknown>[]).entries()) {
    const own = cue.style;
    if (typeof own === "string" && !Object.hasOwn(styles, own)) {
      return `cue ${index} of track ${trackId} names style ${JSON.stringify(own)}, which the track does not have`;
    }
    const patchName =
      typeof own === "string"
        ? own
        : typeof cue.speaker === "string" && Object.hasOwn(styles, cue.speaker)
          ? cue.speaker
          : undefined;
    if (patchName === undefined) continue;
    const merged = patchedStyle(captions.style ?? {}, styles[patchName]);
    if (!validTextStyle(merged, MAX_CAPTION_FONT_SIZE)) {
      return `cue ${index} of track ${trackId} has a style out of range once its patch is applied`;
    }
  }
  return undefined;
}

function runTimesLieIn(text: unknown, duration: number): boolean {
  if (!Array.isArray(text)) return true;
  return text.every((run: unknown) => {
    if (!isRecord(run) || run.time == null) return true;
    const time = run.time as Record<string, unknown>;
    return (
      finiteNumber(time.start) &&
      finiteNumber(time.end) &&
      time.start >= 0 &&
      time.end <= duration
    );
  });
}

/** Mirrors `TextStylePatch::apply`; `null` clears a field. */
function patchedStyle(base: unknown, patch: unknown): Record<string, unknown> {
  const merged: Record<string, unknown> = isRecord(base) ? { ...base } : {};
  if (!isRecord(patch)) return merged;
  for (const [field, value] of Object.entries(patch)) {
    if (value === undefined) continue;
    if (value === null) delete merged[field];
    else merged[field] = value;
  }
  return merged;
}

/** Mirrors `TextStyle::is_valid`. */
function validTextStyle(style: unknown, maxFontSize: number): boolean {
  if (!isRecord(style)) return false;
  const within = (value: unknown, low: number, high: number, open = false) =>
    finiteNumber(value) && (open ? value > low : value >= low) && value <= high;
  const background = style.background;
  const family = style.font_family;
  return (
    (family == null ||
      (typeof family === "string" &&
        family.length > 0 &&
        new TextEncoder().encode(family).length <= MAX_FONT_FAMILY_BYTES)) &&
    within(style.font_size ?? 0.05, 0, maxFontSize, true) &&
    (style.fill === undefined || validTextFill(style.fill)) &&
    within(style.line_height ?? 1.2, MIN_LINE_HEIGHT, MAX_LINE_HEIGHT) &&
    within(style.letter_spacing ?? 0, MIN_LETTER_SPACING, MAX_LETTER_SPACING) &&
    validFontWeight(style.font_weight ?? 400) &&
    within(style.max_width ?? 0.9, 0, 1, true) &&
    (style.position === undefined || validVec2(style.position)) &&
    (style.anchor === undefined || validVec2(style.anchor)) &&
    (style.strokes === undefined || validTextStrokes(style.strokes)) &&
    (style.shadows === undefined || validTextShadows(style.shadows)) &&
    (style.animators === undefined ||
      validTextAnimators(style.animators, false)) &&
    (background == null ||
      (isRecord(background) &&
        validRgba(background.color) &&
        within(background.padding ?? 0, 0, MAX_TEXT_PADDING)))
  );
}

/** Mirrors `TextContent::is_valid`. */
function validTextContent(text: unknown, encoder: TextEncoder): boolean {
  if (typeof text === "string") {
    return (
      text.length > 0 && encoder.encode(text).length <= MAX_TEXT_CONTENT_BYTES
    );
  }
  if (!Array.isArray(text) || text.length > MAX_TEXT_RUNS) return false;
  let bytes = 0;
  for (const run of text) {
    if (!isRecord(run) || typeof run.text !== "string" || run.text.length === 0)
      return false;
    bytes += encoder.encode(run.text).length;
    const classes = run.class ?? [];
    if (
      !Array.isArray(classes) ||
      classes.length > MAX_RUN_CLASSES ||
      !classes.every(validClassName)
    ) {
      return false;
    }
    const time = run.time;
    if (
      time != null &&
      !(
        isRecord(time) &&
        safeInteger(time.start) &&
        safeInteger(time.end) &&
        time.start >= 0 &&
        time.start < time.end
      )
    ) {
      return false;
    }
  }
  return bytes > 0 && bytes <= MAX_TEXT_CONTENT_BYTES;
}

/** Mirrors `valid_class_name`. */
function validClassName(name: unknown): boolean {
  return (
    typeof name === "string" &&
    name.length > 0 &&
    name.length <= MAX_CLASS_NAME_BYTES &&
    /^[A-Za-z0-9_-]+$/.test(name)
  );
}

/** Mirrors `valid_animators`; `rangeOnly` allows only Range selectors (layout nodes). */
function validTextAnimators(animators: unknown, rangeOnly: boolean): boolean {
  if (!Array.isArray(animators) || animators.length > MAX_TEXT_ANIMATORS)
    return false;
  return animators.every((animator: unknown) => {
    if (!isRecord(animator) || !isRecord(animator.selector)) return false;
    const selector = animator.selector;
    if (rangeOnly && selector.type !== "range") return false;
    let selected: boolean;
    switch (selector.type) {
      case "range": {
        const [low, high] =
          selector.units === "index"
            ? [-MAX_RANGE_INDEX, 2 * MAX_RANGE_INDEX]
            : [MIN_RANGE_PERCENT, MAX_RANGE_PERCENT];
        selected =
          [selector.start, selector.end, selector.offset].every((value) =>
            validScalarAnimation(value, low, high),
          ) && validScalarAnimation(selector.amount, 0, 1);
        break;
      }
      case "timing":
        selected =
          selector.ramp === undefined ||
          (safeInteger(selector.ramp) && selector.ramp >= 0);
        break;
      case "class":
        selected = validClassName(selector.class);
        break;
      default:
        selected = false;
    }
    return selected && validAnimatorProperties(animator.properties ?? {});
  });
}

/** Mirrors `TextAnimatorProperties::is_valid`. */
function validAnimatorProperties(properties: unknown): boolean {
  if (!isRecord(properties)) return false;
  const vec2 = (animation: unknown, low: number, high: number) => {
    if (animation === undefined) return true;
    const values = animationValues<unknown>(animation);
    return (
      values !== undefined &&
      values.every(
        (value) =>
          isRecord(value) &&
          [value.x, value.y].every(
            (axis) => finiteNumber(axis) && axis >= low && axis <= high,
          ),
      )
    );
  };
  const color = (animation: unknown) => {
    if (animation === undefined) return true;
    const values = animationValues<unknown>(animation);
    return values !== undefined && values.every(validRgba);
  };
  return (
    vec2(properties.offset, -Infinity, Infinity) &&
    vec2(properties.scale, 0, MAX_ANIMATOR_SCALE) &&
    validScalarAnimation(properties.rotation_degrees) &&
    validScalarAnimation(properties.opacity, 0, 1) &&
    color(properties.fill) &&
    color(properties.stroke) &&
    validScalarAnimation(
      properties.tracking,
      MIN_LETTER_SPACING,
      MAX_LETTER_SPACING,
    )
  );
}

function validTextFill(fill: unknown): boolean {
  return isRecord(fill) && "type" in fill ? validPaint(fill) : validRgba(fill);
}

/** Mirrors `valid_strokes`; widths are summed in `f32` as the model does. */
function validTextStrokes(strokes: unknown): boolean {
  if (!Array.isArray(strokes) || strokes.length > MAX_TEXT_STROKES)
    return false;
  let extent = 0;
  for (const stroke of strokes) {
    if (
      !isRecord(stroke) ||
      !validRgba(stroke.color) ||
      !finiteNumber(stroke.width) ||
      Math.fround(stroke.width) <= 0
    ) {
      return false;
    }
    extent = Math.fround(extent + Math.fround(stroke.width));
  }
  return extent <= Math.fround(MAX_TEXT_STROKE_EXTENT);
}

/** Mirrors `valid_shadows`. */
function validTextShadows(shadows: unknown): boolean {
  if (!Array.isArray(shadows) || shadows.length > MAX_TEXT_SHADOWS)
    return false;
  const offset = (value: unknown) =>
    finiteNumber(value) && Math.abs(value) <= MAX_TEXT_SHADOW_OFFSET;
  return shadows.every((shadow: unknown) => {
    if (!isRecord(shadow)) return false;
    const blur = shadow.blur ?? 0;
    return (
      validRgba(shadow.color) &&
      (shadow.offset === undefined ||
        (isRecord(shadow.offset) &&
          offset(shadow.offset.x) &&
          offset(shadow.offset.y))) &&
      finiteNumber(blur) &&
      blur >= 0 &&
      blur <= MAX_TEXT_SHADOW_BLUR
    );
  });
}

function validFontWeight(weight: unknown): boolean {
  return (
    Number.isInteger(weight) &&
    (weight as number) >= MIN_FONT_WEIGHT &&
    (weight as number) <= MAX_FONT_WEIGHT
  );
}

/**
 * Whether a provider reads a file of the source's own. One that does not, such as LaTeX, holds its
 * content in its settings, so its source has an empty locator and no fingerprint. Mirrors
 * `ProviderSource::reads_file`.
 */
export function providerReadsFile(provider: {
  readonly format: string;
}): boolean {
  return provider.format !== "latex";
}

/** The kind of source a provider's media is. Mirrors `ProviderSource::kind`. */
export function providerKind(provider: {
  readonly format: string;
}): "image" | "video" {
  return ["svg", "exr", "latex", "pdf"].includes(provider.format)
    ? "image"
    : "video";
}

/** Mirrors `ProviderSource::kind` and `ProviderSource::is_valid`. */
function validProvider(provider: unknown, kind: unknown): boolean {
  if (!isRecord(provider)) return false;
  if (provider.format === "svg" || provider.format === "exr")
    return kind === "image";
  if (provider.format === "animated_image") return kind === "video";
  if (provider.format === "latex") {
    const { latex, font_size: size, color } = provider;
    return (
      kind === "image" &&
      typeof latex === "string" &&
      latex.length > 0 &&
      new TextEncoder().encode(latex).length <= MAX_LATEX_BYTES &&
      finiteNumber(size) &&
      size > 0 &&
      size <= MAX_LATEX_FONT_SIZE &&
      (color == null || validRgba(color)) &&
      (provider.display == null || typeof provider.display === "boolean")
    );
  }
  if (provider.format === "pdf") {
    const { page, background } = provider;
    return (
      kind === "image" &&
      (page == null ||
        (Number.isInteger(page) &&
          (page as number) >= 1 &&
          (page as number) <= MAX_PDF_PAGES)) &&
      (background == null || validRgba(background))
    );
  }
  if (provider.format === "lottie")
    return (
      kind === "video" &&
      (provider.animation == null ||
        (typeof provider.animation === "string" &&
          provider.animation.length > 0))
    );
  if (provider.format !== "image_sequence" || kind !== "video") return false;
  const frames = provider.frames;
  if (!Array.isArray(frames) || frames.length === 0) return false;
  if (frames.length > MAX_SEQUENCE_FRAMES || frames[0] == null) return false;
  const present = frames.filter((frame) => frame != null);
  if (
    present.some((frame) => typeof frame !== "string" || frame.length === 0) ||
    new Set(present).size !== present.length
  )
    return false;
  return (
    present.length === frames.length ||
    (provider.missing != null && provider.missing !== "reject")
  );
}

/** Mirrors `MediaFingerprint::is_valid`: each recorded part well formed. */
function validFingerprint(fingerprint: unknown): boolean {
  if (!isRecord(fingerprint)) return false;
  const hash = fingerprint.content_sha256;
  if (
    hash != null &&
    !(typeof hash === "string" && /^[0-9a-f]{64}$/.test(hash))
  )
    return false;
  const duration = fingerprint.duration_ticks;
  if (duration != null && !(typeof duration === "number" && duration >= 0))
    return false;
  if (fingerprint.color != null && !consistentColor(fingerprint.color))
    return false;
  const streams = fingerprint.streams;
  if (streams == null) return true;
  if (!isRecord(streams)) return false;
  const { video, audio } = streams;
  if (
    video != null &&
    !(isRecord(video) && positive(video.width) && positive(video.height))
  )
    return false;
  return (
    audio == null ||
    (isRecord(audio) && positive(audio.channels) && positive(audio.sample_rate))
  );
}

/** Mirrors `ColorMetadata::is_consistent`: HDR exactly when the transfer is PQ or HLG. */
function consistentColor(color: unknown): boolean {
  if (!isRecord(color)) return true;
  const hdrTransfer = color.transfer === "pq" || color.transfer === "hlg";
  return (color.dynamic_range === "hdr") === hdrTransfer;
}

function positive(value: unknown): boolean {
  return typeof value === "number" && value > 0;
}

function validGenerator(generator: unknown): boolean {
  if (!isRecord(generator)) return false;
  if (generator.type === "fill") return validPaint(generator.paint);
  if (generator.type === "shapes")
    return shapeItemsIssue(generator.items) === undefined;
  if (generator.type === "layout")
    return layoutIssue(generator.root) === undefined;
  if (generator.type === "text")
    return (
      validTextContent(generator.text, new TextEncoder()) &&
      validTextStyle(generator.style ?? {}, MAX_TEXT_GENERATOR_FONT_SIZE)
    );
  return false;
}

interface ShapeIssue {
  readonly at: string;
  readonly problem: string;
}

/** Mirrors `ShapeProblem`'s wording. */
const SHAPE_PROBLEM = {
  tooManyItems: "has more items than a shapes generator may hold",
  tooDeep: "nests groups too deeply",
  pathTooLarge: "has more subpaths or vertices than a path may hold",
  invalidDash: "has an invalid dash pattern",
  invalidKeyframes: "has invalid keyframes",
  outOfRange: "has a value out of range",
  pathKeyframesDiffer: "has path keyframes that differ in structure",
  paintKeyframesDiffer: "has paint keyframes that differ in kind or stops",
} as const;

function generatorShapeIssue(generator: unknown): ShapeIssue | undefined {
  return isRecord(generator) && generator.type === "shapes"
    ? shapeItemsIssue(generator.items)
    : undefined;
}

function shapeProblem(owner: string, issue: ShapeIssue): string {
  return `${owner} has an invalid shape: ${issue.at} ${issue.problem}`;
}

/** Mirrors `LayoutProblem`'s wording. */
const LAYOUT_PROBLEM = {
  tooManyNodes: "has more nodes than a layout may hold",
  tooDeep: "nests nodes too deeply",
  tooManyTexts: "has more text nodes than a layout may hold",
  tooManyIsolated:
    "has more nodes with opacity, transforms, or hidden overflow than a layout may hold",
  mixedContent: "has more than one of children, text, and shape",
  displayOnLeaf: "sets display on a node with text or shape content",
  containerFieldOnLeaf:
    "sets a container field on a node with text or shape content",
  rootItemField: "sets a size, margin, position, or item field on the root",
  gridFieldOnFlex: (field: string) =>
    `sets ${field}, a grid container field, without display grid`,
  flexFieldOnGrid: (field: string) =>
    `sets ${field}, a flex container field, on a grid container`,
  flexItemFieldInGrid: (field: string) =>
    `sets ${field}, a flex item field, in a grid container`,
  gridItemFieldInFlex: (field: string) =>
    `sets ${field}, a grid item field, in a flex container`,
  tooManyTracks: "has more than 64 tracks",
  repeatCount: "repeats tracks a number of times outside 1 to 64",
  repeatInAutoTracks: "uses repeat() in implicit track sizes",
  autoRepeatNeedsFixedTracks:
    "repeats auto-fill or auto-fit beside another, or with a track without a fixed size",
  gridLineOutOfRange:
    "names a grid line outside -65 to 65 or line 0, or a span outside 1 to 64",
  invalidKeyframes: "has invalid keyframes",
  outOfRange: "has a value out of range",
  lengthKeyframesDiffer: "has size keyframes that differ in kind",
  paintKeyframesDiffer: "has paint keyframes that differ in kind or stops",
  textKeyframesInterpolate: "has text keyframes that do not hold",
} as const;

// Problems in a shape node's items (`shape` set) are reported as shape problems, like the engine.
interface LayoutIssue extends ShapeIssue {
  readonly shape?: boolean;
}

type Canvas = readonly [number, number];

function generatorLayoutIssue(
  generator: unknown,
  canvas?: Canvas,
): LayoutIssue | undefined {
  return isRecord(generator) && generator.type === "layout"
    ? layoutIssue(generator.root, canvas)
    : undefined;
}

function layoutProblem(owner: string, issue: LayoutIssue): string {
  return issue.shape === true
    ? shapeProblem(owner, issue)
    : `${owner} has an invalid layout: ${issue.at} ${issue.problem}`;
}

const LAYOUT_FLEX_CONTAINER_FIELDS = ["flex_direction", "flex_wrap"] as const;

const LAYOUT_GRID_CONTAINER_FIELDS = [
  "grid_template_columns",
  "grid_template_rows",
  "grid_auto_columns",
  "grid_auto_rows",
  "grid_auto_flow",
  "justify_items",
] as const;

const LAYOUT_CONTAINER_FIELDS = [
  ...LAYOUT_FLEX_CONTAINER_FIELDS,
  ...LAYOUT_GRID_CONTAINER_FIELDS,
  "gap",
  "row_gap",
  "column_gap",
  "justify_content",
  "align_items",
  "align_content",
] as const;

const LAYOUT_FLEX_ITEM_FIELDS = [
  "flex_grow",
  "flex_shrink",
  "flex_basis",
] as const;

const LAYOUT_GRID_ITEM_FIELDS = [
  "grid_column",
  "grid_row",
  "justify_self",
] as const;

const LAYOUT_ITEM_FIELDS = [
  "width",
  "height",
  "min_width",
  "min_height",
  "max_width",
  "max_height",
  "aspect_ratio",
  "margin",
  ...LAYOUT_FLEX_ITEM_FIELDS,
  ...LAYOUT_GRID_ITEM_FIELDS,
  "align_self",
  "position",
  "inset",
] as const;

const LAYOUT_SIZE_FIELDS = [
  "width",
  "height",
  "min_width",
  "min_height",
  "max_width",
  "max_height",
] as const;

const LAYOUT_VISUAL_FIELDS = [
  "opacity",
  "translate",
  "scale",
  "rotation_degrees",
] as const;

// The model reads `null` as absent.
function present(value: unknown): boolean {
  return value !== undefined && value !== null;
}

function firstPresent(
  node: Record<string, unknown>,
  fields: readonly string[],
): string | undefined {
  return fields.find((field) => present(node[field]));
}

function lengthOf(value: unknown): Length | undefined {
  return isRecord(value) && typeof value.unit === "string"
    ? (value as unknown as Length)
    : undefined;
}

/**
 * Mirrors `validate_layout`: the first problem of a layout tree in preorder. With `canvas`, pixel
 * font sizes are checked against the layout text limit.
 */
function layoutIssue(root: unknown, canvas?: Canvas): LayoutIssue | undefined {
  const node = isRecord(root) ? root : {};
  if (LAYOUT_ITEM_FIELDS.some((field) => present(node[field]))) {
    return { at: "root", problem: LAYOUT_PROBLEM.rootItemField };
  }
  const counts = { nodes: 0, texts: 0, isolated: 0, shapes: { count: 0 } };
  const shorterSide =
    canvas === undefined ? undefined : Math.min(canvas[0], canvas[1]);
  return layoutNodeIssue(root, "root", 1, counts, {
    shorterSide,
    parent: undefined,
  });
}

function layoutNodeIssue(
  value: unknown,
  at: string,
  depth: number,
  counts: {
    nodes: number;
    texts: number;
    isolated: number;
    shapes: { count: number };
  },
  context: { shorterSide: number | undefined; parent: string | undefined },
): LayoutIssue | undefined {
  const issue = (problem: string): LayoutIssue => ({ at, problem });
  const node = isRecord(value) ? value : {};
  counts.nodes += 1;
  if (counts.nodes > MAX_LAYOUT_NODES)
    return issue(LAYOUT_PROBLEM.tooManyNodes);
  if (depth > MAX_LAYOUT_DEPTH) return issue(LAYOUT_PROBLEM.tooDeep);
  const children = Array.isArray(node.children)
    ? (node.children as readonly unknown[])
    : [];
  const text = present(node.text);
  const shape = present(node.shape);
  if (Number(children.length > 0) + Number(text) + Number(shape) > 1) {
    return issue(LAYOUT_PROBLEM.mixedContent);
  }
  if ((text || shape) && present(node.display)) {
    return issue(LAYOUT_PROBLEM.displayOnLeaf);
  }
  if (
    (text || shape) &&
    LAYOUT_CONTAINER_FIELDS.some((field) => present(node[field]))
  ) {
    return issue(LAYOUT_PROBLEM.containerFieldOnLeaf);
  }
  const display = node.display === "grid" ? "grid" : "flex";
  const mismatch =
    display === "flex"
      ? firstPresent(node, LAYOUT_GRID_CONTAINER_FIELDS)
      : firstPresent(node, LAYOUT_FLEX_CONTAINER_FIELDS);
  if (mismatch !== undefined) {
    return issue(
      display === "flex"
        ? LAYOUT_PROBLEM.gridFieldOnFlex(mismatch)
        : LAYOUT_PROBLEM.flexFieldOnGrid(mismatch),
    );
  }
  if (context.parent === "flex") {
    const field = firstPresent(node, LAYOUT_GRID_ITEM_FIELDS);
    if (field !== undefined)
      return issue(LAYOUT_PROBLEM.gridItemFieldInFlex(field));
  } else if (context.parent === "grid") {
    const field = firstPresent(node, LAYOUT_FLEX_ITEM_FIELDS);
    if (field !== undefined)
      return issue(LAYOUT_PROBLEM.flexItemFieldInGrid(field));
  }
  if (text) {
    counts.texts += 1;
    if (counts.texts > MAX_LAYOUT_TEXTS)
      return issue(LAYOUT_PROBLEM.tooManyTexts);
  }
  if (
    LAYOUT_VISUAL_FIELDS.some((field) => present(node[field])) ||
    node.overflow === "hidden"
  ) {
    counts.isolated += 1;
    if (counts.isolated > MAX_LAYOUT_ISOLATED) {
      return issue(LAYOUT_PROBLEM.tooManyIsolated);
    }
  }
  const fields =
    layoutFieldsIssue(node, at) ??
    layoutGridIssue(node, at) ??
    layoutStyleIssue(node, at, context.shorterSide);
  if (fields) return fields;
  if (text) {
    const textIssue = layoutTextIssue(node.text, `${at}.text`);
    if (textIssue) return textIssue;
  }
  if (shape) {
    const items = isRecord(node.shape) ? node.shape.items : undefined;
    const shapeIssue = shapeGroupIssue(
      items,
      `${at}.shape.items`,
      1,
      counts.shapes,
    );
    if (shapeIssue) return { ...shapeIssue, shape: true };
  }
  for (const [index, child] of children.entries()) {
    const childIssue = layoutNodeIssue(
      child,
      `${at}.children[${index}]`,
      depth + 1,
      counts,
      { shorterSide: context.shorterSide, parent: display },
    );
    if (childIssue) return childIssue;
  }
  return undefined;
}

/** Mirrors `checks::animated`: keyframes first, then values. */
function layoutAnimatedIssue(
  animation: unknown,
  at: string,
  check: (value: unknown) => boolean,
): LayoutIssue | undefined {
  if (!present(animation)) return undefined;
  const values = animationValues<unknown>(animation);
  if (values === undefined)
    return { at, problem: LAYOUT_PROBLEM.invalidKeyframes };
  return values.every(check)
    ? undefined
    : { at, problem: LAYOUT_PROBLEM.outOfRange };
}

function layoutScalarIssue(
  animation: unknown,
  at: string,
  check: (value: number) => boolean,
): LayoutIssue | undefined {
  return layoutAnimatedIssue(
    animation,
    at,
    (value) => finiteNumber(value) && check(value),
  );
}

function layoutSizeIssue(
  animation: unknown,
  at: string,
  check: (value: number) => boolean,
): LayoutIssue | undefined {
  return layoutAnimatedIssue(animation, at, (value) => {
    const length = lengthOf(value);
    return (
      length !== undefined && finiteNumber(length.value) && check(length.value)
    );
  });
}

function layoutInsetsIssue(
  animation: unknown,
  at: string,
  check: (value: number) => boolean,
): LayoutIssue | undefined {
  return layoutAnimatedIssue(
    animation,
    at,
    (value) =>
      isRecord(value) &&
      (["top", "right", "bottom", "left"] as const).every((side) => {
        const length = lengthOf(value[side]);
        return (
          length !== undefined &&
          finiteNumber(length.value) &&
          check(length.value)
        );
      }),
  );
}

// Checks values, then that keyframes are all percents or all lengths.
function layoutLengthIssue(
  animation: unknown,
  at: string,
  check: (value: number) => boolean,
): LayoutIssue | undefined {
  const issue = layoutSizeIssue(animation, at, check);
  if (issue || !present(animation)) return issue;
  const kinds = new Set(
    (animationValues<unknown>(animation) ?? []).map(
      (value) => lengthOf(value)?.unit === "percent",
    ),
  );
  return kinds.size > 1
    ? { at, problem: LAYOUT_PROBLEM.lengthKeyframesDiffer }
    : undefined;
}

function layoutPaintIssue(
  animation: unknown,
  at: string,
): LayoutIssue | undefined {
  if (!present(animation)) return undefined;
  const issue = animatedPaintIssue(animation, at);
  if (issue === undefined) return undefined;
  const problem =
    issue.problem === SHAPE_PROBLEM.invalidKeyframes
      ? LAYOUT_PROBLEM.invalidKeyframes
      : issue.problem === SHAPE_PROBLEM.paintKeyframesDiffer
        ? LAYOUT_PROBLEM.paintKeyframesDiffer
        : LAYOUT_PROBLEM.outOfRange;
  return { at: issue.at, problem };
}

/** Mirrors `layout::validate_fields`, field by field in the same order. */
function layoutFieldsIssue(
  node: Record<string, unknown>,
  at: string,
): LayoutIssue | undefined {
  const field = (name: string) => `${at}.${name}`;
  const border = isRecord(node.border) ? node.border : undefined;
  for (const name of ["gap", "row_gap", "column_gap"] as const) {
    const issue = layoutSizeIssue(node[name], field(name), nonNegativeValue);
    if (issue) return issue;
  }
  const first =
    layoutInsetsIssue(node.padding, field("padding"), nonNegativeValue) ??
    layoutPaintIssue(node.background, field("background")) ??
    (border
      ? (layoutSizeIssue(
          border.width,
          field("border.width"),
          nonNegativeValue,
        ) ?? layoutPaintIssue(border.paint, field("border.paint")))
      : undefined) ??
    layoutSizeIssue(
      node.border_radius,
      field("border_radius"),
      nonNegativeValue,
    );
  if (first) return first;
  for (const name of LAYOUT_SIZE_FIELDS) {
    const issue = layoutLengthIssue(node[name], field(name), nonNegativeValue);
    if (issue) return issue;
  }
  if (
    present(node.aspect_ratio) &&
    !(finiteNumber(node.aspect_ratio) && node.aspect_ratio > 0)
  ) {
    return { at: field("aspect_ratio"), problem: LAYOUT_PROBLEM.outOfRange };
  }
  const item =
    layoutInsetsIssue(node.margin, field("margin"), anyValue) ??
    layoutScalarIssue(node.flex_grow, field("flex_grow"), nonNegativeValue) ??
    layoutScalarIssue(
      node.flex_shrink,
      field("flex_shrink"),
      nonNegativeValue,
    ) ??
    layoutLengthIssue(node.flex_basis, field("flex_basis"), nonNegativeValue);
  if (item) return item;
  if (isRecord(node.inset)) {
    for (const side of ["left", "top", "right", "bottom"] as const) {
      const issue = layoutLengthIssue(
        node.inset[side],
        field(`inset.${side}`),
        anyValue,
      );
      if (issue) return issue;
    }
  }
  return (
    layoutScalarIssue(node.opacity, field("opacity"), unitValue) ??
    layoutAnimatedIssue(
      node.translate,
      field("translate"),
      (value) =>
        isRecord(value) &&
        finiteNumber(lengthOf(value.x)?.value) &&
        finiteNumber(lengthOf(value.y)?.value),
    ) ??
    layoutAnimatedIssue(node.scale, field("scale"), validVec2) ??
    layoutScalarIssue(
      node.rotation_degrees,
      field("rotation_degrees"),
      anyValue,
    )
  );
}

function trackParts(size: TrackSize): {
  lengths: Length[];
  share: number | undefined;
} {
  const breadths =
    size.kind === "breadth"
      ? [size.breadth]
      : size.kind === "minmax"
        ? [size.min, size.max]
        : [];
  const lengths = breadths.flatMap((breadth) =>
    breadth.kind === "length" ? [breadth.length] : [],
  );
  if (size.kind === "fit-content") lengths.push(size.limit);
  const last = breadths.at(-1);
  return { lengths, share: last?.kind === "fr" ? last.share : undefined };
}

/** Mirrors `TrackSize::is_fixed`. */
function fixedTrack(size: TrackSize): boolean {
  const fixed = (breadth: TrackBreadth) => breadth.kind === "length";
  if (size.kind === "breadth") return fixed(size.breadth);
  if (size.kind === "minmax")
    return fixed(size.min) || (fixed(size.max) && size.min.kind !== "fr");
  return false;
}

/** Mirrors `layout::track_problem`. */
function trackProblem(
  tracks: readonly TrackComponent[],
  explicit: boolean,
): string | undefined {
  const sizes = tracks.flatMap((component) =>
    component.kind === "single" ? [component.size] : component.tracks,
  );
  const parts = sizes.map(trackParts);
  if (
    !parts.every(
      ({ lengths, share }) =>
        lengths.every(
          (length) => Number.isFinite(length.value) && length.value >= 0,
        ) &&
        (share === undefined || (Number.isFinite(share) && share > 0)),
    )
  ) {
    return LAYOUT_PROBLEM.outOfRange;
  }
  const repeats = tracks.flatMap((component) =>
    component.kind === "repeat" ? [component.count] : [],
  );
  if (!explicit && repeats.length > 0) return LAYOUT_PROBLEM.repeatInAutoTracks;
  if (
    repeats.some(
      (count) => typeof count === "number" && (count < 1 || count > 64),
    )
  )
    return LAYOUT_PROBLEM.repeatCount;
  const count = tracks.reduce(
    (total, component) =>
      total +
      (component.kind === "single"
        ? 1
        : (typeof component.count === "number" ? component.count : 1) *
          component.tracks.length),
    0,
  );
  if (count > MAX_GRID_TRACKS) return LAYOUT_PROBLEM.tooManyTracks;
  const automatic = repeats.filter((count) => typeof count !== "number").length;
  if (automatic > 1 || (automatic === 1 && !sizes.every(fixedTrack)))
    return LAYOUT_PROBLEM.autoRepeatNeedsFixedTracks;
  return undefined;
}

/** Mirrors `layout::validate_grid`. */
function layoutGridIssue(
  node: Record<string, unknown>,
  at: string,
): LayoutIssue | undefined {
  for (const [name, explicit] of [
    ["grid_template_columns", true],
    ["grid_template_rows", true],
    ["grid_auto_columns", false],
    ["grid_auto_rows", false],
  ] as const) {
    const tracks = node[name];
    if (!Array.isArray(tracks)) continue;
    const problem = trackProblem(tracks as TrackComponent[], explicit);
    if (problem) return { at: `${at}.${name}`, problem };
  }
  for (const name of ["grid_column", "grid_row"] as const) {
    const placement = node[name];
    if (!isRecord(placement)) continue;
    const inRange = (edge: GridEdge) =>
      edge.kind === "auto" ||
      (edge.kind === "line"
        ? edge.line !== 0 && Math.abs(edge.line) <= 65
        : edge.count >= 1 && edge.count <= 64);
    if (
      ![placement.start, placement.end].every((edge) =>
        inRange(edge as GridEdge),
      )
    ) {
      return {
        at: `${at}.${name}`,
        problem: LAYOUT_PROBLEM.gridLineOutOfRange,
      };
    }
  }
  return undefined;
}

/** Mirrors `layout::validate_style`. */
function layoutStyleIssue(
  node: Record<string, unknown>,
  at: string,
  shorterSide: number | undefined,
): LayoutIssue | undefined {
  const outOfRange = (name: string): LayoutIssue => ({
    at: `${at}.${name}`,
    problem: LAYOUT_PROBLEM.outOfRange,
  });
  const family = node.font_family;
  if (
    present(family) &&
    !(
      typeof family === "string" &&
      family.length > 0 &&
      new TextEncoder().encode(family).length <= MAX_FONT_FAMILY_BYTES
    )
  ) {
    return outOfRange("font_family");
  }
  const size = layoutAnimatedIssue(
    node.font_size,
    `${at}.font_size`,
    (value) => {
      const length = lengthOf(value);
      if (
        length === undefined ||
        !finiteNumber(length.value) ||
        length.value <= 0
      )
        return false;
      const fraction =
        length.unit !== "px"
          ? length.value
          : shorterSide === undefined
            ? 0
            : Math.fround(length.value / shorterSide);
      return fraction <= MAX_LAYOUT_FONT_SIZE;
    },
  );
  if (size) return size;
  if (present(node.fill) && !validTextFill(node.fill)) {
    return outOfRange("fill");
  }
  const lineHeight = node.line_height;
  if (
    present(lineHeight) &&
    !(
      finiteNumber(lineHeight) &&
      lineHeight >= MIN_LINE_HEIGHT &&
      lineHeight <= MAX_LINE_HEIGHT
    )
  ) {
    return outOfRange("line_height");
  }
  const letterSpacing = node.letter_spacing;
  if (
    present(letterSpacing) &&
    !(
      finiteNumber(letterSpacing) &&
      letterSpacing >= MIN_LETTER_SPACING &&
      letterSpacing <= MAX_LETTER_SPACING
    )
  ) {
    return outOfRange("letter_spacing");
  }
  if (present(node.font_weight) && !validFontWeight(node.font_weight)) {
    return outOfRange("font_weight");
  }
  if (present(node.strokes) && !validTextStrokes(node.strokes)) {
    return outOfRange("strokes");
  }
  if (present(node.shadows) && !validTextShadows(node.shadows)) {
    return outOfRange("shadows");
  }
  if (
    present(node.text_animators) &&
    !validTextAnimators(node.text_animators, true)
  ) {
    return outOfRange("text_animators");
  }
  return undefined;
}

/** Mirrors `layout::validate_text`. */
function layoutTextIssue(
  content: unknown,
  at: string,
): LayoutIssue | undefined {
  const keyframes = layoutAnimatedIssue(content, at, () => true);
  if (keyframes) return keyframes;
  if (
    isRecord(content) &&
    content.mode === "keyframes" &&
    (content.keyframes as readonly unknown[]).some(
      (keyframe) =>
        !isRecord(keyframe) ||
        !isRecord(keyframe.interpolation) ||
        keyframe.interpolation.type !== "hold",
    )
  ) {
    return { at, problem: LAYOUT_PROBLEM.textKeyframesInterpolate };
  }
  return layoutAnimatedIssue(
    content,
    at,
    (value) =>
      typeof value === "string" &&
      new TextEncoder().encode(value).length <= MAX_TEXT_CONTENT_BYTES,
  );
}

/** Mirrors `validate_shape_items`: the first problem of a content tree in document order. */
function shapeItemsIssue(items: unknown): ShapeIssue | undefined {
  const counted = { count: 0 };
  return shapeGroupIssue(items, "items", 1, counted);
}

function shapeGroupIssue(
  items: unknown,
  at: string,
  depth: number,
  counted: { count: number },
): ShapeIssue | undefined {
  if (depth > MAX_SHAPE_DEPTH) return { at, problem: SHAPE_PROBLEM.tooDeep };
  const list = Array.isArray(items) ? (items as readonly unknown[]) : [];
  for (const [index, item] of list.entries()) {
    const here = `${at}[${index}]`;
    counted.count += 1;
    if (counted.count > MAX_SHAPE_ITEMS) {
      return { at: here, problem: SHAPE_PROBLEM.tooManyItems };
    }
    const issue = shapeItemIssue(item, here, depth, counted);
    if (issue) return issue;
  }
  return undefined;
}

function shapeItemIssue(
  item: unknown,
  at: string,
  depth: number,
  counted: { count: number },
): ShapeIssue | undefined {
  if (!isRecord(item)) return { at, problem: SHAPE_PROBLEM.outOfRange };
  switch (item.type) {
    case "group":
      return (
        shapeTransformIssue(item.transform, `${at}.transform`) ??
        shapeGroupIssue(item.items, `${at}.items`, depth + 1, counted)
      );
    case "rectangle":
    case "ellipse":
    case "polystar":
    case "path":
      return shapeGeometryIssue(item, at);
    case "fill":
      return (
        animatedPaintIssue(item.paint, `${at}.paint`) ??
        shapeScalarIssue(item.opacity, `${at}.opacity`, unitValue)
      );
    case "stroke":
      return strokeIssue(item, at);
    case "trim":
      return (
        shapeScalarIssue(item.start, `${at}.start`, unitValue) ??
        shapeScalarIssue(item.end, `${at}.end`, unitValue) ??
        shapeScalarIssue(item.offset, `${at}.offset`, anyValue)
      );
    default:
      return { at, problem: SHAPE_PROBLEM.outOfRange };
  }
}

const unitValue = (value: number): boolean => value >= 0 && value <= 1;
const nonNegativeValue = (value: number): boolean => value >= 0;
const anyValue = (): boolean => true;

// Keyframe problems first, then values. A missing value takes its (valid) default.
function shapeScalarIssue(
  animation: unknown,
  at: string,
  check: (value: number) => boolean,
): ShapeIssue | undefined {
  if (animation === undefined) return undefined;
  const values = animationValues<unknown>(animation);
  if (values === undefined) {
    return { at, problem: SHAPE_PROBLEM.invalidKeyframes };
  }
  return values.every((value) => finiteNumber(value) && check(value))
    ? undefined
    : { at, problem: SHAPE_PROBLEM.outOfRange };
}

function shapeVec2Issue(
  animation: unknown,
  at: string,
  check: (value: Vec2) => boolean,
): ShapeIssue | undefined {
  if (animation === undefined) return undefined;
  const values = animationValues<unknown>(animation);
  if (values === undefined) {
    return { at, problem: SHAPE_PROBLEM.invalidKeyframes };
  }
  return values.every((value) => validVec2(value) && check(value as Vec2))
    ? undefined
    : { at, problem: SHAPE_PROBLEM.outOfRange };
}

/** Mirrors `valid_point_animation`. */
function validPointAnimation(animation: unknown): boolean {
  return shapePointIssue(animation, "") === undefined;
}

function shapePointIssue(
  animation: unknown,
  at: string,
): ShapeIssue | undefined {
  if (animation === undefined) return undefined;
  const values = animationValues<unknown>(animation);
  if (values === undefined) {
    return { at, problem: SHAPE_PROBLEM.invalidKeyframes };
  }
  const keyframes =
    isRecord(animation) && Array.isArray(animation.keyframes)
      ? (animation.keyframes as readonly unknown[])
      : [];
  const tangents = keyframes.every(
    (keyframe) =>
      isRecord(keyframe) &&
      (keyframe.in_tangent === undefined || validVec2(keyframe.in_tangent)) &&
      (keyframe.out_tangent === undefined || validVec2(keyframe.out_tangent)),
  );
  return tangents && values.every(validVec2)
    ? undefined
    : { at, problem: SHAPE_PROBLEM.outOfRange };
}

const nonNegativeSize = (value: Vec2): boolean => value.x >= 0 && value.y >= 0;

/** Mirrors `ShapeGeometry::validate`. */
function shapeGeometryIssue(
  geometry: Record<string, unknown>,
  at: string,
): ShapeIssue | undefined {
  switch (geometry.type) {
    case "rectangle":
      return (
        shapePointIssue(geometry.center, `${at}.center`) ??
        shapeVec2Issue(geometry.size, `${at}.size`, nonNegativeSize) ??
        shapeScalarIssue(
          geometry.corner_radius,
          `${at}.corner_radius`,
          nonNegativeValue,
        ) ??
        shapeScalarIssue(
          geometry.rotation_degrees,
          `${at}.rotation_degrees`,
          anyValue,
        )
      );
    case "ellipse":
      return (
        shapePointIssue(geometry.center, `${at}.center`) ??
        shapeVec2Issue(geometry.size, `${at}.size`, nonNegativeSize) ??
        shapeScalarIssue(
          geometry.rotation_degrees,
          `${at}.rotation_degrees`,
          anyValue,
        )
      );
    case "polystar":
      return (
        shapePointIssue(geometry.center, `${at}.center`) ??
        shapeScalarIssue(
          geometry.points,
          `${at}.points`,
          (points) =>
            points >= MIN_POLYSTAR_POINTS && points <= MAX_POLYSTAR_POINTS,
        ) ??
        shapeScalarIssue(
          geometry.outer_radius,
          `${at}.outer_radius`,
          nonNegativeValue,
        ) ??
        shapeScalarIssue(
          geometry.inner_radius,
          `${at}.inner_radius`,
          nonNegativeValue,
        ) ??
        shapeScalarIssue(
          geometry.outer_roundness,
          `${at}.outer_roundness`,
          unitValue,
        ) ??
        shapeScalarIssue(
          geometry.inner_roundness,
          `${at}.inner_roundness`,
          unitValue,
        ) ??
        shapeScalarIssue(
          geometry.rotation_degrees,
          `${at}.rotation_degrees`,
          anyValue,
        )
      );
    case "path":
      return animatedPathIssue(geometry.path, `${at}.path`);
    default:
      return { at, problem: SHAPE_PROBLEM.outOfRange };
  }
}

function shapeTransformIssue(
  transform: unknown,
  at: string,
): ShapeIssue | undefined {
  if (!isRecord(transform)) return undefined;
  return (
    shapeVec2Issue(transform.anchor, `${at}.anchor`, () => true) ??
    shapePointIssue(transform.position, `${at}.position`) ??
    shapeVec2Issue(transform.scale, `${at}.scale`, () => true) ??
    shapeScalarIssue(
      transform.rotation_degrees,
      `${at}.rotation_degrees`,
      anyValue,
    ) ??
    shapeScalarIssue(transform.opacity, `${at}.opacity`, unitValue)
  );
}

function strokeIssue(
  stroke: Record<string, unknown>,
  at: string,
): ShapeIssue | undefined {
  const issue =
    animatedPaintIssue(stroke.paint, `${at}.paint`) ??
    shapeScalarIssue(stroke.width, `${at}.width`, nonNegativeValue) ??
    shapeScalarIssue(stroke.opacity, `${at}.opacity`, unitValue);
  if (issue) return issue;
  const miter = stroke.miter_limit ?? 4;
  if (!finiteNumber(miter) || miter < 1) {
    return { at: `${at}.miter_limit`, problem: SHAPE_PROBLEM.outOfRange };
  }
  if (stroke.dash == null) return undefined;
  const dash = isRecord(stroke.dash) ? stroke.dash : {};
  const pattern = Array.isArray(dash.pattern)
    ? (dash.pattern as readonly unknown[])
    : [];
  const patternAt = `${at}.dash.pattern`;
  if (pattern.length === 0 || pattern.length > MAX_DASH_ENTRIES) {
    return { at: patternAt, problem: SHAPE_PROBLEM.invalidDash };
  }
  for (const [index, entry] of pattern.entries()) {
    const entryIssue = shapeScalarIssue(
      entry,
      `${patternAt}[${index}]`,
      nonNegativeValue,
    );
    if (entryIssue) return entryIssue;
  }
  // The pattern needs some positive entry to have a length to repeat over.
  const positive = pattern.some((entry) =>
    (animationValues<number>(entry) ?? []).some((value) => value > 0),
  );
  if (!positive) return { at: patternAt, problem: SHAPE_PROBLEM.invalidDash };
  return shapeScalarIssue(dash.offset, `${at}.dash.offset`, anyValue);
}

function pathIssue(path: unknown, at: string): ShapeIssue | undefined {
  const subpaths =
    isRecord(path) && Array.isArray(path.subpaths)
      ? (path.subpaths as readonly unknown[])
      : [];
  let vertices = 0;
  for (const subpath of subpaths) {
    if (isRecord(subpath) && Array.isArray(subpath.vertices)) {
      vertices += subpath.vertices.length;
    }
  }
  if (subpaths.length > MAX_PATH_SUBPATHS || vertices > MAX_PATH_VERTICES) {
    return { at, problem: SHAPE_PROBLEM.pathTooLarge };
  }
  const finite = subpaths.every(
    (subpath) =>
      isRecord(subpath) &&
      Array.isArray(subpath.vertices) &&
      (subpath.vertices as readonly unknown[]).every(
        (vertex) =>
          isRecord(vertex) &&
          validVec2(vertex.point) &&
          (vertex.in_tangent === undefined || validVec2(vertex.in_tangent)) &&
          (vertex.out_tangent === undefined || validVec2(vertex.out_tangent)),
      ),
  );
  return finite ? undefined : { at, problem: SHAPE_PROBLEM.outOfRange };
}

// Path keyframes must share each subpath's closing and vertex count.
function pathStructure(path: unknown): string {
  const subpaths =
    isRecord(path) && Array.isArray(path.subpaths)
      ? (path.subpaths as readonly unknown[])
      : [];
  return subpaths
    .map((subpath) => {
      const record = isRecord(subpath) ? subpath : {};
      const count = Array.isArray(record.vertices) ? record.vertices.length : 0;
      return `${record.closed === true}:${count}`;
    })
    .join(",");
}

function animatedPathIssue(
  animation: unknown,
  at: string,
): ShapeIssue | undefined {
  if (isRecord(animation) && animation.mode === "constant") {
    return pathIssue(animation.value, at);
  }
  const values = animationValues<unknown>(animation);
  if (values === undefined) {
    return { at, problem: SHAPE_PROBLEM.invalidKeyframes };
  }
  for (const value of values) {
    const issue = pathIssue(value, at);
    if (issue) return issue;
  }
  const first = pathStructure(values[0]);
  return values.every((value) => pathStructure(value) === first)
    ? undefined
    : { at, problem: SHAPE_PROBLEM.pathKeyframesDiffer };
}

// Paint keyframes must share kind, stop count and gradient spread.
function paintStructure(paint: unknown): string {
  const record = isRecord(paint) ? paint : {};
  const stops = Array.isArray(record.stops) ? record.stops.length : 1;
  const spread =
    record.type === "linear_gradient" || record.type === "radial_gradient"
      ? text(record.spread ?? "pad")
      : "";
  return `${text(record.type)}:${stops}:${spread}`;
}

function animatedPaintIssue(
  animation: unknown,
  at: string,
): ShapeIssue | undefined {
  if (isRecord(animation) && animation.mode === "constant") {
    return validPaint(animation.value)
      ? undefined
      : { at, problem: SHAPE_PROBLEM.outOfRange };
  }
  const values = animationValues<unknown>(animation);
  if (values === undefined) {
    return { at, problem: SHAPE_PROBLEM.invalidKeyframes };
  }
  if (!values.every(validPaint)) {
    return { at, problem: SHAPE_PROBLEM.outOfRange };
  }
  const first = paintStructure(values[0]);
  return values.every((value) => paintStructure(value) === first)
    ? undefined
    : { at, problem: SHAPE_PROBLEM.paintKeyframesDiffer };
}

function validPaint(paint: unknown): boolean {
  if (!isRecord(paint)) return false;
  switch (paint.type) {
    case "solid":
      return validRgba(paint.color);
    case "linear_gradient": {
      const { start, end } = paint;
      return (
        validVec2(start) &&
        validVec2(end) &&
        ((start as Vec2).x !== (end as Vec2).x ||
          (start as Vec2).y !== (end as Vec2).y) &&
        validStops(paint.stops)
      );
    }
    case "radial_gradient":
      return (
        validVec2(paint.center) &&
        finiteNumber(paint.radius) &&
        paint.radius > 0 &&
        validStops(paint.stops)
      );
    case "conic_gradient":
      return (
        validVec2(paint.center) &&
        finiteNumber(paint.angle_degrees ?? 0) &&
        validStops(paint.stops)
      );
    default:
      return false;
  }
}

function validStops(stops: unknown): boolean {
  if (
    !Array.isArray(stops) ||
    stops.length < 2 ||
    stops.length > MAX_GRADIENT_STOPS
  ) {
    return false;
  }
  let previous = 0;
  return stops.every((stop: unknown) => {
    if (!isRecord(stop) || !finiteNumber(stop.offset)) return false;
    const valid =
      stop.offset >= previous && stop.offset <= 1 && validRgba(stop.color);
    previous = stop.offset;
    return valid;
  });
}

function validRgba(color: unknown): boolean {
  return (
    isRecord(color) &&
    [color.red, color.green, color.blue, color.alpha].every(
      (value) => finiteNumber(value) && value >= 0 && value <= 1,
    )
  );
}

function nodeInputs(op: Record<string, unknown>): [string, unknown][] {
  switch (op.type) {
    case "effect": {
      // The model's map sorts by bytes; names are ASCII, so default string order matches.
      const named = isRecord(op.inputs) ? op.inputs : {};
      return [
        ["input", op.input],
        ...Object.keys(named)
          .sort()
          .map((name): [string, unknown] => [`inputs.${name}`, named[name]]),
      ];
    }
    case "transform":
    case "shape_mask":
      return [["input", op.input]];
    case "transition":
      return [
        ["from", op.from],
        ["to", op.to],
      ];
    case "matte":
      return [
        ["input", op.input],
        ["matte", op.matte],
      ];
    case "merge":
      return (Array.isArray(op.layers) ? op.layers : []).map(
        (layer: unknown, index) => [
          `layers[${index}]`,
          isRecord(layer) ? layer.input : undefined,
        ],
      );
    case "group":
      return (Array.isArray(op.inputs) ? op.inputs : []).map(
        (input: unknown, index) => [`inputs[${index}]`, input],
      );
    default:
      return [];
  }
}

// One entry per node: the composition it draws through a group or composition source, if any.
function drawnCompositions(
  composition: Record<string, unknown>,
  compositions: ReadonlyMap<string, Record<string, unknown>>,
  sources: ReadonlyMap<string, unknown>,
): (string | undefined)[] {
  return (Array.isArray(composition.nodes) ? composition.nodes : []).map(
    (node: unknown) => {
      let target: string | undefined;
      if (isRecord(node) && isRecord(node.op)) {
        if (node.op.type === "group") target = text(node.op.composition);
        else if (node.op.type === "media") {
          const source = sources.get(text(node.op.source_id));
          if (isRecord(source) && source.composition != null)
            target = text(source.composition);
        }
      }
      return target !== undefined && compositions.has(target)
        ? target
        : undefined;
    },
  );
}

// Mirrors the model's self-use check. An explicit stack keeps long chains off the call stack.
function compositionUsingItself(
  compositions: ReadonlyMap<string, Record<string, unknown>>,
  sources: ReadonlyMap<string, unknown>,
): string | undefined {
  const uses = (id: string): string[] => {
    const composition = compositions.get(id);
    return composition
      ? drawnCompositions(composition, compositions, sources).filter(
          (target): target is string => target !== undefined,
        )
      : [];
  };
  const state = new Map<string, "open" | "done">();
  for (const root of compositions.keys()) {
    if (state.has(root)) continue;
    state.set(root, "open");
    const stack: { id: string; next: string[]; at: number }[] = [
      { id: root, next: uses(root), at: 0 },
    ];
    while (stack.length > 0) {
      const top = stack[stack.length - 1];
      if (top.at === top.next.length) {
        state.set(top.id, "done");
        stack.pop();
        continue;
      }
      const target = top.next[top.at++];
      const mark = state.get(target);
      if (mark === "open") return target;
      if (mark === undefined) {
        state.set(target, "open");
        stack.push({ id: target, next: uses(target), at: 0 });
      }
    }
  }
  return undefined;
}

// Mirrors the model's inlined depth and node bounds. References must be acyclic. Within bounds,
// `nodes` is what each composition draws.
function compositionExpansion(
  compositions: ReadonlyMap<string, Record<string, unknown>>,
  sources: ReadonlyMap<string, unknown>,
): { problem?: string; nodes?: ReadonlyMap<string, number> } {
  const walked = new Map<string, { depth: number; nodes: number }>();
  let total = 0;
  for (const root of compositions.keys()) {
    const stack: [string, boolean][] = [[root, false]];
    while (stack.length > 0) {
      const [id, expanded] = stack.pop()!;
      if (walked.has(id)) continue;
      const drawn = drawnCompositions(
        compositions.get(id)!,
        compositions,
        sources,
      );
      if (!expanded) {
        stack.push([id, true]);
        for (const target of drawn)
          if (target !== undefined && !walked.has(target))
            stack.push([target, false]);
        continue;
      }
      let depth = 1;
      let nodes = 0;
      for (const target of drawn) {
        const inner =
          target === undefined ? { depth: 0, nodes: 1 } : walked.get(target)!;
        depth = Math.max(depth, inner.depth + 1);
        // Clamp so repeated doubling cannot reach Infinity.
        nodes = Math.min(nodes + inner.nodes, MAX_INLINED_NODES + 1);
      }
      walked.set(id, { depth, nodes });
    }
    const { depth, nodes } = walked.get(root)!;
    if (depth > MAX_COMPOSITION_DEPTH)
      return {
        problem: `composition ${root} nests compositions more than ${MAX_COMPOSITION_DEPTH} deep`,
      };
    total += nodes;
    if (total > MAX_INLINED_NODES)
      return {
        problem: `compositions draw more than ${MAX_INLINED_NODES} nodes, counting each group and composition source as the nodes it draws`,
      };
  }
  return {
    nodes: new Map([...walked].map(([id, { nodes }]) => [id, nodes])),
  };
}

function frameEdges(
  track: unknown,
  sources: ReadonlyMap<string, unknown>,
  inlined: ReadonlyMap<string, number>,
): FrameEdge[] {
  const nodes = (id: unknown): number =>
    typeof id === "string" ? (inlined.get(id) ?? 0) : 0;
  const edges: FrameEdge[] = [];
  if (inlined.size === 0 || !isRecord(track) || !Array.isArray(track.clips))
    return edges;
  for (const clip of track.clips as unknown[]) {
    if (!isRecord(clip) || !validRange(clip.timeline_range)) continue;
    const source = sources.get(text(clip.source_id));
    const drawn =
      nodes(isRecord(clip.composition) ? clip.composition.id : undefined) +
      nodes(isRecord(source) ? source.composition : undefined);
    if (drawn === 0) continue;
    const range = clip.timeline_range as { start: number; end: number };
    const id = text(clip.id);
    edges.push({ time: range.start, starts: true, nodes: drawn, clip: id });
    edges.push({ time: range.end, starts: false, nodes: drawn, clip: id });
  }
  return edges;
}

interface FrameEdge {
  readonly time: number;
  readonly starts: boolean;
  readonly nodes: number;
  readonly clip: string;
}

// Mirrors the model's per-frame inlined node bound and reports the first clip at the first
// instant past it.
function frameNodesOver(
  tracks: readonly (readonly FrameEdge[])[],
): string | undefined {
  const edges = tracks.flat();
  // Ends sort before starts at one instant (half-open ranges); stable sort keeps document order.
  edges.sort((a, b) => a.time - b.time || Number(a.starts) - Number(b.starts));
  let drawn = 0;
  for (let at = 0; at < edges.length;) {
    const edge = edges[at];
    if (!edge.starts) {
      drawn -= edge.nodes;
      at += 1;
      continue;
    }
    for (
      ;
      at < edges.length && edges[at].starts && edges[at].time === edge.time;
      at += 1
    )
      drawn += edges[at].nodes;
    if (drawn > MAX_FRAME_INLINED_NODES)
      return `the clips on the timeline where clip ${edge.clip} starts draw ${drawn} composition nodes; a frame draws at most ${MAX_FRAME_INLINED_NODES}`;
  }
  return undefined;
}

function compositionProblems(
  composition: unknown,
  sources: ReadonlyMap<string, unknown>,
  effectResources: ReadonlySet<string>,
  compositions: ReadonlyMap<string, Record<string, unknown>>,
  controlNamesOf: (target: Record<string, unknown>) => ReadonlySet<string>,
  builtinControlsOf: (
    target: Record<string, unknown>,
  ) => ReadonlyMap<string, BuiltinControl>,
  canvas: Canvas,
): string[] {
  if (!isRecord(composition)) return ["each composition must be an object"];
  const id = text(composition.id);
  const nodes = Array.isArray(composition.nodes)
    ? (composition.nodes as unknown[])
    : [];
  const problems: string[] = [];
  const byId = new Map<string, Record<string, unknown>>();
  for (const node of nodes) {
    if (!isRecord(node)) continue;
    const nodeId = text(node.id);
    if (byId.has(nodeId)) {
      problems.push(`composition ${id} has duplicate node id ${nodeId}`);
    }
    byId.set(nodeId, node);
  }
  const unit = (animation: unknown): boolean =>
    validScalarAnimation(animation, 0, 1);
  const ports = Array.isArray(composition.inputs) ? composition.inputs : [];
  const controls = Array.isArray(composition.controls)
    ? (composition.controls as unknown[])
    : [];
  const portNames = new Set(ports.map(text));
  const controlNames = new Set<string>();
  const settings = new Set<string>();
  const controlsValid = controls.every((control) => {
    if (!isRecord(control)) return false;
    const name = text(control.name);
    const param = text(control.param);
    const target = byId.get(text(control.node));
    const setting = `${text(control.node)}\u0000${param}`;
    const fresh = !controlNames.has(name) && !settings.has(setting);
    controlNames.add(name);
    settings.add(setting);
    return (
      name.length > 0 &&
      param.length > 0 &&
      fresh &&
      isRecord(target) &&
      isRecord(target.op) &&
      (target.op.type === "effect" || target.op.type === "transition")
    );
  });
  if (
    ports.length > MAX_GROUP_INPUTS ||
    portNames.size !== ports.length ||
    [...portNames].some((name) => name.length === 0) ||
    controls.length > MAX_COMPOSITION_CONTROLS ||
    !controlsValid
  )
    problems.push(`composition ${id} has invalid group inputs or controls`);
  else {
    for (const control of controls.filter(isRecord)) {
      const target = byId.get(text(control.node));
      const effect =
        isRecord(target) && isRecord(target.op) ? target.op.effect : undefined;
      const param = text(control.param);
      if (
        isRecord(effect) &&
        builtinRules(text(effect.id)) !== undefined &&
        builtinParamRule(text(effect.id), param) === undefined
      ) {
        problems.push(
          `effect ${text(effect.id)} in composition ${id} node ${text(control.node)} has no parameter ${param}`,
        );
        break;
      }
    }
  }
  for (const node of nodes) {
    if (!isRecord(node) || !isRecord(node.op)) continue;
    const nodeId = text(node.id);
    const op = node.op;
    const invalid = (): void => {
      problems.push(
        `node ${nodeId} of composition ${id} has an invalid setting`,
      );
    };
    const builtinProblem = (
      effect: unknown,
      kind: BuiltinEffectRules["kind"],
      effectInputs: unknown,
    ): void => {
      const problem = builtinEffectProblem(effect, kind, effectInputs);
      if (problem !== undefined && isRecord(effect)) {
        problems.push(
          `effect ${text(effect.id)} in composition ${id} node ${nodeId} ${problem}`,
        );
      }
    };
    const inputs = nodeInputs(op);
    if (
      node.bypass === true &&
      (["clip", "media", "generator", "group_input"].includes(text(op.type)) ||
        (op.type === "group" && inputs.length === 0))
    ) {
      invalid();
    }
    switch (op.type) {
      case "media": {
        const source = sources.get(text(op.source_id));
        if (
          !isRecord(source) ||
          !["video", "image", "generated", "composition"].includes(
            text(source.kind),
          )
        ) {
          problems.push(
            `node ${nodeId} of composition ${id} cannot show source ${text(op.source_id)}`,
          );
        } else if (op.start !== undefined && (op.start as number) < 0) {
          invalid();
        }
        break;
      }
      case "generator": {
        const issue = generatorShapeIssue(op.generator);
        const layout = generatorLayoutIssue(op.generator, canvas);
        if (issue) {
          problems.push(
            shapeProblem(`node ${nodeId} of composition ${id}`, issue),
          );
        } else if (layout) {
          problems.push(
            layoutProblem(`node ${nodeId} of composition ${id}`, layout),
          );
        } else if (!validGenerator(op.generator)) {
          invalid();
        }
        break;
      }
      case "effect":
        if (!validClipEffect(op.effect) || !validEffectInputs(op.inputs)) {
          invalid();
        } else {
          builtinProblem(op.effect, "effect", op.inputs);
        }
        break;
      case "transition":
        if (!validClipEffect(op.effect) || !unit(op.progress)) invalid();
        else builtinProblem(op.effect, "transition", undefined);
        break;
      case "transform":
        if (!validVisual(op.transform)) invalid();
        break;
      case "merge": {
        const layers = Array.isArray(op.layers) ? op.layers : [];
        if (
          layers.length > MAX_MERGE_LAYERS ||
          !layers.every(
            (layer: unknown) => isRecord(layer) && unit(layer.opacity),
          )
        ) {
          invalid();
        }
        break;
      }
      case "shape_mask": {
        const issue = masksIssue(op.masks);
        if (issue) {
          problems.push(
            shapeProblem(`node ${nodeId} of composition ${id}`, issue),
          );
        } else if (!validMaskCount(op.masks, "video")) {
          invalid();
        }
        break;
      }
      case "group": {
        const target = compositions.get(text(op.composition));
        if (!target) {
          problems.push(
            `group node ${nodeId} of composition ${id} names unknown composition ${text(op.composition)}`,
          );
          break;
        }
        const targetPorts = Array.isArray(target.inputs) ? target.inputs : [];
        const targetControls = controlNamesOf(target);
        const values = op.controls ?? {};
        if (
          !Array.isArray(op.inputs ?? []) ||
          inputs.length !== targetPorts.length ||
          !isRecord(values) ||
          !Object.entries(values).every(
            ([name, value]) =>
              targetControls.has(name) && validEffectParam(value),
          )
        ) {
          invalid();
          break;
        }
        for (const [name, value] of Object.entries(values)) {
          const control = builtinControlsOf(target).get(name);
          if (control && !acceptsBuiltinParam(control.rule, value)) {
            problems.push(
              `effect ${control.effect} in composition ${id} node ${nodeId} has a parameter ${control.param} of the wrong type or out of range`,
            );
            break;
          }
        }
        break;
      }
      case "group_input":
        if (
          !Number.isInteger(op.index) ||
          (op.index as number) < 0 ||
          (op.index as number) >= ports.length
        )
          invalid();
        break;
    }
    const effect = isRecord(op.effect) ? op.effect : undefined;
    if (effect && isRecord(effect.resources)) {
      for (const resource of Object.values(effect.resources)) {
        if (!effectResources.has(text(resource))) {
          problems.push(
            `node ${nodeId} of composition ${id} refers to unknown effect resource ${text(resource)}`,
          );
        }
      }
    }
    for (const [name, target] of inputs) {
      if (target != null && !byId.has(text(target))) {
        problems.push(
          `input ${name} of node ${nodeId} in composition ${id} names unknown node ${text(target)}`,
        );
      }
    }
  }
  const output = composition.output;
  if (output != null && !byId.has(text(output))) {
    problems.push(`composition ${id} outputs unknown node ${text(output)}`);
  }
  const state = new Map<string, "open" | "done">();
  const visit = (nodeId: string): boolean => {
    const mark = state.get(nodeId);
    if (mark === "open") return true;
    if (mark === "done") return false;
    state.set(nodeId, "open");
    const node = byId.get(nodeId);
    const op = node && isRecord(node.op) ? node.op : {};
    for (const [, target] of nodeInputs(op)) {
      if (target != null && byId.has(text(target)) && visit(text(target))) {
        return true;
      }
    }
    state.set(nodeId, "done");
    return false;
  };
  for (const nodeId of byId.keys()) {
    if (visit(nodeId)) {
      problems.push(`composition ${id} has a cycle`);
      break;
    }
  }
  return problems;
}

function validMaskCount(masks: unknown, trackKind: unknown): boolean {
  if (masks === undefined) return true;
  if (!Array.isArray(masks)) return false;
  if (masks.length === 0) return true;
  return trackKind === "video" && masks.length <= MAX_CLIP_MASKS;
}

/** Mirrors `validate_masks`. */
function masksIssue(masks: unknown): ShapeIssue | undefined {
  if (!Array.isArray(masks)) return undefined;
  for (const [index, mask] of (masks as readonly unknown[]).entries()) {
    const at = `masks[${index}]`;
    if (!isRecord(mask)) return { at, problem: SHAPE_PROBLEM.outOfRange };
    const outline = isRecord(mask.shape) ? mask.shape : {};
    const issue = shapeGeometryIssue(outline, `${at}.shape`);
    if (issue) return issue;
    if (mask.feather !== undefined && !validScalarAnimation(mask.feather, 0)) {
      return { at: `${at}.feather`, problem: SHAPE_PROBLEM.outOfRange };
    }
  }
  return undefined;
}

// A matte names another visual clip that has no matte and is in no transition.
function matteProblems(tracks: readonly Track[]): string[] {
  const visual = new Map<string, Record<string, unknown>>();
  const inTransition = new Set<string>();
  for (const track of tracks) {
    if (!isRecord(track) || track.kind !== "video") continue;
    for (const clip of Array.isArray(track.clips) ? track.clips : []) {
      if (isRecord(clip)) visual.set(text(clip.id), clip);
    }
    for (const transition of Array.isArray(track.transitions)
      ? track.transitions
      : []) {
      if (!isRecord(transition)) continue;
      inTransition.add(text(transition.from_clip));
      inTransition.add(text(transition.to_clip));
    }
  }
  const problems: string[] = [];
  for (const [id, clip] of visual) {
    if (clip.matte == null) continue;
    const source = isRecord(clip.matte) ? text(clip.matte.source) : "";
    const matteClip = visual.get(source);
    if (
      source === id ||
      matteClip === undefined ||
      matteClip.matte != null ||
      inTransition.has(source)
    ) {
      problems.push(`clip ${id} has an invalid matte`);
    }
  }
  return problems;
}

// Tracks whose ducking sidechain leads back to themselves. Each track names at most one sidechain,
// so each step is visited once over all walks. A repeated id means its first track, as in Rust.
function duckingCycles(tracks: readonly Track[]): ReadonlySet<string> {
  const sidechains = new Map<string, string | undefined>();
  for (const track of tracks) {
    if (!isRecord(track) || sidechains.has(text(track.id))) continue;
    const audio = track.audio;
    const ducking = isRecord(audio) ? audio.ducking : undefined;
    sidechains.set(
      text(track.id),
      isRecord(ducking) ? text(ducking.sidechain_track) : undefined,
    );
  }
  const cyclic = new Set<string>();
  // A walk that meets its own mark has closed a cycle.
  const walkOf = new Map<string, number>();
  let walk = 0;
  for (const start of sidechains.keys()) {
    walk += 1;
    let cursor: string | undefined = start;
    const path: string[] = [];
    while (cursor !== undefined && sidechains.has(cursor)) {
      const seen = walkOf.get(cursor);
      if (seen !== undefined) {
        if (seen === walk)
          for (const id of path.slice(path.indexOf(cursor))) cyclic.add(id);
        break;
      }
      walkOf.set(cursor, walk);
      path.push(cursor);
      cursor = sidechains.get(cursor);
    }
  }
  return cyclic;
}

/** What a {@link LintIssue} is about. */
export type LintOwner =
  | { readonly type: "clip"; readonly clip: string }
  | {
      readonly type: "transition";
      readonly track: string;
      readonly from_clip: string;
      readonly to_clip: string;
    }
  | { readonly type: "cue"; readonly track: string; readonly index: number }
  /** Keyed in composition time. */
  | {
      readonly type: "composition_node";
      readonly composition: string;
      readonly node: string;
    }
  /** Keyed in the generated picture's time. */
  | { readonly type: "source"; readonly source: string }
  /** Animators are keyed in each cue's own time. */
  | { readonly type: "caption_track"; readonly track: string }
  /** Times are from the cue's start. */
  | {
      readonly type: "cue_run";
      readonly track: string;
      readonly cue: number;
      readonly run: number;
    };

/** The shapes generator a shape {@link LintIssue} is about. */
export type ShapeLintOwner =
  | { readonly type: "source"; readonly source: string }
  | {
      readonly type: "node";
      readonly composition: string;
      readonly node: string;
    };

/** Something in a valid project that is probably a mistake. Checks are conservative. */
export type LintIssue =
  /** A keyframe no time its owner is shown at depends on. */
  | {
      readonly kind: "unreached_keyframe";
      readonly owner: LintOwner;
      /** Such as `visual.opacity`, `effect.params.radius` or `items[0].transform.opacity`. */
      readonly property: string;
      readonly time: number;
    }
  /** A clip covered for its whole time by an opaque full-frame clip above. */
  | { readonly kind: "hidden_clip"; readonly clip: string; readonly by: string }
  /** A transition no frame shows: shorter than a frame, or one frame long and off the grid. */
  | {
      readonly kind: "short_transition";
      readonly track: string;
      readonly from_clip: string;
      readonly to_clip: string;
    }
  /** A boundary between frames: its change shows at the next frame. */
  | {
      readonly kind: "off_frame_boundary";
      readonly owner: LintOwner;
      readonly edge: "start" | "end";
      readonly time: number;
    }
  /** A source no clip or reachable `media` node reads. */
  | { readonly kind: "unused_source"; readonly source: string }
  /** A high-dynamic-range source in a project whose color pipeline refuses HDR. */
  | { readonly kind: "hdr_source"; readonly source: string }
  /** A path's subpath without vertices, which draws nothing. */
  | {
      readonly kind: "empty_subpath";
      readonly owner: ShapeLintOwner;
      /** Such as `items[0].path.subpaths[1]`. */
      readonly at: string;
    }
  /** A fill or stroke with no geometry before it in its group, which paints nothing. */
  | {
      readonly kind: "paint_without_geometry";
      readonly owner: ShapeLintOwner;
      /** Such as `items[1].items[0]`. */
      readonly at: string;
    }
  /** A caption style no cue names and no cue's speaker matches. */
  | {
      readonly kind: "unused_caption_style";
      readonly track: string;
      readonly style: string;
    }
  /** A text animator that sets no property, which changes nothing. */
  | {
      readonly kind: "empty_text_animator";
      readonly owner: LintOwner;
      /** Such as `style.animators[0]`, or `root.children[1].text_animators[0]` in a layout. */
      readonly at: string;
    };

export interface LintBinding {
  lintProject(projectJson: string): string;
}

/**
 * A valid project's non-fatal diagnostics from the engine's `Project::lint`, in a fixed order:
 * unused sources, shapes generators, HDR sources, each track's clips, transitions and cues,
 * composition nodes, then generated sources' trees. Diagnostics never block a load or export.
 *
 * @throws The binding's error when the project does not load, as for `loadProject`.
 */
export function lintProject(
  binding: LintBinding,
  project: Project,
): LintIssue[] {
  return JSON.parse(
    binding.lintProject(JSON.stringify(project)),
  ) as LintIssue[];
}
