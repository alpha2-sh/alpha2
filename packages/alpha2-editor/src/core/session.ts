import type { Clip, Composition } from "alpha2/project";
import { compositionTree } from "./compositions.js";
import {
  createEmptyDocument,
  validateDocument,
  withInstanceIds,
  canonicalDocument,
  indexDocument,
  inheritDocumentIndex,
  EditorError,
} from "./document.js";
import type { EditorDocument, Result } from "./document.js";
import { executeCommand } from "./commands.js";
import { frozenValid, owned } from "./validated.js";
import type { Command } from "./commands.js";
/** An immutable view of an {@link EditorSession}; a new object after every change. */
export interface EditorSnapshot {
  /** The committed document. */
  readonly document: EditorDocument;
  /**
   * Views and preview use the active gesture's valid candidate, or `document`. History, export and
   * `canExecute` use only the committed `document`.
   */
  readonly draft: EditorDocument;
  /** Selected clip ids; ids of clips the document no longer has are dropped. */
  readonly selection: readonly string[];
  /** Increases with every change of the committed document (commit, undo, redo, open). */
  readonly revision: number;
  /** Increases when the document is replaced wholesale ({@link EditorSession.open}) or disposed. */
  readonly generation: number;
  readonly canUndo: boolean;
  readonly canRedo: boolean;
}
/**
 * Holds an editor document with undo history, selection, clipboard and pointer gestures. Every
 * document it holds is frozen; edits produce new documents through {@link Command}s.
 *
 * Methods other than `cancelGesture` and `dispose` throw an `EditorError` (`disposed`) after
 * {@link EditorSession.dispose}.
 */
