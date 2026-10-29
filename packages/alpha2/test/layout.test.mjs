// The layout builder's shorthands and the measuring wrapper. Measuring itself is the engine's,
// checked against the native engine by `npm run test:wasm-parity`.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import { box, layout, measureLayout, shape, text } from "../dist/layout.js";
import { validateProject } from "../dist/project.js";

const constant = (value) => ({ mode: "constant", value });
const red = { red: 1, green: 0, blue: 0, alpha: 1 };

/** A 1920x1080 project showing `generator` as its one source. */
function projectOf(generator) {
  return {
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
}

test("props become the document, CSS value strings as written", () => {
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
      gap: "10px",
      padding: [0.1, "20px", 0.3, 0.4],
      margin: undefined,
      width: "50%",
      max_height: 0.5,
      background: "#ff0000",
      border: { width: "2px", paint: red },
      inset: { left: "10%" },
      translate: [0, "30px"],
      scale: 1.5,
      font_family: "Inter",
      color: "#f5f5f0",
      opacity: {
        mode: "keyframes",
        keyframes: [
          { time: 0, value: 0, interpolation: { type: "linear" } },
          { time: 6000, value: 1, interpolation: { type: "hold" } },
        ],
      },
    },
    text("Title", { font_size: "54px", color: red, padding: "8px 16px" }),
    shape(ellipse, { width: 0.1, aspect_ratio: 1 }),
    box(),
  );
  assert.deepEqual(root, {
    flex_direction: "column",
    gap: constant("10px"),
    padding: constant({ top: 0.1, right: "20px", bottom: 0.3, left: 0.4 }),
    width: constant("50%"),
    max_height: constant(0.5),
    background: constant("#ff0000"),
    border: { width: constant("2px"), paint: constant(red) },
    inset: { left: constant("10%") },
    translate: constant({ x: 0, y: "30px" }),
    scale: constant({ x: 1.5, y: 1.5 }),
    font_family: "Inter",
    color: "#f5f5f0",
    opacity: root.opacity,
    children: [
      {
        font_size: constant("54px"),
        color: red,
        padding: constant("8px 16px"),
        text: constant("Title"),
      },
      { width: constant(0.1), aspect_ratio: 1, shape: { items: ellipse } },
      {},
    ],
  });
  assert.equal(root.opacity.mode, "keyframes");
  // The root is the picture: the box sits in one.
  assert.deepEqual(validateProject(projectOf(layout(box({}, root)))), []);
});

test("the builder writes the shared cases' documents, as the Python builder does", async () => {
  // tests/cases/layout-builder-cases.json is asserted by python/alpha2/tests/test_layout.py too.
  const { cases } = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/layout-builder-cases.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const build = (call) => {
    switch (call.builder) {
      case "box":
        return box(call.props, ...(call.children ?? []).map(build));
      case "text":
        return text(call.content, call.props);
      case "shape":
        return shape(call.items, call.props);
      default:
        throw new Error(`unknown builder ${call.builder}`);
    }
  };
  for (const { name, root, document } of cases) {
    const generator = layout(build(root));
    assert.deepEqual(generator, document, name);
    assert.deepEqual(validateProject(projectOf(generator)), [], name);
  }
});

test("a CSS value string the engine cannot read throws its message", () => {
  assert.throws(
    () => box({ width: "5 px" }),
    /^TypeError: invalid length "5 px" \(write 5px\)/,
  );
  assert.throws(
    () => box({ padding: "1px 2px 3px 4px 5px" }),
    /expected 1 to 4 lengths, got 5/,
  );
  assert.throws(() => box({ gap: "5%" }), /percent is not allowed here/);
  assert.throws(
    () => box({ display: "grid", grid_template_columns: "1 fr" }),
    /unknown track "1 fr" \(write 1fr\)/,
  );
  assert.throws(
    () => box({ grid_column: "header" }),
    /named grid lines are not supported/,
  );
  assert.throws(() => box({ background: "#ggg" }), /invalid color "#ggg"/);
});

test("a built grid with inherited style is a valid project document", () => {
  const generator = layout(
    box(
      {
        display: "grid",
        grid_template_columns: "1fr 1fr",
        gap: "24px",
        padding: "48px 64px",
        font_family: "Inter",
        color: "#f5f5f0",
      },
      text("Revenue", { font_size: "32px" }),
      text("Margin", { font_size: "32px", color: "#ffb020" }),
      box({
        background: "#1e2230",
        border_radius: "12px",
        height: "50%",
        grid_column: "1 / -1",
      }),
    ),
  );
  const project = projectOf(generator);
  assert.deepEqual(validateProject(project), []);
  // A field that does not apply is refused naming it and why.
  const flex = structuredClone(project);
  delete flex.sources[0].generator.root.display;
  assert.deepEqual(validateProject(flex), [
    "source g has an invalid layout: root sets grid_template_columns, a grid container field, without display grid",
  ]);
  // A field the model does not take is refused by name.
  const areas = structuredClone(project);
  areas.sources[0].generator.root.grid_template_areas = "a b";
  assert.deepEqual(validateProject(areas), [
    'project.sources[0].generator.root has unknown field "grid_template_areas"',
  ]);
  // A pixel font size past the canvas's shorter side is too large on this canvas.
  const huge = structuredClone(project);
  huge.sources[0].generator.root.font_size = constant("1200px");
  assert.deepEqual(validateProject(huge), [
    "source g has an invalid layout: root.font_size has a value out of range",
  ]);
  huge.settings.height = 1440;
  assert.deepEqual(validateProject(huge), []);
});

test("measureLayout sends a layout generator and its canvas and reads the engine's bounds", () => {
  const calls = [];
  const answer = {
    nodes: [
      {
        at: "root",
        border: { left: 0, top: 0, right: 1, bottom: 1 },
        content: { left: 0, top: 0, right: 1, bottom: 1 },
        overflow: false,
        lineCount: null,
        baselines: null,
      },
    ],
  };
  const binding = {
    measureLayout(json, ...rest) {
      calls.push([JSON.parse(json), ...rest]);
      return JSON.stringify(answer);
    },
  };
  const root = box({ padding: 0.1 });
  const picture = { width: 1280, height: 720 };
  assert.deepEqual(measureLayout(binding, root, picture), answer);
  assert.deepEqual(measureLayout(binding, layout(root), picture, 600), answer);
  assert.deepEqual(
    measureLayout(binding, root, { width: 960, height: 540 }, 0, {
      width: 3840,
      height: 2160,
    }),
    answer,
  );
  assert.deepEqual(calls, [
    [{ type: "layout", root }, 1280, 720, 0, 1280, 720],
    [{ type: "layout", root }, 1280, 720, 600, 1280, 720],
    [{ type: "layout", root }, 960, 540, 0, 3840, 2160],
  ]);
});
