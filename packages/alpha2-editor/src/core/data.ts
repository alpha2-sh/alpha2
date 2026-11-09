import { dataReferences, describeDataReference } from "alpha2/project";
import type {
  Animated,
  DataRemap,
  DataReference,
  DataSpace,
  DataVia,
  Project,
} from "alpha2/project";
import { EditorError } from "./document.js";

/** A value that follows a data channel at render time instead of holding its own. */
export type DataBoundValue = Extract<Animated<unknown>, { mode: "data" }>;

/** What {@link dataValue} binds to. */
export interface DataValueOptions {
  readonly data: string;
  readonly channel: string;
  /** The clip or media node whose time map reads a source-anchored set; omit for the timeline. */
  readonly via?: DataVia;
  /** Whether a point read through `via` lands in the clip's `picture` or on the `canvas`. */
  readonly space?: DataSpace;
  /** `[x, y]` added to a point after the space mapping. */
  readonly offset?: readonly [number, number];
  /** A linear map of a scalar. */
  readonly remap?: DataRemap;
}

/**
 * A value bound to a data channel, for a `clip.update`, `track.update` or `graph.node.update`
 * patch. Only the given options are written; validation of the command checks them against the
 * set (kind, channel, `via`, `space`).
 */
export function dataValue(options: DataValueOptions): DataBoundValue {
  return {
    mode: "data",
    data: options.data,
    channel: options.channel,
    ...(options.via === undefined ? {} : { via: options.via }),
    ...(options.space === undefined ? {} : { space: options.space }),
    ...(options.offset === undefined ? {} : { offset: options.offset }),
    ...(options.remap === undefined ? {} : { remap: options.remap }),
  };
}

/** Whether `value` follows a data channel (`mode: "data"`). */
export function isDataBound(value: unknown): value is DataBoundValue {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { mode?: unknown }).mode === "data"
  );
}

function listed(references: readonly DataReference[]): string {
  return references.map(describeDataReference).join(", ");
}

/** Refuses removing data set `id` while a place of `project` reads it. */
export function assertDataUnread(
  project: Project,
  id: string,
  operation: string,
): void {
  const references = dataReferences(project).filter((r) => r.data === id);
  if (references.length)
    throw new EditorError(
      "data_referenced",
      `Data set ${id} is read by ${listed(references)}`,
      operation,
      id,
      true,
      { details: { references } },
    );
}

/** Refuses removing source `id` while a data set is anchored to it. */
export function assertSourceUnanchored(
  project: Project,
  id: string,
  operation: string,
): void {
  const dataSets = (project.data ?? [])
    .filter(
      (set) => set.anchor.type === "source" && set.anchor.source_id === id,
    )
    .map((set) => set.id);
  if (dataSets.length)
    throw new EditorError(
      "data_referenced",
      `Data sets ${dataSets.join(", ")} are anchored to source ${id}`,
      operation,
      id,
      true,
      { details: { dataSets } },
    );
}

/** Refuses removing the clips `removed` while a binding of a staying clip reads through one. */
export function assertClipsUnread(
  project: Project,
  removed: ReadonlySet<string>,
  operation: string,
): void {
  if (!removed.size || !project.data?.length) return;
  const references = dataReferences(project).filter(
    (r) =>
      r.via !== undefined &&
      "clip" in r.via &&
      removed.has(r.via.clip) &&
      !(r.owner.kind === "clip" && removed.has(r.owner.clip)),
  );
  if (references.length) {
    const via = references[0].via as { readonly clip: string };
    throw new EditorError(
      "data_referenced",
      `Clip ${via.clip} is read through by ${listed(references)}`,
      operation,
      via.clip,
      true,
      { details: { references } },
    );
  }
}

/** Ids of the data sets whose object differs between `before` and `after`, or that only one has. */
export function changedDataSets(
  before: Project,
  after: Project,
): ReadonlySet<string> {
  const changed = new Set<string>();
  if (before.data === after.data) return changed;
  const was = new Map((before.data ?? []).map((set) => [set.id, set]));
  const is = new Map((after.data ?? []).map((set) => [set.id, set]));
  for (const [id, set] of was) if (is.get(id) !== set) changed.add(id);
  for (const id of is.keys()) if (!was.has(id)) changed.add(id);
  return changed;
}
