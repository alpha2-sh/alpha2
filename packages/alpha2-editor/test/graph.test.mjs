import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  createDocument,
  createEditor,
  graphIssues,
  graphLayout,
  effectStack,
} from "../dist/index.js";

const document = () =>
  createDocument({
    schema_version: 1,
    id: "p",
    name: "Graph",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "s", kind: "video", locator: "test://s" },
      { id: "i", kind: "image", locator: "test://i.png" },
    ],
    timeline: {
      tracks: [
        {
          id: "t",
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "s",
              timeline_range: { start: 0, end: 120000 },
              source_range: { start: 0, end: 120000 },
            },
          ],
        },
      ],
    },
  });
const blur = { id: "alpha2.effect.gaussian-blur", version: 1 };
const nodes = (e) =>
  Object.fromEntries(
    e.document.project.compositions[0].nodes.map((n) => [n.id, n]),
  );

test("a clip gets a composition, then a graph built node by node", () => {
  const e = createEditor({ document: document() });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({ type: "graph.create", clipId: "c" });
  const id = e.query().clips.get("c").clip.composition.id;
  assert.equal(id, "c-fx");
  assert.deepEqual(
    graphIssues(e.document.project.compositions[0], "clip", e.document.project),
    [],
  );
  for (const name of ["soft", "sharp"])
    ok({
      type: "graph.node.add",
      compositionId: id,
      node: { id: name, op: { type: "effect", input: null, effect: blur } },
      position: { x: 10, y: 20 },
    });
  ok({
    type: "graph.connect",
    compositionId: id,
    from: "clip",
    to: "soft",
    input: "input",
  });
  ok({
    type: "graph.connect",
    compositionId: id,
    from: "clip",
    to: "sharp",
    input: "input",
  });
  ok({
    type: "graph.node.add",
    compositionId: id,
    node: { id: "mix", op: { type: "merge", layers: [] } },
  });
  ok({
    type: "graph.connect",
    compositionId: id,
    from: "soft",
    to: "mix",
    input: "layer",
  });
  ok({
    type: "graph.connect",
    compositionId: id,
    from: "sharp",
    to: "mix",
    input: "layer",
  });
  ok({ type: "graph.output", compositionId: id, nodeId: "mix" });
  assert.deepEqual(
    nodes(e).mix.op.layers.map((l) => l.input),
    ["soft", "sharp"],
  );
  assert.deepEqual(
    graphIssues(e.document.project.compositions[0], "clip", e.document.project),
    [],
  );
  assert.equal(
    effectStack(e.document.project, e.query().clips.get("c").clip),
    undefined,
  );
  assert.deepEqual(graphLayout(e.document)[id].soft, { x: 10, y: 20 });

  const before = e.document;
  const cycle = e.dispatch({
    type: "graph.connect",
    compositionId: id,
    from: "mix",
    to: "soft",
    input: "input",
  });
  assert.equal(cycle.ok, false);
  assert.match(cycle.error.message, /cycle/);
  assert.equal(e.document, before);

  // Disconnecting a merge layer removes its slot; the graph then misses nothing it needs.
  ok({ type: "graph.disconnect", compositionId: id, to: "mix", input: 0 });
  assert.deepEqual(
    nodes(e).mix.op.layers.map((l) => l.input),
    ["sharp"],
  );
  // An unconnected input the output needs is an issue, not an error.
  ok({
    type: "graph.disconnect",
    compositionId: id,
    to: "sharp",
    input: "input",
  });
  assert.deepEqual(
    graphIssues(e.document.project.compositions[0], "clip", e.document.project),
    [{ kind: "missing_input", node: "sharp", input: "input" }],
  );
  // Bypassed, only its pass-through input matters: still missing here.
  ok({
    type: "graph.node.update",
    compositionId: id,
    node: { ...nodes(e).sharp, bypass: true },
  });
  assert.equal(
    graphIssues(e.document.project.compositions[0], "clip", e.document.project)
      .length,
    1,
  );

  ok({ type: "graph.node.remove", compositionId: id, ids: ["mix"] });
  assert.equal(e.document.project.compositions[0].output, null);
  assert.deepEqual(
    graphIssues(e.document.project.compositions[0], "clip", e.document.project),
    [{ kind: "no_output" }],
  );
  ok({
    type: "graph.connect",
    compositionId: id,
    from: "clip",
    to: "soft",
    input: "input",
  });
  ok({ type: "graph.node.remove", compositionId: id, ids: ["clip"] });
  assert.equal(nodes(e).soft.op.input, null);

  const project = e.document.project;
  ok({
    type: "graph.layout",
    compositionId: id,
    positions: { soft: { x: 300, y: 40 } },
  });
  assert.equal(e.document.project, project);
  assert.deepEqual(graphLayout(e.document)[id].soft, { x: 300, y: 40 });
  e.undo();
  assert.deepEqual(graphLayout(e.document)[id].soft, { x: 10, y: 20 });
});