export class EditorSession {
  #document: EditorDocument;
  #selection: readonly string[] = Object.freeze([]);
  #undo: EditorDocument[] = [];
  #redo: EditorDocument[] = [];
  #revision = 0;
  #generation = 0;
  #disposed = false;
  #listeners = new Set<() => void>();
  #snapshot: EditorSnapshot;
  #cancelGesture: (() => void) | null = null;
  #draft: EditorDocument | null = null;
  /** Copied clips, each with its name and the compositions it drew when copied. */
  #clipboard: readonly {
    clip: Clip;
    trackId: string;
    name: string | undefined;
    compositions: readonly Composition[];
  }[] = [];
  /**
   * @param document The initial document, copied; an empty document by default. Throws an
   *   `EditorError` when it does not validate.
   * @param historyLimit The most undo steps kept. Defaults to 120.
   * @param historyByteLimit The most bytes the undo history keeps beyond the document: each step
   *   counts the serialized size of what it does not share with the document after it, by
   *   object identity, and the oldest steps are dropped first. Defaults to 64 MiB.
   * @throws RangeError when a limit is not a non-negative safe integer.
   */
  constructor(
    document = createEmptyDocument(),
    readonly historyLimit = 120,
    readonly historyByteLimit = 64 * 1024 * 1024,
  ) {
    if (
      !Number.isSafeInteger(historyLimit) ||
      historyLimit < 0 ||
      !Number.isSafeInteger(historyByteLimit) ||
      historyByteLimit < 0
    )
      throw new RangeError("Invalid history budget");
    document = withInstanceIds(canonicalDocument(document));
    validateDocument(document);
    this.#document = frozenValid(owned(document));
    this.#snapshot = this.#read();
  }
  get document() {
    return this.#document;
  }
  get draft() {
    return this.#draft ?? this.#document;
  }
  get revision() {
    return this.#revision;
  }
  get generation() {
    return this.#generation;
  }
  get disposed() {
    return this.#disposed;
  }
  /**
   * Every document the session can still show or return to: the undo history, the document, the
   * active gesture's draft and the redo history. A host keeps what these refer to and may release
   * the rest.
   */
  reachableDocuments(): readonly EditorDocument[] {
    return [
      ...this.#undo,
      this.#document,
      ...(this.#draft ? [this.#draft] : []),
      ...this.#redo,
    ];
  }
  /**
   * The current snapshot; the same object until the next change, as `useSyncExternalStore` needs.
   */
  getSnapshot = (): EditorSnapshot => this.#snapshot;
  /** Calls `listener` after every change. Returns the unsubscribe function. */
  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };
  /**
   * Calls `listener` with `selector`'s result whenever it changes by `equal` (`Object.is` by
   * default). Returns the unsubscribe function.
   */
  subscribeSelector<T>(
    selector: (snapshot: EditorSnapshot) => T,
    listener: (value: T) => void,
    equal: (a: T, b: T) => boolean = Object.is,
  ): () => void {
    let previous = selector(this.#snapshot);
    return this.subscribe(() => {
      const next = selector(this.#snapshot);
      if (!equal(previous, next)) {
        previous = next;
        listener(next);
      }
    });
  }
  #read(): EditorSnapshot {
    return Object.freeze({
      document: this.#document,
      draft: this.#draft ?? this.#document,
      selection: this.#selection,
      revision: this.#revision,
      generation: this.#generation,
      canUndo: this.#undo.length > 0,
      canRedo: this.#redo.length > 0,
    });
  }
  #emit() {
    this.#snapshot = this.#read();
    for (const listener of this.#listeners) listener();
  }
  #active() {
    if (this.#disposed)
      throw new EditorError("disposed", "Session is disposed", "session");
  }
  /** Each undo step's {@link stepBytes}, with the document after it they were counted against. */
  #stepSizes = new WeakMap<
    EditorDocument,
    { next: EditorDocument; bytes: number }
  >();
  /** Drops the oldest undo steps past the budget; `next` is the document after the newest step. */
  #trimHistory(next: EditorDocument) {
    const size = (index: number) => {
      const step = this.#undo[index];
      const after = this.#undo[index + 1] ?? next;
      let known = this.#stepSizes.get(step);
      if (known?.next !== after) {
        known = { next: after, bytes: stepBytes(step, after) };
        this.#stepSizes.set(step, known);
      }
      return known.bytes;
    };
    let bytes = 0;
    for (let index = 0; index < this.#undo.length; index++)
      bytes += size(index);
    while (
      this.#undo.length &&
      (this.#undo.length > this.historyLimit || bytes > this.historyByteLimit)
    ) {
      bytes -= size(0);
      this.#undo.shift();
    }
  }
  #commit(document: EditorDocument) {
    inheritDocumentIndex(this.#document, document);
    this.#undo.push(this.#document);
    this.#trimHistory(document);
    this.#redo = [];
    // What it commits passed a dispatch's checks.
    this.#document = frozenValid(document);
    this.#revision++;
    this.#prune();
    this.#emit();
  }
  #prune() {
    const index = indexDocument(this.#document);
    const kept = this.#selection.filter((id) => index.clips.has(id));
    if (kept.length !== this.#selection.length)
      this.#selection = Object.freeze(kept);
  }
  /**
   * The document `command` would produce from the committed one, or why it is refused. Changes
   * nothing.
   */
  canExecute(command: Command): Result<EditorDocument> {
    this.#active();
    return executeCommand(this.#document, command);
  }
  /**
   * Runs `command` on the committed document and commits the result as one undo step, cancelling an
   * active gesture first. A refusal is returned, not thrown, and leaves the session unchanged; so
   * does a command that changes nothing (its result is the committed document itself), which keeps
   * the redo history.
   */
  dispatch(command: Command): Result<EditorDocument> {
    this.#active();
    this.#cancelGesture?.();
    const result = this.canExecute(command);
    if (result.ok && result.value !== this.#document)
      this.#commit(result.value);
    return result;
  }
  /**
   * Replaces the document, clearing history, selection and clipboard and increasing `generation`.
   * Throws an `EditorError` when `document` does not validate, leaving the session unchanged.
   */
  open(document: EditorDocument): void {
    this.#active();
    document = withInstanceIds(canonicalDocument(document));
    validateDocument(document);
    const next = frozenValid(owned(document));
    this.#cancelGesture?.();
    this.#document = next;
    this.#undo = [];
    this.#redo = [];
    this.#selection = Object.freeze([]);
    this.#clipboard = [];
    this.#generation++;
    this.#revision++;
    this.#emit();
  }
  /** Steps back one committed change. Returns `false` when there is none. */
  undo(): boolean {
    this.#active();
    this.#cancelGesture?.();
    const previous = this.#undo.pop();
    if (!previous) return false;
    this.#redo.push(this.#document);
    this.#document = previous;
    this.#revision++;
    this.#prune();
    this.#emit();
    return true;
  }
  /** Reapplies the last undone change. Returns `false` when there is none. */
  redo(): boolean {
    this.#active();
    this.#cancelGesture?.();
    const next = this.#redo.pop();
    if (!next) return false;
    this.#undo.push(this.#document);
    this.#document = next;
    this.#revision++;
    this.#prune();
    this.#emit();
    return true;
  }
  /**
   * Forgets the undo and redo history and keeps everything else: the document (so `revision` and
   * `generation` stay), the selection, the clipboard and an active gesture. For a host that made
   * changes the user should not step back through, such as relinking media it restored, without
   * {@link open}'s new generation, which tells views and the bridge the document was replaced.
   */
  clearHistory(): void {
    this.#active();
    if (!this.#undo.length && !this.#redo.length) return;
    this.#undo = [];
    this.#redo = [];
    this.#emit();
  }
  /**
   * Selects the clips `ids` names; unknown ids and duplicates are dropped. The selection it would
   * set already holds (the same ids in the same order), it changes nothing and tells no one: a
   * marquee calls it on every pointer move.
   */
  select(ids: readonly string[]): void {
    this.#active();
    const index = indexDocument(this.#document);
    const next = [...new Set(ids)].filter((id) => index.clips.has(id));
    if (
      next.length === this.#selection.length &&
      next.every((id, i) => id === this.#selection[i])
    )
      return;
    this.#selection = Object.freeze(next);
    this.#emit();
  }
  /** The committed document's tracks and clips by id. */
  query() {
    return indexDocument(this.#document);
  }
  /**
   * Copies the clips `ids` names (the selection by default) to the session's clipboard, with their
   * names and the compositions they draw. Unknown ids are skipped.
   */
  copy(ids: readonly string[] = this.#selection): void {
    this.#active();
    const { project, metadata } = this.#document;
    this.#clipboard = ids
      .map((id) => this.query().clips.get(id))
      .filter((v): v is NonNullable<typeof v> => !!v)
      .map(({ clip, trackId }) => ({
        clip,
        trackId,
        name: metadata.clips[clip.id]?.name,
        compositions: clip.composition
          ? compositionTree(project, clip.composition.id)
          : [],
      }));
  }
  /**
   * Inserts the clipboard's clips on their original tracks, under their names, as one undo step,
   * the earliest starting at tick `at` and the others keeping their offsets from it. New clip ids
   * come from `idFactory`. With nothing copied it changes nothing and returns the document.
   */
  paste(
    at: number,
    idFactory: () => string = () => crypto.randomUUID(),
  ): Result<EditorDocument> {
    this.#active();
    if (!this.#clipboard.length) return { ok: true, value: this.#document };
    const anchor = Math.min(
      ...this.#clipboard.map((c) => c.clip.timeline_range.start),
    );
    const commands: Command[] = [];
    const moves: { id: string; at: number }[] = [];
    for (const { clip, trackId, name, compositions } of this.#clipboard) {
      const id = idFactory();
      commands.push({
        type: "clip.insert",
        trackId,
        clip: { ...clip, id },
        ...(name ? { name } : {}),
        ...(compositions.length ? { compositions } : {}),
      });
      moves.push({ id, at: at + clip.timeline_range.start - anchor });
    }
    // Insert each copy where its original is, then move them all off together in one `clips.move`.
    commands.push({ type: "clips.move", moves });
    return this.dispatch({ type: "transaction", commands });
  }
  /**
   * Cancels the active gesture, if any, discarding its draft: nothing it previewed is committed.
   * Returns whether a gesture was active. A host calls it before it tears down the view a gesture
   * belongs to.
   */
  cancelGesture(): boolean {
    const cancel = this.#cancelGesture;
    if (!cancel) return false;
    cancel();
    return true;
  }
  /**
   * Starts a pointer gesture, cancelling an active one. Each `update` runs a command on the
   * document the gesture started from and, when valid, shows the result as the snapshot's `draft`
   * without touching history. `commit` validates the last candidate fully and commits it as one
   * undo step, returning whether anything was committed; `cancel` discards it. A gesture ends on
   * commit, cancel, a dispatch, undo, redo, open or dispose, or the next `beginGesture`; `update`
   * then returns a `stale_gesture` error.
   */
  beginGesture() {
    this.#active();
    this.#cancelGesture?.();
    const base = this.#document;
    let candidate = base;
    let active = true;
    const end = (): boolean => {
      active = false;
      candidate = base;
      if (this.#cancelGesture === cancel) this.#cancelGesture = null;
      const hadDraft = this.#draft !== null;
      this.#draft = null;
      return hadDraft;
    };
    const cancel = () => {
      if (active && end() && !this.#disposed) this.#emit();
    };
    this.#cancelGesture = cancel;
    return {
      get candidate() {
        return candidate;
      },
      update: (command: Command): Result<EditorDocument> => {
        if (!active)
          return {
            ok: false,
            error: new EditorError(
              "stale_gesture",
              "Gesture is no longer active",
              "gesture",
            ),
          };
        const result = executeCommand(base, command, { gesture: true });
        if (result.ok) {
          candidate = frozenValid(result.value);
          this.#draft = candidate;
          this.#emit();
        }
        return result;
      },
      cancel,
      commit: (): boolean => {
        if (!active) return false;
        const value = candidate;
        const hadDraft = end();
        if (value === base) {
          if (hadDraft) this.#emit();
          return false;
        }
        // Updates validated only the candidate's shape; the commit passes every dispatch check.
        try {
          validateDocument(value);
        } catch {
          if (!this.#disposed) this.#emit();
          return false;
        }
        this.#commit(value);
        return true;
      },
    };
  }
  #owners = new Set<{ dispose(): void | Promise<void> }>();
  #disposal: Promise<void> | null = null;
  /**
   * Disposes `resource` when the session is disposed. Returns a function that releases it from the
   * session without disposing it.
   */
  own(resource: { dispose(): void | Promise<void> }): () => void {
    this.#active();
    this.#owners.add(resource);
    return () => {
      this.#owners.delete(resource);
    };
  }
  /**
   * Ends the session: cancels a gesture, drops listeners, history and clipboard, and disposes every
   * owned resource. Idempotent; every call returns the same promise, which rejects with an
   * `AggregateError` when any resource's disposal failed.
   */
  dispose(): Promise<void> {
    if (this.#disposal) return this.#disposal;
    this.#cancelGesture?.();
    this.#disposed = true;
    this.#generation++;
    this.#listeners.clear();
    this.#undo = [];
    this.#redo = [];
    this.#clipboard = [];
    const resources = [...this.#owners];
    this.#owners.clear();
    this.#disposal = Promise.allSettled(
      resources.map((resource) =>
        Promise.resolve().then(() => resource.dispose()),
      ),
    ).then((results) => {
      const failures = results.filter(
        (r): r is PromiseRejectedResult => r.status === "rejected",
      );
      if (failures.length)
        throw new AggregateError(
          failures.map((r) => r.reason as unknown),
          "Session cleanup failed",
        );
    });
    return this.#disposal;
  }
}
/** Creates an {@link EditorSession}; see its constructor for the options and their defaults. */
export function createEditor(
  options: {
    document?: EditorDocument;
    historyLimit?: number;
    historyByteLimit?: number;
  } = {},
): EditorSession {
  return new EditorSession(
    options.document,
    options.historyLimit,
    options.historyByteLimit,
  );
}

/** {@link jsonBytes} of frozen objects, by identity: nothing can change them. */
const frozenBytes = new WeakMap<object, number>();

/** UTF-8 bytes of `text` as a JSON string, quotes included and escapes not. */
function stringBytes(text: string): number {
  let bytes = 2;
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    // A surrogate is half of a 4-byte character.
    bytes +=
      unit < 0x80
        ? 1
        : unit < 0x800
          ? 2
          : unit >= 0xd800 && unit < 0xe000
            ? 2
            : 3;
  }
  return bytes;
}

/** The bytes a primitive serializes to. */
function primitiveBytes(value: unknown): number {
  if (typeof value === "string") return stringBytes(value);
  if (
    typeof value === "number" ||
    typeof value === "boolean" ||
    typeof value === "bigint"
  )
    return String(value).length;
  // `null`; `undefined`, functions and symbols are left out.
  return value === null ? 4 : 0;
}

/** The fields a plain object serializes, as `JSON.stringify` writes them. */
function serializedEntries(value: object): [string, unknown][] {
  return Object.entries(value).filter(
    ([, field]) => field !== undefined && typeof field !== "function",
  );
}

/**
 * About the bytes `JSON.stringify(value)` writes (strings unescaped), without building the string.
 * A frozen part's size is kept, so a part every document shares is measured once.
 */
function jsonBytes(value: unknown): number {
  if (value === null || typeof value !== "object") return primitiveBytes(value);
  const known = frozenBytes.get(value);
  if (known !== undefined) return known;
  let bytes: number;
  if (Array.isArray(value)) {
    bytes = 1 + Math.max(value.length, 1);
    for (const item of value as unknown[]) bytes += jsonBytes(item ?? null);
  } else {
    const entries = serializedEntries(value);
    bytes = 1 + Math.max(entries.length, 1);
    for (const [key, field] of entries)
      bytes += stringBytes(key) + 1 + jsonBytes(field);
  }
  if (Object.isFrozen(value)) frozenBytes.set(value, bytes);
  return bytes;
}

/**
 * About the bytes an undo step holds that `after`, the document after it, does not: the serialized
 * size of its parts that are not the same objects as `after`'s. Edits share what they leave
 * unchanged, so a step that renames a marker in a project with a 100,000-frame image sequence holds
 * a few hundred bytes, not the sequence again. An array's item is matched to `after`'s by identity
 * anywhere in the array, then by its `id`, then by position; a part moved elsewhere is counted
 * whole, so the estimate errs high.
 */
function stepBytes(step: unknown, after: unknown): number {
  if (step === after) return 0;
  if (step === null || typeof step !== "object") return primitiveBytes(step);
  if (
    after === null ||
    typeof after !== "object" ||
    Array.isArray(step) !== Array.isArray(after)
  )
    return jsonBytes(step);
  if (Array.isArray(step)) {
    const later = after as unknown[];
    const kept = new Set(later);
    let byId: Map<unknown, unknown> | null = null;
    const idOf = (item: unknown) =>
      item !== null && typeof item === "object"
        ? (item as { id?: unknown }).id
        : undefined;
    let bytes = 1 + Math.max(step.length, 1);
    step.forEach((item: unknown, index) => {
      if (kept.has(item)) return;
      const id = idOf(item);
      let counterpart: unknown = later[index];
      if (typeof id === "string") {
        byId ??= new Map(later.map((other) => [idOf(other), other]));
        if (byId.has(id)) counterpart = byId.get(id);
      }
      bytes += stepBytes(item ?? null, counterpart);
    });
    return bytes;
  }
  const entries = serializedEntries(step);
  let bytes = 1 + Math.max(entries.length, 1);
  for (const [key, field] of entries)
    bytes +=
      stringBytes(key) +
      1 +
      stepBytes(field, (after as Record<string, unknown>)[key]);
  return bytes;
}
