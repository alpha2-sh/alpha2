# Layout generators

A layout generator draws boxes laid out by CSS flexbox or grid around their text and shapes: a title card, a lower third, a dashboard, a panel of wrapped text. The engine lays it out with the fonts it draws with, at the size it draws at, so the boxes fit their text in preview and export alike, in the browser and natively. Before rendering, an author can ask where every box lies and whether anything overflows (`measureLayout`).

A layout is a generator, like a fill, a text or shapes: a generated source's `generator`, a composition's generator node, or a media node naming a layout source. Its picture is the canvas's size. A clip places, times and animates that picture and applies effects, masks and transitions to it as to any generator.

```json
{ "id": "lower-third", "kind": "generated", "generator": { "type": "layout", "root": { ... } } }
```

Every animatable value is still written in the project's animated form, `{ "mode": "constant", "value": ... }` or `{ "mode": "keyframes", ... }`, as everywhere in a project. What goes in `value` is what this page describes.

## Boxes

Every node of the tree is one kind of box. A box is a container when it has `children`, a leaf when it has `text` or `shape`, and empty otherwise (a spacer, a bar, a decoration). A field the layout does not take is refused by name: `project.sources[1].generator.root has unknown field "float"`. A field that does not apply where it is written is refused naming it and why: `root.children[0] sets grid_column, a grid item field, in a flex container`.

