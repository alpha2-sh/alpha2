import {
  frozenValid,
  knownValid,
  rememberValid,
  withValidated,
} from "./validated.js";
import type { ExportFailureCode } from "alpha2/export-failure";
import {
  validateProject,
  MAX_JSON_DEPTH,
  MAX_PROJECT_JSON_BYTES,
} from "alpha2/project";
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
/** A JSON value: what editor metadata and asset authoring data may hold. */
export type Json =
  | null
  | boolean
  | number
  | string
  | readonly Json[]
  | { readonly [key: string]: Json };
/** A bin asset: what the editor knows about a project source or effect resource. */
export interface AssetDescriptor {
  /** The id of the project source or effect resource it describes. */
  readonly id: string;
  /**
   * The source kind (`video`, `image`, `generated`, ...), or an editor-only kind such as
   * `captions`.
   */
  readonly kind: string;
  /** The display name, usually the imported file's name. */
  readonly name: string;
  /**
   * A non-negative safe integer that changes whenever the asset's content is replaced (a relink).
   */
  readonly contentRevision: number;
  /** Whether the asset's media is offline. */
  readonly missing?: boolean;
  /** Where the media is read from, when it is not kept in the document. */
  readonly locator?: string;
  /** Data the asset is built from: a text asset's `text` style, a captions asset's cue rows. */
  readonly authoring?: Json;
  /** Evidence of the file imported, as the project source records it too. */
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
  /**
   * Whether a ripple edit on another track moves this track's content after the edit point too,
   * keeping it in sync.
   */
  readonly syncLock?: boolean;
}
/** A clip's editor metadata. A clip without a `name` shows its source's name. */
export interface ClipMetadata {
  readonly name?: string;
}
/** A timeline marker: an instant, or a range when it has an `end`. */
export interface Marker {
  readonly id: string;
  /** Where it is, in timeline ticks. */
  readonly time: number;
  /** Where a range marker ends; after `time`. */
  readonly end?: number;
  readonly name: string;
  readonly note?: string;
  /** A `#rrggbb` color. */
  readonly color?: string;
  /** Whether what it notes is done. */
  readonly done?: boolean;
}
/** A node's place in the node editor, in canvas pixels. */
export interface GraphPosition {
  readonly x: number;
  readonly y: number;
}
/** Node positions by composition, then node: editor metadata, not render input. */
export type GraphLayout = Readonly<
  Record<string, Readonly<Record<string, GraphPosition>>>
>;
/** What the editor keeps beside the engine project: none of it is render input. */
export interface EditorMetadata {
  /** By track id. */
  readonly tracks: Readonly<Record<string, TrackMetadata>>;
  /** By clip id. */
  readonly clips: Readonly<Record<string, ClipMetadata>>;
  /**
   * Stable ids for list items the project keeps without one, by list (`<trackId>/cues`,
   * `<trackId>/transitions`, `<clipId>/masks`). Maintained by {@link withInstanceIds}.
   */
  readonly instanceIds: Readonly<Record<string, readonly string[]>>;
  readonly markers: readonly Marker[];
  /** The range an export renders by default, in timeline ticks; `null` for the whole timeline. */
  readonly exportRange: TimeRange | null;
  /** Node positions of the effect graph view. */
  readonly graphLayout?: GraphLayout;
  /**
   * Names of compositions drawn as groups, by composition id: editor metadata, not render input.
   */
  readonly graphNames?: Readonly<Record<string, string>>;
}
/** An editor project file: the engine project plus editor metadata and bin assets. */
export interface EditorDocument {
  readonly format: "alpha2-editor-project";
  readonly version: 1;
  readonly project: Project;
  readonly metadata: EditorMetadata;
  readonly assets: Readonly<Record<string, AssetDescriptor>>;
}
/**
 * The export failure class each editor error code is evidence of (`alpha2/export-failure`): a
 * refusal of the document or its media is `INVALID_INPUT`, which a retry meets again; a budget or
 * storage refusal says so; a state that may pass (a busy export, a pending conform) is `UNKNOWN`.
 * `tests/cases/failure-classes.json` holds the same table, and a test checks every code the editor makes
 * is in it.
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
    invalid_time: "INVALID_INPUT",
    invalid_transition: "INVALID_INPUT",
    invalid_video_decode: "INVALID_INPUT",
    locked: "INVALID_INPUT",
    missing_asset: "INVALID_INPUT",
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
   * The export failure class this error is evidence of, which `classifyBrowserFailure` reads: the
   * editor knows what its own refusals mean. `UNKNOWN` for a code without a class.
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
  // Without recursion: a value nested deeper than the stack is frozen too.
  const pending: unknown[] = [value];
  const sealing: object[] = [];
  while (pending.length) {
    const next = pending.pop();
    if (!next || typeof next !== "object" || Object.isFrozen(next)) continue;
    sealing.push(next);
    // A frozen child is not looked into: most of an edit's result is frozen parts it kept.
    for (const child of Object.values(next))
      if (child && typeof child === "object" && !Object.isFrozen(child))
        pending.push(child);
  }
  for (const object of sealing) Object.freeze(object);
  return value;
}
/**
 * The deepest nesting of arrays and objects an editor document may have: the engine's for its
 * project ({@link MAX_JSON_DEPTH}), and one more for the document that holds it.
 */
