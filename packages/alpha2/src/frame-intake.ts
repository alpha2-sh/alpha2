// How a decoded `VideoFrame` the engine converts from planes reaches it: its planes read, read
// again when the browser could not read them back, handed to the browser's own conversion when the
// host allows that, or sent back for a software decode. `FrameSupply` (pull: it decodes again
// itself) and the render worker host (push: it asks the page to decode again) share this one path.

import type { ColorMetadata } from "./generated/project-schema.js";
import {
  NonPlanarVideoFrameError,
  type PlanarCopyPool,
  type PlanarFrameIdentity,
  type PlanarYuvFrame,
  readPlanarYuvFrame,
} from "./hdr-frames.js";

/** Reads repeated on a frame whose planes the browser could not read back, before a new decode. */
export const READBACK_RETRIES = 2;

/** How a host recovered frames the browser could not read back; cumulative. */
export interface ReadbackRecoveryStats {
  /**
   * Frames whose planes the browser still could not read back after the repeated reads, and frames
   * the engine had to convert that had no planar format it reads.
   */
  readonly unreadableFrames: number;
  /** Reads repeated on the same frame. */
  readonly readbackRetries: number;
  /** Frames decoded again in software and copied from that decode. */
  readonly redecodedFrames: number;
  /** Frames given to the browser's conversion (`unreadableFrames: "present"`). */
  readonly presentedFrames: number;
}

/** The counters {@link intakeVideoFrame} adds to. */
export type ReadbackCounters = {
  -readonly [Key in keyof ReadbackRecoveryStats]: ReadbackRecoveryStats[Key];
};

/** Fresh counters for a host that recovers unreadable frames. */
export function readbackCounters(): ReadbackCounters {
  return {
    unreadableFrames: 0,
    readbackRetries: 0,
    redecodedFrames: 0,
    presentedFrames: 0,
  };
}

/** What becomes of a frame offered to the engine's planar conversion. */
export type VideoFrameIntake =
  /** Its planes, copied out; the frame is still open. */
  | { readonly kind: "planar"; readonly planar: PlanarYuvFrame }
  /** The browser's conversion takes it (`unreadable: "present"`); the frame is still open. */
  | { readonly kind: "present" }
  /**
   * It must be decoded again in software: it has no planar format the engine reads (`cause` a
   * {@link NonPlanarVideoFrameError}), or the browser could not read its planes back. The frame is
   * still open.
   */
  | { readonly kind: "redecode"; readonly cause: unknown };

/** How {@link intakeVideoFrame} treats one frame. */
export interface VideoFrameIntakeOptions {
  /** Whether the engine must convert it: HDR, or color the project declares. */
  readonly required: boolean;
  /** The color the project declares for the source, which wins over the frame's own. */
  readonly declared: ColorMetadata | null;
  readonly identity: PlanarFrameIdentity;
  readonly pool: PlanarCopyPool;
  readonly counters: ReadbackCounters;
  /**
   * What becomes of a frame whose planes the browser cannot read back, once reading them again
   * has failed: decoded again in software, or, unless the engine must convert it, given to the
   * browser's conversion.
   */
  readonly unreadable: "redecode" | "present";
  /** Whether the engine takes presented `VideoFrame` uploads, which `"present"` needs. */
  readonly takesPresented: boolean;
}

/**
 * Offers `frame` to the engine's planar conversion. Every outcome leaves the frame open: the caller
 * closes it, uploads it, or decodes its picture again. Rejects with the copy's error when it fails
 * for any other reason than an unreadable or non-planar frame.
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
 * The frame's planes, read again up to {@link READBACK_RETRIES} times, a task apart, while the
 * browser fails to read them back. Leaves the frame open. Rejects with
 * {@link NonPlanarVideoFrameError} for a frame no read can copy, and with the last read's error.
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
 * Whether a planar copy failed because the browser could not read a GPU-backed frame back: Chrome
 * rejects `copyTo` with `InvalidStateError` ("Failed to read VideoFrame data."), now and then, for
 * hardware-decoded frames. A closed frame rejects with the same name, but the hosts own the frames
 * they copy and have not closed them.
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
 * A render host could not take a frame the engine must convert, and asks for the same picture
 * decoded in software: it had no planar format the engine reads (`"non-planar"`, which every later
 * frame of the source repeats), or the browser could not read its planes back (`"unreadable"`).
 * The frame was closed. {@link Alpha2Player} and `exportToSink` answer it by asking their refill
 * for the frame again with `decode: "software"`; it is not a failure of the export. The source and
 * time travel in `failure.diagnostics`, so the request survives a worker boundary.
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

/** The frame a {@link SoftwareDecodeRequiredError} asks for again; times as decimal ticks. */
export interface SoftwareDecodeRequest {
  readonly sourceId: string;
  readonly mediaTimeTicks: string;
  readonly reason: SoftwareDecodeReason;
}

/**
 * The frame `error` asks to decode again in software, when it is a
 * {@link SoftwareDecodeRequiredError}, in this realm or rebuilt from a worker's report; otherwise
 * `undefined`.
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

/** Resolves once the browser has run other tasks, such as the GPU process's replies. */
function nextTask(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}
