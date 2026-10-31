import {
  frozenValid,
  knownValid,
  rememberValid,
  withValidated,
} from "./validated.js";
import type { ExportFailureCode } from "alpha2/export-failure";
import {
  canonicalProject,
  validateProject,
  MAX_JSON_DEPTH,
  MAX_PROJECT_JSON_BYTES,
} from "alpha2/project";
import type { Canonical, CanonicalProject } from "alpha2/project";
import { CAPTIONS_ASSET_KIND, validCaptionsAuthoring } from "./captions.js";
import type {
  ClipTransition,
  MediaFingerprint,
  Project,
  Track,
  Clip,
  ProjectSettings,
  TimeRange,
  VisualProperties,
} from "alpha2/project";
/** A JSON value, as editor metadata and asset authoring data hold. */
export type Json =
  | null
  | boolean
  | number
  | string
  | readonly Json[]
  | { readonly [key: string]: Json };
/** A bin asset: what the editor knows about a project source or effect resource. */
export interface AssetDescriptor {
  /** Id of the project source or effect resource it describes. */
  readonly id: string;
  /** Source kind (`video`, `image`, `generated`, ...) or an editor-only kind like `captions`. */
  readonly kind: string;
  /** The display name, usually the imported file's name. */
  readonly name: string;
  /** A non-negative safe integer that changes whenever the content is replaced (a relink). */
  readonly contentRevision: number;
  /** Whether the asset's media is offline. */
  readonly missing?: boolean;
  /** Where the media is read from, when it is not kept in the document. */
  readonly locator?: string;
  /** Data the asset is built from, such as a text asset's style or a captions asset's cues. */
  readonly authoring?: Json;
  /** Evidence of the imported file, also recorded on the project source. */
  readonly fingerprint?: MediaFingerprint;
}
/**
 * A track's editor metadata. A `locked` track refuses every edit that changes what it renders; a
 * track with `enabled: false` draws and plays nothing (see {@link compileEditorDocument}).
 */
export interface TrackMetadata {
  readonly locked?: boolean;
  readonly enabled?: boolean;
  /** A `#rrggbb` color. */
  readonly color?: string;
  /** Whether a ripple edit on another track also moves this track's later content. */
  readonly syncLock?: boolean;
}
/** A clip's editor metadata. A clip without a `name` shows its source's name. */
export interface ClipMetadata {
  readonly name?: string;
}
/** A timeline marker: an instant, or a range when it has an `end`. */
export interface Marker {
  readonly id: string;
  /** Timeline ticks. */
  readonly time: number;
  /** End of a range marker; after `time`. */
  readonly end?: number;
  readonly name: string;
  readonly note?: string;
  /** A `#rrggbb` color. */
  readonly color?: string;
  readonly done?: boolean;
}
/** A node's place in the node editor, in canvas pixels. */
export interface GraphPosition {
  readonly x: number;
  readonly y: number;
}
/** Node positions by composition, then node. Editor metadata, not render input. */
export type GraphLayout = Readonly<
  Record<string, Readonly<Record<string, GraphPosition>>>
>;
/** What the editor keeps beside the engine project. None of it is render input. */
export interface EditorMetadata {
  /** By track id. */
  readonly tracks: Readonly<Record<string, TrackMetadata>>;
  /** By clip id. */
  readonly clips: Readonly<Record<string, ClipMetadata>>;
  /**
   * Stable ids for list items the project stores without one, by list (`<trackId>/cues`,
   * `<trackId>/transitions`, `<clipId>/masks`). Maintained by {@link withInstanceIds}.
   */
  readonly instanceIds: Readonly<Record<string, readonly string[]>>;
  readonly markers: readonly Marker[];
  /** Default export range in timeline ticks; `null` for the whole timeline. */
  readonly exportRange: TimeRange | null;
  readonly graphLayout?: GraphLayout;
  /** Names of compositions drawn as groups, by composition id. */
  readonly graphNames?: Readonly<Record<string, string>>;
}
/** An editor project file: the engine project plus editor metadata and bin assets. */
export interface EditorDocument {
  readonly format: "alpha2-editor-project";
  readonly version: 1;
  /**
   * The engine project in the forms the engine writes ({@link CanonicalProject}): component colors,
   * tagged paints, four-sided padding and margin, `{ x, y }` offsets. Every document entering the
   * editor is converted to this form, so readers never see CSS hex or a bare color as a paint.
   */
  readonly project: CanonicalProject;
  readonly metadata: EditorMetadata;
  readonly assets: Readonly<Record<string, AssetDescriptor>>;
}
/**
 * The export failure class each editor error code implies (`alpha2/export-failure`). Refusals of
 * the document or its media are `INVALID_INPUT` (a retry meets them again), budget and storage
 * refusals say so, and states that may pass (a busy export, a pending conform) are `UNKNOWN`.
 * `tests/cases/failure-classes.json` holds the same table, and a test checks they agree.
 */
