/**
 * Layout generators for authors: boxes laid out by CSS flexbox around their text and shapes, as
 * a project's `{ "type": "layout" }` generator holds them.
 *
 * {@link measureLayout} answers, in wasm and with the fonts the engine draws with, where every box
 * of a layout lies on a picture and whether anything overflows, before anything is rendered. The
 * builder ({@link box}, {@link text}, {@link shape}, {@link layout}) writes the explicit document
 * from CSS-like shorthands: a number is a constant, `"50%"` a percent of the parent's content
 * box, `[top, right, bottom, left]` or one number the sides of a box. Its output is a plain
 * document that `validateProject` accepts.
 *
 * Lengths are fractions of the picture's shorter side, as the document's are; {@link px} turns
 * pixels of a design with a given shorter side (1080 by default) into one.
 */

import type { PictureSize } from "./geometry.js";
import type {
  AlignItems,
  Animated,
  FlexDirection,
  FlexWrap,
  Generator,
  Insets,
  JustifyContent,
  LayoutLength,
  LayoutNode,
  LayoutText,
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
}

/** Where every node of a layout lies, the root first, in document order. */
export interface LayoutBounds {
  readonly nodes: readonly NodeBounds[];
}

/**
 * Where every node of `generator` (a layout generator, or its root) lies on `picture` at picture
 * time `time`, laid out with `binding`'s fonts as a frame lays it out. Measure at the size the
 * picture is drawn at: at another size, lines can break elsewhere.
 *
 * @throws An `INVALID_INPUT` error for a layout the model rejects, a picture outside 1 to 16384
 * pixels a side, or a time that is not whole ticks; a text error for a measurer without fonts
 * when the layout has text.
 */
export function measureLayout(
  binding: LayoutBinding,
  generator: Generator | LayoutNode,
  picture: PictureSize,
  time: Ticks = 0,
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
    ),
  ) as LayoutBounds;
}

/** A value, or the document's animated form of it. */
export type Value<T> = T | Animated<T>;

/** A length: a fraction of the picture's shorter side, `"N%"` of the parent's content box. */
export type Length = number | `${number}%` | LayoutLength;

/** The sides of a box: one for all, `[top, right, bottom, left]`, or `Insets`. */
export type Sides =
  number | readonly [number, number, number, number] | Partial<Insets>;

/** A paint, or a color painted solid. */
export type Fill = Paint | Rgba;

/** A box's CSS fields, with shorthands. */
export interface BoxProps {
  readonly flex_direction?: FlexDirection;
  readonly flex_wrap?: FlexWrap;
  readonly gap?: Value<number>;
  readonly justify_content?: JustifyContent;
  readonly align_items?: AlignItems;
  readonly padding?: Value<Sides> | Animated<Insets>;
  readonly background?: Value<Fill> | Animated<Paint>;
  readonly border?: {
    readonly width: Value<number>;
    readonly paint: Value<Fill> | Animated<Paint>;
  };
  readonly border_radius?: Value<number>;
  readonly overflow?: Overflow;
  readonly width?: Value<Length> | Animated<LayoutLength>;
  readonly height?: Value<Length> | Animated<LayoutLength>;
  readonly min_width?: Value<Length> | Animated<LayoutLength>;
  readonly min_height?: Value<Length> | Animated<LayoutLength>;
  readonly max_width?: Value<Length> | Animated<LayoutLength>;
  readonly max_height?: Value<Length> | Animated<LayoutLength>;
  readonly aspect_ratio?: number;
  readonly margin?: Value<Sides> | Animated<Insets>;
  readonly flex_grow?: Value<number>;
  readonly flex_shrink?: Value<number>;
  readonly flex_basis?: Value<Length> | Animated<LayoutLength>;
  readonly align_self?: AlignItems;
  readonly position?: Position;
  readonly inset?: {
    readonly left?: Value<Length> | Animated<LayoutLength>;
    readonly top?: Value<Length> | Animated<LayoutLength>;
    readonly right?: Value<Length> | Animated<LayoutLength>;
    readonly bottom?: Value<Length> | Animated<LayoutLength>;
  };
  readonly opacity?: Value<number>;
  readonly translate?: Value<Vec2> | readonly [number, number];
  /** One number scales both axes. */
  readonly scale?: Value<Vec2> | number | readonly [number, number];
  readonly rotation_degrees?: Value<number>;
}

/** A text node's style, with shorthands. */
export interface TextProps {
  readonly font_family?: string;
  readonly font_size?: Value<number>;
  readonly color?: Rgba;
  readonly line_height?: number;
  readonly text_align?: TextAlign;
  readonly outline?: TextOutline;
}