test("refusals name what is wrong", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "graph.create", clipId: "c" });
  const refuse = (command, pattern) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, false);
    assert.match(result.error.message, pattern);
  };
  refuse({ type: "graph.create", clipId: "c" }, /already has a composition/);
  refuse(
    {
      type: "graph.connect",
      compositionId: "c-fx",
      from: "clip",
      to: "clip",
      input: "input",
    },
    /own picture/,
  );
  refuse(
    {
      type: "graph.node.add",
      compositionId: "c-fx",
      node: { id: "clip", op: { type: "clip" } },
    },
    /Duplicate node/,
  );
  refuse(
    {
      type: "graph.node.add",
      compositionId: "c-fx",
      node: { id: "m", op: { type: "media", source_id: "nowhere" } },
    },
    /cannot show source/,
  );
  refuse(
    { type: "graph.output", compositionId: "c-fx", nodeId: "gone" },
    /Unknown node/,
  );
  refuse(
    { type: "graph.output", compositionId: "nope", nodeId: null },
    /Unknown composition/,
  );
  e.dispatch({ type: "track.metadata", id: "t", patch: { locked: true } });
  const locked = e.dispatch({
    type: "graph.output",
    compositionId: "c-fx",
    nodeId: null,
  });
  assert.equal(locked.error.code, "locked");
});

test("nodes fold into a group and back without changing the graph's meaning", () => {
  const e = createEditor({ document: document() });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
    return result;
  };
  ok({ type: "graph.create", clipId: "c" });
  const id = "c-fx";
  // clip -> soft -> sharp -> merge(sharp, clip)
  const effect = (node, input, radius) => ({
    type: "graph.node.add",
    compositionId: id,
    node: {
      id: node,
      op: {
        type: "effect",
        input,
        effect: {
          ...blur,
          params: {
            radius: {
              mode: "constant",
              value: { type: "float", value: radius },
            },
          },
        },
      },
    },
    position: { x: radius * 100, y: 0 },
  });
  ok(effect("soft", "clip", 1));
  ok(effect("sharp", "soft", 2));
  ok({
    type: "graph.node.add",
    compositionId: id,
    node: {
      id: "mix",
      op: { type: "merge", layers: [{ input: "sharp" }, { input: "clip" }] },
    },
  });
  ok({ type: "graph.output", compositionId: id, nodeId: "mix" });
  const before = e.document;
  // A selection read from two places has no single output.
  const refused = e.dispatch({
    type: "graph.group",
    compositionId: id,
    ids: ["clip", "soft"],
  });
  assert.equal(refused.ok, false);
  ok({
    type: "graph.group",
    compositionId: id,
    ids: ["soft", "sharp"],
    groupId: "look",
    definitionId: "look-def",
  });
  const compositions = Object.fromEntries(
    e.document.project.compositions.map((c) => [c.id, c]),
  );
  const parent = Object.fromEntries(
    compositions[id].nodes.map((n) => [n.id, n]),
  );
  assert.deepEqual(parent.look.op, {
    type: "group",
    composition: "look-def",
    inputs: ["clip"],
  });
  assert.deepEqual(
    parent.mix.op.layers.map((l) => l.input),
    ["look", "clip"],
  );
  const definition = compositions["look-def"];
  assert.deepEqual(definition.inputs, ["Input 1"]);
  assert.equal(definition.output, "sharp");
  assert.deepEqual(
    definition.nodes.map((n) => [n.id, n.op.type, n.op.input ?? n.op.index]),
    [
      ["input-1", "group_input", 0],
      ["soft", "effect", "input-1"],
      ["sharp", "effect", "soft"],
    ],
  );
  assert.ok(graphLayout(e.document)["look-def"]["input-1"]);
  // Exposed, the first radius is set per group node; ungrouping keeps the set value.
  ok({
    type: "graph.control.add",
    compositionId: "look-def",
    control: { name: "softness", node: "soft", param: "radius" },
  });
  ok({
    type: "graph.node.update",
    compositionId: id,
    node: {
      ...parent.look,
      op: {
        ...parent.look.op,
        controls: {
          softness: { mode: "constant", value: { type: "float", value: 7 } },
        },
      },
    },
  });
  ok({ type: "graph.ungroup", compositionId: id, nodeId: "look" });
  const flat = Object.fromEntries(
    e.document.project.compositions
      .find((c) => c.id === id)
      .nodes.map((n) => [n.id, n]),
  );
  assert.equal(flat["look-soft"].op.input, "clip");
  assert.equal(flat["look-sharp"].op.input, "look-soft");
  assert.equal(
    flat["look-soft"].op.effect.params.radius.value.value,
    7,
    "the group node's control value",
  );
  assert.deepEqual(
    flat.mix.op.layers.map((l) => l.input),
    ["look-sharp", "clip"],
  );
  assert.equal(
    e.document.project.compositions.some((c) => c.id === "look-def"),
    false,
  );
  for (let i = 0; i < 4; i++) e.undo();
  assert.equal(e.document, before);
});