const EDITOR_FAILURE_CODES: Readonly<Record<string, ExportFailureCode>> =
  Object.freeze({
    audio_unavailable: "UNKNOWN",
    cancel_failed: "UNKNOWN",
    cancelled: "CANCELLED",
    conform_failed: "INVALID_INPUT",
    conform_pending: "UNKNOWN",
    disposed: "UNKNOWN",
    duplicate_resource: "INVALID_INPUT",
    effect_unavailable: "INVALID_INPUT",
    effect_conflict: "INVALID_INPUT",
    empty_export: "INVALID_INPUT",
    export_busy: "UNKNOWN",
    invalid_asset: "INVALID_INPUT",
    invalid_captions: "INVALID_INPUT",
    invalid_command: "INVALID_INPUT",
    invalid_document: "INVALID_INPUT",
    invalid_effect: "INVALID_INPUT",
    invalid_effect_resource: "INVALID_INPUT",
    invalid_font: "INVALID_INPUT",
    invalid_frame_rate: "INVALID_INPUT",
    invalid_metadata: "INVALID_INPUT",
    invalid_policy: "INVALID_INPUT",
    invalid_preview_scale: "INVALID_INPUT",
    invalid_project: "INVALID_INPUT",
    invalid_projection: "INVALID_INPUT",
    invalid_rate: "INVALID_INPUT",
    invalid_text: "INVALID_INPUT",
    invalid_time: "INVALID_INPUT",
    invalid_transition: "INVALID_INPUT",
    invalid_video_conversion: "INVALID_INPUT",
    invalid_video_decode: "INVALID_INPUT",
    locked: "INVALID_INPUT",
    missing_asset: "INVALID_INPUT",
    preview_unavailable: "DEVICE_LOST",
    preview_worker_failed: "UNKNOWN",
    provider_stalled: "RESOURCE_LIMIT",
    proxy_busy: "UNKNOWN",
    relink_conflict: "INVALID_INPUT",
    resource_budget: "RESOURCE_LIMIT",
    resource_limit: "RESOURCE_LIMIT",
    sequence_gap: "INVALID_INPUT",
    sequence_size: "INVALID_INPUT",
    stale_gesture: "UNKNOWN",
    stale_import: "UNKNOWN",
    storage_quota_exceeded: "STORAGE_QUOTA_EXCEEDED",
    sync_conflict: "INVALID_INPUT",
    text_unrenderable: "INVALID_INPUT",
    too_large: "INVALID_INPUT",
    unreadable_media: "INVALID_INPUT",
    unsupported_media: "INVALID_INPUT",
    unsupported_version: "INVALID_INPUT",
  });

/** An editor refusal or failure, with a machine-readable `code` (a snake_case identifier). */
export class EditorError extends Error {
  /**
   * @param code What went wrong, such as `invalid_command` or `locked`.
   * @param message A human-readable description.
   * @param operation The operation that failed, such as `validate`, `open` or a command name.
   * @param entityId The track, clip, asset or other entity the error concerns, when there is one.
   * @param recoverable Whether the editor stays usable after the error. Defaults to `true`.
   */
  constructor(
    readonly code: string,
    message: string,
    readonly operation: string,
    readonly entityId?: string,
    readonly recoverable = true,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "EditorError";
  }

  /**
   * The export failure class this error implies, read by `classifyBrowserFailure`. `UNKNOWN` for
   * a code without a class.
   */
  get failureCode(): ExportFailureCode {
    return Object.hasOwn(EDITOR_FAILURE_CODES, this.code)
      ? EDITOR_FAILURE_CODES[this.code]
      : "UNKNOWN";
  }
}
/** The outcome of an operation that reports its refusal instead of throwing it. */
export type Result<T> =
  | { readonly ok: true; readonly value: T }
  | { readonly ok: false; readonly error: EditorError };
