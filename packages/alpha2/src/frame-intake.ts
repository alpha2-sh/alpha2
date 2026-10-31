// Shared path by which decoded frames reach the engine's planar conversion: read the planes (with
// retries), hand the frame to the browser's conversion, or request a software decode. Used by
// `FrameSupply` and the render worker host.

import type { ColorMetadata } from "./generated/project-schema.js";
import {
  NonPlanarVideoFrameError,
  type PlanarCopyPool,
  type PlanarFrameIdentity,
  type PlanarYuvFrame,
  readPlanarYuvFrame,
} from "./hdr-frames.js";

/** Retries of a failed plane readback before falling back to a new decode. */
export const READBACK_RETRIES = 2;

/** Cumulative counts of frames the browser could not read back. */
export interface ReadbackRecoveryStats {
  /** Frames still unreadable after retries, plus required frames with no readable planar format. */
  readonly unreadableFrames: number;
  readonly readbackRetries: number;
  /** Frames decoded again in software. */
  readonly redecodedFrames: number;
  /** Frames given to the browser's conversion (`unreadableFrames: "present"`). */
  readonly presentedFrames: number;
}

export type ReadbackCounters = {
  -readonly [Key in keyof ReadbackRecoveryStats]: ReadbackRecoveryStats[Key];
};

export function readbackCounters(): ReadbackCounters {
  return {
    unreadableFrames: 0,
    readbackRetries: 0,
    redecodedFrames: 0,
    presentedFrames: 0,
  };
}

/** Outcome of {@link intakeVideoFrame}. The frame stays open in every case. */
export type VideoFrameIntake =
  | { readonly kind: "planar"; readonly planar: PlanarYuvFrame }
  /** Hand the frame to the browser's conversion (`unreadable: "present"`). */
  | { readonly kind: "present" }
  /** Decode again in software; `cause` is {@link NonPlanarVideoFrameError} or a readback error. */
  | { readonly kind: "redecode"; readonly cause: unknown };

export interface VideoFrameIntakeOptions {
  /** Whether the engine must convert the frame (HDR, or project-declared color). */
  readonly required: boolean;
  /** Project-declared color, which overrides the frame's own. */
  readonly declared: ColorMetadata | null;
  readonly identity: PlanarFrameIdentity;
  readonly pool: PlanarCopyPool;
  readonly counters: ReadbackCounters;
  /** Fallback for frames still unreadable after retries; `"present"` is ignored when `required`. */
  readonly unreadable: "redecode" | "present";
  /** Whether the engine accepts presented `VideoFrame` uploads, which `"present"` needs. */
  readonly takesPresented: boolean;
}

/**
 * Offers `frame` to the engine's planar conversion. The caller still owns the frame. Rejects when
 * the copy fails for a reason other than an unreadable or non-planar frame.
 */
export async function intakeVideoFrame(
  frame: VideoFrame,
  options: VideoFrameIntakeOptions,
): Promise<VideoFrameIntake> {
  try {
    return {
      kind: "planar",
      planar: await readPlanesWithRetries(
        frame,
        options.declared,
        options.identity,
        options.pool,
        options.counters,
      ),
    };
  } catch (error) {
    const nonPlanar = error instanceof NonPlanarVideoFrameError;
    if (!nonPlanar && !isUnreadableFrame(error)) throw error;
    options.counters.unreadableFrames += 1;
    if (
      !nonPlanar &&
      options.unreadable === "present" &&
      !options.required &&
      options.takesPresented
    ) {
      options.counters.presentedFrames += 1;
      return { kind: "present" };
    }
    return { kind: "redecode", cause: error };
  }
}

/**
 * Reads the frame's planes, retrying up to {@link READBACK_RETRIES} times a task apart on readback
 * failures. Rejects with {@link NonPlanarVideoFrameError} or the last read's error.
 */
export async function readPlanesWithRetries(
  frame: VideoFrame,
  declared: ColorMetadata | null,
  identity: PlanarFrameIdentity,
  pool: PlanarCopyPool,
  counters: Pick<ReadbackCounters, "readbackRetries">,
): Promise<PlanarYuvFrame> {
  for (let retry = 0; ; retry += 1) {
    try {
      return await readPlanarYuvFrame(frame, declared, identity, pool);
    } catch (error) {
      if (retry >= READBACK_RETRIES || !isUnreadableFrame(error)) throw error;
      counters.readbackRetries += 1;
      await nextTask();
    }
  }
}

/**
 * Whether a planar copy failed because the browser could not read back a GPU-backed frame. Chrome
 * intermittently rejects `copyTo` on hardware-decoded frames with `InvalidStateError`. A closed
 * frame gives the same error, but callers only copy frames they own and have not closed.
 */
export function isUnreadableFrame(error: unknown): boolean {
  return (
    typeof DOMException !== "undefined" &&
    error instanceof DOMException &&
    error.name === "InvalidStateError"
  );
}

/** Why a host sent a frame back for a software decode. */
export type SoftwareDecodeReason = "non-planar" | "unreadable";

/**
 * Thrown by a render host that needs a frame decoded again in software, because it has no planar
 * format the engine reads (`"non-planar"`, which persists for the source) or its planes could not
 * be read back (`"unreadable"`). Not an export failure: {@link Alpha2Player} and `exportToSink`
 * refill the frame with `decode: "software"`. The request travels in `failure.diagnostics` so it
 * survives a worker boundary.
 */
export class SoftwareDecodeRequiredError extends Error {
  readonly code = "SOFTWARE_DECODE_REQUIRED";
  readonly fatal = false;
  readonly failure: {
    readonly code: "UNKNOWN";
    readonly phase: "decode";
    readonly stage: "software-decode-required";
    readonly diagnostics: SoftwareDecodeRequest;
  };

  constructor(request: SoftwareDecodeRequest, options?: ErrorOptions) {
    super(
      request.reason === "non-planar"
        ? `the frame of ${request.sourceId} at ${request.mediaTimeTicks} has no planar format the engine reads: decode it again in software`
        : `the browser could not read back the frame of ${request.sourceId} at ${request.mediaTimeTicks}: decode it again in software`,
      options,
    );
    this.name = "SoftwareDecodeRequiredError";
    this.failure = {
      code: "UNKNOWN",
      phase: "decode",
      stage: "software-decode-required",
      diagnostics: { ...request },
    };
  }
}

/** Frame requested by a {@link SoftwareDecodeRequiredError}; the time is decimal ticks. */
export interface SoftwareDecodeRequest {
  readonly sourceId: string;
  readonly mediaTimeTicks: string;
  readonly reason: SoftwareDecodeReason;
}

/**
 * The request carried by a {@link SoftwareDecodeRequiredError}, including one rebuilt from a
 * worker's report; otherwise `undefined`.
 */
export function softwareDecodeRequest(
  error: unknown,
): SoftwareDecodeRequest | undefined {
  const value = error as {
    code?: unknown;
    failure?: { diagnostics?: Partial<SoftwareDecodeRequest> };
  } | null;
  if (value?.code !== "SOFTWARE_DECODE_REQUIRED") return undefined;
  const request = value.failure?.diagnostics;
  if (
    typeof request?.sourceId !== "string" ||
    typeof request.mediaTimeTicks !== "string" ||
    (request.reason !== "non-planar" && request.reason !== "unreadable")
  )
    return undefined;
  return {
    sourceId: request.sourceId,
    mediaTimeTicks: request.mediaTimeTicks,
    reason: request.reason,
  };
}

/** Yields a task so the browser can process GPU process replies. */
function nextTask(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}
