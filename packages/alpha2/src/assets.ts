// The wire form of the engine's asset decisions (`alpha2-assets`). The engine makes the decisions;
// they are never re-derived here.

import type {
  ColorPipeline,
  MediaFingerprint,
  Project,
} from "./generated/project-schema.js";

/** What media is read for. */
export type MediaPurpose =
  "preview" | "thumbnail" | "remote_edit" | "export_original" | "export_proxy";

/** Whether a source's original media can be read. */
export type OriginalState =
  | { readonly state: "online"; readonly fingerprint: MediaFingerprint }
  | { readonly state: "offline" }
  | { readonly state: "unsupported"; readonly reason: string };

/** A proxy's generation state. */
export type ProxyState =
  | { readonly state: "ready" }
  | { readonly state: "generating" }
  | { readonly state: "failed"; readonly reason: string };

/** A lighter copy of a source's media. */
export interface ProxyVariant {
  readonly id: string;
  /** The source revision it was made from. */
  readonly source_revision: number;
  /** Evidence of the original it was made from. */
  readonly made_from: MediaFingerprint;
  /** Displayed size in pixels. */
  readonly width: number;
  readonly height: number;
  readonly frame_rate?: { readonly num: number; readonly den: number };
  readonly duration_ticks?: number;
  readonly audio_sample_rate?: number;
  /** The color pipeline baked in, when it was tone-mapped. */
  readonly color_pipeline?: string;
  readonly state: ProxyState;
}

/** One source's media, as a host sees it now. */
export interface AssetEntry {
  readonly source_id: string;
  /** The host's revision of the media; relinking makes a new one. */
  readonly revision: number;
  readonly original: OriginalState;
  /** In the host's order of preference. */
  readonly proxies?: readonly ProxyVariant[];
}

/** Every source's media as one host sees it now. Runtime state, never part of a project. */
export interface AssetCatalog {
  readonly assets: readonly AssetEntry[];
}

/** The media a purpose reads for one source. */
export type Selection =
  | { readonly media: "original" }
  | { readonly media: "proxy"; readonly id: string }
  | { readonly media: "unavailable" };

/** A part of a fingerprint. */
export type FingerprintField =
  | "content"
  | "byte_length"
  | "video"
  | "audio"
  | "streams"
  | "duration"
  | "color";

/** What comparing a recorded fingerprint with a found one shows. */
export type FingerprintMatch =
  | { readonly kind: "identical" }
  | { readonly kind: "consistent" }
  | { readonly kind: "mismatch"; readonly fields: readonly FingerprintField[] }
  | { readonly kind: "unverified" };

/** A way a proxy differs from its original. */
export type ProxyMismatch =
  | "made_from"
  | "frame_rate"
  | "duration"
  | "aspect"
  | "audio_missing"
  | "audio_sample_rate";

/** Why a proxy cannot stand in for its source. */
export type ProxyProblem =
  | { readonly kind: "stale_revision" }
  | { readonly kind: "stale_color_pipeline" }
  | { readonly kind: "pending" }
  | { readonly kind: "failed"; readonly reason: string }
  | { readonly kind: "invalid"; readonly reasons: readonly ProxyMismatch[] };

/** Something about a source's media, and whether it blocks the purpose. */
export type ConformIssue = { readonly blocking: boolean } & (
  | { readonly kind: "missing" }
  | { readonly kind: "offline" }
  | { readonly kind: "unsupported"; readonly reason: string }
  | {
      readonly kind: "fingerprint_mismatch";
      readonly fields: readonly FingerprintField[];
    }
  | { readonly kind: "fingerprint_unverified" }
  | {
      readonly kind: "proxy";
      readonly proxy: string;
      readonly problem: ProxyProblem;
    }
  | { readonly kind: "using_proxy"; readonly proxy: string }
  | { readonly kind: "hdr_rejected" }
  /**
   * Neither the project nor the video declares its color: it is read as BT.709 limited-range SDR.
   * Never blocking.
   */
  | { readonly kind: "undeclared_color" }
);

/** One source's media for the purpose. */
export interface SourceConform {
  readonly source_id: string;
  readonly selection: Selection;
  /** A proxy that could stand in for the source, whether or not the purpose reads it. */
  readonly usable_proxy?: string;
  readonly issues: readonly ConformIssue[];
}

/** Whether a project's sources can be read for a purpose, before a render starts. */
export interface ConformReport {
  readonly purpose: MediaPurpose;
  /** Whether no issue blocks the purpose. */
  readonly ready: boolean;
  /** Each host-supplied source a render reads, in project order. */
  readonly sources: readonly SourceConform[];
}

/** Every blocking issue, with its source. */
export function blockingIssues(
  report: ConformReport,
): readonly { readonly sourceId: string; readonly issue: ConformIssue }[] {
  return report.sources.flatMap((source) =>
    source.issues
      .filter((issue) => issue.blocking)
      .map((issue) => ({ sourceId: source.source_id, issue })),
  );
}

/** The generated wasm functions these wrappers call. */
export interface AssetBinding {
  compareFingerprints(recorded: string, found: string): string;
  conformProject(project: string, catalog: string, purpose: string): string;
  colorPipelineFingerprint(pipeline: string): string;
}

/**
 * Whether `found` is the media `recorded` describes, decided by the engine: hashes decide alone;
 * otherwise every part both recorded must agree, within demuxer tolerances.
 *
 * @throws When either fingerprint has a part that is not well formed, such as a hash in uppercase:
 * an input error, not other media.
 */
export function compareFingerprints(
  binding: AssetBinding,
  recorded: MediaFingerprint,
  found: MediaFingerprint,
): FingerprintMatch {
  return JSON.parse(
    binding.compareFingerprints(
      JSON.stringify(recorded),
      JSON.stringify(found),
    ),
  ) as FingerprintMatch;
}

/**
 * Whether `project`'s media, as `catalog` describes it, is ready for `purpose`, decided by the
 * engine.
 *
 * @throws When the project or catalog does not load, or the catalog holds a fingerprint that is not
 * well formed.
 */
export function conformProject(
  binding: AssetBinding,
  project: Project,
  catalog: AssetCatalog,
  purpose: MediaPurpose,
): ConformReport {
  return JSON.parse(
    binding.conformProject(
      JSON.stringify(project),
      JSON.stringify(catalog),
      purpose,
    ),
  ) as ConformReport;
}

/** A color pipeline's identity, as a proxy that bakes it in records it. */
export function colorPipelineFingerprint(
  binding: AssetBinding,
  pipeline: ColorPipeline,
): string {
  return binding.colorPipelineFingerprint(JSON.stringify(pipeline));
}
export * from "./sha256.js";