/** Deep-freezes `value` in place and returns it. Already frozen objects are not descended into. */
export function freeze<T>(value: T): T {
  // Iterative, so values nested deeper than the stack are frozen too.
  const pending: unknown[] = [value];
  const sealing: object[] = [];
  while (pending.length) {
    const next = pending.pop();
    if (!next || typeof next !== "object" || Object.isFrozen(next)) continue;
    sealing.push(next);
    // Skip frozen children: most of an edit's result is frozen parts it kept.
    for (const child of Object.values(next))
      if (child && typeof child === "object" && !Object.isFrozen(child))
        pending.push(child);
  }
  for (const object of sealing) Object.freeze(object);
  return value;
}
/**
 * The deepest array and object nesting an editor document may have: the engine's project limit
 * ({@link MAX_JSON_DEPTH}) plus one for the document that holds it.
 */
export const MAX_DOCUMENT_JSON_DEPTH = MAX_JSON_DEPTH + 1;
/**
 * Throws an `EditorError` (`invalid_document`) unless `value` is plain JSON (finite numbers, arrays
 * and plain objects) nested at most `maxDepth` deep ({@link MAX_DOCUMENT_JSON_DEPTH} by default).
 */
export function assertJson(
  value: unknown,
  maxDepth = MAX_DOCUMENT_JSON_DEPTH,
): void {
  const pending: [unknown, number][] = [[value, 0]];
  while (pending.length) {
    const [next, depth] = pending.pop()!;
    if (next === null || typeof next === "string" || typeof next === "boolean")
      continue;
    if (typeof next === "number" && Number.isFinite(next)) continue;
    const container =
      Array.isArray(next) ||
      (typeof next === "object" &&
        Object.getPrototypeOf(next) === Object.prototype);
    if (!container)
      throw new EditorError(
        "invalid_document",
        "Document must contain finite JSON values",
        "validate",
      );
    if (depth >= maxDepth)
      throw new EditorError(
        "invalid_document",
        `Document nests deeper than ${maxDepth} levels`,
        "validate",
      );
    for (const child of Object.values(next as object))
      pending.push([child, depth + 1]);
  }
}
/**
 * Throws an `EditorError` unless `document` is a valid editor document: finite JSON within the
 * engine and editor size limits, a supported format and version, a project the engine accepts,
 * and well-formed editor metadata and assets.
 */
export function validateDocument(document: EditorDocument): void {
  assertJson(document);
  if (!isRecord(document))
    throw new EditorError(
      "invalid_document",
      "An editor document is an object",
      "validate",
    );
  // Size first, since the engine reads no project over its budget.
  assertDocumentSize(document);
  validateDocumentShape(document);
  assertEditorMetadata(document);
  assertProjection(document);
}
/**
 * Throws the first `invalid_projection` error of `document`'s projection (the project without
 * disabled tracks), so a document that opens is one the engine renders.
 */
export function assertProjection(document: EditorDocument): void {
  if (
    !document.project.timeline.tracks.some(
      (t) => document.metadata.tracks[t.id]?.enabled === false,
    )
  )
    return;
  const [error] = compileEditorDocument(document).diagnostics;
  if (error) throw error;
}
/**
 * The checks a gesture's candidate needs on every pointer move: the format and the project's
 * rules. The rest of `validateDocument` walks or serializes the whole document, so a gesture runs
 * it once at commit. `validateProject` reuses its results for frozen parts the candidate shares
 * with the gesture's start. Throws an `EditorError` (`unsupported_version` or `invalid_project`).
 */
export function validateDocumentShape(document: EditorDocument): void {
  if (document.format !== "alpha2-editor-project" || document.version !== 1)
    throw new EditorError(
      "unsupported_version",
      "Unsupported editor format/version",
      "validate",
    );
  if (knownValid(document.project)) return;
  const errors = validateProject(document.project);
  if (errors.length)
    throw new EditorError("invalid_project", errors.join("\n"), "validate");
  rememberValid(document.project);
}
const isRecord = (value: unknown): value is Readonly<Record<string, unknown>> =>
  typeof value === "object" && value !== null && !Array.isArray(value);
const optionalString = (value: unknown) =>
  value === undefined || typeof value === "string";
const optionalBoolean = (value: unknown) =>
  value === undefined || typeof value === "boolean";
