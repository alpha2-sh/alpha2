import {
  classifyBrowserFailure,
  failureOf,
  withSecondaryFailure,
  type ExportFailure,
} from "alpha2/export-failure";
import {
  decideRetry,
  RETRY_PROFILES,
  type AttemptOutcome,
  type AttemptRenderer,
  type AttemptVideoEncoder,
  type RetryPolicyOptions,
  type RetryProfile,
} from "alpha2/export-retry";
import { validFrameRate } from "alpha2/project";
import { EditorError } from "../core/document.js";
import type { EditorDocument } from "../core/document.js";
import type {
  ExportAttempt,
  ExportBackend,
  ExportProgress,
  ExportArtifact,
} from "../ports/index.js";

/**
 * Refuses an export `frameRate` the engine cannot read, before an export starts, for every
 * backend: one rate, the same everywhere.
 */
export function requireExportFrameRate(frameRate: unknown): void {
  if (frameRate !== undefined && !validFrameRate(frameRate))
    throw new EditorError(
      "invalid_frame_rate",
      `Export frameRate must be { num, den }, positive 32-bit integers with a frame at least one tick long; received ${JSON.stringify(frameRate)}`,
      "export",
    );
}

/** What happened to one attempt of a job. */
export interface ExportAttemptRecord {
  readonly id: string;
  readonly number: number;
  readonly profile: RetryProfile;
  readonly renderer: AttemptRenderer;
  readonly videoEncoder: AttemptVideoEncoder;
  readonly startedAt: number;
  readonly endedAt?: number;
  readonly outcome: "running" | "succeeded" | "failed" | "cancelled";
  readonly failure?: ExportFailure;
  /** What the retry policy decided after this attempt failed, and why. */
  readonly decision?: { readonly retry: boolean; readonly reason: string };
}

/** Options of an {@link ExportJob}. */
export interface ExportJobOptions {
  /** The job's id, when its backend already named it (a service's job); a fresh UUID otherwise. */
  readonly id?: string;
  /**
   * The retry policy (see `alpha2/export-retry`), or `false` for one attempt. Default: three
   * attempts, the reference backoff.
   */
  readonly retry?: RetryPolicyOptions | false;
  /** Waits `ms` or rejects when `signal` aborts; replaceable for tests. */
  readonly sleep?: (ms: number, signal: AbortSignal) => Promise<void>;
}

/**
 * One export of a pinned document, run in bounded attempts.
 *
 * Every attempt renders the same document from the start, on a fresh device or host, with the same
 * output settings; a failed one is retried only as the retry policy allows, with less in flight
 * when the failure is evidence of memory exhaustion. Progress from an attempt that has ended is
 * ignored, so a late answer cannot move a newer attempt's progress or change its result. Cancelling
 * stops the running attempt and any pending retry; no new attempt starts after it.
 */
