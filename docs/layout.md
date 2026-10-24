# Layout generators

A layout generator draws boxes laid out by CSS flexbox around their text and shapes: a title card, a lower third, a panel of wrapped text. The engine lays it out with the fonts it draws with, at the size it draws at, so the boxes fit their text in preview and export alike, in the browser and natively. Before rendering, an author can ask where every box lies and whether anything overflows (`measureLayout`).

A layout is a generator, like a fill, a text or shapes: a generated source's `generator`, a composition's generator node, or a media node naming a layout source. Its picture is the canvas's size. A clip places, times and animates that picture and applies effects, masks and transitions to it as to any generator.

```json
{ "id": "lower-third", "kind": "generated", "generator": { "type": "layout", "root": { ... } } }
```

## Boxes

Every node of the tree is one kind of box, its fields CSS property names in snake_case and its values CSS keywords in snake_case (`"flex_direction": "column"`, `"justify_content": "space_between"`). A box is a flex container when it has `children`, a leaf when it has `text` or `shape`, and empty otherwise (a spacer, a bar, a decoration). A field the layout does not take is refused by name: `project.sources[1].generator.root has unknown field "display"`.

| Group | Fields | Notes |
| --- | --- | --- |
| Content (one of) | `children`, `text`, `shape` | |
| Flex container | `flex_direction`, `flex_wrap`, `gap`, `justify_content`, `align_items` | Not on a box with `text` or `shape`. Defaults: `row`, `nowrap`, 0, `start`, `stretch`. |
| Box | `padding`, `background`, `border` (`width`, `paint`), `border_radius`, `overflow` | `border-box` sizing: a width includes padding and border. `overflow: hidden` clips the content to the padding box. |
| Flex item | `width`, `height`, `min_width`, `min_height`, `max_width`, `max_height`, `aspect_ratio`, `margin`, `flex_grow`, `flex_shrink`, `flex_basis`, `align_self`, `position` (`relative`, `absolute`), `inset` | Not on the root. Absent sizes are `auto`; `min_*` auto is the CSS automatic minimum size. No `auto` margins. |
| Drawn after layout | `opacity`, `translate`, `scale`, `rotation_degrees` | About the border box's centre; they never move other boxes. A box with any of them, or with hidden overflow, is drawn as one group: its opacity is group opacity. |

The root is the picture: its border box is the canvas, so it takes no size, margin or flex item field. Place its content with its own `justify_content`, `align_items` and `padding`, as with Remotion's `AbsoluteFill`.

Boxes are drawn in document order, each box's background and border under its content; reversed directions move boxes, not their order, and there is no z-index.

## Units

Every length (sizes, padding, gap, margin, inset, radius, border width, font size, translate) is a fraction of the picture's shorter side, so the same numbers keep their proportions on a 16:9 and a 9:16 canvas: at 1080p the shorter side is 1080 pixels in both. A size or an inset may instead be a percent of the parent's content box, as in CSS:

```json
"width": { "mode": "constant", "value": { "type": "length", "value": 0.25 } },
"max_width": { "mode": "constant", "value": { "type": "percent", "value": 60 } }
```

Boxes are laid out in render pixels and rounded to whole pixels, so text textures are drawn 1:1. A value animated slowly therefore moves in whole-pixel steps, as in browsers: animate `translate` or `scale` for smooth motion.

## Text

A text box holds one paragraph in one style, wrapped at its content width: `content`, `font_family`, `font_size`, `color`, `line_height`, `text_align` (`start` by default, as in CSS) and `outline`. `\n` starts a line. Lines are aligned in the content width; there is no vertical alignment inside a text box, so align it from its parent. In a row a text box is as wide as its text, up to the room it has; in a column it takes the column's width.

`content` may be empty (it then takes no room) and may change over time, but its keyframes must all `hold`: strings do not interpolate. Text is never shrunk to fit.

## Shapes

A shape box draws a shapes content tree (the `items` a shapes generator takes) on a picture the size of its content box: positions normalized to that box, lengths fractions of its shorter side, like an SVG viewBox. It has no size of its own: give it `width` and `height`, an `aspect_ratio`, or `flex_grow`.

Paints of a box's background and border are normalized to its border box: a linear gradient from `x` 0 to 1 spans the box, and a radial radius is a fraction of the box's shorter side.

## Animation

