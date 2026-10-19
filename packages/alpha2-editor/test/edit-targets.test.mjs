import test from "node:test";
import assert from "node:assert/strict";
import {
  createDocument,
  createEditor,
  graphIssues,
  graphLayout,
  serializeDocument,
  parseDocument,
} from "../dist/index.js";

const S = 120000;
const crossfade = { id: "alpha2.transition.crossfade", version: 1 };
const blur = { id: "alpha2.effect.gaussian-blur", version: 1 };
const clip = (id, start, end) => ({
  id,
  source_id: "s",
  timeline_range: { start, end },
  source_range: { start: 0, end: end - start },
});
const project = (clips) => ({
  schema_version: 1,
  id: "p",
  name: "Edit targets",
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
    tracks: [{ id: "t", name: "Video", kind: "video", clips }],
  },
});
const editor = (clips) => {
  const e = createEditor({ document: createDocument(project(clips)) });
  const ok = (command) => {
    const result = e.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
    return result;
  };
  return { e, ok };
};
const track = (e) => e.document.project.timeline.tracks[0];
const at = (e, id) =>
  track(e).clips.find((c) => c.id === id).timeline_range.start;
/** The id a view shows for the transition from `from` to `to`: the entry at its index. */
const transitionId = (e, from, to) =>
  e.document.metadata.instanceIds["t/transitions"][
    track(e).transitions.findIndex(
      (t) => t.from_clip === from && t.to_clip === to,
    )
  ];
const pairs = (e) =>
  (track(e).transitions ?? []).map((t) => `${t.from_clip}>${t.to_clip}`);

test("a transition's id stays its own: a command reaches the transition it names", () => {
  const { e, ok } = editor([
    clip("a", 0, 2 * S),
    clip("b", 2 * S, 4 * S),
    clip("c", 4 * S, 6 * S),
    clip("d", 6 * S, 8 * S),
  ]);
  const add = (from, to) =>
    ok({
      type: "transition.add",
      trackId: "t",
      fromId: from,
      toId: to,
      duration: S / 2,
      effect: crossfade,
    });
  add("a", "b");
  add("c", "d");
  const cd = transitionId(e, "c", "d");
  ok({
    type: "transition.remove",
    trackId: "t",
    instanceId: transitionId(e, "a", "b"),
  });
  assert.equal(transitionId(e, "c", "d"), cd, "removing one renames no other");
  add("a", "b");
  const ids = e.document.metadata.instanceIds["t/transitions"];
  assert.equal(new Set(ids).size, ids.length, `ids repeat: ${ids}`);
  // Removing the re-added transition removes it, not the one that had its id.
  ok({
    type: "transition.remove",
    trackId: "t",
    instanceId: transitionId(e, "a", "b"),
  });
  assert.deepEqual(pairs(e), ["c>d"]);
  assert.equal(transitionId(e, "c", "d"), cd);

  // Deleting a clip takes its transition, and the others keep their ids.
  add("a", "b");
  ok({ type: "clip.delete", id: "a" });
  assert.equal(transitionId(e, "c", "d"), cd);
  ok({ type: "transition.resize", trackId: "t", instanceId: cd, duration: S });
  assert.equal(at(e, "d"), 5 * S);

  // A transaction's later commands see what its earlier ones did.
  add("b", "c");
  const bc = transitionId(e, "b", "c");
  ok({
    type: "transaction",
    commands: [
      { type: "clip.delete", id: "b" },
      {
        type: "transition.resize",
        trackId: "t",
        instanceId: cd,
        duration: S / 2,
      },
    ],
  });
  assert.deepEqual(pairs(e), ["c>d"]);
  assert.equal(at(e, "d"), 4.5 * S);
  const stale = e.dispatch({
    type: "transition.remove",
    trackId: "t",
    instanceId: bc,
  });
  assert.equal(stale.ok, false, "a removed transition's id names nothing");
  assert.deepEqual(pairs(e), ["c>d"]);
});