/** The largest editor file {@link parseDocument} opens; every valid document fits in it. */
export const MAX_EDITOR_FILE_BYTES = 128 * 1024 * 1024;
const utf8Length = (text: string) => new TextEncoder().encode(text).length;
function assertDocumentSize(document: EditorDocument): void {
  if (utf8Length(JSON.stringify(document.project)) > MAX_PROJECT_JSON_BYTES)
    throw new EditorError(
      "too_large",
      "Engine JSON exceeds its size limit",
      "validate",
    );
  if (utf8Length(JSON.stringify(document)) > MAX_EDITOR_FILE_BYTES)
    throw new EditorError(
      "too_large",
      "Editor document exceeds the 128 MiB file limit",
      "validate",
    );
}
/**
 * The only color form accepted for markers and tracks, since a view paints it and other CSS values
 * may load a resource.
 */
const COLOR = /^#[0-9a-f]{6}$/i;
const isColor = (value: unknown) =>
  typeof value === "string" && COLOR.test(value);
function assertEditorMetadata(document: EditorDocument): void {
  if (
    !document.metadata?.tracks ||
    !document.metadata.clips ||
    !document.metadata.instanceIds ||
    !Array.isArray(document.metadata.markers) ||
    !isRecord(document.assets)
  )
    throw new EditorError(
      "invalid_metadata",
      "Missing editor metadata",
      "validate",
    );
  // A non-string here would crash the whole page that renders it.
  const { tracks, clips, instanceIds } = document.metadata;
  if (!isRecord(tracks) || !isRecord(clips) || !isRecord(instanceIds))
    throw new EditorError(
      "invalid_metadata",
      "Editor metadata maps must be objects",
      "validate",
    );
  for (const [id, track] of Object.entries(tracks))
    if (
      !isRecord(track) ||
      !optionalBoolean(track.locked) ||
      !optionalBoolean(track.enabled) ||
      !optionalBoolean(track.syncLock) ||
      !(track.color === undefined || isColor(track.color))
    )
      throw new EditorError(
        "invalid_metadata",
        "Invalid track metadata",
        "validate",
        id,
      );
  for (const [id, clip] of Object.entries(clips))
    if (!isRecord(clip) || !optionalString(clip.name))
      throw new EditorError(
        "invalid_metadata",
        "Invalid clip metadata",
        "validate",
        id,
      );
  for (const [key, ids] of Object.entries(instanceIds))
    if (
      !Array.isArray(ids) ||
      !ids.every((id) => typeof id === "string") ||
      new Set(ids).size !== ids.length
    )
      throw new EditorError(
        "invalid_metadata",
        "Invalid instance ids",
        "validate",
        key,
      );
  const { graphLayout, graphNames } = document.metadata;
  if (
    graphLayout !== undefined &&
    !(
      isRecord(graphLayout) &&
      Object.values(graphLayout).every(
        (positions) =>
          isRecord(positions) &&
          Object.values(positions).every(
            (p) => isRecord(p) && Number.isFinite(p.x) && Number.isFinite(p.y),
          ),
      )
    )
  )
    throw new EditorError(
      "invalid_metadata",
      "Node positions must be finite x and y by composition and node",
      "validate",
    );
  if (
    graphNames !== undefined &&
    !(
      isRecord(graphNames) &&
      Object.values(graphNames).every((name) => typeof name === "string")
    )
  )
    throw new EditorError(
      "invalid_metadata",
      "Group names must be text by composition",
      "validate",
    );
  // A transition's id is the edge it joins, so a command can name the transition it means.
  for (const track of document.project.timeline.tracks) {
    const ids = instanceIds[`${track.id}/transitions`] ?? [];
    const transitions = track.transitions ?? [];
    if (
      ids.length !== transitions.length ||
      transitions.some((t, i) => ids[i] !== transitionInstanceId(track.id, t))
    )
      throw new EditorError(
        "invalid_metadata",
        "Transition ids must name the clips each joins",
        "validate",
        track.id,
      );
  }
  for (const [id, asset] of Object.entries(document.assets)) {
    if (
      !isRecord(asset) ||
      asset.id !== id ||
      !Number.isSafeInteger(asset.contentRevision) ||
      asset.contentRevision < 0
    )
      throw new EditorError(
        "invalid_asset",
        "Invalid asset identity/revision",
        "validate",
        id,
      );
    if (
      asset.kind === CAPTIONS_ASSET_KIND &&
      !validCaptionsAuthoring(asset.authoring)
    )
      throw new EditorError(
        "invalid_asset",
        "Invalid captions: each row must be a list of cues",
        "validate",
        id,
      );
    // `authoring.text` is a text asset's style; other assets hold `null` or a file's contents.
    const text = isRecord(asset.authoring) ? asset.authoring.text : undefined;
    if (
      typeof asset.name !== "string" ||
      typeof asset.kind !== "string" ||
      !optionalString(asset.locator) ||
      !optionalBoolean(asset.missing) ||
      (isRecord(text) &&
        (!optionalString(text.content) || !optionalString(text.fontFamily)))
    )
      throw new EditorError(
        "invalid_asset",
        "Invalid asset name, kind, locator, offline flag or text",
        "validate",
        id,
      );
  }
  const markerIds = new Set<string>();
  for (const marker of document.metadata.markers as EditorMetadata["markers"]) {
    if (!isRecord(marker))
      throw new EditorError("invalid_metadata", "Invalid marker", "validate");
    if (
      !Number.isSafeInteger(marker.time) ||
      marker.time < 0 ||
      (marker.end !== undefined &&
        (!Number.isSafeInteger(marker.end) || marker.end <= marker.time))
    )
      throw new EditorError(
        "invalid_time",
        "Invalid marker time",
        "validate",
        marker.id,
      );
    if (
      typeof marker.id !== "string" ||
      markerIds.has(marker.id) ||
      typeof marker.name !== "string" ||
      (marker.note !== undefined && typeof marker.note !== "string") ||
      (marker.color !== undefined && !isColor(marker.color)) ||
      (marker.done !== undefined && typeof marker.done !== "boolean")
    )
      throw new EditorError(
        "invalid_metadata",
        "Invalid marker",
        "validate",
        marker.id,
      );
    markerIds.add(marker.id);
  }
  const range = document.metadata.exportRange;
  if (
    range &&
    (!Number.isSafeInteger(range.start) ||
      !Number.isSafeInteger(range.end) ||
      range.start < 0 ||
      range.end <= range.start)
  )
    throw new EditorError("invalid_time", "Invalid export range", "validate");
}
/**
 * A new frozen document holding a copy of `project`, with an asset for each source and effect
 * resource (offline unless generated or a composition), overridden by `assets`. Throws an
 * `EditorError` when the result does not validate.
 */
