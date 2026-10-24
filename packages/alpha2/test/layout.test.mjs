// The layout builder's shorthands and the measuring wrapper. Measuring itself is the engine's,
// checked against the native engine by `npm run test:wasm-parity`.
import assert from "node:assert/strict";
import test from "node:test";

import { box, layout, measureLayout, px, shape, text } from "../dist/layout.js";
import { validateProject } from "../dist/project.js";

const constant = (value) => ({ mode: "constant", value });
const red = { red: 1, green: 0, blue: 0, alpha: 1 };

test("shorthands become the explicit document", () => {
  const ellipse = [
    {
      type: "ellipse",
      center: constant({ x: 0.5, y: 0.5 }),
      size: constant({ x: 1, y: 1 }),
    },
    { type: "fill", paint: constant({ type: "solid", color: red }) },
  ];
  const root = box(
    {
      flex_direction: "column",
      gap: 0.01,
      padding: [0.1, 0.2, 0.3, 0.4],
      margin: undefined,
      width: "50%",
      max_height: 0.5,
      background: red,
      border: { width: 0.002, paint: red },
      inset: { left: "10%" },
      translate: [0, 0.03],
      scale: 1.5,
      opacity: {
        mode: "keyframes",
        keyframes: [
          { time: 0, value: 0, interpolation: { type: "linear" } },
          { time: 6000, value: 1, interpolation: { type: "hold" } },
        ],
      },
    },
    text("Title", { font_size: px(54), color: red, padding: 0.01 }),
    shape(ellipse, { width: 0.1, aspect_ratio: 1 }),
    box(),
  );
  assert.deepEqual(root, {
    flex_direction: "column",
    gap: constant(0.01),
    padding: constant({ top: 0.1, right: 0.2, bottom: 0.3, left: 0.4 }),
    width: constant({ type: "percent", value: 50 }),
    max_height: constant({ type: "length", value: 0.5 }),
    background: constant({ type: "solid", color: red }),
    border: {
      width: constant(0.002),
      paint: constant({ type: "solid", color: red }),
    },
    inset: { left: constant({ type: "percent", value: 10 }) },
    translate: constant({ x: 0, y: 0.03 }),
    scale: constant({ x: 1.5, y: 1.5 }),
    opacity: root.opacity,
    children: [
      {
        padding: constant({ top: 0.01, right: 0.01, bottom: 0.01, left: 0.01 }),
        text: {
          content: constant("Title"),
          font_size: constant(0.05),
          color: red,
        },
      },
      {
        width: constant({ type: "length", value: 0.1 }),
        aspect_ratio: 1,
        shape: { items: ellipse },
      },
      {},
    ],
  });
  assert.equal(root.opacity.mode, "keyframes");
  assert.equal(px(27, 540), 0.05);
  assert.throws(() => box({ width: "5" }), /a length is a number or "N%"/);
});

test("a built lower third is a valid project document", () => {
  const card = box(
    {
      flex_direction: "row",
      gap: px(17),
      padding: [px(24), px(32), px(24), px(24)],
      background: { red: 0.06, green: 0.07, blue: 0.09, alpha: 0.92 },
      border_radius: px(13),
      max_width: "60%",
      translate: [0, 0.03],
    },
    box({ width: px(5), flex_shrink: 0, background: red }),
    box(
      { flex_direction: "column", gap: px(6) },
      text("Ada Lovelace", { font_size: px(45) }),
      text("Wrote the first published algorithm for a machine, 1843", {
        font_size: px(28),
      }),
    ),
  );
  const generator = layout(
    box(
      {
        flex_direction: "column",
        justify_content: "end",
        align_items: "start",
        padding: [0.06, 0.06, 0.08, 0.06],
      },
      card,
    ),
  );
  const project = {
    schema_version: 1,
    id: "p",
    name: "P",
    settings: {
      width: 1920,
      height: 1080,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [{ id: "g", kind: "generated", generator }],
    timeline: {
      tracks: [
        {
          id: "t",
          name: "T",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "g",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
            },
          ],
        },
      ],
    },
  };
  assert.deepEqual(validateProject(project), []);
  // A field the model does not take is refused by name, as an agent writing CSS needs.
  const grid = structuredClone(project);
  grid.sources[0].generator.root.display = "grid";
  assert.deepEqual(validateProject(grid), [
    'project.sources[0].generator.root has unknown field "display"',
  ]);
});

test("measureLayout sends a layout generator and reads the engine's bounds", () => {
  const calls = [];
  const answer = {
    nodes: [
      {
        at: "root",
        border: { left: 0, top: 0, right: 1, bottom: 1 },
        content: { left: 0, top: 0, right: 1, bottom: 1 },
        overflow: false,
        lineCount: null,
      },
    ],
  };
  const binding = {
    measureLayout(json, width, height, time) {
      calls.push([JSON.parse(json), width, height, time]);
      return JSON.stringify(answer);
    },
  };
  const root = box({ padding: 0.1 });
  const picture = { width: 1280, height: 720 };
  assert.deepEqual(measureLayout(binding, root, picture), answer);
  assert.deepEqual(measureLayout(binding, layout(root), picture, 600), answer);
  assert.deepEqual(calls, [
    [{ type: "layout", root }, 1280, 720, 0],
    [{ type: "layout", root }, 1280, 720, 600],
  ]);
});
