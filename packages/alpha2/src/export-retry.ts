/**
 * Reference retry policy for exports, decided from classified failures and the attempt history.
 * It is pure: the caller schedules and runs attempts.
 *
 * An export is retried whole on a fresh device or host process. Attempts may change the in-flight
 * budget, the renderer and the video encoder, never the output settings.
 *
 * - `DEVICE_HUNG`, or a GPU loss recurring at the same frame, is never retried on the GPU; it moves
 *   to the CPU renderer when available, else stops.
 * - Memory exhaustion steps down to a smaller profile.
 * - An encode failure retries once with the software video encoder when available.
 * - `RESOURCE_BUSY` retries unchanged after the longer of its `retryAfterMs` and the backoff.
 * - Other transient failures retry unchanged and step down only if they recur at the same point.
 *   A browser that has already lost the GPU twice moves to the CPU or stops, since the browser
 *   withdraws GPU access at the next reset.
 *
 * Under the native host's `inputPolicy: auto` a small budget can also switch to the readback path
 * and its encoder, changing the bitstream; pin `inputPolicy` to avoid that.
 */

import type { ExportFailure, ExportFailureCode } from "./export-failure.js";

/** In-flight budget scale for an attempt. */
export interface RetryProfile {
  /** Profiles in `history` are matched by name. */
  readonly name: string;
  /** Fraction of each in-flight budget, in `(0, 1]`. */
  readonly memoryScale: number;
}

/** Default profiles: full, half and quarter budgets. */
export const RETRY_PROFILES: readonly RetryProfile[] = Object.freeze([
  Object.freeze({ name: "default", memoryScale: 1 }),
  Object.freeze({ name: "reduced", memoryScale: 0.5 }),
  Object.freeze({ name: "minimal", memoryScale: 0.25 }),
]);

export type AttemptRenderer = "gpu" | "cpu";

/** `default` lets the runtime choose the video encoder. */
export type AttemptVideoEncoder = "default" | "software";

/** One failed attempt. */
export interface AttemptOutcome {
  readonly failure: ExportFailure;
  readonly profile: RetryProfile;
  readonly renderer: AttemptRenderer;
  readonly videoEncoder: AttemptVideoEncoder;
}

/** Options for {@link decideRetry}. */
export interface RetryPolicyOptions {
  /** Total attempts, including the first. Default 3. */
  readonly maxAttempts?: number;
  /** Delay before attempt `n + 1`, given `n` failed attempts. Default `1000 * 2^(n-1)` ms. */
  readonly backoffMs?: (failedAttempts: number) => number;
  /** Profiles to step down through, largest first. Default {@link RETRY_PROFILES}. */
  readonly profiles?: readonly RetryProfile[];
  /** Whether the caller can run an attempt on a CPU renderer. Default false. */
  readonly cpuRenderer?: boolean;
  /** GPU losses this browser page has already seen, from any work. Default 0. */
  readonly pageGpuLosses?: number;
  /**
   * Whether a software video encoder is available (`canEncodeInSoftware` in `alpha2/mediabunny`).
   * Default true.
   */
  readonly softwareVideoEncoder?: boolean;
}

/**
 * Result of {@link decideRetry}. On stop, `failure` is the failure as the policy read it (a loss
 * recurring at the same frame becomes `DEVICE_HUNG`). `reason` is a human-readable explanation.
 */
export type RetryDecision =
  | {
      readonly retry: true;
      readonly delayMs: number;
      readonly profile: RetryProfile;
      readonly renderer: AttemptRenderer;
      readonly videoEncoder: AttemptVideoEncoder;
      readonly reason: string;
    }
  | {
      readonly retry: false;
      readonly reason: string;
      readonly failure: ExportFailure | undefined;
    };

const PERMANENT: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "CANCELLED",
  "INVALID_INPUT",
  "VALIDATION_FAILED",
  "RESOURCE_LIMIT",
  "STORAGE_QUOTA_EXCEEDED",
  "OUTPUT_IO_FAILED",
  // `decideRetry` still retries this on a CPU renderer when the caller has one.
  "DEVICE_HUNG",
]);

const MEMORY: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "OUT_OF_MEMORY",
  "ALLOCATION_FAILED",
]);

const GPU_LOSSES: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "DEVICE_LOST",
  "GPU_TIMEOUT",
]);

/**
 * Whether a retry could help. False for invalid input, validation errors, budget or storage
 * refusals, cancellation, `DEVICE_HUNG`, output I/O failures and download failures. Use
 * {@link decideRetry} to also consider a CPU renderer.
 */
export function isRetryableFailure(failure: ExportFailure): boolean {
  return !PERMANENT.has(failure.code) && failure.phase !== "download";
}

/**
 * Decides whether and how to retry after the failed attempts in `history` (oldest first). An empty
 * history never retries. The caller waits `delayMs` and runs the next attempt.
 */
