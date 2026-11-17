import test from "node:test";
import assert from "node:assert/strict";
import { createDocument, createEditor } from "../dist/index.js";

const SECOND = 240000;

const project = () => ({
  schema_version: 1,
  id: "p",
  name: "Extraction",
  settings: {
    width: 64,
    height: 48,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48000,
  },
  sources: [
    { id: "s", kind: "video", locator: "test://s" },
    { id: "s2", kind: "video", locator: "test://s2" },
  ],
  timeline: {
    tracks: [
      {
        id: "v",
        name: "V",
        kind: "video",
        clips: [
          {
            id: "c",
            source_id: "s",
            timeline_range: { start: 0, end: 2 * SECOND },
            source_range: { start: 0, end: 2 * SECOND },
            time_map: { type: "forward" },
          },
        ],
      },
    ],
  },
});

const quiet = (anchor) => ({
  dataSets: [
    {
      id: "quiet",
      kind: "intervals",
      anchor,
      made_by: { extractor: "com.example.quiet", version: 1, params: {} },
      intervals: [{ start: 0, end: SECOND }],
    },
  ],
  timings: {},
});

const fromSource = (id = "s2") => quiet({ type: "source", source_id: id });
const fromTimeline = () => quiet({ type: "timeline" });

test("a source-anchored result goes in as one labelled step", () => {
  const editor = createEditor({ document: createDocument(project()) });
  const start = editor.document;
  const { project: read } = editor.extractionProject();
  assert.equal(read, start.project);
  const result = editor.applyExtraction(fromSource());
  assert.ok(result.ok, result.error?.message);
  assert.deepEqual(
    editor.document.project.data.map((set) => set.id),
    ["quiet"],
  );
  assert.equal(editor.getSnapshot().undoLabel, "extract");
  assert.ok(editor.undo());
  assert.equal(editor.document, start);
  assert.equal(
    editor.applyExtraction({ dataSets: [], timings: {} }).value,
    start,
  );
});

test("a result whose source was removed meanwhile is refused", () => {
  const document = createDocument(project());
  const editor = createEditor({
    document: {
      ...document,
      assets: {
        s2: { id: "s2", kind: "video", name: "s2", contentRevision: 0 },
      },
    },
  });
  const result = fromSource();
  assert.ok(editor.dispatch({ type: "asset.remove", id: "s2" }).ok);
  const refused = editor.applyExtraction(result);
  assert.equal(refused.error?.code, "missing_asset");
  assert.equal(refused.error?.entityId, "s2");
});

test("a timeline-anchored result needs a read that is still current", () => {
  const editor = createEditor({ document: createDocument(project()) });
  const missing = editor.applyExtraction(fromTimeline());
  assert.equal(missing.error?.code, "invalid_command");

  const { read } = editor.extractionProject();
  assert.ok(editor.applyExtraction(fromSource()).ok);
  const applied = editor.applyExtraction(fromTimeline(), {
    read,
    label: "Quiet",
  });
  assert.ok(applied.ok, applied.error?.message);
  assert.equal(editor.getSnapshot().undoLabel, "Quiet");

  const { read: before } = editor.extractionProject();
  assert.ok(editor.dispatch({ type: "clip.move", id: "c", at: SECOND }).ok);
  const stale = editor.applyExtraction(fromTimeline(), { read: before });
  assert.equal(stale.error?.code, "stale_extraction");
  assert.ok(editor.undo());
  assert.ok(editor.applyExtraction(fromTimeline(), { read: before }).ok);
});

test("a disabled track's change counts, as the projection the extraction read changes", () => {
  const editor = createEditor({ document: createDocument(project()) });
  const { read } = editor.extractionProject();
  assert.ok(
    editor.dispatch({
      type: "track.metadata",
      id: "v",
      patch: { enabled: false },
    }).ok,
  );
  assert.equal(
    editor.applyExtraction(fromTimeline(), { read }).error?.code,
    "stale_extraction",
  );
});
