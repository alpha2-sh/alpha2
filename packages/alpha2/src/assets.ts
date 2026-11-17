// Wire types for the engine's asset decisions (`alpha2-assets`); nothing is re-derived here.

import type {
  ColorPipeline,
  MediaFingerprint,
  Project,
} from "./generated/project-schema.js";

/** What media is read for. */
export type MediaPurpose =
  | "preview"
  | "preview_proxy"
  | "thumbnail"
  | "remote_edit"
  | "export_original"
  | "export_proxy";

/** Whether a source's original media can be read. */
export type OriginalState =
  | { readonly state: "online"; readonly fingerprint: MediaFingerprint }
  | {
      readonly state: "offline";
      /** The path the host looked the media up at, when it had one. */
      readonly path?: string;
    }
  | { readonly state: "unsupported"; readonly reason: string };

/** A proxy's generation state. */
export type ProxyState =
  | { readonly state: "ready" }
  | { readonly state: "generating" }
  | { readonly state: "failed"; readonly reason: string };

/** A lighter copy of a source's media. */
export interface ProxyVariant {
  readonly id: string;
  readonly source_revision: number;
  /** Fingerprint of the original it was made from. */
  readonly made_from: MediaFingerprint;
  /** Displayed size in pixels. */
  readonly width: number;
  readonly height: number;
  readonly frame_rate?: { readonly num: number; readonly den: number };
  readonly duration_ticks?: number;
  readonly audio_sample_rate?: number;
  /** Color pipeline baked in when it was tone-mapped. */
  readonly color_pipeline?: string;
  readonly state: ProxyState;
}

/** One source's media, as a host sees it now. */
export interface AssetEntry {
  readonly source_id: string;
  /** Host revision of the media; relinking bumps it. */
  readonly revision: number;
  readonly original: OriginalState;
  /** In the host's order of preference. */
  readonly proxies?: readonly ProxyVariant[];
  /**
   * Why the host drops the original's alpha, when it cannot decode it (for example HEVC auxiliary
   * alpha, or VP9 alpha without `libvpx`).
   */
  readonly alpha_dropped?: string;
  /**
   * Every audio stream of the original in container order, which a clip's `audio.stream` counts
   * over. Without it, clips' stream and channel choices are not checked.
   */
  readonly audio_streams?: readonly AudioStreamFacts[];
}

/** One audio stream of an original, as the host probed it. */
export interface AudioStreamFacts {
  /** Channels the stream decodes to. */
  readonly channels: number;
  /** Whether a clip without `audio.stream` plays this stream. Default false. */
  readonly default?: boolean;
  readonly codec?: string;
  /** The speaker of each channel as `FFmpeg` channel names, when declared. */
  readonly layout?: readonly string[];
  readonly language?: string;
  readonly title?: string;
}

/** Media state of every source on one host. Runtime state, never part of a project. */
export interface AssetCatalog {
  readonly assets: readonly AssetEntry[];
}

/** The media a purpose reads for one source. */
export type Selection =
  | { readonly media: "original" }
  | { readonly media: "proxy"; readonly id: string }
  | { readonly media: "unavailable" };

export type FingerprintField =
  | "content"
  | "byte_length"
  | "video"
  | "audio"
  | "streams"
  | "duration"
  | "color";

/** Result of comparing a recorded fingerprint with a found one. */
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
  | {
      readonly kind: "offline";
      /** The path the host looked the media up at, when it had one. */
      readonly path?: string;
    }
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
  /** No declared color, so the video is read as BT.709 limited-range SDR. Never blocking. */
  | { readonly kind: "undeclared_color" }
  /** The host cannot decode the original's alpha, so it draws opaque. Never blocking. */
  | { readonly kind: "alpha_dropped"; readonly reason: string }
  /** A clip plays an audio stream or channel the original lacks. Always blocking. */
  | {
      readonly kind: "audio_selection";
      readonly clip: string;
      readonly reason: string;
    }
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
  /** True when no issue blocks the purpose. */
  readonly ready: boolean;
  /** Each host-supplied source a render reads, in project order. */
  readonly sources: readonly SourceConform[];
  /**
   * Data sets made from other media than their source's original now is, in data order. This
   * never blocks, since the data still reads, but it may no longer describe the media.
   */
  readonly stale_data: readonly StaleData[];
}

/** A data set whose `made_from` differs from its source's current original. */
export interface StaleData {
  readonly data: string;
  readonly source: string;
  /** The parts of the fingerprint that differ. */
  readonly fields: readonly FingerprintField[];
}

/** Lists every blocking issue with its source id. */
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
 * Asks the engine whether `found` is the media `recorded` describes. Content hashes decide alone;
 * otherwise every part both record must agree within demuxer tolerances.
 *
 * @throws When either fingerprint is malformed (for example an uppercase hash).
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
 * Asks the engine whether `project`'s media, as `catalog` describes it, is ready for `purpose`.
 *
 * @throws When the project or catalog does not load, or the catalog holds a malformed fingerprint.
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

/** Identity string a tone-mapped proxy records for `pipeline`. */
export function colorPipelineFingerprint(
  binding: AssetBinding,
  pipeline: ColorPipeline,
): string {
  return binding.colorPipelineFingerprint(JSON.stringify(pipeline));
}
export * from "./sha256.js";
