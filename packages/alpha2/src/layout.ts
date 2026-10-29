/**
 * Layout generators for authors: boxes laid out by CSS flexbox or grid around their text and
 * shapes, as a project's `{ "type": "layout" }` generator holds them.
 *
 * {@link measureLayout} answers, in wasm and with the fonts the engine draws with, where every box
 * of a layout lies on a picture and whether anything overflows, before anything is rendered. The
 * builder ({@link box}, {@link text}, {@link shape}, {@link layout}) writes the document from
 * CSS-like props: a value that is not animated becomes a constant, a `[top, right, bottom, left]`
 * tuple the sides object, an `[x, y]` tuple a vector. Lengths are written as the document writes
 * them: a number is a fraction of the picture's shorter side, `"12px"` pixels of the project
 * canvas, `"50%"` a percent of the parent's content box. Text style (`font_family`, `font_size`,
 * `color`, `line_height`, `text_align`, `outline`) may be set on any box and is inherited. Its
 * output is a plain document that `validateProject` accepts; a CSS value string it cannot read
 * throws, with the parser's message.
 */

import type { PictureSize } from "./geometry.js";
import {
  parseHex,
  parseInsets,
  parseLength,
  parseOffset,
  parsePlacement,
  parseSize,
  parseTrackList,
  type Parsed,
} from "./layout-values.js";
import type {
  AlignContent,
  AlignItems,
  Animated,
  Display,
  FlexDirection,
  FlexWrap,
  Generator,
  GridAutoFlow,
  JustifyContent,
  LayoutNode,
  Overflow,
  Paint,
  Position,
  Rgba,
  ShapeItem,
  TextAlign,
  TextOutline,
  Ticks,
  Vec2,
} from "./project.js";

/** The part of `alpha2/wasm`'s `WasmTextMeasurer` {@link measureLayout} uses. */
export interface LayoutBinding {
  measureLayout(
    generatorJson: string,
    width: number,
    height: number,
    time: number,
    canvasWidth?: number,
    canvasHeight?: number,
  ): string;
}

/** A rectangle as fractions of the picture, `(0, 0)` top-left to `(1, 1)` bottom-right. */
export interface LayoutRect {
  readonly left: number;
  readonly top: number;
  readonly right: number;
  readonly bottom: number;
}

/** Where one node lies: its layout boxes, before its clip's transform and node visuals. */
export interface NodeBounds {
  /** Its path from the root, like `root.children[1]`. */
  readonly at: string;
  /** The border box. */
  readonly border: LayoutRect;
  /** The content box. */
  readonly content: LayoutRect;
  /**
   * Whether it overflows: its text is taller than its content box or splits a word, or an
   * in-flow child reaches past its padding box. Rendering draws it anyway.
   */
  readonly overflow: boolean;
  /** A text node's line count; `null` for other nodes. */
  readonly lineCount: number | null;
  /**
   * A text node's first and last lines' alphabetic baselines, as fractions of the picture's height:
   * its content box's top plus its text's baselines. `null` for other nodes and empty text.
   */
  readonly baselines: Baselines | null;
}

/** Where a text's first and last lines' alphabetic baselines lie, y as a fraction of the height. */
export interface Baselines {
  readonly first: number;
  readonly last: number;
}

/** Where every node of a layout lies, the root first, in document order. */
export interface LayoutBounds {
  readonly nodes: readonly NodeBounds[];
}

/**
 * Where every node of `generator` (a layout generator, or its root) lies on `picture` at picture
 * time `time`, laid out with `binding`'s fonts as a frame lays it out. Measure at the size the
 * picture is drawn at: at another size, lines can break elsewhere. The layout's `"<n>px"` lengths
 * are pixels of `canvas`, the project canvas: `picture` itself by default, so a preview measured
 * at half size passes the project's own size here.
 *
 * @throws An `INVALID_INPUT` error for a layout the model rejects (on that canvas), a picture or
 * canvas outside 1 to 16384 pixels a side, or a time that is not whole ticks; a text error for a
 * measurer without fonts when the layout has text.
 */
export function measureLayout(
  binding: LayoutBinding,
  generator: Generator | LayoutNode,
  picture: PictureSize,
  time: Ticks = 0,
  canvas: PictureSize = picture,
): LayoutBounds {
  const document =
    "type" in generator && generator.type === "layout"
      ? generator
      : layout(generator as LayoutNode);
  return JSON.parse(
    binding.measureLayout(
      JSON.stringify(document),
      picture.width,
      picture.height,
      time,
      canvas.width,
      canvas.height,
    ),
  ) as LayoutBounds;
}

/** A value, or the document's animated form of it. */
export type Value<T> = T | Animated<T>;

/** A length that may not be a percent: a fraction of the picture's shorter side, or `"12px"`. */
export type Size = number | `${number}px`;

/** A size or inset: a {@link Size}, or `"50%"` of the parent's content box. */
export type Length = Size | `${number}%`;