test("a group input added to a definition appears on every group node, and a removed control leaves them", () => {
  const e = createEditor({ document: document() });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({ type: "graph.create", clipId: "c" });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: { id: "fx", op: { type: "effect", input: "clip", effect: blur } },
  });
  ok({ type: "graph.output", compositionId: "c-fx", nodeId: "fx" });
  ok({ type: "graph.group", compositionId: "c-fx", ids: ["fx"] });
  const group = () =>
    e.document.project.compositions
      .find((c) => c.id === "c-fx")
      .nodes.find((n) => n.op.type === "group");
  const definition = group().op.composition;
  ok({ type: "graph.port.add", compositionId: definition, name: "Matte" });
  assert.deepEqual(group().op.inputs, ["clip", null]);
  ok({
    type: "graph.control.add",
    compositionId: definition,
    control: { name: "amount", node: "fx", param: "radius" },
  });
  ok({
    type: "graph.node.update",
    compositionId: "c-fx",
    node: {
      ...group(),
      op: {
        ...group().op,
        controls: {
          amount: { mode: "constant", value: { type: "float", value: 3 } },
        },
      },
    },
  });
  ok({
    type: "graph.control.remove",
    compositionId: definition,
    name: "amount",
  });
  assert.deepEqual(group().op.controls, {});
});

test("a group input or control edit leaves compositions without that group as they were, so a locked track elsewhere allows it", () => {
  // Only edited compositions are rebuilt, or a locked clip would look changed and refuse the edit.
  const e = createEditor({ document: document() });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({ type: "graph.create", clipId: "c" });
  ok({
    type: "composition.create",
    compositionId: "lib",
    sourceId: "lib-source",
    name: "Lib",
  });
  ok({
    type: "graph.node.add",
    compositionId: "lib",
    node: { id: "fx", op: { type: "effect", input: null, effect: blur } },
  });
  ok({ type: "graph.output", compositionId: "lib", nodeId: "fx" });
  ok({ type: "graph.group", compositionId: "lib", ids: ["fx"] });
  const definition = e.document.project.compositions
    .find((c) => c.id === "lib")
    .nodes.find((n) => n.op.type === "group").op.composition;
  ok({
    type: "graph.control.add",
    compositionId: definition,
    control: { name: "amount", node: "fx", param: "radius" },
  });
  ok({ type: "track.metadata", id: "t", patch: { locked: true } });
  const locked = e.document.project.compositions.find((c) => c.id === "c-fx");
  ok({ type: "graph.port.add", compositionId: definition, name: "Matte" });
  ok({
    type: "graph.control.remove",
    compositionId: definition,
    name: "amount",
  });
  assert.equal(
    e.document.project.compositions.find((c) => c.id === "c-fx"),
    locked,
  );
});

test("a standalone composition is a Library item clips place like media", () => {
  const e = createEditor({ document: document() });
  const result = e.dispatch({
    type: "composition.create",
    compositionId: "title",
    sourceId: "title-source",
    name: "Title card",
  });
  assert.equal(result.ok, true, result.error?.message);
  assert.deepEqual(e.document.assets["title-source"], {
    id: "title-source",
    kind: "composition",
    name: "Title card",
    contentRevision: 0,
  });
  assert.deepEqual(
    e.document.project.sources.find((s) => s.id === "title-source"),
    { id: "title-source", kind: "composition", composition: "title" },
  );
  const placed = e.dispatch({
    type: "clip.insert",
    trackId: "t",
    clip: {
      id: "card",
      source_id: "title-source",
      timeline_range: { start: 120000, end: 240000 },
      source_range: { start: 0, end: 120000 },
    },
  });
  assert.equal(placed.ok, true, placed.error?.message);
  // Deleting its clip keeps the composition: its source still shows it.
  assert.equal(e.dispatch({ type: "clip.delete", id: "card" }).ok, true);
  assert.ok(e.document.project.compositions.some((c) => c.id === "title"));
});

