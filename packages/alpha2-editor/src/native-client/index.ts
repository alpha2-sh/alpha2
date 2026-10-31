import type { MediaFingerprint } from "alpha2/project";
import type { EditorDocument } from "../core/document.js";
import {
  compileEditorDocument,
  EditorError,
  sequenceEnd,
} from "../core/document.js";
import type { EditorSnapshot } from "../core/session.js";
import { ExportFailureError, type ExportFailure } from "alpha2/export-failure";
import { ExportJob, requireExportFrameRate } from "../browser/jobs.js";
import type { BrowserExportOptions } from "../browser/types.js";
import type { ExportAttempt, ExportProgress } from "../ports/index.js";

/**
 * A file uploaded for a native export, usually from the browser bridge's `nativeAssets`. The host
 * reads media only from these uploads, never from a path the document names.
 */
export interface NativeAsset {
  /** The source, resource or font id the file belongs to (for `frames`, the upload's own id). */
  readonly id: string;
  /**
   * `proxies`: a proxy for source `id`, made from `madeFrom`. `frames`: one image sequence frame,
   * the file at `locator`.
   */
  readonly kind:
    "assets" | "effect-assets" | "caption-fonts" | "proxies" | "frames";
  readonly blob: Blob;
  /** The original file name, sent to the host percent-encoded. */
  readonly name: string;
  readonly madeFrom?: MediaFingerprint;
  readonly locator?: string;
}
/**
 * Export settings for a native job: the browser export's options plus the MPEG-4 Part 2 codec the
 * host can encode. An absent `container`, `videoCodec` or `audioCodec` takes the host's default, as
 * in the browser.
 */
export interface NativeExportOptions extends Omit<
  BrowserExportOptions,
  "videoCodec"
> {
  readonly videoCodec?: BrowserExportOptions["videoCodec"] | "mpeg4-part2";
}
/** A finished native export's file, as the service keeps it. */
export interface NativeArtifactDescriptor {
  readonly name: string;
  readonly bytes: number;
  readonly sha256: string;
  readonly createdAt: number;
  /** When the service deletes it, in milliseconds since the epoch. */
  readonly expiresAt: number;
}
/** Whether a kept file can still be downloaded, and if not, why. */
export type NativeArtifactStatus =
  | { readonly status: "available"; readonly expiresAt: number | null }
  | {
      readonly status: "gone";
      /** `expired`, `deleted`, or `evicted`, as the service says. */
      readonly reason: string;
    }
  | { readonly status: "missing" | "unreachable" };
/**
 * Asks the service whether an artifact's `url` can still be downloaded, without downloading it.
 * Never rejects: a network error is `unreachable`, a 404 is `missing`, and a 410 is `gone` with the
 * service's reason (`expired` when it gives none).
 */
