/**
 * Shape geometry queries for editors, computed in wasm by the engine's shape code so results match
 * rendered frames.
 *
 * Positions are normalized, `(0, 0)` top-left to `(1, 1)` bottom-right of a `width` by `height`
 * picture. Lengths and distances are in pixels; times are in ticks.
 *
 * Every function throws an `INVALID_INPUT` error for a picture outside 1 to 16384 pixels a side, a
 * non-integer tick time, or input the model rejects (invalid content tree or geometry, non-finite
 * point or tangent).
 */

import type {
  AnimatedPoint,
  Path,
  ShapeGeometry,
  ShapeItem,
  Ticks,
  Vec2,
} from "./project.js";

/** The `alpha2/wasm` functions these wrappers use. */
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

/** Picture size in pixels. */
export interface PictureSize {
  readonly width: number;
  readonly height: number;
}

/** Normalized rectangle from top-left (`x0`, `y0`) to bottom-right (`x1`, `y1`). */
export interface Bounds {
  readonly x0: number;
  readonly y0: number;
  readonly x1: number;
  readonly y1: number;
}

/** An item's bounds; `at` is its tree path, such as `items[0].items[2]`. */
export interface ItemBounds {
  readonly at: string;
  /** `null` for an item that draws nothing. */
  readonly bounds: Bounds | null;
}

/** The closest point on a path to a query point. */
export interface NearestPoint {
  readonly subpath: number;
  /** Segment `i` runs from vertex `i` to the next. */
  readonly segment: number;
  /** Parameter along the segment, in `0..=1`. */
  readonly t: number;
  /** Normalized position. */
  readonly point: Vec2;
  /** Distance from the query point, in pixels. */
  readonly distance: number;
}

/**
 * Each item's bounds at `time`, in document order. Stroke bounds include the stroke width.
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
 * Tree path of the front-most fill or stroke at normalized `point` at `time`, honoring fill rules,
 * or `undefined` where nothing is painted.
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

/** Closest point of `path` to normalized `point`, or `undefined` for an empty path. */
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
 * The part of `path` a trim item keeps. `start` and `end` are fractions of the total length of all
 * subpaths, clamped to `0..=1`; `offset` is in turns. Empty when `start >= end`.
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
 * Resamples subpath `i` to `vertexCounts[i]` evenly spaced vertices, so two paths can share a
 * structure and morph between keyframes. Subpaths with a missing count, a count below 2 (3 when
 * closed), or zero length are kept unchanged.
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
 * Converts a rectangle, ellipse or polystar at `time` to a path with the same outline, start point
 * and direction.
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

/** Polyline a position follows between keyframes, along their spatial tangents. */
export function motionPath(
  binding: GeometryBinding,
  position: AnimatedPoint,
  size: PictureSize,
): Vec2[] {
  return JSON.parse(
    binding.motionPath(JSON.stringify(position), size.width, size.height),
  ) as Vec2[];
}