test("a group saved as a macro applies to another clip in one undoable edit", async () => {
  const { graphPreset, presetShortfall, applyGraphPreset } =
    await import("../dist/index.js");
  const e = createEditor({ document: document() });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({
    type: "clip.insert",
    trackId: "t",
    clip: {
      id: "d",
      source_id: "s",
      timeline_range: { start: 120000, end: 240000 },
      source_range: { start: 0, end: 120000 },
    },
  });
  ok({ type: "graph.create", clipId: "c" });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: { id: "fx", op: { type: "effect", input: "clip", effect: blur } },
  });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: { id: "still", op: { type: "media", source_id: "i" } },
  });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: {
      id: "mix",
      op: { type: "merge", layers: [{ input: "fx" }, { input: "still" }] },
    },
  });
  ok({ type: "graph.output", compositionId: "c-fx", nodeId: "mix" });
  ok({
    type: "graph.group",
    compositionId: "c-fx",
    ids: ["fx", "still", "mix"],
    definitionId: "soft",
    name: "Soft overlay",
  });
  const preset = graphPreset(e.document.project, "soft", "Soft overlay");
  assert.deepEqual(
    preset.compositions.map((c) => c.id),
    ["soft"],
  );
  assert.deepEqual(
    preset.requires.sources.map((s) => s.id),
    ["i"],
  );
  assert.deepEqual(preset.requires.effects, [blur]);
  // Every requirement met here; an engine without the blur, or a project without the image, falls
  // short.
  const runs = () => true;
  assert.deepEqual(presetShortfall(e.document, preset, runs), {
    sources: [],
    resources: [],
    effects: [],
  });
  assert.deepEqual(presetShortfall(e.document, preset, () => false).effects, [
    "alpha2.effect.gaussian-blur@1",
  ]);
  const bare = createEditor({
    document: createDocument({
      ...document().project,
      sources: [{ id: "s", kind: "video", locator: "test://s" }],
    }),
  });
  assert.deepEqual(presetShortfall(bare.document, preset, runs).sources, ["i"]);

  const before = e.document;
  const command = applyGraphPreset(e.document, preset, "d", {
    groupId: "overlay",
  });
  ok(command);
  const d = e.query().clips.get("d").clip;
  const composition = e.document.project.compositions.find(
    (c) => c.id === d.composition.id,
  );
  const group = composition.nodes.find((n) => n.id === "overlay");
  assert.equal(composition.output, "overlay");
  assert.deepEqual(group.op.inputs, ["clip"]);
  assert.notEqual(group.op.composition, "soft", "a copy under a free id");
  assert.ok(
    e.document.project.compositions.some((c) => c.id === group.op.composition),
  );
  e.undo();
  assert.equal(e.document, before);
});

