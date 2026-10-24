import type { Selection } from "alpha2/assets";
import type { ExportFailure } from "alpha2/export-failure";
import type { RetryProfile } from "alpha2/export-retry";
import type { Project } from "alpha2/project";
import type { EditorDocument, AssetDescriptor } from "../core/document.js";
/** Shows a project in a preview surface. Implemented by the browser bridge and native previews. */
export interface PreviewBridge {
  /**
   * Replaces the shown project. `revision` identifies this version and increases with every call.
   */
  replaceProject(project: Project, revision: number): Promise<void>;
  /** Shows the frame at `tick` (timeline ticks). */
  seek(tick: number): Promise<void>;
  /** Releases the preview's resources. The bridge is unusable afterwards. */
  dispose(): Promise<void>;
}
/** Turns an imported input (a file, a URL) into a project source and its editor asset. */
export interface MediaAdapter<T = unknown> {
  /**
   * Makes `input` readable as a source. Rejects when the input cannot be read or `signal` aborts.
   * The caller calls `dispose` once the source is no longer used.
   */
  stage(
    input: T,
    signal: AbortSignal,
  ): Promise<{
    source: Project["sources"][number];
    asset: AssetDescriptor;
    dispose(): void;
  }>;
}
/** Plays a project's audio and owns the playhead while it plays. */
export interface AudioTransport {
  /** Moves the playhead to `tick` (timeline ticks), playing or not. */
  seek(tick: number): void;
  /** Starts playback from the playhead. Rejects when the output cannot start. */
  play(): Promise<void>;
  pause(): void;
  /** The playhead, in timeline ticks. */
  readonly tick: number;
  dispose(): Promise<void>;
}
/** An export job's progress, as its backends report it and `ExportJob.progress` holds it. */
export interface ExportProgress {
  /**
   * Where the job is. `rendering` counts frames; `finalizing` means every frame is encoded and the
   * output is being flushed and closed; `publishing` that the file is being made available;
   * `downloading` that a native result is crossing to this browser. Only `completed` means the
   * result exists: 100 % of frames does not.
   */
  readonly phase:
    | "preparing"
    | "rendering"
    | "finalizing"
    | "publishing"
    | "downloading"
    | "retrying"
    | "completed"
    | "cancelling"
    | "cancelled"
    | "failed";
  /** The whole job's share done, from 0 to 1. */
  readonly fraction: number;
  /** The attempt this progress belongs to, from 1; `retrying` names the one about to start. */
  readonly attempt?: number;
  /** While `retrying`: why, and how long until the next attempt starts. */
  readonly retry?: { readonly reason: string; readonly delayMs: number };
  /** Frames encoded, when the backend counts them. */
  readonly frames?: { readonly completed: number; readonly total: number };
}
/**
 * The file a completed export made: a `blob` in memory, a disk-backed `file` (temporary
 * origin-private storage), a file the user picked (`saved`), or a `url` on an export service.
 */
export type ExportArtifact = (
  | { readonly kind: "blob"; readonly blob: Blob; readonly name: string }
  | {
      readonly kind: "file";
      /** A disk-backed file: reading or downloading it does not load it into memory. */
      readonly file: File;
      readonly name: string;
      /** Deletes the file. The artifact is unusable afterwards. */
      release(): Promise<void>;
    }
  | {
      readonly kind: "saved";
      /** The name of the file the user picked, which now holds the export. */
      readonly name: string;
    }
  | {
      readonly kind: "url";
      /** Where the file can be downloaded, any number of times, until it expires or is released. */
      readonly url: string;
      readonly name: string;
      /** When the file stops being available, in milliseconds since the epoch. */
      readonly expiresAt?: number;
      /** Deletes the file behind `url`, when its holder allows it. */
      release?(): Promise<void>;
    }
) & {
  /** The media each source read, as the engine selected it, when the exporter reports it. */
  readonly media?: Readonly<Record<string, Selection>>;
  /** The file's length, when known. */
  readonly bytes?: number;
  /**
   * What the exporter reported about the run: timings, `performance` (with `memory` and `output`),
   * rates. A browser export adds `videoDecode`, the decode mode it ran with, and `decodeFallbacks`:
   * per source, frames read by element seeks rather than decoded (`kind: "element-seek"`), with
   * why, how many frames, and whether for good, and sources `videoDecode: "software"` could not
   * decode in software (`kind: "software-unavailable"`).
   */
  readonly report?: Readonly<Record<string, unknown>>;
};
/** One run of an export job: a fresh device or host process, from the job's pinned document. */
export interface ExportAttempt {
  /**
   * `<job id>/<number>`: what a backend tags this run's work with, so a late answer from an earlier
   * one is told apart.
   */
  readonly id: string;
  /** From 1. */
  readonly number: number;
  /**
   * How much this run keeps in flight: a backend scales its byte budgets by `profile.memoryScale`.
   */
  readonly profile: RetryProfile;
  /** Why the previous attempt failed, when this one is a retry. */
  readonly previous?: ExportFailure;
}
/** Runs export attempts for an export job, in the browser, a native host, or a service. */
export interface ExportBackend {
  /** Runs one attempt. A retry is a new call, with the same document and a new `attempt`. */
  run(
    document: EditorDocument,
    signal: AbortSignal,
    progress: (value: ExportProgress) => void,
    attempt: ExportAttempt,
  ): Promise<ExportArtifact>;
  /**
   * Called once, when the job has settled: after the attempt that succeeded, the last that failed,
   * or the cancellation. What attempts share (uploads, a job on a service) is released here, not
   * between attempts. Rejecting reports an unconfirmed cleanup.
   */
  dispose?(outcome: {
    readonly ok: boolean;
    readonly cancelled: boolean;
  }): Promise<void>;
}
/** Stores a serialized editor document. */
export interface PersistenceAdapter {
  /** Returns the last saved JSON. Rejects when nothing can be read. */
  load(): Promise<string>;
  /** Stores `json`, replacing what was saved before. */
  save(json: string): Promise<void>;
}
/** Whether a feature works on this host. */
export interface Capability {
  readonly available: boolean;
  /** Why the feature is unavailable, for display; absent when it is available. */
  readonly reason?: string;
}
