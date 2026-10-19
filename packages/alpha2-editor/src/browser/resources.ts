import { EditorError } from "../core/document.js";
/** A counted reference to a registry entry. */
export interface Lease<T> {
  readonly value: T;
  /**
   * Drops this reference. Idempotent. The entry's `dispose` runs when its last lease is
   * released.
   */
  release(): void;
}
/**
 * Holds runtime values (decoders, bitmaps, files) that must never enter the project JSON or
 * its history, keyed by id and revision and kept alive by explicit leases from owners and
 * export jobs.
 */
export class ResourceRegistry<T> {
  #entries = new Map<string, { value: T; refs: number; dispose: () => void }>();
  #closed = false;
  /** @param limit Most entries held at once (default 128). */
  constructor(readonly limit = 128) {}
  /**
   * Stores `value` under `id` at `revision` and returns its first lease. `dispose` runs once,
   * when the last lease is released or the registry is disposed.
   *
   * Throws `EditorError` `disposed` after {@link dispose}, `duplicate_resource` if that
   * revision is already stored, and `resource_budget` when `limit` entries are held.
   */
  put(id: string, revision: number, value: T, dispose: () => void): Lease<T> {
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
    if (this.#entries.size >= this.limit)
      throw new EditorError(
        "resource_budget",
        "Resource budget reached",
        "resource",
        id,
      );
    let closed = false;
    this.#entries.set(key, {
      value,
      refs: 0,
      dispose: () => {
        if (!closed) {
          closed = true;
          dispose();
        }
      },
    });
    return this.acquire(id, revision);
  }
  /**
   * Takes another lease on a stored entry. Throws `EditorError` `missing_asset` if no entry
   * exists for `id` at `revision`.
   */
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
          entry.dispose();
        }
      },
    };
  }
  /** Entries currently held. */
  get size() {
    return this.#entries.size;
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
  }
}
