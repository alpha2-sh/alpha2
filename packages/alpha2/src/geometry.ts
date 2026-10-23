/**
 * Shape geometry for editors, answered in wasm by the code the engine draws shapes with: where a
 * shapes generator's items are, which paint is under a point, how long a path is, and paths cut,
 * resampled, or converted. A hit test or a trim here agrees with what a frame shows.
 *
 * Positions are normalized as the project document's are, `(0, 0)` top-left to `(1, 1)`
 * bottom-right of a `width` by `height` picture; lengths and distances are in its pixels. Times
 * are ticks, as keyframe times are.
 *
 * Every function throws an `INVALID_INPUT` error for a picture outside 1 to 16384 pixels a side, a
 * time that is not a whole number of ticks, or input the model rejects (a content tree or geometry
 * that is not valid, a path point or tangent that is not finite).
 */

import type {
  AnimatedPoint,
  Path,
  ShapeGeometry,
  ShapeItem,
  Ticks,
  Vec2,
} from "./project.js";

/** The part of `alpha2/wasm` these functions use. */
export interface GeometryBinding {
  shapeBounds(
    itemsJson: string,
    time: number,
    width: number,
    height: number,
  ): string;
  hitTest(
    itemsJson: string,
    time: number,
    width: number,
    height: number,
    x: number,
    y: number,
  ): string | undefined;
  nearestPoint(
    pathJson: string,
    width: number,
    height: number,
    x: number,
    y: number,
  ): string;
  pathLength(pathJson: string, width: number, height: number): string;
  trimPath(
    pathJson: string,
    width: number,
    height: number,
    start: number,
    end: number,
    offset: number,
  ): string;
  resamplePath(
    pathJson: string,
    width: number,
    height: number,
    vertexCounts: Uint32Array,
  ): string;
  toPath(
    geometryJson: string,
    time: number,
    width: number,
    height: number,
  ): string;
  motionPath(pointJson: string, width: number, height: number): string;
}

/** The size of the picture shapes are drawn on, in pixels. */
export interface PictureSize {
  readonly width: number;
  readonly height: number;
}

/** A normalized rectangle: `x0`, `y0` its top-left corner, `x1`, `y1` its bottom-right. */
export interface Bounds {
  readonly x0: number;
  readonly y0: number;
  readonly x1: number;
  readonly y1: number;
}

/** An item's bounds, by its place in the tree (`items[0].items[2]`). */
export interface ItemBounds {
  readonly at: string;
  /** What it covers: geometry's outline, a paint's area; `null` for an item that shows nothing. */
  readonly bounds: Bounds | null;
}

/** The point of a path nearest another. */
export interface NearestPoint {
  /** The subpath. */
  readonly subpath: number;
  /** The segment within it: segment `i` runs from vertex `i` to the next. */
  readonly segment: number;
  /** The parameter along the segment, in `0..=1`. */
  readonly t: number;
  /** The point, normalized. */
  readonly point: Vec2;
  /** Its distance from the point asked about, in pixels. */
  readonly distance: number;
}

/**
 * Each item's bounds at `time`, in document order: a group's are its items', a stroke's include
 * its width.
 *
 * @throws When `items` is not a valid content tree.
 */
export function shapeBounds(
  binding: GeometryBinding,
  items: readonly ShapeItem[],
  time: Ticks,
  size: PictureSize,
): ItemBounds[] {
  return JSON.parse(
    binding.shapeBounds(JSON.stringify(items), time, size.width, size.height),
  ) as ItemBounds[];
}

/**
 * The front-most fill or stroke at normalized `point` at `time`, by its fill rule (a stroke by
 * its outline): its place in the tree, or `undefined` where nothing is painted.
 *
 * @throws When `items` is not a valid content tree.
 */
export function hitTest(
  binding: GeometryBinding,
  items: readonly ShapeItem[],
  time: Ticks,
  size: PictureSize,
  point: Vec2,
): string | undefined {
  return (
    binding.hitTest(
      JSON.stringify(items),
      time,
      size.width,
      size.height,
      point.x,
      point.y,
    ) ?? undefined
  );
}

/** The point of `path` nearest normalized `point`, or `undefined` for an empty path. */
export function nearestPoint(
  binding: GeometryBinding,
  path: Path,
  size: PictureSize,
  point: Vec2,
): NearestPoint | undefined {
  return (
    (JSON.parse(
      binding.nearestPoint(
        JSON.stringify(path),
        size.width,
        size.height,
        point.x,
        point.y,
      ),
    ) as NearestPoint | null) ?? undefined
  );
}

/** Each subpath's length, in pixels. */
export function pathLength(
  binding: GeometryBinding,
  path: Path,
  size: PictureSize,
): number[] {
  return JSON.parse(
    binding.pathLength(JSON.stringify(path), size.width, size.height),
  ) as number[];
}

/**
 * The part of `path` a trim item keeps: from `start` to `end` (fractions of its pixel length, all
 * subpaths end to end, clamped to `0..=1`), shifted by `offset` turns. Nothing when `start` is not
 * before `end`.
 */
export function trimPath(
  binding: GeometryBinding,
  path: Path,
  size: PictureSize,
  start: number,
  end: number,
  offset = 0,
): Path {
  return JSON.parse(
    binding.trimPath(
      JSON.stringify(path),
      size.width,
      size.height,
      start,
      end,
      offset,
    ),
  ) as Path;
}

/**
 * `path` with `vertexCounts[i]` vertices on subpath `i`, evenly along its length, so two paths can
 * be given the same structure and their keyframes can morph. The engine never resamples on its
 * own. A subpath whose count is missing, below 2 (3 when closed), or that has no length is kept as
 * it is.
 *
 * @throws When a count is over `MAX_PATH_VERTICES`.
 */
export function resamplePath(
  binding: GeometryBinding,
  path: Path,
  size: PictureSize,
  vertexCounts: readonly number[],
): Path {
  return JSON.parse(
    binding.resamplePath(
      JSON.stringify(path),
      size.width,
      size.height,
      Uint32Array.from(vertexCounts),
    ),
  ) as Path;
}

/**
 * A rectangle, ellipse, or polystar at `time` as a path of the same outline, from the same start
 * point and in the same direction: "convert to path".
 *
 * @throws When `geometry` is not valid.
 */
export function toPath(
  binding: GeometryBinding,
  geometry: ShapeGeometry,
  time: Ticks,
  size: PictureSize,
): Path {
  return JSON.parse(
    binding.toPath(JSON.stringify(geometry), time, size.width, size.height),
  ) as Path;
}

/** The polyline a position travels between its keyframes, along their spatial tangents. */
export function motionPath(
  binding: GeometryBinding,
  position: AnimatedPoint,
  size: PictureSize,
): Vec2[] {
  return JSON.parse(
    binding.motionPath(JSON.stringify(position), size.width, size.height),
  ) as Vec2[];
}
