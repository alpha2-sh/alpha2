import { EditorError } from "../core/document.js";
/** A counted reference to a registry entry. */
export interface Lease<T> {
  readonly value: T;
  /**
   * Drops this reference. Idempotent. The entry's `dispose` runs when its last lease is released.
   */
  release(): void;
}
/**
 * Holds runtime values (decoders, bitmaps, files) that must never enter the project JSON or its
 * history, keyed by id and revision and kept alive by leases from owners and export jobs.
 */
export class ResourceRegistry<T> {
  #entries = new Map<
    string,
    { value: T; refs: number; counted: boolean; dispose: () => void }
  >();
  #counted = 0;
  #closed = false;
  /** @param limit Most counted entries held at once (default 128). */
  constructor(readonly limit = 128) {}
  /**
   * Stores `value` under `id` at `revision` and returns its first lease. `dispose` runs once,
   * when the last lease is released or the registry is disposed. An entry put with
   * `counted: false` is held whatever the limit and does not use its room.
   *
   * Throws `EditorError` `disposed` after {@link dispose}, `duplicate_resource` if that revision
   * is already stored, and `resource_budget` when a counted entry finds `limit` counted entries.
   */
  put(
    id: string,
    revision: number,
    value: T,
    dispose: () => void,
    { counted = true }: { counted?: boolean } = {},
  ): Lease<T> {
    if (this.#closed)
      throw new EditorError("disposed", "Resources are closed", "resource");
    const key = JSON.stringify([id, revision]);
    if (this.#entries.has(key))
      throw new EditorError(
        "duplicate_resource",
        "Resource revision already exists",
        "resource",
        id,
      );
    if (counted && this.#counted >= this.limit)
      throw new EditorError(
        "resource_budget",
        "Resource budget reached",
        "resource",
        id,
      );
    let closed = false;
    if (counted) this.#counted++;
    this.#entries.set(key, {
      value,
      refs: 0,
      counted,
      dispose: () => {
        if (!closed) {
          closed = true;
          dispose();
        }
      },
    });
    return this.acquire(id, revision);
  }
  /** Takes another lease on a stored entry. Throws `EditorError` `missing_asset` if absent. */
  acquire(id: string, revision: number): Lease<T> {
    const key = JSON.stringify([id, revision]);
    const entry = this.#entries.get(key);
    if (!entry)
      throw new EditorError(
        "missing_asset",
        "Runtime resource is missing",
        "resource",
        id,
      );
    entry.refs++;
    let released = false;
    return {
      value: entry.value,
      release: () => {
        if (released) return;
        released = true;
        if (--entry.refs === 0) {
          this.#entries.delete(key);
          if (entry.counted) this.#counted--;
          entry.dispose();
        }
      },
    };
  }
  /** Entries currently held. */
  get size() {
    return this.#entries.size;
  }
  /** Entries currently held that count against `limit`. */
  get counted() {
    return this.#counted;
  }
  /**
   * Disposes every entry regardless of outstanding leases and refuses later `put` calls.
   * Idempotent.
   */
  dispose() {
    if (this.#closed) return;
    this.#closed = true;
    for (const entry of this.#entries.values()) entry.dispose();
    this.#entries.clear();
    this.#counted = 0;
  }
}
