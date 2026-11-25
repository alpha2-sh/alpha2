// Files for a plugin's resource slots, resolved and read as host-core's `ResourceSet` does.

import {
  MAX_SUPPLIED_FILES,
  MAX_SUPPLIED_NAME_BYTES,
  type SuppliedFile,
} from "./generated/project-schema.js";
import {
  MAX_PLUGIN_RESOURCE_BYTES,
  PluginError,
  type PluginManifest,
  type SuppliedPluginResource,
} from "./plugin-manifest.js";
import { Sha256 } from "./sha256.js";

/** Mirrors `alpha2_host_core::plugins::MAX_SLOT_DEPTH`: directories below a slot's own. */
export const MAX_SLOT_DEPTH = 8;

/**
 * What fills one slot: a `File` for a `file` slot; for a `directory` slot a directory handle, or
 * the `File`s of a directory picker (`<input webkitdirectory>`), named by `webkitRelativePath`
 * below the picked directory.
 */
export type SuppliedSource = File | FileSystemDirectoryHandle | readonly File[];

/** One file a slot offers. */
interface Offered {
  readonly slot: string;
  readonly file: File;
  readonly maxBytes: number | undefined;
}

/** The filled slots of one plugin, resolved at load; nothing is read. */
export interface SuppliedSet {
  /** By resource name, in UTF-8 byte order. */
  readonly files: ReadonlyMap<string, Offered>;
  /** As `begin`'s context names them. */
  readonly context: Readonly<
    Record<
      string,
      {
        readonly shape: "file" | "directory";
        readonly files: readonly { name: string; bytes: number }[];
      }
    >
  >;
}

const encoder = new TextEncoder();

function byteOrder(a: string, b: string): number {
  const left = encoder.encode(a);
  const right = encoder.encode(b);
  for (let index = 0; index < Math.min(left.length, right.length); index++)
    if (left[index] !== right[index]) return left[index] - right[index];
  return left.length - right.length;
}

function invalid(slot: string, reason: string, message: string): PluginError {
  return new PluginError("PLUGIN_RESOURCE_INVALID", message, {
    details: { slot, reason },
  });
}

function fetchFailed(name: string, cause: unknown): PluginError {
  return new PluginError(
    "PLUGIN_FETCH_FAILED",
    `${name} could not be read: ${String(cause)}`,
    { cause },
  );
}

const isDirectoryHandle = (
  value: unknown,
): value is FileSystemDirectoryHandle =>
  typeof value === "object" &&
  value !== null &&
  (value as { kind?: unknown }).kind === "directory" &&
  typeof (value as { entries?: unknown }).entries === "function";

/** Each file under `handle` by its path below it, in the walk's order. */
async function walkHandle(
  slot: string,
  handle: FileSystemDirectoryHandle,
  below: readonly string[],
  found: [string[], File][],
): Promise<void> {
  if (below.length > MAX_SLOT_DEPTH)
    throw invalid(
      slot,
      "too_deep",
      `${[slot, ...below].join("/")} lies more than ${MAX_SLOT_DEPTH} directories below slot ${slot}`,
    );
  const entries: [string, FileSystemHandle][] = [];
  try {
    for await (const entry of (
      handle as unknown as {
        entries(): AsyncIterable<[string, FileSystemHandle]>;
      }
    ).entries())
      entries.push(entry);
  } catch (cause) {
    throw fetchFailed([slot, ...below].join("/"), cause);
  }
  entries.sort(([a], [b]) => byteOrder(a, b));
  for (const [name, entry] of entries) {
    if (name.startsWith(".")) continue;
    const path = [...below, name];
    if (entry.kind === "directory")
      await walkHandle(slot, entry as FileSystemDirectoryHandle, path, found);
    else {
      let file: File;
      try {
        file = await (entry as FileSystemFileHandle).getFile();
      } catch (cause) {
        throw fetchFailed([slot, ...path].join("/"), cause);
      }
      found.push([path, file]);
    }
  }
}

