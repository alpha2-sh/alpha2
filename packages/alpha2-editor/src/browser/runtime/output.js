// Where a browser export's file goes: streamed into a temporary OPFS file (or a user-picked file)
// so encoded video never accumulates in memory. In-memory output is only for small outputs sized
// up front, never a silent fallback for large ones.

import {
  estimatedOutputBytes,
  memoryOutputPeakBytes,
  MEMORY_OUTPUT_COPIES,
  MEMORY_OUTPUT_MARGIN,
  WritableFileDestination,
} from "alpha2/export-destination";
import { containerPolicy } from "alpha2/containers";
import { ExportFailureError } from "alpha2/export-failure";
import { holdShared, whenUnheld } from "./locks.js";

/** Default peak size of an in-memory output. */
export const DEFAULT_MEMORY_OUTPUT_LIMIT = 512 * 1024 * 1024;
const EXPORT_DIRECTORY = "alpha2-exports";
/**
 * Unheld temporary exports older than this belong to closed pages. Without Web Locks, age alone
 * decides.
 */
const STALE_EXPORT_MS = 24 * 60 * 60 * 1000;
/** The Web Lock a page holds on a temporary export file while it writes or keeps it. */
const exportLock = (fileName) => `alpha2-export:${fileName}`;
/** Free storage reported as wanted beyond the estimate. */
const STORAGE_MARGIN = MEMORY_OUTPUT_MARGIN;

/** Which storage APIs this page has, as an export result reports them. */
export function storageSupport() {
  const storage = globalThis.navigator?.storage;
  return {
    // Handles gained `createWritable` later than the directory API, so that is checked at open.
    opfs: typeof storage?.getDirectory === "function",
    savePicker: typeof globalThis.showSaveFilePicker === "function",
    estimate: typeof storage?.estimate === "function",
  };
}

/**
 * Opens where an export writes, refusing up front only an in-memory output past its limit.
 *
 * Storage is never refused ahead: `navigator.storage.estimate()` is a guess, and WebKit's usage
 * counts every write since its last disk scan, so `quota - usage` can be near zero on an empty
 * disk. A full disk fails at the write (`STORAGE_QUOTA_EXCEEDED`); the estimate is only reported.
 *
 * `container` (default `mp4`) sets the temporary file's extension and the artifact's MIME type.
 * Returns `{ mode, destination, storageEstimate, abort, finish }`: `destination` is the sink's,
 * `abort` discards the file, and `finish(sinkOutput)` turns the finished output into artifact
 * fields.
 */
export async function openExportOutput({
  jobId,
  name,
  container = "mp4",
  target,
  durationSeconds,
  videoBitrate,
  audioBitrate,
}) {
  const estimate = estimatedOutputBytes({
    durationSeconds,
    videoBitrate,
    audioBitrate,
  });
  const policy = containerPolicy(container);
  const kind = target?.kind ?? "opfs";
  if (kind === "file") return openPickedFile(target.handle, estimate);
  if (kind === "opfs") {
    const opened = await openOriginPrivateFile(jobId, name, policy, estimate);
    if (opened) return opened;
  }
  return openMemory(target?.maxBytes ?? DEFAULT_MEMORY_OUTPUT_LIMIT, estimate, {
    name,
    mimeType: policy.mimeType,
    fellBack: kind !== "memory",
  });
}

async function openPickedFile(handle, estimate) {
  const writable = await handle
    .createWritable({ keepExistingData: false })
    .catch((cause) => {
      throw refused(
        "OUTPUT_IO_FAILED",
        `The chosen file cannot be written: ${cause?.message ?? cause}`,
        { cause },
      );
    });
  const destination = new WritableFileDestination(writable);
  return {
    mode: "file",
    estimatedBytes: estimate,
    storageEstimate: null,
    destination: { kind: "stream", writable: destination.writable },
    stats: () => destination.stats,
    abort: (reason) => destination.abort(reason),
    finish: async (output) => ({
      kind: "saved",
      name: handle.name,
      bytes: output.bytes,
    }),
  };
}