export function decideRetry(
  history: readonly AttemptOutcome[],
  options: RetryPolicyOptions = {},
): RetryDecision {
  const maxAttempts = options.maxAttempts ?? 3;
  const profiles = options.profiles ?? RETRY_PROFILES;
  const backoff =
    options.backoffMs ?? ((failed: number) => 1_000 * 2 ** (failed - 1));
  const last = history.at(-1);
  if (last === undefined)
    return { retry: false, reason: "nothing failed", failure: undefined };
  const { failure } = last;
  // Explains a skipped rule; appended to the deciding rule's reason.
  let note = "";
  const stop = (why: string, final: ExportFailure = failure) => ({
    retry: false as const,
    reason: why + note,
    failure: final,
  });
  const attemptsLeft = history.length < maxAttempts;
  const index = Math.max(
    0,
    profiles.findIndex((profile) => profile.name === last.profile.name),
  );
  const smaller = profiles[index + 1];
  const retry = (
    why: string,
    changes: {
      readonly profile?: RetryProfile;
      readonly renderer?: AttemptRenderer;
      readonly videoEncoder?: AttemptVideoEncoder;
      readonly delayMs?: number;
    } = {},
  ): RetryDecision => ({
    retry: true,
    delayMs: changes.delayMs ?? Math.max(0, backoff(history.length)),
    profile: changes.profile ?? profiles[index] ?? last.profile,
    renderer: changes.renderer ?? last.renderer,
    videoEncoder: changes.videoEncoder ?? last.videoEncoder,
    reason: why + note,
  });
  const previous = history.at(-2);
  const samePoint =
    previous !== undefined &&
    previous.failure.phase === failure.phase &&
    (previous.failure.frameIndex ?? null) === (failure.frameIndex ?? null);
  const recurredLoss =
    GPU_LOSSES.has(failure.code) &&
    previous !== undefined &&
    previous.renderer === "gpu" &&
    (GPU_LOSSES.has(previous.failure.code) ||
      previous.failure.code === "DEVICE_HUNG") &&
    (previous.failure.frameIndex ?? null) === (failure.frameIndex ?? null);
  if (failure.code === "DEVICE_HUNG" || recurredLoss) {
    const hung: ExportFailure = { ...failure, code: "DEVICE_HUNG" };
    if (last.renderer === "gpu" && options.cpuRenderer === true && attemptsLeft)
      return retry(
        `${failure.code}: the export's own GPU work stops the GPU; retrying on the CPU renderer`,
        { renderer: "cpu" },
      );
    return stop(
      `${failure.code}: the export's own GPU work stops the GPU, and ${
        last.renderer === "cpu"
          ? "it failed on the CPU renderer too"
          : "there is no CPU renderer to retry on"
      }`,
      hung,
    );
  }
  if (!isRetryableFailure(failure))
    return stop(`${failure.code} at ${failure.phase} is not helped by a retry`);
  if (!attemptsLeft) return stop(`all ${maxAttempts} attempts failed`);
  if (MEMORY.has(failure.code))
    return smaller
      ? retry(
          `${failure.code}: retrying with less in flight (${smaller.name})`,
          { profile: smaller },
        )
      : stop(
          `${failure.code} with the smallest profile (${last.profile.name}): nothing left to reduce`,
        );
  if (failure.phase === "encode" && last.videoEncoder === "default") {
    if (options.softwareVideoEncoder !== false)
      return retry(
        `${failure.code} while encoding: retrying with the software video encoder`,
        { videoEncoder: "software" },
      );
    note =
      " (this browser has no software video encoder for the export to switch to)";
  }
  if (failure.code === "RESOURCE_BUSY")
    return retry(`${failure.code}: retrying when the device may be free`, {
      delayMs: Math.max(0, failure.retryAfterMs ?? 0, backoff(history.length)),
    });
  const repeated = samePoint && previous?.failure.code === failure.code;
  if (!repeated && GPU_LOSSES.has(failure.code)) {
    if (failure.origin === "browser" && (options.pageGpuLosses ?? 0) >= 2)
      return options.cpuRenderer === true && last.renderer === "gpu"
        ? retry(
            `${failure.code}: the browser withdraws GPU access after a second GPU reset; retrying on the CPU renderer`,
            { renderer: "cpu" },
          )
        : stop(
            `${failure.code}: the browser withdraws GPU access after a second GPU reset`,
          );
    return retry(
      `${failure.code}: retrying on a fresh ${failure.origin === "browser" ? "device" : "host"}`,
    );
  }
  if (!repeated)
    return retry(
      `${failure.code}: retrying on a fresh ${failure.origin === "browser" ? "device" : "host"}`,
    );
  return smaller
    ? retry(
        `${failure.code} again at the same point: retrying with less in flight (${smaller.name})`,
        { profile: smaller },
      )
    : stop(
        `${failure.code} again at the same point with the smallest profile: stopping early`,
      );
}