export const MAX_DOCUMENT_JSON_DEPTH = MAX_JSON_DEPTH + 1;
/**
 * Throws an `EditorError` (`invalid_document`) unless `value` is plain JSON: finite numbers, and
 * arrays and plain objects of JSON values only, nested at most `maxDepth` arrays and objects deep
 * ({@link MAX_DOCUMENT_JSON_DEPTH} by default).
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
 * engine and editor file size limits, a supported format and version, a project the engine
 * accepts, and well-formed editor metadata and assets.
 */
export function validateDocument(document: EditorDocument): void {
  assertJson(document);
  if (!isRecord(document))
    throw new EditorError(
      "invalid_document",
      "An editor document is an object",
      "validate",
    );
  // Size first: the engine reads no project over its budget.
  assertDocumentSize(document);
  validateDocumentShape(document);
  assertEditorMetadata(document);
  assertProjection(document);
}
/**
 * Throws the first `invalid_projection` error of `document`'s projection (the project without what
 * disabled tracks draw and play), so a document that opens is one the engine renders.
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
 * rules. What `validateDocument` adds (every value is finite JSON, the engine JSON and the editor
 * file fit their size limits) walks or serializes the whole document, so a gesture runs it once,
 * when it commits. The project's rules cost a candidate the parts its command changed:
 * `validateProject` reuses what it found of the frozen parts the candidate shares with the
 * gesture's start. Throws an `EditorError` (`unsupported_version` or `invalid_project`).
 */