export async function nativeArtifactStatus(
  url: string,
  request: typeof fetch = fetch,
): Promise<NativeArtifactStatus> {
  let response: Response;
  try {
    response = await request(url, { method: "HEAD" });
  } catch {
    return { status: "unreachable" };
  }
  if (response.ok) {
    const expires = Number(response.headers.get("x-alpha2-expires-at"));
    return {
      status: "available",
      expiresAt: Number.isFinite(expires) && expires > 0 ? expires : null,
    };
  }
  if (response.status === 410) {
    // A HEAD response has no body, so ask again for the reason.
    const detail = (await request(url)
      .then((full) => full.json())
      .catch(() => ({}))) as { reason?: string };
    return { status: "gone", reason: detail.reason ?? "expired" };
  }
  return { status: response.status === 404 ? "missing" : "unreachable" };
}
/** A native job's state as the service reports it. */
export interface NativeJobState {
  readonly phase:
    | "preparing"
    | "rendering"
    | "finalizing"
    | "publishing"
    | "completed"
    | "failed"
    | "cancelled";
  /** The finished file, once `completed`. */
  readonly artifact?: NativeArtifactDescriptor;
  /** Share done, from 0 to 1. */
  readonly fraction: number;
  readonly frames?: { readonly completed: number; readonly total: number };
  /** The failure message, when `failed` or `cancelled`. */
  readonly error?: string;
  /** The host's classified failure, when `failed`. */
  readonly failure?: ExportFailure;
  /**
   * Render cache report of the last attempt that finished rendering: `used` with its counts, or
   * `unavailable` with the reason. `null` when the attempt rendered without one. Absent until an
   * attempt reports.
   */
  readonly resume?:
    | { readonly status: "used"; readonly [count: string]: unknown }
    | { readonly status: "unavailable"; readonly reason: string }
    | null;
}
/** The error a failed or cancelled native job reports, carrying the host's failure. */
function nativeFailure(state: NativeJobState, jobId: string): Error {
  const message = state.error ?? "Native export cancelled";
  if (state.phase === "cancelled")
    return new DOMException(message, "AbortError");
  const failure: ExportFailure = state.failure ?? {
    code: "UNKNOWN",
    message,
    origin: "node",
    operation: "export",
    backend: "native",
    phase: "prepare",
    resource: "unknown",
  };
  return new ExportFailureError({ ...failure, message, jobId });
}
/** Consecutive failed status requests before an attempt gives up on the job. */
const STATUS_ATTEMPTS = 5;
/** Status retry delay, doubled each time (0.25 s to 2 s). */
const STATUS_RETRY_MS = 250;
function delay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const stop = () => {
      clearTimeout(timer);
      reject(new DOMException("Native export cancelled", "AbortError"));
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", stop);
      resolve();
    }, ms);
    signal.addEventListener("abort", stop, { once: true });
    if (signal.aborted) stop();
  });
}
export interface NativeClientOptions {
  /** The export service's root URL; a trailing slash is ignored. */
  readonly baseUrl: string;
  /** The `fetch` used for every request; the global one by default. */
  readonly fetch?: typeof fetch;
}
/**
 * A client for the native export service (`@alpha2-sh/node`'s HTTP host). Exports run with the
 * editor's retry policy; each job uploads its assets once and reuses them across attempts.
 */
