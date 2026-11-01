/**
 * Destinations for a browser export's encoded bytes.
 *
 * The default streams to a file through a bounded queue, so the output never accumulates in
 * memory. In-memory output is an explicit mode for small exports, sized before the export starts.
 */

import { ExportFailureError, type ExportFailure } from "./export-failure.js";

/** A positioned write, as emitted by mediabunny's `StreamTarget`. */
export interface PositionedWrite {
  readonly type: "write";
  readonly data: Uint8Array;
  readonly position: number;
}

/** A file that takes positioned writes, such as a `FileSystemWritableFileStream`. */
export interface PositionalFile {
  /** Writes `chunk.data` at `chunk.position`; a rejection fails the export. */
  write(chunk: PositionedWrite): Promise<void>;
  /** Commits the file once the muxer has finished. */
  close(): Promise<void>;
  /** Discards the file instead of committing it. */
  abort?(reason?: unknown): Promise<void>;
}

/** Progress of a file destination. */
export interface FileDestinationStats {
  /** Bytes handed to the file, counting rewrites of earlier positions. */
  readonly bytesWritten: number;
  /** Furthest position written. */
  readonly size: number;
  /** Peak bytes queued between the muxer and the file. */
  readonly peakQueuedBytes: number;
  /** Queue bound; past it the muxer waits for the file. */
  readonly maxQueuedBytes: number;
}

/** Bytes queued for the file before the muxer is made to wait. */
export const DEFAULT_MAX_QUEUED_BYTES = 8 * 1024 * 1024;

/**
 * A seekable file as an export destination. `writable` is the stream for mediabunny's
 * `StreamTarget`.
 *
 * The queue is bounded by `maxQueuedBytes` (default {@link DEFAULT_MAX_QUEUED_BYTES}), so a slow
 * disk slows the export instead of growing memory. A refused write fails the export in phase
 * `publish` with `STORAGE_QUOTA_EXCEEDED` or `OUTPUT_IO_FAILED`.
 *
 * @throws RangeError When `maxQueuedBytes` is not a positive safe integer.
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
          // A muxer cancelled after `abort` still closes its stream.
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

  get stats(): FileDestinationStats {
    return {
      bytesWritten: this.#bytesWritten,
      size: this.#size,
      peakQueuedBytes: this.#peakQueued,
      maxQueuedBytes: this.#maxQueuedBytes,
    };
  }

  /**
   * Discards the file. Queued writes are dropped and a later close from the muxer does not commit
   * it.
   */
  async abort(reason?: unknown): Promise<void> {
    if (this.#aborted) return;
    this.#aborted = true;
    await this.#file.abort?.(reason);
  }

  /** Set when the file refused a write or the close. */
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
 * Peak memory of an in-memory output of `estimatedBytes`.
 *
 * A doubling `ArrayBuffer` reaches up to twice the output, finishing copies it once and the `Blob`
 * copies it again, so four output-sized buffers. `margin` covers the variable-rate estimate.
 */
export function memoryOutputPeakBytes(
  estimatedBytes: number,
  margin = MEMORY_OUTPUT_MARGIN,
): number {
  return Math.ceil(estimatedBytes * margin) * MEMORY_OUTPUT_COPIES;
}

/** Output-sized buffers an in-memory output can hold at once. */
export const MEMORY_OUTPUT_COPIES = 4;
/** Allowed overshoot of a variable-rate encoder past its estimated size. */
export const MEMORY_OUTPUT_MARGIN = 1.5;

/** Expected output size: bitrates times duration, plus a fixed container allowance. */
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

const CONTAINER_ALLOWANCE_BYTES = 1024 * 1024;