export function validateDocumentShape(document: EditorDocument): void {
  if (document.format !== "alpha2-editor-project" || document.version !== 1)
    throw new EditorError(
      "unsupported_version",
      "Unsupported editor format/version",
      "validate",
    );
  // A project the editor already validated is not walked again.
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
/**
 * The largest editor file {@link parseDocument} opens. Every document that validates fits it, as
 * {@link serializeDocument} writes it.
 */
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
 * A marker's or track's color, as the editor writes it and a view paints it: a CSS value of any
 * other form may load a resource.
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
  // A non-string here takes down the whole page that renders it.
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
  // A transition's id is the edge it joins, so a command names the transition it means.
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
    // `authoring.text` is a text asset's style; other assets keep no text (`null`) or a file's
    // contents (a string) there.
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
 * resource (offline unless generated or a composition) overridden by `assets`. Throws an
 * `EditorError` when the result does not validate.
 */
export function createDocument(
  project: Project,
  assets: EditorDocument["assets"] = {},
): EditorDocument {
  return withValidated(() => {
    assertJson(project, MAX_JSON_DEPTH);
    // Frozen before validation, so validated parts are reused when edits check them.
    const copy = freeze(structuredClone(project));
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
  project: Project,
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
 * The editor file of `document`: compact JSON, so a document that validates is a file
 * {@link parseDocument} opens. Throws an `EditorError` when the document does not validate.
 */
export function serializeDocument(document: EditorDocument): string {
  validateDocument(document);
  return JSON.stringify(document);
}
/**
 * Opens an editor file, or a bare engine project (JSON with `schema_version`) as a new document.
 * Returns a frozen document. Every refusal is an `EditorError`: `too_large` for a file over
 * {@link MAX_EDITOR_FILE_BYTES}, `invalid_document` for text that is not an editor document's JSON,
 * and the code of the check a document (or project) fails otherwise.
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
    const document = freeze(withInstanceIds(data as EditorDocument));
    validateDocument(document);
    return frozenValid(document);
  } catch (cause) {
    if (cause instanceof EditorError) throw cause;
    // A shape the checks above did not foresee is still the file's, not the editor's.
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
 * The id of `transition` on track `trackId`: the edge it joins, which a track has one transition
 * on. It stays the transition's own while other transitions come and go, and changes when its clips
 * do (a split hands an outgoing transition to the right part).
 */
export function transitionInstanceId(
  trackId: string,
  transition: Pick<ClipTransition, "from_clip" | "to_clip">,
): string {
  return `${trackId}/transitions/${encodeURIComponent(transition.from_clip)}/${encodeURIComponent(transition.to_clip)}`;
}
/**
 * Frozen tracks and clips {@link withInstanceIds} found named as they are in a frozen `instanceIds`
 * record.
 */
const namedIn = new WeakMap<object, unknown>();
/**
 * `document` with an instance id for every caption cue, transition and clip mask: each track's
 * transitions named by {@link transitionInstanceId}; cues and masks keep the ids they have, and
 * those without one get a new one no other in the list has. Returns `document` itself when nothing
 * changes, and leaves a document of the wrong shape for validation to refuse.
 */
export function withInstanceIds(document: EditorDocument): EditorDocument {
  const tracks: unknown = document?.project?.timeline?.tracks;
  const original: unknown = document?.metadata?.instanceIds;
  if (!Array.isArray(tracks) || !isRecord(original)) return document;
  // Asked once: of a record of many keys, the answer takes a look at each.
  const lasting = Object.isFrozen(original);
  // Copied on the first change: a copy per changed key would be O(n^2) over n clips.
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
  /**
   * The ids a list of `count` keeps, `<key>/<index>` (numbered past one taken) for the new ones.
   */
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
    // A track whose ids are unchanged in this frozen record needs nothing.
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
const indexes = new WeakMap<EditorDocument, EntityIndex>();
/** A document's tracks and clips by id. */
export interface EntityIndex {
  readonly tracks: ReadonlyMap<string, Track>;
  readonly clips: ReadonlyMap<
    string,
    { readonly clip: Clip; readonly trackId: string }
  >;
}
/** The {@link EntityIndex} of `document`, built once per document object and cached. */
export function indexDocument(document: EditorDocument): EntityIndex {
  let index = indexes.get(document);
  if (index) return index;
  const tracks = new Map<string, Track>();
  const clips = new Map<string, { clip: Clip; trackId: string }>();
  for (const track of document.project.timeline.tracks) {
    tracks.set(track.id, track);
    for (const clip of track.clips ?? [])
      clips.set(clip.id, { clip, trackId: track.id });
  }
  index = { tracks, clips };
  indexes.set(document, index);
  return index;
}
/** What the engine renders of a document. */
export interface CompiledEditorDocument {
  /**
   * The project as the engine renders it: what disabled tracks draw and play left out, the mattes
   * enabled clips name kept (see `compileEditorDocument`).
   */
  readonly project: Project;
  /** The engine's validation errors for `project`, as `invalid_projection` errors. */
  readonly diagnostics: readonly EditorError[];
  /** The projection's JSON: equal keys render the same. Serialized on first read. */
  readonly revisionKey: string;
}
// A frozen document never changes, so its projection is computed once.
const compiled = new WeakMap<EditorDocument, CompiledEditorDocument>();
/**
 * The project the engine renders for `document`. A disabled track draws and plays nothing: it is
 * left out, but for the clips enabled clips name as mattes (never drawn themselves), kept on it
 * muted; a track that ducks under a disabled track does not duck; a clip only disabled clips name
 * as their matte draws nothing but still plays. Never throws for an invalid projection; the errors
 * are in `diagnostics`. Cached for frozen documents.
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
/**
 * Where `project`'s timeline ends, in ticks: the end of its last clip or caption cue, 0 when it has
 * neither.
 */
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
 * Where `document`'s program ends, in ticks: the {@link sequenceEnd} of what the engine renders of
 * it ({@link compileEditorDocument}), so a disabled track adds no length. Playback without an
 * export range stops there, an export without one ends there, and an export range with only its
 * start set runs to it.
 */
export function programEnd(document: EditorDocument): number {
  return sequenceEnd(compileEditorDocument(document).project);
}
/**
 * The tracks the engine renders of `document`. A disabled track draws and plays nothing, but what
 * enabled tracks read from it stays: the clips they name as mattes, on the track muted and with
 * nothing else. A matte clip only sets another clip's coverage and is never composited itself, so
 * keeping it draws nothing of the disabled track. A track that ducks under a disabled track ducks
 * under silence, so it does not duck. A clip that only disabled clips name as their matte is still
 * a matte: it is not drawn (no opacity), and it plays as before.
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
  // Named by hidden clips alone: the projection leaves its naming clips out, so the engine would
  // draw it as an ordinary clip.
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
const DEFAULT_VISUAL: VisualProperties = {
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
 * Seeds the {@link indexDocument} cache of `next` from that of `previous`, rebuilding entries only
 * for tracks that changed. Does nothing when `previous` has no index.
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