Numeric values are keyframed in the picture's time, as shapes are: a clip's source time, a generator node's composition time, a media node's composition time plus its `start`. Keywords and booleans do not animate; size keyframes keep one kind (`length` or `percent`). A layout without keyframes is laid out once and every later frame reuses it.

## Measuring

`measureLayout` (from `alpha2/layout`, on `alpha2/wasm`'s `WasmTextMeasurer`) lays a layout out with the given fonts, at the given picture size and time, as a frame lays it out, and answers each node's border and content box as fractions of the picture, its text's line count, and whether it overflows: its text is taller than its box or splits a word, or an in-flow child reaches past its padding box. Rendering draws overflowing boxes anyway; measure at the size the picture is drawn at, and fix the document.

```ts
import init, { WasmTextMeasurer } from "alpha2/wasm";
import { measureLayout } from "alpha2/layout";

await init();
const measurer = new WasmTextMeasurer();
measurer.addFont("Inter", interBytes, 0);
const { nodes } = measureLayout(measurer, generator, { width: 1920, height: 1080 }, 0);
const overflowing = nodes.filter((node) => node.overflow).map((node) => node.at);
```

Natively, `alpha2_engine::measure_layout` and `TextTextures::measure_layout` answer the same, to the bit.

## Building

`alpha2/layout` also writes the document from CSS-like shorthands: a number is a constant, `"50%"` a percent, one number or `[top, right, bottom, left]` the sides of a box, a color a solid paint, and `px(n)` a length from pixels of a 1080-pixel design. Its output is a plain document that `validateProject` accepts.

## Examples

These are the documents the engine's golden images (`tests/golden/layout_*.png`) are drawn from, on a 320 x 180 canvas.

### Title card

A gradient backdrop with a bordered, rounded card in its middle: a radial-gradient icon beside a title, over a centred subtitle.

```ts
import { box, layout, shape, text } from "alpha2/layout";

const ink = { red: 0.1, green: 0.1, blue: 0.15, alpha: 1 };
const icon = shape(
  [
    { type: "ellipse", center: { mode: "constant", value: { x: 0.5, y: 0.5 } }, size: { mode: "constant", value: { x: 1, y: 1 } } },
    { type: "fill", paint: { mode: "constant", value: { type: "radial_gradient", center: { x: 0.5, y: 0.5 }, radius: 0.5,
      stops: [ { offset: 0, color: { red: 1, green: 0.9, blue: 0.2, alpha: 1 } }, { offset: 1, color: { red: 1, green: 0.4, blue: 0, alpha: 1 } } ] } } },
  ],
  { width: 0.12, height: 0.12 },
);
const titleCard = layout(
  box(
    {
      justify_content: "center",
      align_items: "center",
      background: { type: "linear_gradient", start: { x: 0, y: 0 }, end: { x: 1, y: 1 }, spread: "pad",
        stops: [ { offset: 0, color: { red: 0.05, green: 0.1, blue: 0.3, alpha: 1 } }, { offset: 1, color: { red: 0.3, green: 0.05, blue: 0.2, alpha: 1 } } ] },
    },
    box(
      {
        flex_direction: "column",
        align_items: "center",
        gap: 0.03,
        padding: [0.05, 0.08, 0.05, 0.08],
        background: { red: 0.95, green: 0.95, blue: 0.92, alpha: 1 },
        border: { width: 0.012, paint: { red: 0.9, green: 0.5, blue: 0.1, alpha: 1 } },
        border_radius: 0.06,
      },
      box({ flex_direction: "row", align_items: "center", gap: 0.03 }, icon, text("Title", { font_size: 0.14, color: ink })),
      text("a centred subtitle", { font_size: 0.07, color: { red: 0.3, green: 0.3, blue: 0.35, alpha: 1 } }),
    ),
  ),
);
```

### Lower third

A card that sizes to its text up to 60% of the picture's content width, where the subtitle wraps. The accent bar stretches to the card's inner height; `flex_shrink: 0` keeps it from giving up its width when the text is long. The card fades and rises in as one group over 0.3 s, without laying out again.

```json
{
  "type": "layout",
  "root": {
    "flex_direction": "column",
    "justify_content": "end",
    "align_items": "start",
    "padding": { "mode": "constant", "value": { "top": 0.06, "right": 0.06, "bottom": 0.08, "left": 0.06 } },
    "children": [
      {
        "flex_direction": "row",
        "gap": { "mode": "constant", "value": 0.016 },
        "padding": { "mode": "constant", "value": { "top": 0.022, "right": 0.03, "bottom": 0.022, "left": 0.022 } },
        "background": { "mode": "constant", "value": { "type": "solid", "color": { "red": 0.06, "green": 0.07, "blue": 0.09, "alpha": 0.92 } } },
        "border_radius": { "mode": "constant", "value": 0.012 },
        "max_width": { "mode": "constant", "value": { "type": "percent", "value": 60 } },
        "opacity": { "mode": "keyframes", "keyframes": [
          { "time": 0, "value": 0, "interpolation": { "type": "cubic_bezier", "x1": 0.2, "y1": 0.0, "x2": 0.0, "y2": 1.0 } },
          { "time": 36000, "value": 1, "interpolation": { "type": "hold" } }
        ] },
        "translate": { "mode": "keyframes", "keyframes": [
          { "time": 0, "value": { "x": 0, "y": 0.03 }, "interpolation": { "type": "cubic_bezier", "x1": 0.2, "y1": 0.0, "x2": 0.0, "y2": 1.0 } },
          { "time": 36000, "value": { "x": 0, "y": 0 }, "interpolation": { "type": "hold" } }
        ] },
        "children": [
          {
            "width": { "mode": "constant", "value": { "type": "length", "value": 0.005 } },
            "flex_shrink": { "mode": "constant", "value": 0 },
            "background": { "mode": "constant", "value": { "type": "solid", "color": { "red": 1.0, "green": 0.62, "blue": 0.1, "alpha": 1.0 } } },
            "border_radius": { "mode": "constant", "value": 0.0025 }
          },
          {
            "flex_direction": "column",
            "gap": { "mode": "constant", "value": 0.006 },
            "children": [
              { "text": { "content": { "mode": "constant", "value": "Ada Lovelace" }, "font_size": { "mode": "constant", "value": 0.07 } } },
              { "text": { "content": { "mode": "constant", "value": "Wrote the first published algorithm for a machine, 1843" }, "font_size": { "mode": "constant", "value": 0.045 }, "color": { "red": 0.8, "green": 0.82, "blue": 0.86, "alpha": 1.0 } } }
            ]
          }
        ]
      }
    ]
  }
}
```

### Text panel

A paragraph aligned to its end in a fixed-width card, a box with hidden overflow clipping a child twice its size, and a tilted badge placed absolutely from the bottom-right corner.

```ts
import { box, layout, text } from "alpha2/layout";

const panel = layout(
  box(
    { flex_direction: "row", gap: 0.04, padding: 0.05, align_items: "start" },
    box(
      { width: 0.9, padding: 0.03, background: { red: 1, green: 1, blue: 1, alpha: 0.9 }, border_radius: 0.03 },
      text("Layout resolves where text is drawn, so a paragraph wraps the same in preview and export.", {
        font_size: 0.06, color: { red: 0, green: 0, blue: 0, alpha: 1 }, text_align: "end", line_height: 1.1,
      }),
    ),
    box(
      { width: 0.4, height: 0.4, overflow: "hidden", border_radius: 0.03,
        background: { red: 0.1, green: 0.3, blue: 0.8, alpha: 1 },
        border: { width: 0.01, paint: { red: 1, green: 1, blue: 1, alpha: 1 } } },
      box({ width: 0.6, height: 0.6, flex_shrink: 0, background: { red: 0.9, green: 0.1, blue: 0.1, alpha: 1 }, margin: { left: 0.1, top: 0.1 } }),
    ),
    box(
      { position: "absolute", inset: { right: 0.05, bottom: 0.05 }, padding: [0.01, 0.03, 0.01, 0.03],
        background: { red: 0.1, green: 0.6, blue: 0.3, alpha: 1 }, border_radius: 0.5, rotation_degrees: -8 },
      text("NEW", { font_size: 0.06 }),
    ),
  ),
);
```

## Not supported

Grid, block and inline layout; baseline alignment; `auto` margins; style inheritance; rich-text spans; per-side border widths and per-corner radii; box shadows; ellipsis and line clamping; right-to-left flex direction; and image or video boxes. Bidirectional text inside a paragraph is shaped as text generators shape it.