test("graphIssues finds what the engine's planner refuses, for all four reasons", () => {
  // The same corpus drives alpha2-eval's planner test: the engine refuses exactly the cases listing
  // issues, with one of them.
  const corpus = JSON.parse(
    readFileSync(
      new URL(
        "../../../tests/cases/composition-issues-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const kinds = new Set();
  for (const c of corpus.cases) {
    const composition = c.compositions.find((x) => x.id === c.composition);
    const issues = graphIssues(composition, c.use, {
      compositions: c.compositions,
      sources: c.sources,
    });
    assert.deepEqual(issues, c.issues, c.name);
    for (const issue of issues) kinds.add(issue.kind);
  }
  assert.deepEqual([...kinds].sort(), [
    "clip_without_clip",
    "missing_input",
    "no_output",
    "unbound_group_input",
  ]);
});

test("a group's definition opened on its own reads its group node's inputs", () => {
  const definition = {
    id: "soften",
    inputs: ["Image"],
    nodes: [
      { id: "in", op: { type: "group_input", index: 0 } },
      { id: "clip", op: { type: "clip" } },
      {
        id: "mix",
        op: { type: "merge", layers: [{ input: "in" }, { input: "clip" }] },
      },
    ],
    output: "mix",
  };
  // Where the group is drawn decides its clip node; its group input is fed.
  assert.deepEqual(graphIssues(definition, "group"), []);
  assert.deepEqual(graphIssues(definition, "clip"), [
    { kind: "unbound_group_input", node: "in" },
  ]);
  assert.deepEqual(graphIssues(definition, "source"), [
    { kind: "clip_without_clip", node: "clip" },
    { kind: "unbound_group_input", node: "in" },
  ]);
});

/** A document whose clip draws composition `main` among `compositions`. */
const drawing = (compositions) => {
  const base = document();
  const project = structuredClone(base.project);
  project.sources.push({
    id: "gen",
    kind: "generated",
    generator: {
      type: "fill",
      paint: {
        type: "solid",
        color: { red: 1, green: 0, blue: 0, alpha: 1 },
      },
    },
  });
  project.compositions = compositions;
  project.timeline.tracks[0].clips[0].composition = { id: "main" };
  return createDocument(project);
};
const nodesOf = (doc, id) =>
  Object.fromEntries(
    doc.project.compositions
      .find((c) => c.id === id)
      .nodes.map((n) => [n.id, n.op]),
  );

test("a group made inside a definition leaves its group inputs where they are, and draws the same", () => {
  const e = createEditor({
    document: drawing([
      {
        id: "main",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          { id: "gen", op: { type: "media", source_id: "gen" } },
          {
            id: "g",
            op: { type: "group", composition: "D", inputs: ["clip", "gen"] },
          },
        ],
        output: "g",
      },
      {
        id: "D",
        inputs: ["A", "B"],
        nodes: [
          { id: "in1", op: { type: "group_input", index: 0 } },
          { id: "in2", op: { type: "group_input", index: 1 } },
          { id: "fxA", op: { type: "effect", input: "in1", effect: blur } },
          {
            id: "mix",
            op: { type: "merge", layers: [{ input: "fxA" }, { input: "in2" }] },
          },
        ],
        output: "mix",
      },
    ]),
  });
  const result = e.dispatch({
    type: "graph.group",
    compositionId: "D",
    ids: ["in1", "fxA", "mix"],
    groupId: "inner",
    definitionId: "D2",
  });
  assert.equal(result.ok, true, result.error?.message);
  // The group inputs read D's inputs, so they stay in D and feed the new group's ports in order.
  const outer = nodesOf(e.document, "D");
  assert.deepEqual(Object.keys(outer).sort(), ["in1", "in2", "inner"]);
  assert.deepEqual(outer.inner.inputs, ["in1", "in2"]);
  const inner = nodesOf(e.document, "D2");
  assert.deepEqual(inner["input-1"], { type: "group_input", index: 0 });
  assert.deepEqual(inner["input-2"], { type: "group_input", index: 1 });
  assert.equal(inner.fxA.input, "input-1");
  assert.deepEqual(
    inner.mix.layers.map((l) => l.input),
    ["fxA", "input-2"],
  );
  assert.equal(inner.in1, undefined);
  // One group input and what reads it groups too; group inputs alone are refused, saying why.
  e.undo();
  const one = e.dispatch({
    type: "graph.group",
    compositionId: "D",
    ids: ["in1", "fxA"],
  });
  assert.equal(one.ok, true, one.error?.message);
  e.undo();
  const alone = e.canExecute({
    type: "graph.group",
    compositionId: "D",
    ids: ["in1", "in2"],
  });
  assert.equal(alone.ok, false);
  assert.match(alone.error.message, /group input stays where it is/);
});

test("a bypassed group is refused ungrouping, since its nodes would draw", () => {
  const e = createEditor({
    document: drawing([
      {
        id: "main",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          {
            id: "g",
            bypass: true,
            op: { type: "group", composition: "look", inputs: ["clip"] },
          },
        ],
        output: "g",
      },
      {
        id: "look",
        inputs: ["In"],
        nodes: [
          { id: "in", op: { type: "group_input", index: 0 } },
          { id: "fx", op: { type: "effect", input: "in", effect: blur } },
        ],
        output: "fx",
      },
    ]),
  });
  const refused = e.dispatch({
    type: "graph.ungroup",
    compositionId: "main",
    nodeId: "g",
  });
  assert.equal(refused.ok, false);
  assert.equal(refused.error.code, "invalid_command");
  assert.match(refused.error.message, /Turn group g on/);
  const on = e.dispatch({
    type: "graph.node.update",
    compositionId: "main",
    node: {
      id: "g",
      op: { type: "group", composition: "look", inputs: ["clip"] },
    },
  });
  assert.equal(on.ok, true, on.error?.message);
  const ungrouped = e.dispatch({
    type: "graph.ungroup",
    compositionId: "main",
    nodeId: "g",
  });
  assert.equal(ungrouped.ok, true, ungrouped.error?.message);
});

test("removing a node a control exposes removes the control and its values; grouping it says which", () => {
  const softness = { mode: "constant", value: { type: "float", value: 4 } };
  const e = createEditor({
    document: drawing([
      {
        id: "main",
        nodes: [
          { id: "clip", op: { type: "clip" } },
          {
            id: "g",
            op: {
              type: "group",
              composition: "look",
              inputs: ["clip"],
              controls: { softness, sharpness: softness },
            },
          },
        ],
        output: "g",
      },
      {
        id: "look",
        inputs: ["In"],
        controls: [
          { name: "softness", node: "soft", param: "radius" },
          { name: "sharpness", node: "sharp", param: "radius" },
        ],
        nodes: [
          { id: "in", op: { type: "group_input", index: 0 } },
          { id: "soft", op: { type: "effect", input: "in", effect: blur } },
          { id: "sharp", op: { type: "effect", input: "soft", effect: blur } },
        ],
        output: "sharp",
      },
    ]),
  });
  const grouped = e.canExecute({
    type: "graph.group",
    compositionId: "look",
    ids: ["soft"],
  });
  assert.equal(grouped.ok, false);
  assert.match(grouped.error.message, /control softness/);
  const removed = e.dispatch({
    type: "graph.node.remove",
    compositionId: "look",
    ids: ["soft"],
  });
  assert.equal(removed.ok, true, removed.error?.message);
  const look = e.document.project.compositions.find((c) => c.id === "look");
  assert.deepEqual(
    look.controls.map((c) => c.name),
    ["sharpness"],
  );
  assert.deepEqual(Object.keys(nodesOf(e.document, "main").g.controls), [
    "sharpness",
  ]);
});

test("a preset's composition sources get ids apart from the project's and from each other", async () => {
  const { applyGraphPreset } = await import("../dist/index.js");
  const fill = (id) => ({
    id,
    nodes: [{ id: "bg", op: { type: "media", source_id: "gen" } }],
    output: "bg",
  });
  const card = (id, composition) => ({ id, kind: "composition", composition });
  const base = drawing([
    {
      id: "main",
      nodes: [{ id: "clip", op: { type: "clip" } }],
      output: "clip",
    },
    fill("cardc"),
  ]);
  const project = structuredClone(base.project);
  project.sources.push(card("card", "cardc"));
  const e = createEditor({ document: createDocument(project) });
  // The preset reads `card` and `card~2`, an earlier rename's: two compositions.
  const preset = {
    format: "alpha2-graph-preset",
    version: 1,
    name: "Cards",
    compositions: [
      {
        id: "root",
        inputs: ["In"],
        nodes: [
          { id: "in", op: { type: "group_input", index: 0 } },
          { id: "m1", op: { type: "media", source_id: "card" } },
          { id: "m2", op: { type: "media", source_id: "card~2" } },
          {
            id: "mix",
            op: {
              type: "merge",
              layers: [{ input: "in" }, { input: "m1" }, { input: "m2" }],
            },
          },
        ],
        output: "mix",
      },
      fill("red"),
      fill("blue"),
    ],
    requires: {
      sources: [card("card", "red"), card("card~2", "blue")],
      resources: [],
      effects: [],
    },
  };
  const result = e.dispatch(
    applyGraphPreset(e.document, preset, "c", { groupId: "cards" }),
  );
  assert.equal(result.ok, true, result.error?.message);
  const sources = Object.fromEntries(
    e.document.project.sources.map((s) => [s.id, s.composition]),
  );
  assert.equal(sources.card, "cardc");
  const root = e.document.project.compositions.find((c) =>
    c.nodes.some((n) => n.id === "m1"),
  );
  const read = Object.fromEntries(
    root.nodes
      .filter((n) => n.op.type === "media")
      .map((n) => [n.id, n.op.source_id]),
  );
  assert.notEqual(read.m1, read.m2);
  assert.notEqual(read.m1, "card");
  assert.notEqual(sources[read.m1], sources[read.m2]);
  assert.ok(sources[read.m1].startsWith("red"));
  assert.ok(sources[read.m2].startsWith("blue"));
});

const glow = { id: "test.add-glow", version: 1 };

test("an effect's named inputs are graph inputs: listed in name order, connected, kept or removed", async () => {
  const { inputKeys, inputOf, withInput, withoutEffectInput, acceptsInput } =
    await import("../dist/index.js");
  const op = {
    type: "effect",
    input: "clip",
    inputs: { map: "noise", glow: null },
    effect: glow,
  };
  assert.deepEqual(inputKeys(op), ["input", "inputs.glow", "inputs.map"]);
  assert.equal(inputOf(op, "inputs.map"), "noise");
  assert.equal(inputOf(op, "inputs.glow"), undefined);
  assert.equal(inputOf(op, "inputs.mask"), undefined);
  // Connecting a name the node does not have adds it; disconnecting keeps the slot.
  const masked = withInput(op, "inputs.mask", "clip");
  assert.deepEqual(masked.inputs, { map: "noise", glow: null, mask: "clip" });
  assert.deepEqual(withInput(op, "inputs.map", null).inputs, {
    map: null,
    glow: null,
  });
  assert.equal(acceptsInput(op, "inputs.2map"), false);
  assert.throws(() => withInput(op, "inputs.a-b", "clip"), /no input/);
  assert.throws(
    () =>
      withInput(
        { type: "transform", input: "clip", transform: {} },
        "inputs.map",
        "clip",
      ),
    /no input/,
  );
  // Removing the last named input leaves no `inputs`, as the engine writes it.
  const plain = withoutEffectInput(withoutEffectInput(op, "glow"), "map");
  assert.deepEqual(plain, { type: "effect", input: "clip", effect: glow });
  assert.equal(withoutEffectInput(op, "absent"), op);
});

test("graph commands connect, disconnect, remove, group, and ungroup through named inputs", () => {
  const e = createEditor({ document: document() });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  ok({ type: "graph.create", clipId: "c" });
  const id = "c-fx";
  ok({
    type: "graph.node.add",
    compositionId: id,
    node: { id: "still", op: { type: "media", source_id: "i" } },
  });
  ok({
    type: "graph.node.add",
    compositionId: id,
    node: { id: "soft", op: { type: "effect", input: "still", effect: blur } },
  });
  ok({
    type: "graph.node.add",
    compositionId: id,
    node: { id: "fx", op: { type: "effect", input: "clip", effect: glow } },
  });
  ok({
    type: "graph.connect",
    compositionId: id,
    from: "soft",
    to: "fx",
    input: "inputs.glow",
  });
  ok({ type: "graph.output", compositionId: id, nodeId: "fx" });
  assert.deepEqual(nodes(e).fx.op.inputs, { glow: "soft" });
  const project = () => e.document.project;
  const composition = () => project().compositions.find((c) => c.id === id);
  assert.deepEqual(graphIssues(composition(), "clip", project()), []);
  // An effect with a named input reads another branch, so the clip's graph is no effect stack.
  assert.equal(
    effectStack(project(), e.query().clips.get("c").clip),
    undefined,
  );
  const refused = e.dispatch({
    type: "graph.connect",
    compositionId: id,
    from: "soft",
    to: "fx",
    input: "inputs.not-a-name",
  });
  assert.equal(refused.ok, false);

  // Grouped, the blur and its image fold into a group the effect reads by name.
  ok({
    type: "graph.group",
    compositionId: id,
    ids: ["still", "soft"],
    groupId: "glow-source",
    definitionId: "glow-source-def",
  });
  assert.deepEqual(nodes(e).fx.op.inputs, { glow: "glow-source" });
  ok({ type: "graph.ungroup", compositionId: id, nodeId: "glow-source" });
  assert.deepEqual(nodes(e).fx.op.inputs, { glow: "glow-source-soft" });
  assert.deepEqual(graphIssues(composition(), "clip", project()), []);

  // The effect grouped alone takes its source and its named input as ports, in name order.
  ok({
    type: "graph.group",
    compositionId: id,
    ids: ["fx"],
    groupId: "look",
    definitionId: "look-def",
  });
  assert.deepEqual(nodes(e).look.op.inputs, ["clip", "glow-source-soft"]);
  const definition = project().compositions.find((c) => c.id === "look-def");
  const inner = Object.fromEntries(definition.nodes.map((n) => [n.id, n]));
  assert.equal(inner.fx.op.input, "input-1");
  assert.deepEqual(inner.fx.op.inputs, { glow: "input-2" });
  ok({ type: "graph.ungroup", compositionId: id, nodeId: "look" });
  assert.deepEqual(nodes(e)["look-fx"].op.inputs, { glow: "glow-source-soft" });

  // Removing what a named input reads leaves it unconnected, a missing input of the output's graph.
  ok({
    type: "graph.node.remove",
    compositionId: id,
    ids: ["glow-source-soft"],
  });
  assert.deepEqual(nodes(e)["look-fx"].op.inputs, { glow: null });
  assert.deepEqual(graphIssues(composition(), "clip", project()), [
    { kind: "missing_input", node: "look-fx", input: "inputs.glow" },
  ]);
  ok({
    type: "graph.disconnect",
    compositionId: id,
    to: "look-fx",
    input: "inputs.glow",
  });
  assert.deepEqual(nodes(e)["look-fx"].op.inputs, { glow: null });
});

test("an effect chain whose effect reads a named input is no stack, even of the clip", () => {
  const e = createEditor({ document: document() });
  e.dispatch({ type: "graph.create", clipId: "c" });
  const result = e.dispatch({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: {
      id: "fx",
      op: {
        type: "effect",
        input: "clip",
        inputs: { glow: "clip" },
        effect: glow,
      },
    },
  });
  assert.equal(result.ok, true, result.error?.message);
  e.dispatch({ type: "graph.output", compositionId: "c-fx", nodeId: "fx" });
  const clip = e.query().clips.get("c").clip;
  assert.equal(effectStack(e.document.project, clip), undefined);
});

test("multi-pass groups save as presets with the images and effects their passes read", async () => {
  // Bloom (bright parts, blur, glow added back by name) and an image displacement (an image
  // read as a named input): groups of ordinary nodes, saved and applied like any group.
  const { graphPreset, presetShortfall, applyGraphPreset } =
    await import("../dist/index.js");
  const { validateProject } = await import("alpha2/project");
  const constant = (value) => ({
    mode: "constant",
    value: { type: "float", value },
  });
  const bright = { id: "test.bright-pass", version: 1 };
  const displace = { id: "test.displace", version: 1 };
  const base = document().project;
  const project = {
    ...base,
    compositions: [
      {
        id: "bloom",
        inputs: ["Source"],
        controls: [
          { name: "threshold", node: "bright", param: "threshold" },
          { name: "radius", node: "blur", param: "radius" },
          { name: "intensity", node: "combine", param: "intensity" },
        ],
        nodes: [
          { id: "source", op: { type: "group_input", index: 0 } },
          {
            id: "bright",
            op: { type: "effect", input: "source", effect: bright },
          },
          { id: "blur", op: { type: "effect", input: "bright", effect: blur } },
          {
            id: "combine",
            op: {
              type: "effect",
              input: "source",
              inputs: { glow: "blur" },
              effect: glow,
            },
          },
        ],
        output: "combine",
      },
      {
        id: "warp",
        inputs: ["Source"],
        nodes: [
          { id: "source", op: { type: "group_input", index: 0 } },
          { id: "noise", op: { type: "media", source_id: "i" } },
          {
            id: "bloomed",
            op: {
              type: "group",
              composition: "bloom",
              inputs: ["source"],
              controls: { intensity: constant(2) },
            },
          },
          {
            id: "warped",
            op: {
              type: "effect",
              input: "bloomed",
              inputs: { map: "noise" },
              effect: displace,
            },
          },
        ],
        output: "warped",
      },
    ],
  };
  assert.deepEqual(validateProject(project), []);
  const preset = graphPreset(project, "warp", "Bloom and warp");
  assert.deepEqual(
    preset.compositions.map((c) => c.id),
    ["warp", "bloom"],
  );
  assert.deepEqual(
    preset.requires.sources.map((s) => s.id),
    ["i"],
  );
  assert.deepEqual(
    preset.requires.effects
      .map((effect) => `${effect.id}@${effect.version}`)
      .sort(),
    [
      "alpha2.effect.gaussian-blur@1",
      "test.add-glow@1",
      "test.bright-pass@1",
      "test.displace@1",
    ],
  );
  // A preset does not carry a file-backed image: a project without that source falls short of it.
  const bare = createDocument({ ...base, sources: [base.sources[0]] });
  assert.deepEqual(presetShortfall(bare, preset, () => true).sources, ["i"]);
  const e = createEditor({ document: createDocument(base) });
  assert.deepEqual(
    presetShortfall(e.document, preset, () => true),
    {
      sources: [],
      resources: [],
      effects: [],
    },
  );
  const command = applyGraphPreset(e.document, preset, "c", {
    groupId: "look",
  });
  const result = e.dispatch(command);
  assert.equal(result.ok, true, result.error?.message);
  const applied = e.document.project;
  assert.deepEqual(validateProject(applied), []);
  const own = applied.compositions.find(
    (c) => c.id === e.query().clips.get("c").clip.composition.id,
  );
  assert.deepEqual(graphIssues(own, "clip", applied), []);
});