test("a document whose transition ids repeat or drift opens with each transition's own", () => {
  const { e, ok } = editor([
    clip("a", 0, 2 * S),
    clip("b", 2 * S, 4 * S),
    clip("c", 4 * S, 6 * S),
    clip("d", 6 * S, 8 * S),
  ]);
  for (const [from, to] of [
    ["a", "b"],
    ["c", "d"],
  ])
    ok({
      type: "transition.add",
      trackId: "t",
      fromId: from,
      toId: to,
      duration: S / 2,
      effect: crossfade,
    });
  const ab = transitionId(e, "a", "b");
  const cd = transitionId(e, "c", "d");
  const saved = JSON.parse(serializeDocument(e.document));
  saved.metadata.instanceIds["t/transitions"] = ["x", "x"];
  const reopened = createEditor({
    document: parseDocument(JSON.stringify(saved)),
  });
  assert.deepEqual(reopened.document.metadata.instanceIds["t/transitions"], [
    ab,
    cd,
  ]);
  const opened = createEditor({ document: e.document });
  opened.open({
    ...e.document,
    metadata: {
      ...e.document.metadata,
      instanceIds: {
        ...e.document.metadata.instanceIds,
        "t/transitions": ["x", "x"],
      },
    },
  });
  assert.deepEqual(opened.document.metadata.instanceIds["t/transitions"], [
    ab,
    cd,
  ]);
});

test("a transition before another moves the clips the chain holds together", () => {
  const { e, ok } = editor([
    clip("a", 0, 3 * S),
    clip("b", 3 * S, 6 * S),
    clip("c", 6 * S, 9 * S),
    clip("d", 9 * S, 12 * S),
  ]);
  const add = (from, to) =>
    ok({
      type: "transition.add",
      trackId: "t",
      fromId: from,
      toId: to,
      duration: S,
      effect: crossfade,
    });
  // Added last to first: each one moves the chain after it.
  add("c", "d");
  add("b", "c");
  add("a", "b");
  const starts = () => ["a", "b", "c", "d"].map((id) => at(e, id) / S);
  assert.deepEqual(starts(), [0, 2, 4, 6]);
  const ab = transitionId(e, "a", "b");
  ok({
    type: "transition.resize",
    trackId: "t",
    instanceId: ab,
    duration: S / 2,
  });
  assert.deepEqual(starts(), [0, 2.5, 4.5, 6.5]);
  ok({
    type: "transition.resize",
    trackId: "t",
    instanceId: ab,
    duration: 2 * S,
  });
  assert.deepEqual(starts(), [0, 1, 3, 5]);
  for (const t of track(e).transitions) {
    const to = track(e).clips.find((c) => c.id === t.to_clip);
    const from = track(e).clips.find((c) => c.id === t.from_clip);
    assert.equal(t.timeline_range.start, to.timeline_range.start);
    assert.equal(t.timeline_range.end, from.timeline_range.end);
  }
  ok({ type: "transition.remove", trackId: "t", instanceId: ab });
  assert.deepEqual(starts(), [0, 3, 5, 7]);
  assert.deepEqual(pairs(e), ["c>d", "b>c"]);
});

/**
 * Clip c with a graph: a blur, then the image `i` over it, the image folded into group `overlay`.
 */
const grouped = () => {
  const { e, ok } = editor([clip("c", 0, S)]);
  ok({ type: "graph.create", clipId: "c" });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: { id: "fx", op: { type: "effect", input: "clip", effect: blur } },
    position: { x: 10, y: 20 },
  });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: { id: "logo", op: { type: "media", source_id: "i" } },
    position: { x: 30, y: 40 },
  });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: {
      id: "mix",
      op: { type: "merge", layers: [{ input: "fx" }, { input: "logo" }] },
    },
  });
  ok({ type: "graph.output", compositionId: "c-fx", nodeId: "mix" });
  ok({
    type: "graph.group",
    compositionId: "c-fx",
    ids: ["fx"],
    groupId: "g",
    definitionId: "look",
    name: "Look",
  });
  ok({
    type: "graph.group",
    compositionId: "c-fx",
    ids: ["logo"],
    groupId: "o",
    definitionId: "overlay",
    name: "Overlay",
  });
  return { e, ok };
};
const composition = (e, id) =>
  e.document.project.compositions.find((c) => c.id === id);