async function openOriginPrivateFile(jobId, name, policy, estimate) {
  const storage = globalThis.navigator?.storage;
  if (typeof storage?.getDirectory !== "function") return null;
  let directory;
  let handle;
  let writable;
  const fileName = `${Date.now()}-${jobId}.${policy.extensions[0]}`;
  // Held from before the file exists until it is removed, so other pages' sweeps skip it.
  const lease = await holdShared(exportLock(fileName));
  try {
    directory = await (
      await storage.getDirectory()
    ).getDirectoryHandle(EXPORT_DIRECTORY, { create: true });
    handle = await directory.getFileHandle(fileName, { create: true });
    if (typeof handle.createWritable !== "function") {
      await directory.removeEntry(fileName).catch(() => {});
      void lease.release();
      return null;
    }
  } catch {
    void lease.release();
    return null;
  }
  const storageEstimate = await reportedStorage(storage, estimate);
  try {
    writable = await handle.createWritable();
  } catch {
    await directory.removeEntry(fileName).catch(() => {});
    void lease.release();
    return null;
  }
  const destination = new WritableFileDestination(writable);
  let released = false;
  const remove = async () => {
    if (released) return;
    released = true;
    try {
      await directory.removeEntry(fileName);
    } finally {
      void lease.release();
    }
  };
  return {
    mode: "opfs",
    estimatedBytes: estimate,
    storageEstimate,
    destination: { kind: "stream", writable: destination.writable },
    stats: () => destination.stats,
    abort: async (reason) => {
      await destination.abort(reason);
      await remove().catch(() => {});
    },
    finish: async (output) => {
      const file = await handle.getFile();
      return {
        kind: "file",
        file: new File([file], name, { type: policy.mimeType }),
        name,
        bytes: output.bytes,
        release: remove,
      };
    },
  };
}

/**
 * The origin's storage report beside this export's estimate, with `belowEstimate` when
 * `quota - usage` is less. Informational only; `null` where the browser reports nothing.
 */
async function reportedStorage(storage, estimate) {
  if (typeof storage.estimate !== "function") return null;
  const { quota, usage } = await storage.estimate().catch(() => ({}));
  if (typeof quota !== "number" || typeof usage !== "number") return null;
  const needed = Math.ceil(estimate * STORAGE_MARGIN);
  return {
    quotaBytes: quota,
    usageBytes: usage,
    neededBytes: needed,
    belowEstimate: quota - usage < needed,
  };
}

function openMemory(limit, estimate, { name, mimeType, fellBack }) {
  const peak = memoryOutputPeakBytes(estimate);
  if (peak > limit)
    throw refused(
      "RESOURCE_LIMIT",
      `${fellBack ? "This browser cannot write the export to disk, and an" : "An"} output of about ${megabytes(estimate)} needs up to ${megabytes(peak)} of memory while it is kept in memory, over the ${megabytes(limit)} limit. Export a shorter range or use the native exporter.`,
      { requestedBytes: peak, resource: "js" },
    );
  return {
    mode: "memory",
    estimatedBytes: estimate,
    storageEstimate: null,
    destination: {
      kind: "memory",
      maxBytes: Math.floor(limit / MEMORY_OUTPUT_COPIES),
    },
    stats: () => null,
    abort: async () => {},
    finish: async (output) => ({
      kind: "blob",
      blob: new Blob([output.buffer], { type: mimeType }),
      name,
      bytes: output.buffer.byteLength,
    }),
  };
}

/**
 * Deletes temporary export files left by closed pages (started over a day ago, held by no page).
 * Resolves to the names removed.
 * @returns {Promise<string[]>}
 */
export async function sweepStaleExports(now = Date.now()) {
  const removed = [];
  try {
    const directory = await (
      await globalThis.navigator.storage.getDirectory()
    ).getDirectoryHandle(EXPORT_DIRECTORY, { create: false });
    const stale = [];
    for await (const [entry] of directory.entries()) {
      const started = Number(entry.split("-", 1)[0]);
      if (Number.isFinite(started) && now - started > STALE_EXPORT_MS)
        stale.push(entry);
    }
    for (const entry of stale)
      if (
        await whenUnheld(exportLock(entry), () =>
          directory.removeEntry(entry),
        ).catch(() => false)
      )
        removed.push(entry);
  } catch {
    // No directory yet, or no OPFS, so nothing to sweep.
  }
  return removed;
}

function refused(code, message, extra = {}) {
  return new ExportFailureError(
    {
      code,
      message,
      origin: "browser",
      operation: "export",
      backend: "unknown",
      phase: "prepare",
      resource: extra.resource ?? "unknown",
      ...(extra.requestedBytes === undefined
        ? {}
        : { requestedBytes: extra.requestedBytes }),
    },
    extra.cause === undefined ? undefined : { cause: extra.cause },
  );
}

function megabytes(bytes) {
  return `${(bytes / 1e6).toFixed(bytes < 1e7 ? 1 : 0)} MB`;
}