/** The files a directory picker gave, by their path below the picked directory. */
function pickedFiles(slot: string, files: readonly File[]): [string[], File][] {
  const found: [string[], File][] = [];
  for (const file of files) {
    const parts = (file.webkitRelativePath || file.name).split("/").slice(1);
    if (parts.length === 0 || parts.some((part) => part === ""))
      throw invalid(
        slot,
        "shape",
        `slot ${slot} takes a directory; ${file.name} has no path below a picked directory`,
      );
    if (parts.some((part) => part.startsWith("."))) continue;
    if (parts.length - 1 > MAX_SLOT_DEPTH)
      throw invalid(
        slot,
        "too_deep",
        `${[slot, ...parts].join("/")} lies more than ${MAX_SLOT_DEPTH} directories below slot ${slot}`,
      );
    found.push([parts, file]);
  }
  return found;
}

/**
 * Resolves the slots of `manifest` from `supplied`, refusing what host-core's loader refuses,
 * with the same `details.reason`. Nothing is read; directory handles are walked.
 */
export async function resolveSupplied(
  manifest: PluginManifest,
  supplied: Readonly<Record<string, SuppliedSource>>,
): Promise<SuppliedSet> {
  const slots = (manifest.resources ?? []).filter(
    (resource): resource is SuppliedPluginResource =>
      resource.supplied !== undefined,
  );
  const unknown = Object.keys(supplied).find(
    (name) => !slots.some((slot) => slot.name === name),
  );
  if (unknown !== undefined)
    throw invalid(
      unknown,
      "unknown_slot",
      `${manifest.id} declares no resource slot ${unknown}`,
    );
  const files = new Map<string, Offered>();
  const context: Record<string, SuppliedSet["context"][string]> = {};
  for (const { name, supplied: slot } of [...slots].sort((a, b) =>
    byteOrder(a.name, b.name),
  )) {
    const source = supplied[name];
    if (source === undefined) {
      if (slot.required !== false)
        throw invalid(
          name,
          "missing",
          `${manifest.id} needs a file for its resource slot ${name}`,
        );
      continue;
    }
    const maxBytes = slot.max_bytes;
    let offered: [string, File][];
    if (slot.shape === "file") {
      if (!(source instanceof Blob))
        throw invalid(
          name,
          "shape",
          `slot ${name} takes a file; it was given a directory`,
        );
      const file = source;
      if (maxBytes !== undefined && file.size > maxBytes)
        throw invalid(
          name,
          "too_large",
          `${name} is ${file.size} bytes; slot ${name} takes at most ${maxBytes}`,
        );
      if (file.size > MAX_PLUGIN_RESOURCE_BYTES)
        throw invalid(
          name,
          "too_large",
          `${name} is ${file.size} bytes; a wasm plugin reads at most ${MAX_PLUGIN_RESOURCE_BYTES}`,
        );
      offered = [[name, file]];
    } else {
      let found: [string[], File][];
      if (isDirectoryHandle(source)) {
        found = [];
        await walkHandle(name, source, [], found);
      } else if (Array.isArray(source)) found = pickedFiles(name, source);
      else
        throw invalid(
          name,
          "shape",
          `slot ${name} takes a directory; it was given a file`,
        );
      offered = found.map(([path, file]) => [
        `${name}/${path.join("/")}`,
        file,
      ]);
      for (const [resource] of offered)
        if (encoder.encode(resource).length > MAX_SUPPLIED_NAME_BYTES)
          throw invalid(
            name,
            "name_too_long",
            `${resource} is longer than ${MAX_SUPPLIED_NAME_BYTES} bytes`,
          );
      offered.sort(([a], [b]) => byteOrder(a, b));
    }
    if (files.size + offered.length > MAX_SUPPLIED_FILES)
      throw invalid(
        name,
        "too_many_files",
        `slot ${name} brings the run's supplied files past ${MAX_SUPPLIED_FILES}`,
      );
    for (const [resource, file] of offered)
      files.set(resource, { slot: name, file, maxBytes });
    context[name] = {
      shape: slot.shape,
      files: offered.map(([resource, file]) => ({
        name: resource,
        bytes: file.size,
      })),
    };
  }
  return { files, context };
}