export class ExportJob {
  readonly id: string;
  /**
   * Resolves to the artifact. Rejects with `EditorError` `cancelled` after {@link cancel}, or
   * with the last attempt's error, whose `failure` property (and {@link failure}) holds the
   * classified `ExportFailure`.
   */
  readonly result: Promise<ExportArtifact>;
  #controller = new AbortController();
  #listeners = new Set<(value: ExportProgress) => void>();
  #progress: ExportProgress = { phase: "preparing", fraction: 0 };
  #settled = false;
  #failure: ExportFailure | undefined;
  #attempts: ExportAttemptRecord[] = [];
  #current = 0;
  /**
   * Starts the job at once, on the next microtask.
   *
   * @param revision The editor revision `document` was pinned at.
   * @param backend Runs each attempt; its `dispose` runs once when the job settles.
   * @param release Called once when the job settles, whatever the outcome.
   */
  constructor(
    readonly revision: number,
    document: EditorDocument,
    backend: ExportBackend,
    release: () => void = () => {},
    options: ExportJobOptions = {},
  ) {
    this.id = options.id ?? crypto.randomUUID();
    const signal = this.#controller.signal;
    const progress = (value: ExportProgress) => {
      this.#progress = Object.freeze(value);
      for (const listener of this.#listeners) listener(this.#progress);
    };
    const cancelledBy = (cause: unknown) =>
      signal.aborted &&
      !(cause instanceof EditorError && cause.code === "cancel_failed");
    this.result = this.#attempt(document, backend, progress, options)
      .then(async (value) => {
        // A cancel wins until the job settles, even over an attempt that already finished: what it
        // made is released and the job settles as cancelled.
        if (signal.aborted) {
          this.#record(this.#current, {
            outcome: "cancelled",
            endedAt: Date.now(),
          });
          await releaseArtifact(value);
          signal.throwIfAborted();
        }
        return value;
      })
      .then(
        async (value) => {
          await backend.dispose?.({ ok: true, cancelled: false });
          progress({
            phase: "completed",
            fraction: 1,
            attempt: this.#current,
          });
          return value;
        },
        async (cause: unknown) => {
          const cancelled = cancelledBy(cause);
          let failure: ExportFailure | undefined = cancelled
            ? undefined
            : {
                ...(failureOf(cause) ??
                  classifyBrowserFailure(cause, {
                    phase: phaseOf(this.#progress),
                  })),
                jobId: this.id,
              };
          try {
            await backend.dispose?.({ ok: false, cancelled });
          } catch (cleanup) {
            // A failed job keeps its failure and notes the cleanup beside it.
            if (cancelled) throw cleanup;
            if (failure)
              failure = withSecondaryFailure(failure, {
                code: "UNKNOWN",
                phase: "cleanup",
                message:
                  cleanup instanceof Error ? cleanup.message : String(cleanup),
              });
          }
          if (failure) this.#failure = Object.freeze(failure);
          throw cause;
        },
      )
      .catch((cause: unknown) => {
        const cancelled = cancelledBy(cause);
        progress({
          phase: cancelled ? "cancelled" : "failed",
          fraction: this.#progress.fraction,
          attempt: this.#current,
        });
        throw cancelled
          ? new EditorError(
              "cancelled",
              "Export cancelled",
              "export",
              undefined,
              true,
              { cause },
            )
          : cause;
      })
      .finally(() => {
        this.#settled = true;
        release();
      });
    // Jobs may be cancelled before the caller starts awaiting result.
    void this.result.catch(() => {});
  }