/** `pixels` of a design whose shorter side is `designShortSide` pixels, as a layout length. */
export function px(pixels: number, designShortSide = 1080): number {
  return pixels / designShortSide;
}

/** A layout generator whose root is `root`: the whole picture. */
export function layout(root: LayoutNode): Generator {
  return { type: "layout", root };
}

/** A box: a flex container holding `children`, or empty without them. */
export function box(
  props: BoxProps = {},
  ...children: LayoutNode[]
): LayoutNode {
  return children.length > 0
    ? { ...boxFields(props), children }
    : boxFields(props);
}

/** A box holding one paragraph of text, wrapped at its content width. */
export function text(
  content: Value<string>,
  props: TextProps & Omit<BoxProps, ContainerField> = {},
): LayoutNode {
  const {
    font_family,
    font_size,
    color,
    line_height,
    text_align,
    outline,
    ...rest
  } = props;
  const style: LayoutText = {
    content: animated(content),
    ...(font_family === undefined ? {} : { font_family }),
    ...(font_size === undefined ? {} : { font_size: animated(font_size) }),
    ...(color === undefined ? {} : { color }),
    ...(line_height === undefined ? {} : { line_height }),
    ...(text_align === undefined ? {} : { text_align }),
    ...(outline === undefined ? {} : { outline }),
  };
  return { ...boxFields(rest), text: style };
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

/** The flex container fields a text or shape node may not take. */
type ContainerField =
  "flex_direction" | "flex_wrap" | "gap" | "justify_content" | "align_items";

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

function lengthOf(value: Length): LayoutLength {
  if (typeof value === "number") return { type: "length", value };
  if (typeof value === "string") {
    const percent = Number(value.slice(0, -1));
    if (!value.endsWith("%") || !Number.isFinite(percent)) {
      throw new TypeError(`a length is a number or "N%", not ${value}`);
    }
    return { type: "percent", value: percent };
  }
  return value;
}

function lengthField(
  value: Value<Length> | Animated<LayoutLength>,
): Animated<LayoutLength> {
  return isAnimated(value)
    ? (value as Animated<LayoutLength>)
    : { mode: "constant", value: lengthOf(value) };
}

function insetsOf(value: Sides): Insets {
  if (typeof value === "number") {
    return { top: value, right: value, bottom: value, left: value };
  }
  if (Array.isArray(value)) {
    const [top, right, bottom, left] = value as readonly number[];
    return { top, right, bottom, left };
  }
  const sides = value as Partial<Insets>;
  return {
    top: sides.top ?? 0,
    right: sides.right ?? 0,
    bottom: sides.bottom ?? 0,
    left: sides.left ?? 0,
  };
}

function sidesField(value: Value<Sides> | Animated<Insets>): Animated<Insets> {
  return isAnimated(value)
    ? (value as Animated<Insets>)
    : { mode: "constant", value: insetsOf(value) };
}

function paintOf(value: Fill): Paint {
  return "type" in value ? value : { type: "solid", color: value };
}

function paintField(value: Value<Fill> | Animated<Paint>): Animated<Paint> {
  return isAnimated(value)
    ? (value as Animated<Paint>)
    : { mode: "constant", value: paintOf(value) };
}

function vec2Field(
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
      case "gap":
      case "border_radius":
      case "flex_grow":
      case "flex_shrink":
      case "opacity":
      case "rotation_degrees":
        fields[name] = animated(value as Value<number>);
        break;
      case "padding":
      case "margin":
        fields[name] = sidesField(value as Value<Sides>);
        break;
      case "background":
        fields[name] = paintField(value as Value<Fill>);
        break;
      case "border": {
        const border = value as NonNullable<BoxProps["border"]>;
        fields[name] = {
          width: animated(border.width),
          paint: paintField(border.paint),
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
        fields[name] = lengthField(value as Value<Length>);
        break;
      case "inset":
        fields[name] = Object.fromEntries(
          Object.entries(value as NonNullable<BoxProps["inset"]>)
            .filter(([, side]) => side !== undefined)
            .map(([side, length]) => [
              side,
              lengthField(length as Value<Length>),
            ]),
        );
        break;
      case "translate":
      case "scale":
        fields[name] = vec2Field(value as Value<Vec2>);
        break;
      default:
        // CSS keywords and the aspect ratio are written as they are; anything else the model
        // refuses by name.
        fields[name] = value;
    }
  }
  return fields;
}