/** Bytes read from a file at a time, and hashed as they arrive. */
const READ_STEP_BYTES = 8 * 1024 * 1024;

type SyncReader = { readAsArrayBuffer(blob: Blob): ArrayBuffer };

/**
 * Serves one run's reads of supplied files synchronously, as wasm imports must: in a worker
 * through `FileReaderSync`, step by step; on a page, from bytes read beforehand by
 * {@link SuppliedReader.prepare}. Each file is hashed the first time it is read.
 */
export class SuppliedReader {
  readonly #set: SuppliedSet;
  readonly #read = new Map<string, { sha256: string; bytes: number }>();
  #held: Map<string, Uint8Array> | undefined;
  #sync: SyncReader | undefined;

  constructor(set: SuppliedSet) {
    this.#set = set;
  }

  /** Reads every offered file into memory where no synchronous file read exists. */
  async prepare(): Promise<void> {
    const FileReaderSync = (
      globalThis as { FileReaderSync?: new () => SyncReader }
    ).FileReaderSync;
    if (FileReaderSync !== undefined) {
      this.#sync = new FileReaderSync();
      return;
    }
    if (this.#set.files.size === 0) return;
    const held = new Map<string, Uint8Array>();
    for (const [name, { file }] of this.#set.files) {
      try {
        held.set(name, new Uint8Array(await file.arrayBuffer()));
      } catch (cause) {
        throw fetchFailed(name, cause);
      }
    }
    this.#held = held;
  }

  /** The length of the supplied resource `name`, or `undefined` for another name. */
  size(name: string): number | undefined {
    return this.#set.files.get(name)?.file.size;
  }

  /**
   * Copies the supplied resource `name` into `target`, its length exactly. Throws the failure the
   * call it happened in ends with: `PLUGIN_RESOURCE_INVALID` (`too_large`) past the slot's
   * `max_bytes`, `PLUGIN_FETCH_FAILED` for a file that could not be read or changed.
   */
  read(name: string, target: Uint8Array): void {
    const offered = this.#set.files.get(name);
    if (offered === undefined) throw new Error(`no supplied resource ${name}`);
    const { file, maxBytes, slot } = offered;
    if (maxBytes !== undefined && file.size > maxBytes)
      throw invalid(
        slot,
        "too_large",
        `${name} is ${file.size} bytes; slot ${slot} takes at most ${maxBytes}`,
      );
    const known = this.#read.get(name);
    const hash = known === undefined ? new Sha256() : undefined;
    const held = this.#held?.get(name);
    if (held !== undefined) {
      if (held.byteLength !== target.byteLength)
        throw fetchFailed(name, "its length changed");
      target.set(held);
      hash?.update(held);
    } else {
      const reader = this.#sync;
      if (reader === undefined)
        throw new Error("the supplied files were not prepared");
      for (let at = 0; at < target.byteLength; at += READ_STEP_BYTES) {
        let step: Uint8Array;
        try {
          step = new Uint8Array(
            reader.readAsArrayBuffer(
              file.slice(at, Math.min(at + READ_STEP_BYTES, target.byteLength)),
            ),
          );
        } catch (cause) {
          throw fetchFailed(name, cause);
        }
        if (
          step.byteLength !== Math.min(READ_STEP_BYTES, target.byteLength - at)
        )
          throw fetchFailed(name, "it changed while the plugin read it");
        target.set(step, at);
        hash?.update(step);
      }
    }
    if (hash !== undefined)
      this.#read.set(name, { sha256: hash.hex(), bytes: target.byteLength });
  }

  /** The files read so far, as `made_by.plugin.supplied` records them. */
  supplied(): SuppliedFile[] {
    return [...this.#read]
      .sort(([a], [b]) => byteOrder(a, b))
      .map(([name, { sha256, bytes }]) => ({ name, sha256, bytes }));
  }
}
