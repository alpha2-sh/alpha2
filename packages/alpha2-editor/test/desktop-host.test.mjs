import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createDocument, createEditor } from "../dist/index.js";
import { executable } from "../../../scripts/lib/cargo-target.mjs";
import {
  dataHash,
  digestValues,
} from "../../../scripts/lib/surface-conformance.mjs";
import {
  missingCapability,
  suiteOf,
} from "../../../scripts/lib/capabilities.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..");
const hostPath =
  process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "debug");
const clientModule = resolve(root, "packages/alpha2-node/dist/index.js");
// Skipped until `cargo build -p alpha2-host` and the Node client build; ALPHA2_REQUIRE=host fails.
const skip =
  !(existsSync(hostPath) && existsSync(clientModule)) &&
  missingCapability(
    "host",
    suiteOf(import.meta.url),
    "alpha2-host or the Node client is not built",
  );

const table = JSON.parse(
  readFileSync(resolve(root, "tests/cases/surface-conformance.json"), "utf8"),
);

let client, scratch;
test.before(async () => {
  if (skip) return;
  const { Alpha2NodeClient } = await import(pathToFileURL(clientModule).href);
  client = new Alpha2NodeClient({ hostPath, renderer: "cpu" });
  scratch = await mkdtemp(join(tmpdir(), "alpha2-editor-host-"));
});
test.after(async () => {
  client?.close();
  if (scratch) await rm(scratch, { recursive: true, force: true });
});

/** Packs the plugin crate at `path` (from the repository root): manifest path and SHA-256. */
function pack(path) {
  const plugins = join(root, "plugins");
  const out = join(scratch, path);
  const env = { ...process.env };
  delete env.CARGO_TARGET_DIR;
  const run = spawnSync(
    "cargo",
    [
      "run",
      "--release",
      "--manifest-path",
      join(plugins, "Cargo.toml"),
      "--target-dir",
      join(plugins, "target"),
      "-p",
      "alpha2-plugin-pack",
      "--",
      join(root, path),
      "--out",
      out,
    ],
    { stdio: "inherit", env },
  );
  assert.equal(run.status, 0, `${path} packs`);
  const manifest = join(out, "manifest.json");
  return {
    manifest,
    sha256: createHash("sha256").update(readFileSync(manifest)).digest("hex"),
  };
}

const extractCase = (name) => table.extract.find((c) => c.name === name);
const assets = (fixture) =>
  Object.fromEntries(
    Object.entries(fixture.assets).map(([id, path]) => [id, join(root, path)]),
  );

test(
  "client.edit results applied to a session give the engine's bytes",
  { skip },
  async () => {
    for (const fixture of table.edit) {
      if (fixture.expected.refusal !== undefined) continue;
      const editor = createEditor({
        document: createDocument(table.projects[fixture.project]),
      });
      const { project, read } = editor.editProject();
      const applied = await client.edit(project, fixture.request);
      const result = editor.applyEdit(applied, {
        read,
        request: fixture.request,
      });
      assert.ok(result.ok, `${fixture.name}: ${result.error?.message}`);
      assert.deepEqual(result.value.report, fixture.expected.report);
      assert.deepEqual(editor.document.project, applied.project, fixture.name);
      // The engine's text of the committed project, through the host again.
      const again = await client.edit(editor.document.project, {
        op: "bake",
        targets: [],
      });
      assert.equal(
        createHash("sha256").update(again.projectJson).digest("hex"),
        fixture.expected.project_sha256,
        fixture.name,
      );
    }
  },
);

test(
  "a plugin run through client.extract goes in through applyExtraction",
  { skip, timeout: 600000 },
  async () => {
    const fixture = extractCase("plugin-digest-source-h264-bt709-copy");
    const editor = createEditor({
      document: createDocument(table.projects[fixture.project]),
    });
    const { project, read } = editor.extractionProject();
    const { dataSets } = await client.extract({
      project,
      assets: assets(fixture),
      request: fixture.request,
      plugin: pack(fixture.extractor.plugin),
    });
    const result = editor.applyExtraction({ dataSets, timings: {} }, { read });
    assert.ok(result.ok, result.error?.message);
    assert.equal(await dataHash(editor.document.project.data), fixture.sha256);
  },
);

test(
  "a timeline edit during a timeline run makes its result stale; data put meanwhile does not",
  { skip },
  async () => {
    const fixture = extractCase("digest-timeline-cpu");
    const editor = createEditor({
      document: createDocument(table.projects[fixture.project]),
    });
    const digest = {
      describe: () => fixture.extractor.client,
      video(frame, out) {
        out.sample("digest", frame.time, digestValues(frame.data));
      },
    };
    const run = async () => {
      const { project, read } = editor.extractionProject();
      const result = await client.extract(
        { project, request: fixture.request },
        digest,
      );
      return { result, read };
    };
    const first = await run();
    assert.equal(await dataHash(first.result.dataSets), fixture.sha256);
    const unrelated = {
      id: "elsewhere",
      kind: "intervals",
      anchor: { type: "timeline" },
      intervals: [{ start: 0, end: 4000 }],
    };
    assert.ok(editor.dispatch({ type: "data.put", dataSet: unrelated }).ok);
    assert.ok(editor.applyExtraction(first.result, { read: first.read }).ok);

    const second = await run();
    const clip = editor.document.project.timeline.tracks[0].clips[0];
    assert.ok(
      editor.dispatch({
        type: "clip.trim",
        id: clip.id,
        edge: "end",
        at: clip.timeline_range.end - 8000,
      }).ok,
    );
    const before = editor.document;
    const stale = editor.applyExtraction(second.result, { read: second.read });
    assert.equal(stale.error?.code, "stale_extraction");
    assert.equal(editor.document, before);
  },
);

test("a cancelled run leaves the document as it was", { skip }, async () => {
  const fixture = extractCase("plugin-digest-source-h264-bt709-copy");
  const editor = createEditor({
    document: createDocument(table.projects[fixture.project]),
  });
  const before = editor.document;
  const controller = new AbortController();
  const { project } = editor.extractionProject();
  const running = client.extract(
    {
      project,
      assets: assets(fixture),
      request: fixture.request,
      signal: controller.signal,
    },
    {
      describe: () =>
        extractCase("digest-source-h264-bt709-copy").extractor.client,
      video() {
        controller.abort();
      },
    },
  );
  await assert.rejects(running);
  assert.equal(editor.document, before);
  assert.equal(editor.getSnapshot().canUndo, false);
});