/**
 * The sides of a box: one size for all, the CSS shorthand (`"8px 16px"`), a
 * `[top, right, bottom, left]` tuple, or the sides object (missing sides 0).
 */
export type Sides =
  | number
  | string
  | readonly [Size, Size, Size, Size]
  | {
      readonly top?: Size;
      readonly right?: Size;
      readonly bottom?: Size;
      readonly left?: Size;
    };

/** A color: the components object, or CSS hex (`"#1e2230"`) as `Rgba` allows. */
export type Color = Rgba;

/** A paint, or a color painted solid. */
export type Fill = Paint | Color;

/** An offset: the CSS `translate` string (`"10px 20px"`), an `[x, y]` tuple, or `{ x, y }`. */
export type Offset =
  string | readonly [Size, Size] | { readonly x: Size; readonly y: Size };

/** The style a box gives its text and the boxes in it, inherited as CSS inherits it. */
export interface TextStyleProps {
  readonly font_family?: string;
  readonly font_size?: Value<Size>;
  readonly color?: Color;
  readonly line_height?: number;
  readonly text_align?: TextAlign;
  readonly outline?: TextOutline;
}

/** A box's CSS fields, with shorthands. */
export interface BoxProps extends TextStyleProps {
  readonly display?: Display;
  readonly flex_direction?: FlexDirection;
  readonly flex_wrap?: FlexWrap;
  /** A grid's columns, as CSS writes them: `"1fr 2fr"`, `"repeat(3, minmax(100px, 1fr))"`. */
  readonly grid_template_columns?: string;
  readonly grid_template_rows?: string;
  readonly grid_auto_columns?: string;
  readonly grid_auto_rows?: string;
  readonly grid_auto_flow?: GridAutoFlow;
  readonly gap?: Value<Size>;
  readonly row_gap?: Value<Size>;
  readonly column_gap?: Value<Size>;
  readonly justify_content?: JustifyContent;
  readonly align_items?: AlignItems;
  readonly align_content?: AlignContent;
  readonly justify_items?: AlignItems;
  readonly padding?: Value<Sides>;
  readonly background?: Value<Fill>;
  readonly border?: {
    readonly width: Value<Size>;
    readonly paint: Value<Fill>;
  };
  readonly border_radius?: Value<Size>;
  readonly overflow?: Overflow;
  readonly width?: Value<Length>;
  readonly height?: Value<Length>;
  readonly min_width?: Value<Length>;
  readonly min_height?: Value<Length>;
  readonly max_width?: Value<Length>;
  readonly max_height?: Value<Length>;
  readonly aspect_ratio?: number;
  readonly margin?: Value<Sides>;
  readonly flex_grow?: Value<number>;
  readonly flex_shrink?: Value<number>;
  readonly flex_basis?: Value<Length>;
  readonly align_self?: AlignItems;
  readonly justify_self?: AlignItems;
  /** The box's grid columns, as CSS writes them: `2`, `"1 / 3"`, `"span 2"`. */
  readonly grid_column?: number | string;
  readonly grid_row?: number | string;
  readonly position?: Position;
  readonly inset?: {
    readonly left?: Value<Length>;
    readonly top?: Value<Length>;
    readonly right?: Value<Length>;
    readonly bottom?: Value<Length>;
  };
  readonly opacity?: Value<number>;
  readonly translate?: Value<Offset>;
  /** One number scales both axes. */
  readonly scale?: Value<Vec2> | number | readonly [number, number];
  readonly rotation_degrees?: Value<number>;
}

/** A layout generator whose root is `root`: the whole picture. */
export function layout(root: LayoutNode): Generator {
  return { type: "layout", root };
}

/** A box: a container holding `children`, or empty without them. */
export function box(
  props: BoxProps = {},
  ...children: LayoutNode[]
): LayoutNode {
  return children.length > 0
    ? { ...boxFields(props), children }
    : boxFields(props);
}

/**
 * A box holding one paragraph of text, wrapped at its content width and drawn in the style it
 * sets or inherits.
 */
export function text(
  content: Value<string>,
  props: Omit<BoxProps, ContainerField> = {},
): LayoutNode {
  return { ...boxFields(props), text: animated(content) };
}

/**
 * A box holding shapes, drawn on a picture the size of its content box: positions normalized to
 * it, lengths fractions of its shorter side. It has no size of its own: give it `width` and
 * `height`, an `aspect_ratio`, or `flex_grow`.
 */
export function shape(
  items: readonly ShapeItem[],
  props: Omit<BoxProps, ContainerField> = {},
): LayoutNode {
  return { ...boxFields(props), shape: { items } };
}

/** The container fields a text or shape node may not take. */
type ContainerField =
  | "display"
  | "flex_direction"
  | "flex_wrap"
  | "grid_template_columns"
  | "grid_template_rows"
  | "grid_auto_columns"
  | "grid_auto_rows"
  | "grid_auto_flow"
  | "gap"
  | "row_gap"
  | "column_gap"
  | "justify_content"
  | "align_items"
  | "align_content"
  | "justify_items";