export function createNativeClient(options: NativeClientOptions) {
  const request = options.fetch ?? fetch;
  const base = options.baseUrl.replace(/\/$/, "");
  const jobs = new Set<ExportJob>();
  let disposed = false;
  // Set by `detach`: jobs whose render was requested are released, not removed.
  let detached = false;
  async function checked(path: string, init?: RequestInit) {
    const response = await request(`${base}${path}`, init);
    if (!response.ok) throw Error(`Native host: ${await response.text()}`);
    return response;
  }
  return {
    /** The service's `/health` report. Rejects when the service answers with an error status. */
    async capabilities() {
      return (await checked("/health")).json() as Promise<
        Record<string, unknown>
      >;
    },
    /**
     * Starts an export of `editor.document` on the service. The document, `assets` and `settings`
     * are captured now, so later edits do not reach the job. `assets` may be a function, called
     * once per upload with the document and the attempt's signal. The range is the document's
     * export range, else the whole timeline. Throws when the client is disposed, and an
     * `EditorError` `invalid_frame_rate` for a `frameRate` the engine does not read.
     */
    startExport(
      editor: Pick<EditorSnapshot, "document" | "revision">,
      assets:
        | readonly NativeAsset[]
        | ((
            document: EditorDocument,
            signal: AbortSignal,
          ) => Promise<readonly NativeAsset[]>),
      settings: NativeExportOptions = {},
    ) {
      if (disposed) throw Error("Native client is disposed");
      requireExportFrameRate(settings.frameRate);
      const document = editor.document;
      const capturedSettings = { ...settings };
      const capturedAssets =
        typeof assets === "function" ? assets : [...assets];
      const id = crypto.randomUUID();
      const path = `/jobs/${id}`;
      // Shared by attempts; an attempt starts over only when the service lost the job.
      let captured: readonly NativeAsset[] | null = null;
      // Whether the service holds a job to clean up, from the acknowledged PUT on.
      let allocated = false;
      const acknowledged = new Set<string>();
      let uploaded: {
        readonly assets: readonly NativeAsset[];
        readonly frames: ReadonlyMap<string, string>;
      } | null = null;
      // Whether a render was requested; the service may still run one from an earlier attempt.
      let posted = false;
      const lost = () => {
        allocated = false;
        acknowledged.clear();
        uploaded = null;
        posted = false;
      };
      const allocate = async () => {
        // Allocate without the cancellation signal so a cancelled allocation is still acknowledged
        // before DELETE and cannot leave an orphan job.
        try {
          await checked(path, { method: "PUT" });
        } catch (error) {
          // The allocation's response was lost. The id is unique to this job, so a job there is it.
          const existing = await request(`${base}${path}`).catch(() => null);
          if (!existing?.ok) throw error;
        }
        allocated = true;
      };
      const upload = async (doc: EditorDocument, signal: AbortSignal) => {
        if (!allocated) await allocate();
        captured ??=
          typeof capturedAssets === "function"
            ? await capturedAssets(doc, signal)
            : capturedAssets;
        signal.throwIfAborted();
        for (const asset of captured) {
          const key = `${asset.kind}/${asset.id}`;
          if (acknowledged.has(key)) continue;
          await checked(
            `${path}/${asset.kind}/${encodeURIComponent(asset.id)}`,
            {
              method: "PUT",
              body: asset.blob,
              signal,
              headers: {
                "x-alpha2-filename": encodeURIComponent(asset.name),
              },
            },
          );
          acknowledged.add(key);
        }
        return {
          assets: captured,
          frames: new Map(
            captured.flatMap((asset) =>
              asset.kind === "frames" && asset.locator !== undefined
                ? [[asset.locator, asset.id] as const]
                : [],
            ),
          ),
        };
      };
      /**
       * The job's state. Transient failures (network, a proxy's 502) are retried with backoff so a
       * long render is not abandoned over one lost response. A 404 means the service lost the job
       * and the next attempt starts over.
       */
      const status = async (
        signal: AbortSignal,
        attempt: ExportAttempt,
      ): Promise<NativeJobState> => {
        for (let failures = 1; ; failures += 1) {
          signal.throwIfAborted();
          let failure: unknown;
          try {
            const response = await request(`${base}${path}`, { signal });
            if (response.status === 404) {
              lost();
              throw new ExportFailureError({
                code: "HOST_EXITED",
                message:
                  "The native export service lost this job; it may have restarted",
                origin: "browser",
                operation: "export",
                backend: "native",
                phase: "render",
                resource: "unknown",
                jobId: id,
                attemptId: attempt.id,
              });
            }
            if (response.ok) return (await response.json()) as NativeJobState;
            failure = Error(`Native host: ${await response.text()}`);
          } catch (error) {
            if (signal.aborted || error instanceof ExportFailureError)
              throw error;
            failure = error;
          }
          if (failures >= STATUS_ATTEMPTS) throw failure;
          await delay(STATUS_RETRY_MS * 2 ** (failures - 1), signal);
        }
      };
      const render = async (
        doc: EditorDocument,
        signal: AbortSignal,
        progress: (value: ExportProgress) => void,
        attempt: ExportAttempt,
      ) => {
        uploaded ??= await upload(doc, signal);
        const { assets: captured, frames } = uploaded;
        const compiled = compileEditorDocument(doc);
        const diagnostics = compiled.diagnostics;
        // Sent whole: the host's conform decides which sources need media.
        const project = compiled.project;
        if (diagnostics.length)
          throw Error(diagnostics.map((d) => d.message).join("; "));
        signal.throwIfAborted();
        // Follow a render an earlier attempt requested instead of requesting it again (the service
        // refuses that while it runs). Request anew if it failed or never started.
        const running = posted
          ? !["preparing", "failed"].includes(
              (await status(signal, attempt)).phase,
            )
          : false;
        if (!running) {
          posted = true;
          await checked(path, {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({
              project,
              ...capturedSettings,
              startTicks: doc.metadata.exportRange?.start ?? 0,
              endTicks: doc.metadata.exportRange?.end ?? sequenceEnd(project),
              fonts: captured
                .filter((a) => a.kind === "caption-fonts")
                .map((a) => a.id),
              frames: Object.fromEntries(
                project.sources.flatMap((source) =>
                  source.provider?.format === "image_sequence"
                    ? [
                        [
                          source.id,
                          source.provider.frames.map((locator) =>
                            locator === null
                              ? null
                              : (frames.get(locator) ?? null),
                          ),
                        ],
                      ]
                    : [],
                ),
              ),
              proxies: Object.fromEntries(
                captured
                  .filter((a) => a.kind === "proxies")
                  .map((a) => [a.id, { madeFrom: a.madeFrom }]),
              ),
              media: capturedSettings.media ?? "original",
              // Retries keep less in flight; the host scales its own budgets.
              ...(attempt.profile.memoryScale < 1 || capturedSettings.memory
                ? {
                    memory: {
                      ...capturedSettings.memory,
                      ...(attempt.profile.memoryScale < 1
                        ? { scale: attempt.profile.memoryScale }
                        : {}),
                    },
                  }
                : {}),
              attempt: attempt.id,
            }),
          });
        }
        let published: NativeArtifactDescriptor | undefined;
        for (;;) {
          const state = await status(signal, attempt);
          if (state.phase === "failed" || state.phase === "cancelled")
            throw nativeFailure(state, id);
          if (state.phase === "completed") {
            published = state.artifact;
            break;
          }
          progress({
            phase:
              state.phase === "finalizing" || state.phase === "publishing"
                ? state.phase
                : "rendering",
            fraction: state.fraction,
            ...(state.frames ? { frames: state.frames } : {}),
          });
          await delay(100, signal);
        }
        const artifact = published;
        if (!artifact)
          throw Error("The native service finished without a file to download");
        // The file stays on the service until released, so a failed download can retry without a
        // rerender.
        return {
          kind: "url" as const,
          url: `${base}${path}/artifact`,
          name: artifact.name,
          bytes: artifact.bytes,
          expiresAt: artifact.expiresAt,
          release: async () => {
            await checked(path, { method: "DELETE" });
          },
        };
      };
      const job = new ExportJob(
        editor.revision,
        document,
        {
          run: render,
          // Clean up once the job settles, never between attempts (retries reuse the uploads).
          // DELETE resolves only after the host exits and its files are removed.
          async dispose({ ok }) {
            if (ok || !allocated || (detached && posted)) return;
            lost();
            try {
              await checked(path, { method: "DELETE" });
            } catch (cause) {
              throw new EditorError(
                "cancel_failed",
                "Native cleanup could not be confirmed. The host will expire an abandoned job.",
                "cancel",
                id,
                true,
                { cause },
              );
            }
          },
        },
        () => jobs.delete(job),
        { id },
      );
      jobs.add(job);
      return job;
    },
    /**
     * What the service knows of job `id`, for a reloaded page that lost the job it started: its
     * state and, once complete, where to download its file. `null` when the service no longer has
     * the job. Rejects on any other error status.
     */
    async jobState(
      id: string,
    ): Promise<(NativeJobState & { readonly url?: string }) | null> {
      const response = await request(`${base}/jobs/${encodeURIComponent(id)}`);
      if (response.status === 404) return null;
      if (!response.ok) throw Error(`Native host: ${await response.text()}`);
      const state = (await response.json()) as NativeJobState;
      return state.phase === "completed" && state.artifact
        ? { ...state, url: `${base}/jobs/${encodeURIComponent(id)}/artifact` }
        : state;
    },
    /**
     * Removes job `id` from the service, stopping its render and deleting its kept file. Meant for
     * a job another page left behind (see {@link jobState}). Rejects on an error status.
     */
    async releaseJob(id: string): Promise<void> {
      await checked(`/jobs/${encodeURIComponent(id)}`, { method: "DELETE" });
    },
    /** Cancels every running job and refuses new ones. Resolves once the cancellations settle. */
    async dispose() {
      disposed = true;
      await Promise.all([...jobs].map((job) => job.cancel()));
    },
    /**
     * Releases every running job and refuses new ones, for a page that is leaving, without
     * cancelling renders already requested: those continue on the service and keep their files for
     * a later page to find with {@link jobState}. Jobs still uploading are removed, since only this
     * client could send the rest. Local jobs settle as cancelled; resolves once they do. A failed
     * removal is left to the service's lease.
     */
    async detach() {
      disposed = true;
      detached = true;
      await Promise.allSettled([...jobs].map((job) => job.cancel()));
    },
  };
}