const compositionIds = (e) =>
  (e.document.project.compositions ?? []).map((c) => c.id).sort();

test("a composition nothing draws any more goes with the edit that stopped drawing it", () => {
  for (const [name, remove] of [
    ["clip.delete", { type: "clip.delete", id: "c" }],
    ["track.remove", { type: "track.remove", id: "t" }],
  ]) {
    const { e, ok } = grouped();
    assert.deepEqual(compositionIds(e), ["c-fx", "look", "overlay"]);
    ok(remove);
    assert.deepEqual(compositionIds(e), [], name);
    assert.deepEqual(Object.keys(graphLayout(e.document)), [], name);
    assert.deepEqual(
      Object.keys(e.document.metadata.graphNames ?? {}),
      [],
      name,
    );
    ok({ type: "asset.remove", id: "i" });
  }
  {
    const { e, ok } = grouped();
    ok({ type: "graph.node.remove", compositionId: "c-fx", ids: ["o"] });
    assert.deepEqual(compositionIds(e), ["c-fx", "look"]);
    ok({ type: "asset.remove", id: "i" });
  }
  {
    // A Library composition goes with its source.
    const { e, ok } = editor([clip("c", 0, S)]);
    ok({
      type: "composition.create",
      compositionId: "title",
      sourceId: "title-source",
      name: "Title",
    });
    ok({
      type: "graph.node.add",
      compositionId: "title",
      node: { id: "logo", op: { type: "media", source_id: "i" } },
    });
    ok({ type: "asset.remove", id: "title-source" });
    assert.deepEqual(compositionIds(e), []);
    ok({ type: "asset.remove", id: "i" });
  }
  {
    // A composition added for a later edit stays until something draws it and stops.
    const { e, ok } = editor([clip("c", 0, S)]);
    ok({
      type: "composition.add",
      composition: {
        id: "spare",
        inputs: ["In"],
        nodes: [{ id: "in", op: { type: "group_input", index: 0 } }],
        output: "in",
      },
    });
    ok({ type: "clip.update", id: "c", patch: { blend_mode: "screen" } });
    assert.deepEqual(compositionIds(e), ["spare"]);
  }
});

const definitionOf = (e, clipId, group) => {
  const c = e.query().clips.get(clipId).clip;
  const composition = e.document.project.compositions.find(
    (x) => x.id === c.composition.id,
  );
  return composition.nodes.find((n) => n.id === group).op.composition;
};
const radius = (value) => ({
  ...blur,
  params: {
    radius: { mode: "constant", value: { type: "float", value } },
  },
});

test("a pasted, duplicated or split clip edits its own groups", () => {
  for (const how of ["paste", "split"]) {
    const { e, ok } = grouped();
    let copy;
    if (how === "paste") {
      e.copy(["c"]);
      const pasted = e.paste(2 * S, () => "c2");
      assert.equal(pasted.ok, true, pasted.error?.message);
      copy = "c2";
    } else {
      ok({ type: "clip.split", id: "c", at: S / 2, newId: "c2" });
      copy = "c2";
    }
    const look = definitionOf(e, copy, "g");
    assert.notEqual(look, "look", `${how}: the copy draws its own group`);
    assert.notEqual(definitionOf(e, copy, "o"), "overlay");
    assert.deepEqual(
      graphLayout(e.document)[look],
      graphLayout(e.document).look,
    );
    assert.equal(e.document.metadata.graphNames[look], "Look");
    const node = e.document.project.compositions
      .find((x) => x.id === look)
      .nodes.find((n) => n.id === "fx");
    ok({
      type: "graph.node.update",
      compositionId: look,
      node: { ...node, op: { ...node.op, effect: radius(40) } },
    });
    const original = e.document.project.compositions
      .find((x) => x.id === "look")
      .nodes.find((n) => n.id === "fx");
    assert.equal(
      original.op.effect.params,
      undefined,
      `${how}: the original keeps its blur`,
    );
    ok({ type: "graph.port.add", compositionId: look, name: "Extra" });
    assert.deepEqual(
      graphIssues(composition(e, "c-fx"), "clip", e.document.project),
      [],
      how,
    );
  }
});

