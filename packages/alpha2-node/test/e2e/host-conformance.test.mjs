// The conformance table against a real alpha2-host; `npm run test:node-e2e` builds it first.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";

import { Alpha2NodeClient } from "../../dist/index.js";
import { executable } from "../../../../scripts/lib/cargo-target.mjs";
import {
  dataHash,
  digestValues,
} from "../../../../scripts/lib/surface-conformance.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..", "..");
const hostPath =
  process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "debug");
const client = new Alpha2NodeClient({ hostPath, renderer: "cpu" });
const scratch = await mkdtemp(join(tmpdir(), "alpha2-conformance-"));
test.after(async () => {
  client.close();
  await rm(scratch, { recursive: true, force: true });
});

const table = JSON.parse(
  await readFile(resolve(root, "tests/cases/surface-conformance.json"), "utf8"),
);

function withoutTargetDirectory() {
  const env = { ...process.env };
  delete env.CARGO_TARGET_DIR;
  return env;
}

/** Packs `plugins/<name>` once and returns its manifest's path and SHA-256. */
const packed = new Map();
async function plugin(name) {
  if (!packed.has(name)) {
    const out = join(scratch, name);
    const plugins = join(root, "plugins");
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
        join(plugins, name),
        "--out",
        out,
      ],
      { stdio: "inherit", env: withoutTargetDirectory() },
    );
    assert.equal(run.status, 0, `plugins/${name} packs`);
    const manifest = join(out, "manifest.json");
    packed.set(name, {
      manifest,
      sha256: createHash("sha256")
        .update(await readFile(manifest))
        .digest("hex"),
    });
  }
  return packed.get(name);
}

test("the host's edit operation answers each edit case as pinned", async () => {
  for (const fixture of table.edit) {
    const project = table.projects[fixture.project];
    const { expected } = fixture;
    if (expected.refusal !== undefined) {
      await assert.rejects(
        client.edit(project, fixture.request),
        (error) =>
          error.code === expected.code && error.refusal === expected.refusal,
        fixture.name,
      );
      continue;
    }
    const edited = await client.edit(project, fixture.request);
    assert.equal(
      createHash("sha256").update(edited.projectJson).digest("hex"),
      expected.project_sha256,
      fixture.name,
    );
    assert.deepEqual(edited.report, expected.report, fixture.name);
    assert.deepEqual(edited.project, JSON.parse(edited.projectJson));
  }
});

test("each extract case hashes as pinned", async () => {
  for (const fixture of table.extract) {
    const request = {
      project: table.projects[fixture.project],
      assets: Object.fromEntries(
        Object.entries(fixture.assets).map(([id, path]) => [
          id,
          join(root, path),
        ]),
      ),
      request: fixture.request,
    };
    const result =
      fixture.extractor.plugin === undefined
        ? await client.extract(request, {
            describe: () => fixture.extractor.client,
            video(frame, out) {
              out.sample("digest", frame.time, digestValues(frame.data));
            },
          })
        : await client.extract({
            ...request,
            plugin: await plugin(fixture.extractor.plugin),
          });
    assert.equal(await dataHash(result.dataSets), fixture.sha256, fixture.name);
  }
});
