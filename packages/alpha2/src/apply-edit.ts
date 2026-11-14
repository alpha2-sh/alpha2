/**
 * Edit operations that apply data to a project once, run by the engine's wasm.
 *
 * The operations are named for primitives: `remove_ranges` (drop silences, keep highlights),
 * `split_at` (cut on beats) and `bake` (data bindings to keyframes, words to cues). Kept moments
 * render as before for clip picture and sound, clip keyframes, captions, hold data and linear
 * automation; a matte or a `via` binding follows the part beside it or the edit is refused
 * (`matte_source`, `read_through`); track automation with hold or eased keys across a removed range
 * and a fade under a cut are not exact yet. The result is the same canonical document everywhere.
 *
 * ```ts
 * const { project: tight } = applyEdit(wasm, project, {
 *   op: "remove_ranges",
 *   ranges: { data: "silence" },
 *   target: { track: "v1" },
 *   padding: 12_000,
 * });
 * ```
 */
import type { DataAnchor, Project } from "./generated/project-schema.js";

/** Ticks on an anchor's clock, `start <= end`. */
export interface TickRange {
  readonly start: number;
  readonly end: number;
}

/** The clips an operation works on: every clip of a track, or these clips. */
export type EditTarget =
  { readonly track: string } | { readonly clips: readonly string[] };

/** One edit operation. Times are ticks. */
export type EditRequest =
  | {
      readonly op: "remove_ranges";
      /** An `intervals` data set, or a list on an anchor's clock. */
      readonly ranges:
        | { readonly data: string }
        | { readonly list: readonly TickRange[]; readonly anchor: DataAnchor };
      readonly target: EditTarget;
      /** What closes the gaps; `timeline` by default. */
      readonly ripple?: "none" | "track" | "timeline";
      /** Keeps only the ranges, removing their complement within the target clips' extent. */
      readonly invert?: boolean;
      /** Ticks kept at each edge of every removed range. */
      readonly padding?: number;
      /** Removed ranges shorter than this, after padding, are kept. */
      readonly min_duration?: number;
    }
  | {
      readonly op: "split_at";
      /** An `intervals` data set's starts, or a list of ticks on an anchor's clock. */
      readonly times:
        | { readonly data: string }
        | { readonly list: readonly number[]; readonly anchor: DataAnchor };
      readonly target: EditTarget;
    }
  | {
      readonly op: "bake";
      /** Property paths use the lint path syntax (`visual.position`, `effect.params.radius`). */
      readonly targets: readonly (
        | { readonly clip: string; readonly property: string }
        | {
            readonly composition: string;
            readonly node: string;
            readonly property: string;
          }
        | { readonly track: string; readonly property: string }
      )[];
    };

/** What an operation changed, so a caller can show and select it. */
export interface EditReport {
  /** Timeline ranges removed, in the old timeline's time. */
  readonly removed: readonly TickRange[];
  /** Kept stretches that moved. */
  readonly moved: readonly {
    readonly from: TickRange;
    readonly to: TickRange;
  }[];
  /** Clips the operation created, each with the clip it was cut from (which keeps its id). */
  readonly created: readonly { readonly id: string; readonly from: string }[];
}

/** An operation's result. */
export interface AppliedEdit {
  readonly project: Project;
  /** The engine's canonical text of `project`, byte for byte what every surface answers. */
  readonly projectJson: string;
  readonly report: EditReport;
}

/** Why an operation refused, as `Alpha2Error.details.refusal` names it. */
export type EditRefusal =
  | "invalid_project"
  | "invalid_request"
  | "unknown_data"
  | "wrong_kind"
  | "unknown_target"
  | "empty_target"
  | "missing_anchor"
  | "invalid_range"
  | "nothing_to_bake"
  | "correction_not_bakeable"
  | "matte_source"
  | "read_through"
  | "invalid_result";

/** The generated wasm function this wrapper calls. */
export interface ApplyEditBinding {
  applyEdit(project: string, request: string): [string, string];
}

/**
 * Applies `request` to `project`; neither is changed.
 *
 * @throws An `Alpha2Error` with code `INVALID_INPUT` for an invalid project or request and for
 * every refusal (`ALPHA2_ERROR` for an edited project that fails validation, a defect);
 * `details.refusal` names why.
 */
export function applyEdit(
  binding: ApplyEditBinding,
  project: Project,
  request: EditRequest,
): AppliedEdit {
  const [projectJson, report] = binding.applyEdit(
    JSON.stringify(project),
    JSON.stringify(request),
  );
  return {
    project: JSON.parse(projectJson) as Project,
    projectJson,
    report: JSON.parse(report) as EditReport,
  };
}