function isAnimated(value: unknown): value is Animated<unknown> {
  return (
    typeof value === "object" &&
    value !== null &&
    "mode" in value &&
    (value.mode === "constant" || value.mode === "keyframes")
  );
}

function animated<T>(value: Value<T>): Animated<T> {
  return isAnimated(value) ? value : { mode: "constant", value };
}

/** `value` when `parse` reads it; otherwise the parser's message, thrown. */
function checked<T>(value: T, parsed: Parsed<unknown>): T {
  if (!parsed.ok) throw new TypeError(parsed.error);
  return value;
}

/** A length's or size's string checked as the engine reads it; numbers as they are. */
function lengthValue<T>(value: T, percent: boolean): T {
  if (typeof value !== "string") return value;
  return checked(value, percent ? parseLength(value) : parseSize(value));
}

function sidesValue(value: Sides): unknown {
  if (typeof value === "number") return value;
  if (typeof value === "string") return checked(value, parseInsets(value));
  if (Array.isArray(value)) {
    const [top, right, bottom, left] = (value as readonly Size[]).map((side) =>
      lengthValue(side, false),
    );
    return { top, right, bottom, left };
  }
  return Object.fromEntries(
    Object.entries(value).map(([side, size]) => [
      side,
      lengthValue(size, false),
    ]),
  );
}

function fillValue(value: Fill): unknown {
  return typeof value === "string" ? checked(value, parseHex(value)) : value;
}

function offsetValue(value: Offset): unknown {
  if (typeof value === "string") return checked(value, parseOffset(value));
  if (Array.isArray(value)) {
    const [x, y] = value as readonly Size[];
    return { x: lengthValue(x, false), y: lengthValue(y, false) };
  }
  const { x, y } = value as { readonly x: Size; readonly y: Size };
  return { x: lengthValue(x, false), y: lengthValue(y, false) };
}

/** `value` as an animated field, each of its values mapped by `map` when it is not animated. */
function field<T>(value: Value<T>, map: (value: T) => unknown): unknown {
  return isAnimated(value) ? value : { mode: "constant", value: map(value) };
}

function scaleField(
  value: Value<Vec2> | number | readonly [number, number],
): Animated<Vec2> {
  if (typeof value === "number") {
    return { mode: "constant", value: { x: value, y: value } };
  }
  if (Array.isArray(value)) {
    const [x, y] = value as readonly number[];
    return { mode: "constant", value: { x, y } };
  }
  return animated(value as Value<Vec2>);
}

/** `props` as the document's fields, each shorthand written out; absent fields stay absent. */
function boxFields(props: BoxProps): LayoutNode {
  const fields: Record<string, unknown> = {};
  for (const [name, value] of Object.entries(props)) {
    if (value === undefined) continue;
    switch (name) {
      case "flex_grow":
      case "flex_shrink":
      case "opacity":
      case "rotation_degrees":
        fields[name] = animated(value as Value<number>);
        break;
      case "gap":
      case "row_gap":
      case "column_gap":
      case "border_radius":
      case "font_size":
        fields[name] = field(value as Value<Size>, (size) =>
          lengthValue(size, false),
        );
        break;
      case "padding":
      case "margin":
        fields[name] = field(value as Value<Sides>, sidesValue);
        break;
      case "background":
        fields[name] = field(value as Value<Fill>, fillValue);
        break;
      case "border": {
        const border = value as NonNullable<BoxProps["border"]>;
        fields[name] = {
          width: field(border.width, (size) => lengthValue(size, false)),
          paint: field(border.paint, fillValue),
        };
        break;
      }
      case "width":
      case "height":
      case "min_width":
      case "min_height":
      case "max_width":
      case "max_height":
      case "flex_basis":
        fields[name] = field(value as Value<Length>, (length) =>
          lengthValue(length, true),
        );
        break;
      case "inset":
        fields[name] = Object.fromEntries(
          Object.entries(value as NonNullable<BoxProps["inset"]>)
            .filter(([, side]) => side !== undefined)
            .map(([side, length]) => [
              side,
              field(length, (inset) => lengthValue(inset, true)),
            ]),
        );
        break;
      case "grid_template_columns":
      case "grid_template_rows":
      case "grid_auto_columns":
      case "grid_auto_rows":
        fields[name] = checked(value, parseTrackList(value as string));
        break;
      case "grid_column":
      case "grid_row":
        fields[name] = checked(value, parsePlacement(value as number | string));
        break;
      case "color":
        fields[name] = fillValue(value as Color);
        break;
      case "translate":
        fields[name] = field(value as Value<Offset>, offsetValue);
        break;
      case "scale":
        fields[name] = scaleField(value as Value<Vec2>);
        break;
      default:
        // CSS keywords, the aspect ratio and the plain style values are written as they are;
        // anything else the model refuses by name.
        fields[name] = value;
    }
  }
  return fields;
}