export function createDocument(
  project: Project,
  assets: EditorDocument["assets"] = {},
): EditorDocument {
  return withValidated(() => {
    assertJson(project, MAX_JSON_DEPTH);
    // Freeze before validation so validated parts are reused when edits check them.
    const copy = freeze(canonicalProject(structuredClone(project)));
    if (!knownValid(project)) {
      const errors = validateProject(copy);
      if (errors.length)
        throw new EditorError("invalid_project", errors.join("\n"), "validate");
    }
    rememberValid(copy);
    return documentOf(copy, assets);
  });
}
function documentOf(
  project: CanonicalProject,
  assets: EditorDocument["assets"],
): EditorDocument {
  const document: EditorDocument = {
    format: "alpha2-editor-project",
    version: 1,
    project,
    assets: {
      ...Object.fromEntries(
        [...project.sources, ...(project.effect_resources ?? [])].map(
          (source) => [
            source.id,
            {
              id: source.id,
              kind: source.kind,
              name: source.id,
              contentRevision: 0,
              missing:
                source.kind !== "generated" && source.kind !== "composition",
            },
          ],
        ),
      ),
      ...structuredClone(assets),
    },
    metadata: {
      tracks: {},
      clips: {},
      instanceIds: {},
      markers: [],
      exportRange: null,
    },
  };
  const named = withInstanceIds(document);
  validateDocument(named);
  return frozenValid(named);
}
/** A new document with no media: 1920x1080 at 30 fps and 48 kHz unless `settings` say otherwise. */
export function createEmptyDocument(
  settings: Partial<ProjectSettings> = {},
): EditorDocument {
  return createDocument({
    schema_version: 1,
    id: "project",
    name: "Untitled project",
    settings: {
      width: 1920,
      height: 1080,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
      ...settings,
    },
    sources: [],
    timeline: { tracks: [] },
  });
}
/**
 * Serializes `document` as compact JSON that {@link parseDocument} opens. Throws an `EditorError`
 * when the document does not validate.
 */
export function serializeDocument(document: EditorDocument): string {
  validateDocument(document);
  return JSON.stringify(document);
}
/**
 * Opens an editor file, or a bare engine project (JSON with `schema_version`) as a new document,
 * frozen. Every refusal is an `EditorError`: `too_large` past {@link MAX_EDITOR_FILE_BYTES},
 * `invalid_document` for text that is not an editor document, else the code of the failed check.
 */