  async #attempt(
    document: EditorDocument,
    backend: ExportBackend,
    progress: (value: ExportProgress) => void,
    options: ExportJobOptions,
  ): Promise<ExportArtifact> {
    const signal = this.#controller.signal;
    const policy = options.retry === false ? undefined : (options.retry ?? {});
    const sleep = options.sleep ?? abortableSleep;
    const history: AttemptOutcome[] = [];
    let profile: RetryProfile = policy?.profiles?.[0] ?? RETRY_PROFILES[0];
    let renderer: AttemptRenderer = "gpu";
    let videoEncoder: AttemptVideoEncoder = "default";
    await Promise.resolve();
    for (let number = 1; ; number += 1) {
      signal.throwIfAborted();
      const attempt: ExportAttempt = Object.freeze({
        id: `${this.id}/${number}`,
        number,
        profile,
        renderer,
        videoEncoder,
        ...(history.length > 0 ? { previous: history.at(-1)!.failure } : {}),
      });
      this.#current = number;
      this.#record(number, {
        id: attempt.id,
        number,
        profile,
        renderer,
        videoEncoder,
        startedAt: Date.now(),
        outcome: "running",
      });
      // Progress from an attempt that has ended belongs to nothing.
      const report = (value: ExportProgress) => {
        if (this.#current === number && !this.#settled)
          progress({ ...value, attempt: number });
      };
      try {
        const value = await backend.run(document, signal, report, attempt);
        this.#record(number, { outcome: "succeeded", endedAt: Date.now() });
        return value;
      } catch (cause) {
        if (signal.aborted) {
          this.#record(number, { outcome: "cancelled", endedAt: Date.now() });
          throw cause;
        }
        // The next attempt owns progress from here: a late report from this one is dropped.
        this.#current = number + 1;
        const failure: ExportFailure = {
          ...(failureOf(cause) ??
            classifyBrowserFailure(cause, {
              phase: phaseOf(this.#progress),
            })),
          jobId: this.id,
          attemptId: attempt.id,
        };
        history.push({ failure, profile, renderer, videoEncoder });
        const decision = policy
          ? decideRetry(history, policy)
          : {
              retry: false as const,
              reason: "retries are off for this job",
              failure,
            };
        // A loss that recurred at the same frame is reported as the policy read it: hung.
        const final = decision.retry ? failure : (decision.failure ?? failure);
        this.#record(number, {
          outcome: "failed",
          endedAt: Date.now(),
          failure: final,
          decision: { retry: decision.retry, reason: decision.reason },
        });
        if (!decision.retry) {
          this.#current = number;
          if (cause instanceof Error)
            Object.defineProperty(cause, "failure", {
              value: final,
              configurable: true,
            });
          throw cause;
        }
        progress({
          phase: "retrying",
          fraction: this.#progress.fraction,
          attempt: number + 1,
          retry: { reason: decision.reason, delayMs: decision.delayMs },
        });
        await sleep(decision.delayMs, signal);
        profile = decision.profile;
        renderer = decision.renderer;
        videoEncoder = decision.videoEncoder;
      }
    }
  }

  #record(number: number, patch: Partial<ExportAttemptRecord>) {
    const index = number - 1;
    this.#attempts[index] = Object.freeze({
      ...this.#attempts[index],
      ...patch,
    });
  }

  /** The latest progress, frozen. Reports from an attempt that has ended are not reflected. */
  get progress() {
    return this.#progress;
  }
  /** Why the job failed, classified; undefined unless it failed. */
  get failure(): ExportFailure | undefined {
    return this.#failure;
  }
  /** Every attempt so far, oldest first. */
  get attempts(): readonly ExportAttemptRecord[] {
    return [...this.#attempts];
  }
  /**
   * Calls `listener` with the current progress at once and with every later change. Returns
   * the unsubscribe function.
   */
  subscribe(listener: (value: ExportProgress) => void): () => void {
    this.#listeners.add(listener);
    listener(this.#progress);
    return () => this.#listeners.delete(listener);
  }
  /**
   * Stops the running attempt and any pending retry, and resolves once the job has settled. An
   * attempt that finished before the job settled has what it made released (a temporary file
   * deleted). Resolves without effect on a settled job. Rejects only when cleanup failed
   * (`EditorError` `cancel_failed`).
   */
  async cancel(): Promise<void> {
    if (!this.#settled) {
      this.#progress = {
        phase: "cancelling",
        fraction: this.#progress.fraction,
        attempt: this.#current,
      };
      for (const listener of this.#listeners) listener(this.#progress);
      this.#controller.abort();
    }
    await this.result.catch((error: unknown) => {
      if (error instanceof EditorError && error.code === "cancel_failed")
        throw error;
    });
  }
}

/** Deletes what a finished attempt made, when its kind can be deleted. */
async function releaseArtifact(artifact: ExportArtifact): Promise<void> {
  if ("release" in artifact && typeof artifact.release === "function")
    await artifact.release();
}

/** Waits `ms`, or rejects as soon as `signal` aborts. */
function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const stop = () => {
      clearTimeout(timer);
      reject(
        signal.reason instanceof Error
          ? signal.reason
          : new DOMException("Cancelled", "AbortError"),
      );
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", stop);
      resolve();
    }, ms);
    signal.addEventListener("abort", stop, { once: true });
    if (signal.aborted) stop();
  });
}

/** The export step a job's progress was in, for an error that did not say. */
function phaseOf(progress: ExportProgress): ExportFailure["phase"] {
  switch (progress.phase) {
    case "rendering":
      return "render";
    case "finalizing":
      return "mux";
    case "publishing":
      return "publish";
    case "downloading":
      return "download";
    default:
      return "prepare";
  }
}