test("a clip copied, then deleted, pastes with the composition it had", () => {
  const { e, ok } = editor([clip("c", 0, S)]);
  ok({ type: "effect.add", id: "c", instanceId: "fx", effect: radius(3) });
  e.copy(["c"]);
  // What the clipboard holds is what was copied, not what the document has since.
  ok({ type: "effect.update", id: "c", instanceId: "fx", effect: radius(9) });
  ok({ type: "clip.delete", id: "c" });
  assert.deepEqual(compositionIds(e), []);
  const pasted = e.paste(2 * S, () => "p");
  assert.equal(pasted.ok, true, pasted.error?.message);
  const p = e.query().clips.get("p").clip;
  const shown = composition(e, p.composition.id);
  assert.deepEqual(shown.nodes.find((n) => n.id === "fx").op.effect, radius(3));
  const g = grouped();
  g.e.copy(["c"]);
  g.ok({ type: "clip.delete", id: "c" });
  const again = g.e.paste(0, () => "q");
  assert.equal(again.ok, true, again.error?.message);
  assert.equal(compositionIds(g.e).length, 3);
  assert.deepEqual(
    graphIssues(
      composition(g.e, g.e.query().clips.get("q").clip.composition.id),
      "clip",
      g.e.document.project,
    ),
    [],
  );
});

test("clips move together whatever order the selection lists them in", () => {
  for (const order of [
    ["a", "b", "c"],
    ["c", "b", "a"],
    ["b", "c", "a"],
  ])
    for (const delta of [S / 2, -S / 2, 3 * S]) {
      const { e, ok } = editor([
        clip("a", S, 2 * S),
        clip("b", 2 * S, 3 * S),
        clip("c", 3 * S, 4 * S),
      ]);
      ok({
        type: "clips.move",
        moves: order.map((id) => ({ id, at: at(e, id) + delta })),
      });
      assert.deepEqual(
        ["a", "b", "c"].map((id) => at(e, id)),
        [S + delta, 2 * S + delta, 3 * S + delta],
        `${order} by ${delta}`,
      );
    }
  // Duplicates put down over their originals and moved off them in one transaction.
  const { e, ok } = editor([clip("a", 0, S), clip("b", S, 2 * S)]);
  ok({
    type: "transaction",
    commands: [
      { type: "clip.insert", trackId: "t", clip: clip("a2", 0, S) },
      { type: "clip.insert", trackId: "t", clip: clip("b2", S, 2 * S) },
      {
        type: "clips.move",
        moves: [
          { id: "a2", at: 2 * S },
          { id: "b2", at: 3 * S },
        ],
      },
    ],
  });
  assert.equal(at(e, "b2"), 3 * S);
  const overlap = e.dispatch({
    type: "clips.move",
    moves: [{ id: "a", at: S / 2 }],
  });
  assert.equal(overlap.ok, false, "the result is still checked");
  const twice = e.dispatch({
    type: "clips.move",
    moves: [
      { id: "a", at: 4 * S },
      { id: "a", at: 5 * S },
    ],
  });
  assert.equal(twice.ok, false, "a clip moves once");
});