export function parseDocument(text: string): EditorDocument {
  if (utf8Length(text) > MAX_EDITOR_FILE_BYTES)
    throw new EditorError("too_large", "Editor file exceeds 128 MiB", "open");
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch (cause) {
    throw new EditorError(
      "invalid_document",
      `Editor file is not JSON: ${cause instanceof Error ? cause.message : String(cause)}`,
      "open",
      undefined,
      true,
      { cause },
    );
  }
  try {
    if (isRecord(data) && "schema_version" in data)
      return createDocument(data as unknown as Project);
    const document = freeze(
      withInstanceIds(canonicalDocument(data as EditorDocument)),
    );
    validateDocument(document);
    return frozenValid(document);
  } catch (cause) {
    if (cause instanceof EditorError) throw cause;
    // An unforeseen shape is still a fault of the file, not the editor.
    throw new EditorError(
      "invalid_document",
      `Editor file could not be read: ${cause instanceof Error ? cause.message : String(cause)}`,
      "open",
      undefined,
      true,
      { cause },
    );
  }
}
/**
 * The id of `transition` on track `trackId`: the edge it joins (a track has one transition per
 * edge). It is stable while other transitions change, and changes with its clips (a split hands
 * an outgoing transition to the right part).
 */
export function transitionInstanceId(
  trackId: string,
  transition: Pick<ClipTransition, "from_clip" | "to_clip">,
): string {
  return `${trackId}/transitions/${encodeURIComponent(transition.from_clip)}/${encodeURIComponent(transition.to_clip)}`;
}
/**
 * `document` with its project in the forms the engine writes ({@link canonicalProject}). Returns
 * `document` itself when already canonical and shares unchanged parts. A document of the wrong
 * shape is left for validation to refuse.
 */
export function canonicalDocument(
  document: Omit<EditorDocument, "project"> & { readonly project: Project },
): EditorDocument {
  const project: unknown = document?.project;
  if (!isRecord(project)) return document as EditorDocument;
  const canonical = canonicalProject(project as unknown as Project);
  return canonical === project
    ? (document as EditorDocument)
    : { ...document, project: canonical };
}
/** Frozen tracks and clips {@link withInstanceIds} found fully named in a frozen `instanceIds`. */
const namedIn = new WeakMap<object, unknown>();
/**
 * `document` with an instance id for every caption cue, transition and clip mask. Transitions are
 * named by {@link transitionInstanceId}; cues and masks keep their ids and new ones get unique ids.
 * Returns `document` itself when nothing changes, and leaves a malformed document for validation.
 */
