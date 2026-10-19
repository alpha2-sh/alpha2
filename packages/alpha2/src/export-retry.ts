/**
 * A reference retry policy for exports, decided from classified failures and the attempts so far.
 *
 * It is a policy, not a mechanism: a service calls it with its own history and options and decides
 * what to do with the answer. The defaults (three attempts, backing off one second, then two, then
 * four) are shared by every caller, so services built on it retry alike.
 *
 * An export is retried whole, with a fresh device or host process, never resumed mid-file. What
 * changes between attempts is only how much it keeps in flight: a failure that is evidence of
 * memory exhaustion steps down to a smaller profile, and a transient one (a lost device, a crashed
 * host, a refused mapping) retries as it was, stepping down only if it recurs at the same point.
 * Output settings (resolution, rate, codec, quality, color, audio) are never changed by a retry.
 * The encoder can change: under the native host's `inputPolicy: auto`, a budget too small for the
 * GPU-resident path's surfaces takes the readback path and its encoder, so the bitstream may
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

/** One failed attempt, as the policy reads it. */
export interface AttemptOutcome {
  /** The attempt's classified failure. */
  readonly failure: ExportFailure;
  /** The profile the attempt ran with. */
  readonly profile: RetryProfile;
}

/** Overrides for {@link decideRetry}'s defaults. */
export interface RetryPolicyOptions {
  /** Attempts in all, the first included. Default 3. */
  readonly maxAttempts?: number;
  /** Delay before attempt `n + 1`, given `n` failed attempts. Default `1000 * 2^(n-1)` ms. */
  readonly backoffMs?: (failedAttempts: number) => number;
  /** Profiles to step down through, largest first. Default {@link RETRY_PROFILES}. */
  readonly profiles?: readonly RetryProfile[];
}

/**
 * What {@link decideRetry} decided: retry after `delayMs` with `profile`, or
 * stop. `reason` is a human-readable explanation for logs and diagnostics.
 */
export type RetryDecision =
  | {
      readonly retry: true;
      readonly delayMs: number;
      readonly profile: RetryProfile;
      readonly reason: string;
    }
  | { readonly retry: false; readonly reason: string };

/** Failures the same request would meet again, or that a retry cannot help. */
const PERMANENT: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "CANCELLED",
  "INVALID_INPUT",
  "VALIDATION_FAILED",
  "RESOURCE_LIMIT",
  "STORAGE_QUOTA_EXCEEDED",
  "OUTPUT_IO_FAILED",
]);

/** Failures that are evidence of memory running out. */
const MEMORY: ReadonlySet<ExportFailureCode> = new Set<ExportFailureCode>([
  "OUT_OF_MEMORY",
  "ALLOCATION_FAILED",
]);

/**
 * Whether a retry could succeed where `failure` did: not for invalid input or a GPU validation
 * error, a budget or storage refusal, a cancellation, or a file that could not be written or
 * downloaded (a finished file is downloaded again, not rendered again).
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
  if (last === undefined) return { retry: false, reason: "nothing failed" };
  const { failure } = last;
  if (!isRetryableFailure(failure))
    return {
      retry: false,
      reason: `${failure.code} at ${failure.phase} is not helped by a retry`,
    };
  if (history.length >= maxAttempts)
    return {
      retry: false,
      reason: `all ${maxAttempts} attempts failed`,
    };
  const index = Math.max(
    0,
    profiles.findIndex((profile) => profile.name === last.profile.name),
  );
  const smaller = profiles[index + 1];
  const retry = (profile: RetryProfile, reason: string): RetryDecision => ({
    retry: true,
    delayMs: Math.max(0, backoff(history.length)),
    profile,
    reason,
  });
  if (MEMORY.has(failure.code))
    return smaller
      ? retry(
          smaller,
          `${failure.code}: retrying with less in flight (${smaller.name})`,
        )
      : {
          retry: false,
          reason: `${failure.code} with the smallest profile (${last.profile.name}): nothing left to reduce`,
        };
  const previous = history.at(-2);
  const repeated =
    previous !== undefined &&
    previous.failure.code === failure.code &&
    previous.failure.phase === failure.phase &&
    (previous.failure.frameIndex ?? null) === (failure.frameIndex ?? null);
  if (!repeated)
    return retry(
      profiles[index] ?? last.profile,
      `${failure.code}: retrying on a fresh ${failure.origin === "browser" ? "device" : "host"}`,
    );
  return smaller
    ? retry(
        smaller,
        `${failure.code} again at the same point: retrying with less in flight (${smaller.name})`,
      )
    : {
        retry: false,
        reason: `${failure.code} again at the same point with the smallest profile: stopping early`,
      };
}
