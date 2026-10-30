// A document holds its project in the forms the engine writes, however it entered the editor:
// opened from a file, created from an engine project, handed to a session, or made by a command.
import test from "node:test";
import assert from "node:assert/strict";
import {
  createDocument,
  createEditor,
  createEmptyDocument,
  executeCommand,
  parseDocument,
  serializeDocument,
} from "../dist/index.js";

const red = { red: 1, green: 0, blue: 0, alpha: 1 };
const solidRed = { type: "solid", color: red };

/** An engine project whose colors and paints are written in the forms the engine only reads. */
function authoredProject() {
  return {
    schema_version: 1,
    id: "p",
    name: "P",
    settings: {
      width: 64,
      height: 36,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      {
        id: "fill",
        kind: "generated",
        generator: { type: "fill", paint: "#f00" },
      },
      {
        id: "panel",
        kind: "generated",
        generator: {
          type: "layout",
          root: {
            fill: "#ff0000",
            padding: { mode: "constant", value: "4px 0.02" },
            background: { mode: "constant", value: red },
            children: [{ text: { mode: "constant", value: "Hi" } }],
          },
        },
      },
    ],
    timeline: { tracks: [] },
  };
}

function assertCanonical(project) {
  assert.deepEqual(project.sources[0].generator.paint, solidRed);
  const root = project.sources[1].generator.root;
  assert.deepEqual(root.fill, solidRed);
  assert.deepEqual(root.background.value, solidRed);
  assert.deepEqual(root.padding.value, {
    top: "4px",
    right: 0.02,
    bottom: "4px",
    left: 0.02,
  });
}

test("a document created or opened from an engine project holds it in canonical form", () => {
  assertCanonical(createDocument(authoredProject()).project);
  assertCanonical(parseDocument(JSON.stringify(authoredProject())).project);
});

test("an editor file and a document handed to a session hold their project in canonical form", () => {
  const document = createDocument(authoredProject());
  // An editor file written by hand, or by an older editor, in the forms only read.
  const authored = { ...structuredClone(document), project: authoredProject() };
  assertCanonical(parseDocument(JSON.stringify(authored)).project);
  const editor = createEditor({ document: authored });
  assertCanonical(editor.document.project);
  editor.open(structuredClone(authored));
  assertCanonical(editor.document.project);
  // What the editor writes is canonical too.
  assertCanonical(JSON.parse(serializeDocument(editor.document)).project);
  editor.dispose();
});

test("what a command brings in is stored in canonical form, and the rest is shared", () => {
  const editor = createEditor({
    document: createEmptyDocument({ width: 64, height: 36 }),
  });
  const before = editor.document;
  const result = editor.dispatch({
    type: "asset.put",
    asset: { id: "fill", kind: "generated", name: "Fill", contentRevision: 0 },
    source: {
      id: "fill",
      kind: "generated",
      generator: { type: "fill", paint: "#f00" },
    },
  });
  assert.ok(result.ok, result.error?.message);
  assert.deepEqual(
    editor.document.project.sources[0].generator.paint,
    solidRed,
  );
  // Parts the command did not touch are the same objects.
  assert.equal(editor.document.project.settings, before.project.settings);
  assert.equal(editor.document.project.timeline, before.project.timeline);
  // A command that changes nothing returns the document itself.
  const same = executeCommand(editor.document, {
    type: "transaction",
    commands: [],
  });
  assert.ok(same.ok);
  assert.equal(same.value, editor.document);
  // A color the engine cannot read is refused as validation words it.
  const refused = editor.dispatch({
    type: "asset.put",
    asset: { id: "bad", kind: "generated", name: "Bad", contentRevision: 0 },
    source: {
      id: "bad",
      kind: "generated",
      generator: { type: "fill", paint: "#ggg" },
    },
  });
  assert.equal(refused.ok, false);
  assert.match(refused.error.message, /invalid color "#ggg"/);
  editor.dispose();
});