export function withInstanceIds(document: EditorDocument): EditorDocument {
  const tracks: unknown = document?.project?.timeline?.tracks;
  const original: unknown = document?.metadata?.instanceIds;
  if (!Array.isArray(tracks) || !isRecord(original)) return document;
  // Checked once, since the check is linear in the record's keys.
  const lasting = Object.isFrozen(original);
  // Copied on the first change; a copy per changed key would be O(n^2) over n clips.
  let changed: Record<string, unknown> | undefined;
  const current = (key: string): unknown => (changed ?? original)[key];
  const set = (key: string, values: readonly string[]) => {
    const previous = current(key);
    if (
      Array.isArray(previous) &&
      previous.length === values.length &&
      previous.every((id, i) => id === values[i])
    )
      return;
    (changed ??= { ...original })[key] = values;
  };
  /** The ids a list of `count` keeps; new ones are `<key>/<index>`, numbered past taken ones. */
  const kept = (key: string, count: number): string[] => {
    const previous = current(key);
    const ids = (Array.isArray(previous) ? previous : [])
      .slice(0, count)
      .filter((id): id is string => typeof id === "string");
    const taken = new Set<string>();
    const unique = ids.filter((id) => !taken.has(id) && !!taken.add(id));
    for (let i = unique.length; i < count; i += 1) {
      let id = `${key}/${i}`;
      for (let n = 2; taken.has(id); n += 1) id = `${key}/${i}~${n}`;
      taken.add(id);
      unique.push(id);
    }
    return unique;
  };
  for (const track of tracks as Track[]) {
    if (!isRecord(track) || typeof track.id !== "string") return document;
    if (changed === undefined && namedIn.get(track) === original) continue;
    const transitions: unknown = track.transitions ?? [];
    const clips: unknown = track.clips ?? [];
    const cues: unknown = track.captions?.cues ?? [];
    if (
      !Array.isArray(transitions) ||
      !transitions.every(isRecord) ||
      !Array.isArray(clips) ||
      !clips.every(isRecord) ||
      !Array.isArray(cues)
    )
      return document;
    let frozen =
      lasting &&
      [
        track,
        track.transitions,
        track.clips,
        track.captions,
        track.captions?.cues,
      ].every((part) => part === undefined || Object.isFrozen(part)) &&
      transitions.every(Object.isFrozen);
    const unchanged = changed === undefined;
    set(`${track.id}/cues`, kept(`${track.id}/cues`, cues.length));
    set(
      `${track.id}/transitions`,
      (transitions as unknown as ClipTransition[]).map((t) =>
        transitionInstanceId(track.id, t),
      ),
    );
    for (const clip of clips as unknown as Clip[]) {
      const masks: unknown = clip.masks ?? [];
      frozen &&=
        Object.isFrozen(clip) &&
        (clip.masks === undefined || Object.isFrozen(clip.masks));
      if (!Array.isArray(masks)) continue;
      if (changed === undefined && namedIn.get(clip) === original) continue;
      const key = `${clip.id}/masks`;
      const previous = current(key);
      if (masks.length > 0 || !Array.isArray(previous) || previous.length)
        set(key, kept(key, masks.length));
      if (
        changed === undefined &&
        lasting &&
        Object.isFrozen(clip) &&
        (clip.masks === undefined || Object.isFrozen(clip.masks))
      )
        namedIn.set(clip, original);
    }
    if (frozen && unchanged && changed === undefined)
      namedIn.set(track, original);
  }
  return changed === undefined
    ? document
    : {
        ...document,
        metadata: {
          ...document.metadata,
          instanceIds: changed as EditorMetadata["instanceIds"],
        },
      };
}
const indexes = new WeakMap<object, EntityIndex<Track>>();
/** A document's tracks and clips by id, in the engine's canonical forms by default. */
export interface EntityIndex<T extends Track = Canonical<Track>> {
  readonly tracks: ReadonlyMap<string, T>;
  readonly clips: ReadonlyMap<
    string,
    { readonly clip: NonNullable<T["clips"]>[number]; readonly trackId: string }
  >;
}
/** The {@link EntityIndex} of `document`, built once per document object and cached. */
export function indexDocument<D extends { readonly project: Project }>(
  document: D,
): EntityIndex<D["project"]["timeline"]["tracks"][number]> {
  const known = indexes.get(document);
  if (known) return known;
  const tracks = new Map<string, Track>();
  const clips = new Map<string, { clip: Clip; trackId: string }>();
  for (const track of document.project.timeline.tracks) {
    tracks.set(track.id, track);
    for (const clip of track.clips ?? [])
      clips.set(clip.id, { clip, trackId: track.id });
  }
  const index = { tracks, clips };
  indexes.set(document, index);
  return index;
}
/** What the engine renders of a document. */
export interface CompiledEditorDocument {
  /**
   * The project as the engine renders it: disabled tracks left out except for mattes enabled clips
   * name (see `compileEditorDocument`).
   */
  readonly project: Project;
  /** The engine's validation errors for `project`, as `invalid_projection` errors. */
  readonly diagnostics: readonly EditorError[];
  /** The projection's JSON, serialized on first read. Equal keys render the same. */
  readonly revisionKey: string;
}
// A frozen document never changes, so its projection is computed once.
const compiled = new WeakMap<EditorDocument, CompiledEditorDocument>();
/**
 * The project the engine renders for `document`; see `projectedTracks` for how disabled tracks
 * project. Never throws for an invalid projection; the errors are in `diagnostics`. Cached for
 * frozen documents.
 */
export function compileEditorDocument(
  document: EditorDocument,
): CompiledEditorDocument {
  const cached = compiled.get(document);
  if (cached) return cached;
  const result = compileProjection(document);
  if (Object.isFrozen(document)) compiled.set(document, result);
  return result;
}
/** End of `project`'s timeline in ticks: its last clip or caption cue, or 0 when empty. */
export function sequenceEnd(project: Project): number {
  let end = 0;
  for (const track of project.timeline.tracks) {
    for (const clip of track.clips ?? [])
      end = Math.max(end, clip.timeline_range.end);
    for (const cue of track.captions?.cues ?? [])
      end = Math.max(end, cue.timeline_range.end);
  }
  return end;
}
/**
 * End of `document`'s program in ticks: the {@link sequenceEnd} of its projection, so disabled
 * tracks add no length. Playback and export without an export range stop there, and so does a
 * range with only its start set.
 */