| Group | Fields | Notes |
| --- | --- | --- |
| Content (one of) | `children`, `text`, `shape` | |
| Container | `display` (`flex`, `grid`), `gap`, `row_gap`, `column_gap`, `justify_content`, `align_items`, `align_content` | Not on a box with `text` or `shape`. Defaults: `flex`, 0, `start`, `stretch`, `stretch`. `row_gap` and `column_gap` replace `gap` on their axis. |
| Flex container | `flex_direction`, `flex_wrap` | Defaults `row`, `nowrap`. Not on a grid. |
| Grid container | `grid_template_columns`, `grid_template_rows`, `grid_auto_columns`, `grid_auto_rows`, `grid_auto_flow`, `justify_items` | Only with `display: "grid"`. See [Grid](#grid). |
| Box | `padding`, `background`, `border` (`width`, `paint`), `border_radius`, `overflow` | `border-box` sizing: a width includes padding and border. `overflow: hidden` clips the content to the padding box. |
| Item | `width`, `height`, `min_width`, `min_height`, `max_width`, `max_height`, `aspect_ratio`, `margin`, `align_self`, `position` (`relative`, `absolute`), `inset` | Not on the root. Absent sizes are `auto`; `min_*` auto is the CSS automatic minimum size. No `auto` margins. |
| Flex item | `flex_grow`, `flex_shrink`, `flex_basis` | In a flex container only. |
| Grid item | `grid_column`, `grid_row`, `justify_self` | In a grid only. |
| Text style | `font_family`, `font_size`, `color`, `line_height`, `text_align`, `outline` | On any box; inherited. See [Text](#text). |
| Drawn after layout | `opacity`, `translate`, `scale`, `rotation_degrees` | About the border box's centre; they never move other boxes. A box with any of them, or with hidden overflow, is drawn as one group: its opacity is group opacity. |

The root is the picture: its border box is the canvas, so it takes no size, margin or item field. Place its content with its own `justify_content`, `align_items` and `padding`, as with Remotion's `AbsoluteFill`.

Boxes are drawn in document order, each box's background and border under its content; reversed directions and grid placements move boxes, not their order, and there is no z-index.

## Units

A length is written three ways:

| Written | Means |
| --- | --- |
| `0.05` (a number) | A fraction of the picture's shorter side: the same numbers keep their proportions on a 16:9 and a 9:16 canvas. |
| `"12px"` | Pixels of the project canvas. On a 1920x1080 project, `"108px"` is `0.1`. |
| `"50%"` | A percent of the parent's content box along the field's axis: sizes and insets only, as in CSS. |

Every layout length takes the first two: sizes, padding, margin, gap, inset, `border_radius`, border width, `font_size`, `translate`. Percent is refused where CSS and the layout give it no meaning (`gap: "5%"` is `percent is not allowed here`). Pixels are converted once, against the project canvas: a preview at half size draws `"12px"` as 6 preview pixels, keeping the picture's proportions. Within one keyframe sequence, pixels and numbers mix freely; a percent does not mix with either.

`padding`, `margin` and `inset` take the CSS shorthand: one length for every side, or a string of one to four (`"8px 16px"`: top and bottom 8, left and right 16; `"1px 2px 3px 4px"`: top, right, bottom, left), or the sides object (missing sides 0). `translate` takes one or two lengths as CSS writes them (`"10px 20px"`, y 0 when missing) or `{ "x", "y" }`.

Colors, here and everywhere in a project, may be CSS hex (`"#1e2230"`, `"#fff"`, `"#000000c0"`) or the `{ red, green, blue, alpha }` object. Where a paint is read (`background`, a border's `paint`, a shape's fill), a color is a solid paint of it: `"background": { "mode": "constant", "value": "#1e2230" }`. A gradient stays tagged. The engine writes colors back as objects and paints tagged.

Boxes are laid out in render pixels and rounded to whole pixels, so text textures are drawn 1:1. A value animated slowly therefore moves in whole-pixel steps, as in browsers: animate `translate` or `scale` for smooth motion.

## Names and values

Field names are CSS property names in snake_case, and keyword values CSS keywords in snake_case: `"flex_direction": "column"`, `"justify_content": "space_between"`, `"grid_auto_flow": "row_dense"`. The CSS value strings of track lists and grid lines are their own grammar and keep CSS spelling: `"repeat(auto-fill, minmax(160px, 1fr))"`, `"min-content"`. A misspelling says which was meant: `unknown track "min_content" (write min-content)`, `unknown track "1 fr" (write 1fr)`.

## Text

A box's `text` is its content: one paragraph, wrapped at the box's content width, drawn in the style the box sets or inherits. `\n` starts a line. Lines are aligned in the content width; there is no vertical alignment inside a text box, so align it from its parent. In a row a text box is as wide as its text, up to the room it has; in a column it takes the column's width.

The text style is inherited, as CSS inherits it: `font_family`, `font_size`, `color`, `line_height`, `text_align` and `outline` may be set on any box and apply to every text in it that does not set its own. The root's are the initial values: the host's first font, `0.05`, white, `1.2`, `start` (as in CSS) and no outline. An animated `font_size` on a container animates every text that inherits it. A `font_size` is at most the picture's shorter side; in pixels, it is held to that on the project canvas.

```json
{
  "font_family": "Inter",
  "color": "#f5f5f0",
  "font_size": { "mode": "constant", "value": "32px" },
  "children": [
    { "text": { "mode": "constant", "value": "Revenue" } },
    { "text": { "mode": "constant", "value": "Margin" }, "color": "#ffb020" }
  ]
}
```

A text may be empty (it then takes no room) and may change over time, but its keyframes must all `hold`: strings do not interpolate. Text is never shrunk to fit.

## Grid

`"display": "grid"` lays a box's children out in a CSS grid.

- `grid_template_columns` and `grid_template_rows` are track lists as CSS writes them: lengths (a bare number is a fraction), percents, `<n>fr`, `auto`, `min-content`, `max-content`, `minmax(min, max)`, `fit-content(length)`, and `repeat(count, tracks)` with a count of 1 to 64, `auto-fill` or `auto-fit`. At most 64 tracks on an axis, counts expanded. `auto-fill` and `auto-fit` repeat tracks of a fixed size only, as CSS requires, once per list, beside fixed-size tracks.
- `grid_auto_columns` and `grid_auto_rows` size the implicit tracks (no `repeat()`); `grid_auto_flow` (`row`, `column`, `row_dense`, `column_dense`) places the items without a placement.
- An item's `grid_column` and `grid_row` are lines as CSS writes them: `2`, `"1 / 3"`, `"span 2"`, `"2 / span 3"`, `"1 / -1"`, `"auto"`. Lines count from 1, or from -1 at the end, up to 65 either way; spans are 1 to 64.
- `justify_items` and `align_items` place the items in their areas; `justify_self` and `align_self` place one. `justify_content` and `align_content` place the tracks in the grid.

Named lines and `grid_template_areas` are not supported. A grid cell's text is measured and wraps as in a flex box, and overflow is reported the same way.

```json
{
  "type": "layout",
  "root": {
    "display": "grid",
    "grid_template_columns": "1fr 1fr",
    "gap": { "mode": "constant", "value": "24px" },
    "padding": { "mode": "constant", "value": "48px 64px" },
    "font_family": "Inter",
    "color": "#f5f5f0",
    "children": [
      { "text": { "mode": "constant", "value": "Revenue" }, "font_size": { "mode": "constant", "value": "32px" } },
      { "text": { "mode": "constant", "value": "Margin" }, "font_size": { "mode": "constant", "value": "32px" }, "color": "#ffb020" },
      {
        "grid_column": "1 / -1",
        "height": { "mode": "constant", "value": "50%" },
        "background": { "mode": "constant", "value": "#1e2230" },
        "border_radius": { "mode": "constant", "value": "12px" }
      }
    ]
  }
}
```

## Shapes

A shape box draws a shapes content tree (the `items` a shapes generator takes) on a picture the size of its content box: positions normalized to that box, lengths fractions of its shorter side, like an SVG viewBox. It has no size of its own: give it `width` and `height`, an `aspect_ratio`, or `flex_grow`.

Paints of a box's background and border are normalized to its border box: a linear gradient from `x` 0 to 1 spans the box, and a radial radius is a fraction of the box's shorter side.

## Animation

Numeric values are keyframed in the picture's time, as shapes are: a clip's source time, a generator node's composition time, a media node's composition time plus its `start`. Keywords, track lists, grid lines and booleans do not animate. A layout without keyframes is laid out once and every later frame reuses it.

## Measuring

`measureLayout` (from `alpha2/layout`, on `alpha2/wasm`'s `WasmTextMeasurer`) lays a layout out with the given fonts, at the given picture size and time, as a frame lays it out, and answers each node's border and content box as fractions of the picture, its text's line count, and whether it overflows: its text is taller than its box or splits a word, or an in-flow child reaches past its padding box. Rendering draws overflowing boxes anyway; measure at the size the picture is drawn at, and fix the document. Its `"<n>px"` lengths are pixels of the project canvas: pass the project's size as `canvas` when measuring at another size.

```ts
import init, { WasmTextMeasurer } from "alpha2/wasm";
import { measureLayout } from "alpha2/layout";

await init();
const measurer = new WasmTextMeasurer();
measurer.addFont("Inter", interBytes, 0);
const picture = { width: 1920, height: 1080 };
const { nodes } = measureLayout(measurer, generator, picture, 0, picture);
const overflowing = nodes.filter((node) => node.overflow).map((node) => node.at);
```

Natively, `alpha2_engine::measure_layout` and `TextTextures::measure_layout` answer the same, to the bit. In an editor, `@alpha2-sh/editor`'s `BrowserBridge.measureLayout(generator, time)` measures with the fonts and canvas preview draws the draft with.

## Building

`alpha2/layout` also writes the document from CSS-like props: a value that is not animated becomes a constant, `[top, right, bottom, left]` the sides object, `[x, y]` a vector, and a color is a solid paint. Lengths and CSS value strings are written as the document writes them (`gap: "24px"`, `width: "50%"`, `grid_template_columns: "1fr 1fr"`), and one the engine cannot read throws its message. Its output is a plain document that `validateProject` accepts.

```ts
import { box, layout, text } from "alpha2/layout";

const dashboard = layout(
  box(
    { display: "grid", grid_template_columns: "1fr 1fr", gap: "24px", padding: "48px 64px", font_family: "Inter", color: "#f5f5f0" },
    text("Revenue", { font_size: "32px" }),
    text("Margin", { font_size: "32px", color: "#ffb020" }),
    box({ grid_column: "1 / -1", height: "50%", background: "#1e2230", border_radius: "12px" }),
  ),
);
```

## Examples

These are the documents the engine's golden images (`tests/golden/layout_*.png`) are drawn from, on a 320 x 180 canvas.

### Title card

A gradient backdrop with a bordered, rounded card in its middle: a radial-gradient icon beside a title, over a centred subtitle. The card's text color is set once, on the card.

```ts
import { box, layout, shape, text } from "alpha2/layout";

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
        color: { red: 0.1, green: 0.1, blue: 0.15, alpha: 1 },
      },
      box({ flex_direction: "row", align_items: "center", gap: 0.03 }, icon, text("Title", { font_size: 0.14 })),
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
    "padding": { "mode": "constant", "value": "0.06 0.06 0.08" },
    "children": [
      {
        "flex_direction": "row",
        "gap": { "mode": "constant", "value": 0.016 },
        "padding": { "mode": "constant", "value": "0.022 0.03 0.022 0.022" },
        "background": { "mode": "constant", "value": { "red": 0.06, "green": 0.07, "blue": 0.09, "alpha": 0.92 } },
        "border_radius": { "mode": "constant", "value": 0.012 },
        "max_width": { "mode": "constant", "value": "60%" },
        "opacity": { "mode": "keyframes", "keyframes": [
          { "time": 0, "value": 0, "interpolation": { "type": "cubic_bezier", "x1": 0.2, "y1": 0.0, "x2": 0.0, "y2": 1.0 } },
          { "time": 36000, "value": 1, "interpolation": { "type": "hold" } }
        ] },
        "translate": { "mode": "keyframes", "keyframes": [
          { "time": 0, "value": "0 0.03", "interpolation": { "type": "cubic_bezier", "x1": 0.2, "y1": 0.0, "x2": 0.0, "y2": 1.0 } },
          { "time": 36000, "value": "0 0", "interpolation": { "type": "hold" } }
        ] },
        "children": [
          {
            "width": { "mode": "constant", "value": 0.005 },
            "flex_shrink": { "mode": "constant", "value": 0 },
            "background": { "mode": "constant", "value": { "red": 1.0, "green": 0.62, "blue": 0.1, "alpha": 1.0 } },
            "border_radius": { "mode": "constant", "value": 0.0025 }
          },
          {
            "flex_direction": "column",
            "gap": { "mode": "constant", "value": 0.006 },
            "children": [
              { "text": { "mode": "constant", "value": "Ada Lovelace" }, "font_size": { "mode": "constant", "value": 0.07 } },
              {
                "text": { "mode": "constant", "value": "Wrote the first published algorithm for a machine, 1843" },
                "font_size": { "mode": "constant", "value": 0.045 },
                "color": { "red": 0.8, "green": 0.82, "blue": 0.86, "alpha": 1.0 }
              }
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
        font_size: 0.06, color: "#000000", text_align: "end", line_height: 1.1,
      }),
    ),
    box(
      { width: 0.4, height: 0.4, overflow: "hidden", border_radius: 0.03,
        background: { red: 0.1, green: 0.3, blue: 0.8, alpha: 1 },
        border: { width: 0.01, paint: "#ffffff" } },
      box({ width: 0.6, height: 0.6, flex_shrink: 0, background: { red: 0.9, green: 0.1, blue: 0.1, alpha: 1 }, margin: { left: 0.1, top: 0.1 } }),
    ),
    box(
      { position: "absolute", inset: { right: 0.05, bottom: 0.05 }, padding: "0.01 0.03",
        background: { red: 0.1, green: 0.6, blue: 0.3, alpha: 1 }, border_radius: 0.5, rotation_degrees: -8 },
      text("NEW", { font_size: 0.06 }),
    ),
  ),
);
```

### Grid dashboard

Two text cards, a bar column spanning both rows, and a bar chart that is itself a grid, its bars aligned to its bottom; written in canvas pixels.

```json
{
  "type": "layout",
  "root": {
    "display": "grid", "grid_template_columns": "1fr 2fr 60px", "grid_template_rows": "minmax(40px, auto) 1fr",
    "gap": { "mode": "constant", "value": "8px" }, "padding": { "mode": "constant", "value": "12px 16px" },
    "background": { "mode": "constant", "value": "#101418" },
    "font_size": { "mode": "constant", "value": "14px" }, "color": "#f5f5f0", "line_height": 1.1,
    "children": [
      { "text": { "mode": "constant", "value": "Revenue 4.2M" }, "background": { "mode": "constant", "value": "#1e2230" },
        "padding": { "mode": "constant", "value": "6px 8px" }, "border_radius": { "mode": "constant", "value": "6px" } },
      { "text": { "mode": "constant", "value": "Margin up four points" }, "background": { "mode": "constant", "value": "#1e2230" },
        "padding": { "mode": "constant", "value": "6px 8px" }, "border_radius": { "mode": "constant", "value": "6px" },
        "color": "#ffb020", "text_align": "end" },
      { "grid_row": "1 / 3", "grid_column": 3, "background": { "mode": "constant", "value": "#2b5c3a" },
        "border_radius": { "mode": "constant", "value": "6px" } },
      { "grid_column": "1 / span 2", "background": { "mode": "constant", "value": "#1e2230" },
        "border_radius": { "mode": "constant", "value": "6px" },
        "display": "grid", "grid_template_columns": "repeat(4, 1fr)", "align_items": "end",
        "padding": { "mode": "constant", "value": "6px 8px" }, "column_gap": { "mode": "constant", "value": "6px" },
        "children": [
          { "height": { "mode": "constant", "value": "30%" }, "background": { "mode": "constant", "value": "#4c8dff" } },
          { "height": { "mode": "constant", "value": "60%" }, "background": { "mode": "constant", "value": "#4c8dff" } },
          { "height": { "mode": "constant", "value": "45%" }, "background": { "mode": "constant", "value": "#4c8dff" } },
          { "height": { "mode": "constant", "value": "80%" }, "justify_self": "center", "width": { "mode": "constant", "value": "50%" },
            "background": { "mode": "constant", "value": "#ff5c7a" } }
        ] }
    ]
  }
}
```

## Not supported

Block and inline layout; named grid lines and `grid_template_areas`; subgrid; baseline alignment; `auto` margins; `em` and `rem`; color names and color functions; animated text color; rich-text spans; per-side border widths and per-corner radii; box shadows; ellipsis and line clamping; right-to-left flex direction; percent padding and margin; and image or video boxes. Bidirectional text inside a paragraph is shaped as text generators shape it.
