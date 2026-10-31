/**
 * A reference retry policy for exports, decided from classified failures and the attempts so far.
 *
 * It is a policy, not a mechanism: a service calls it with its own history and options and decides
 * what to do with the answer. The defaults (three attempts, backing off one second, then two, then
 * four) are shared by every caller, so services built on it retry alike.
 *
 * An export is retried whole, with a fresh device or host process, never resumed mid-file. What
 * changes between attempts is how much it keeps in flight, which renderer draws it, and which video
 * encoder writes it; output settings (resolution, rate, codec, quality, color, audio) never change.
 *
 * - A device the export's own work stopped (`DEVICE_HUNG`, or a lost device or GPU timeout that
 *   recurs at the same frame on the GPU) is never retried on the GPU: the same work stops it again.
 *   The next attempt renders on the CPU when the caller has a CPU renderer, and otherwise the export
 *   stops.
 * - A failure evidence of memory exhaustion steps down to a smaller profile.
 * - A failure while encoding retries once with the software video encoder, when the caller has
 *   one (`softwareVideoEncoder`).
 * - A busy device (`RESOURCE_BUSY`) retries as it was, after the wait its failure names when that
 *   is longer than the backoff.
 * - A transient failure (a lost device, a crashed host, a refused mapping) retries as it was, and
 *   steps down only if it recurs at the same point. In a browser that has already reset the GPU
 *   twice for this page, a lost device goes to the CPU renderer or stops: the browser withdraws GPU
 *   access after the next reset.
 *
 * The encoder can also change under the native host's `inputPolicy: auto`: a budget too small for
 * the GPU-resident path's surfaces takes the readback path and its encoder, so the bitstream may
 * differ; pin `inputPolicy` to keep it.
 */

import type { ExportFailure, ExportFailureCode } from "./export-failure.js";

/** How much an attempt keeps in flight, as a fraction of each budget it would otherwise use. */
export interface RetryProfile {
  /** Identifies the profile in `history`; profiles are matched by name. */
  readonly name: string;
  /** The fraction of each in-flight budget the attempt uses, in `(0, 1]`. */
  readonly memoryScale: number;
}

/** The profiles attempts step down through: everything, half, a quarter. */
export const RETRY_PROFILES: readonly RetryProfile[] = Object.freeze([
  Object.freeze({ name: "default", memoryScale: 1 }),
  Object.freeze({ name: "reduced", memoryScale: 0.5 }),
  Object.freeze({ name: "minimal", memoryScale: 0.25 }),
]);

/** Which renderer an attempt drew on. */
export type AttemptRenderer = "gpu" | "cpu";

/** Which video encoder an attempt asked for: the runtime's choice, or a software one. */
export type AttemptVideoEncoder = "default" | "software";

/** One failed attempt, as the policy reads it. */
export interface AttemptOutcome {
  /** The attempt's classified failure. */
  readonly failure: ExportFailure;
  /** The profile the attempt ran with. */
  readonly profile: RetryProfile;
  /** The renderer it drew on. */
  readonly renderer: AttemptRenderer;
  /** The video encoder it asked for. */
  readonly videoEncoder: AttemptVideoEncoder;
}

/** Overrides for {@link decideRetry}'s defaults, and what the caller can switch to. */
export interface RetryPolicyOptions {
  /** Attempts in all, the first included. Default 3. */
  readonly maxAttempts?: number;
  /** Delay before attempt `n + 1`, given `n` failed attempts. Default `1000 * 2^(n-1)` ms. */
  readonly backoffMs?: (failedAttempts: number) => number;
  /** Profiles to step down through, largest first. Default {@link RETRY_PROFILES}. */
  readonly profiles?: readonly RetryProfile[];
  /** Whether the caller can run an attempt on a CPU renderer. Default false. */
  readonly cpuRenderer?: boolean;
  /**
   * GPU resets the browser has already reported for this page (device losses of earlier
   * attempts and other work), for a browser export. Default 0.
   */
  readonly pageGpuLosses?: number;
  /**
   * Whether the caller's browser has a software video encoder for this export
   * (`canEncodeInSoftware` in `alpha2/mediabunny`). `false` skips the switch to it after an encode
   * failure, and the decision's reason says so. Default: assumed available.
   */
  readonly softwareVideoEncoder?: boolean;
}

/**
 * What {@link decideRetry} decided: retry after `delayMs` with `profile` on `renderer` with
 * `videoEncoder`, or stop with `failure`, the attempt's failure as the policy read it (a loss that
 * recurred at the same frame reads as `DEVICE_HUNG`). `reason` is a human-readable explanation for
 * logs and diagnostics.
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

/** Failures the same request would meet again, or that a retry cannot help. */
const PERMANENT: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "CANCELLED",
  "INVALID_INPUT",
  "VALIDATION_FAILED",
  "RESOURCE_LIMIT",
  "STORAGE_QUOTA_EXCEEDED",
  "OUTPUT_IO_FAILED",
  // Only a CPU renderer helps; `decideRetry` gives one to a caller that has it.
  "DEVICE_HUNG",
]);

/** Failures that are evidence of memory running out. */
const MEMORY: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "OUT_OF_MEMORY",
  "ALLOCATION_FAILED",
]);

/** Losses of the GPU that a retry on a fresh device can survive once. */
const GPU_LOSSES: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "DEVICE_LOST",
  "GPU_TIMEOUT",
]);

/**
 * Whether a retry could succeed where `failure` did: not for invalid input or a GPU validation
 * error, a budget or storage refusal, a cancellation, a device the export's own work stopped, or a
 * file that could not be written or downloaded (a finished file is downloaded again, not rendered
 * again). A caller with a CPU renderer asks {@link decideRetry}, which retries a stopped device
 * there.
 */
export function isRetryableFailure(failure: ExportFailure): boolean {
  return !PERMANENT.has(failure.code) && failure.phase !== "download";
}

/**
 * Whether to try again after the attempts in `history` (oldest first) failed,
 * and how. An empty history decides not to retry. Pure: it schedules nothing,
 * and the caller waits `delayMs` and runs the next attempt itself.
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
  // Why a rule that would have applied did not, appended to the reason of the one that decides.
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
  // The GPU stopped for this export's own work: said so, or lost twice at the same frame.
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
