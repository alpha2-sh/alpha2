/**
 * Where a browser export's encoded bytes go.
 *
 * The default is a file: bytes are written as the muxer produces them, a bounded queue in between,
 * so the encoded video never accumulates in memory and finishing adds no copy of it. Keeping an
 * export in memory is a separate, explicit mode for small outputs, sized before the export starts.
 */

import { ExportFailureError, type ExportFailure } from "./export-failure.js";

/**
 * One positioned write, as mediabunny's `StreamTarget` emits it and
 * `FileSystemWritableFileStream.write` takes it.
 */
export interface PositionedWrite {
  readonly type: "write";
  readonly data: Uint8Array;
  readonly position: number;
}

/**
 * A file that takes positioned writes: a `FileSystemWritableFileStream` (a user-picked file or an
 * origin-private one), or anything shaped like it.
 */
export interface PositionalFile {
  /** Writes `chunk.data` at `chunk.position`; a rejection fails the export. */
  write(chunk: PositionedWrite): Promise<void>;
  /** Commits the file once the muxer has finished. */
  close(): Promise<void>;
  /** Discards the file instead of committing it. */
  abort?(reason?: unknown): Promise<void>;
}

/** What a file destination has done so far. */
export interface FileDestinationStats {
  /** Bytes handed to the file, counting rewrites of earlier positions. */
  readonly bytesWritten: number;
  /** The file's length: the furthest position written. */
  readonly size: number;
  /** The most bytes ever queued between the muxer and the file. */
  readonly peakQueuedBytes: number;
  /** The queue's bound: past it, the muxer waits for the file. */
  readonly maxQueuedBytes: number;
}

/** Bytes queued for the file before the muxer is made to wait. */
export const DEFAULT_MAX_QUEUED_BYTES = 8 * 1024 * 1024;

/**
 * A seekable file as an export destination.
 *
 * `writable` is the stream mediabunny's `StreamTarget` writes into. The
 * constructor throws a `RangeError` when `maxQueuedBytes` (default
 * {@link DEFAULT_MAX_QUEUED_BYTES}) is not a positive safe integer. Its queue is bounded in bytes,
 * and mediabunny waits while it is full, so a slow disk slows the export instead of growing memory;
 * the encoder and the renderer wait behind the muxer. A write the file refuses fails the export at
 * step `publish`, as `STORAGE_QUOTA_EXCEEDED` when the browser says the quota is exhausted and
 * `OUTPUT_IO_FAILED` otherwise.
 */
export class WritableFileDestination {
  readonly writable: WritableStream<PositionedWrite>;
  readonly #maxQueuedBytes: number;
  #bytesWritten = 0;
  #size = 0;
  #queued = 0;
  #peakQueued = 0;
  #failure: ExportFailureError | undefined;
  readonly #file: PositionalFile;
  #aborted = false;

  constructor(
    file: PositionalFile,
    options: { readonly maxQueuedBytes?: number } = {},
  ) {
    const maxQueuedBytes = options.maxQueuedBytes ?? DEFAULT_MAX_QUEUED_BYTES;
    if (!Number.isSafeInteger(maxQueuedBytes) || maxQueuedBytes <= 0)
      throw new RangeError("maxQueuedBytes must be a positive safe integer");
    this.#maxQueuedBytes = maxQueuedBytes;
    this.#file = file;
    const refused = (error: unknown): never => {
      this.#failure ??= writeFailure(error);
      throw this.#failure;
    };
    this.writable = new WritableStream<PositionedWrite>(
      {
        write: async (chunk) => {
          if (this.#aborted) {
            this.#queued -= chunk.data.byteLength;
            return;
          }
          try {
            await file.write(chunk);
          } catch (error) {
            refused(error);
          } finally {
            this.#queued -= chunk.data.byteLength;
          }
          this.#bytesWritten += chunk.data.byteLength;
          this.#size = Math.max(
            this.#size,
            chunk.position + chunk.data.byteLength,
          );
        },
        close: async () => {
          // A muxer cancelled after `abort` still closes its stream; the file was already
          // discarded.
          if (this.#aborted) return;
          try {
            await file.close();
          } catch (error) {
            refused(error);
          }
        },
        abort: async (reason) => {
          await this.abort(reason);
        },
      },
      {
        highWaterMark: maxQueuedBytes,
        size: (chunk) => {
          this.#queued += chunk.data.byteLength;
          this.#peakQueued = Math.max(this.#peakQueued, this.#queued);
          return chunk.data.byteLength;
        },
      },
    );
  }

  /** A snapshot of what has been written and queued so far. */
  get stats(): FileDestinationStats {
    return {
      bytesWritten: this.#bytesWritten,
      size: this.#size,
      peakQueuedBytes: this.#peakQueued,
      maxQueuedBytes: this.#maxQueuedBytes,
    };
  }

  /**
   * Discards the file: writes still queued are dropped, and a close the muxer sends afterwards does
   * not commit it. A `FileSystemWritableFileStream` then leaves the file as it was before the
   * export.
   */
  async abort(reason?: unknown): Promise<void> {
    if (this.#aborted) return;
    this.#aborted = true;
    await this.#file.abort?.(reason);
  }

  /** The failure the file reported, if it refused a write or its close. */
  get failure(): ExportFailure | undefined {
    return this.#failure?.failure;
  }
}

function writeFailure(error: unknown): ExportFailureError {
  const name = (error as { name?: unknown } | null)?.name;
  const message = error instanceof Error ? error.message : String(error);
  return new ExportFailureError(
    {
      code:
        name === "QuotaExceededError"
          ? "STORAGE_QUOTA_EXCEEDED"
          : "OUTPUT_IO_FAILED",
      message: `The export file could not be written: ${message}`,
      origin: "browser",
      operation: "export",
      backend: "unknown",
      phase: "publish",
      resource: "unknown",
    },
    { cause: error },
  );
}

/**
 * What an in-memory output of `estimatedBytes` can hold at its peak.
 *
 * A growing `ArrayBuffer` doubles, so its capacity reaches up to twice the output; without in-place
 * resizing, the old and new buffers briefly coexist, which costs no more than that. Finishing
 * copies the used part out (one more output), and handing it on as a `Blob` copies it once more.
 * Four times the output, then, and the estimate itself is a guess about a variable-rate encoder,
 * which `margin` covers.
 */
export function memoryOutputPeakBytes(
  estimatedBytes: number,
  margin = MEMORY_OUTPUT_MARGIN,
): number {
  return Math.ceil(estimatedBytes * margin) * MEMORY_OUTPUT_COPIES;
}

/** How many output-sized buffers an in-memory output can hold at once. */
export const MEMORY_OUTPUT_COPIES = 4;
/** How far a variable-rate encoder is allowed past its estimated size. */
export const MEMORY_OUTPUT_MARGIN = 1.5;

/**
 * The size an export's encoded output is expected to reach: the rates it encodes at over its
 * duration, plus a fixed allowance for the container.
 */
export function estimatedOutputBytes(options: {
  readonly durationSeconds: number;
  readonly videoBitrate: number;
  readonly audioBitrate?: number;
}): number {
  const bitsPerSecond = options.videoBitrate + (options.audioBitrate ?? 0);
  return (
    Math.ceil((bitsPerSecond * Math.max(0, options.durationSeconds)) / 8) +
    CONTAINER_ALLOWANCE_BYTES
  );
}

/** Container overhead an estimate allows for: headers and the sample tables. */
const CONTAINER_ALLOWANCE_BYTES = 1024 * 1024;