export function programEnd(document: EditorDocument): number {
  return sequenceEnd(compileEditorDocument(document).project);
}
/**
 * The tracks the engine renders of `document`. A disabled track draws and plays nothing, but keeps
 * the clips enabled tracks name as mattes, muted. Mattes only set coverage and are never
 * composited, so nothing of the track draws. A track that ducks under a disabled track does not
 * duck. A clip only disabled clips name as their matte stays a matte: not drawn, still playing.
 */
function projectedTracks(document: EditorDocument): readonly Track[] {
  const tracks = document.project.timeline.tracks;
  const disabled = new Set(
    tracks
      .filter((t) => document.metadata.tracks[t.id]?.enabled === false)
      .map((t) => t.id),
  );
  if (!disabled.size) return tracks;
  const named = (enabled: boolean) =>
    new Set(
      tracks
        .filter((t) => disabled.has(t.id) !== enabled)
        .flatMap((t) => (t.clips ?? []).flatMap((c) => c.matte?.source ?? [])),
    );
  const mattes = named(true);
  // Named only by hidden clips, which the projection drops, so the engine would draw it plainly.
  const orphaned = new Set([...named(false)].filter((id) => !mattes.has(id)));
  return tracks.flatMap((track): Track[] => {
    if (disabled.has(track.id)) {
      const clips = (track.clips ?? []).filter((c) => mattes.has(c.id));
      return clips.length
        ? [
            {
              id: track.id,
              name: track.name,
              kind: track.kind,
              clips,
              audio: { muted: true },
            },
          ]
        : [];
    }
    let projected = track;
    if (orphaned.size && track.clips?.some((c) => orphaned.has(c.id)))
      projected = {
        ...projected,
        clips: track.clips.map((c) =>
          orphaned.has(c.id)
            ? {
                ...c,
                visual: {
                  ...(c.visual ?? DEFAULT_VISUAL),
                  opacity: { mode: "constant", value: 0 },
                },
              }
            : c,
        ),
      };
    const sidechain = track.audio?.ducking?.sidechain_track;
    if (sidechain != null && disabled.has(sidechain))
      projected = {
        ...projected,
        audio: { ...projected.audio, ducking: null },
      };
    return [projected];
  });
}
/** A clip's visual properties when it has none (the engine's `VisualProperties::default`). */
const DEFAULT_VISUAL: Required<VisualProperties> = {
  position: { mode: "constant", value: { x: 0.5, y: 0.5 } },
  anchor: { x: 0.5, y: 0.5 },
  scale: { mode: "constant", value: { x: 1, y: 1 } },
  rotation_degrees: { mode: "constant", value: 0 },
  opacity: { mode: "constant", value: 1 },
};
function compileProjection(document: EditorDocument): CompiledEditorDocument {
  const tracks = projectedTracks(document);
  const project =
    tracks === document.project.timeline.tracks
      ? document.project
      : {
          ...document.project,
          timeline: { ...document.project.timeline, tracks },
        };
  const diagnostics = (knownValid(project) ? [] : validateProject(project)).map(
    (message) => new EditorError("invalid_projection", message, "compile"),
  );
  // Missing media is the engine's conform check, not a document problem.
  let revisionKey: string | undefined;
  return {
    project,
    diagnostics,
    get revisionKey() {
      return (revisionKey ??= JSON.stringify(project));
    },
  };
}

/**
 * Seeds the {@link indexDocument} cache of `next` from `previous`, rebuilding only changed tracks.
 * Does nothing when `previous` has no index.
 */
export function inheritDocumentIndex(
  previous: EditorDocument,
  next: EditorDocument,
): void {
  const prior = indexes.get(previous);
  if (!prior) return;
  if (previous.project.timeline === next.project.timeline) {
    indexes.set(next, prior);
    return;
  }
  const tracks = new Map(prior.tracks),
    clips = new Map(prior.clips);
  const incoming = new Map(next.project.timeline.tracks.map((t) => [t.id, t]));
  for (const [id, old] of prior.tracks) {
    if (incoming.get(id) === old) continue;
    tracks.delete(id);
    for (const clip of old.clips ?? []) clips.delete(clip.id);
  }
  for (const [id, track] of incoming) {
    if (prior.tracks.get(id) === track) continue;
    tracks.set(id, track);
    for (const clip of track.clips ?? [])
      clips.set(clip.id, { clip, trackId: id });
  }
  indexes.set(next, { tracks, clips });
}
