import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { after } from "node:test";

import { missingCapability } from "../../../scripts/lib/capabilities.mjs";
import { Alpha2NodeClient } from "../dist/index.js";
import { quarantineRefusal } from "../dist/quarantine.js";
import { failureOf } from "alpha2/export-failure";

const SUITE = "packages/alpha2-node/test/quarantine.test.mjs";

const scratchDirectories = [];

after(async () => {
  await Promise.all(
    scratchDirectories.map((directory) =>
      rm(directory, { recursive: true, force: true }),
    ),
  );
});

/** An unpacked archive's layout: `bin/alpha2-host` and `lib/` with `libraries`. */
async function archiveTree(libraries) {
  const root = await mkdtemp(join(tmpdir(), "alpha2-node-quarantine-"));
  scratchDirectories.push(root);
  await mkdir(join(root, "bin"));
  await mkdir(join(root, "lib"));
  await writeFile(join(root, "bin", "alpha2-host"), "");
  for (const name of libraries) await writeFile(join(root, "lib", name), "");
  return root;
}

/** A probe that says `quarantined` and `accepted` of every file, recording what it assessed. */
function probe({ quarantined, accepted }) {
  const assessed = [];
  return {
    assessed,
    async quarantined(path) {
      return quarantined(path);
    },
    async accepted(path, kind) {
      assessed.push([path, kind]);
      return accepted(path);
    },
  };
}

test("quarantine refusal lists quarantined files that Gatekeeper rejects", async () => {
  const root = await archiveTree([
    "libavcodec.62.dylib",
    "libavformat.62.dylib",
    "libavutil.60.dylib",
    "libswscale.9.dylib",
    "notes.txt",
  ]);
  const checks = probe({ quarantined: () => true, accepted: () => false });
  const message = await quarantineRefusal(
    join(root, "bin", "alpha2-host"),
    checks,
  );
  assert.match(message, /^macOS quarantined 5 file\(s\) alpha2-host needs/);
  assert.match(message, /and 2 more\./);
  assert.ok(message.includes(join(root, "bin", "alpha2-host")), message);
  assert.ok(!message.includes("notes.txt"), message);
  assert.deepEqual(
    checks.assessed.map(([, kind]) => kind),
    ["execute", "library", "library", "library", "library"],
  );
});

test("a quarantined file Gatekeeper accepts is not refused", async () => {
  const root = await archiveTree(["libavcodec.62.dylib"]);
  assert.equal(
    await quarantineRefusal(
      join(root, "bin", "alpha2-host"),
      probe({ quarantined: () => true, accepted: () => true }),
    ),
    undefined,
  );
  // Files that are not quarantined are never assessed.
  const checks = probe({ quarantined: () => false, accepted: () => false });
  assert.equal(
    await quarantineRefusal(join(root, "bin", "alpha2-host"), checks),
    undefined,
  );
  assert.deepEqual(checks.assessed, []);
});

test("an unknown assessment does not refuse", async () => {
  const root = await archiveTree(["libavcodec.62.dylib"]);
  assert.equal(
    await quarantineRefusal(
      join(root, "bin", "alpha2-host"),
      probe({ quarantined: () => true, accepted: () => undefined }),
    ),
    undefined,
  );
});

test("the remedy names the archive root", async () => {
  const root = await archiveTree(["libavcodec.62.dylib"]);
  const rejected = probe({ quarantined: () => true, accepted: () => false });
  assert.match(
    await quarantineRefusal(join(root, "bin", "alpha2-host"), rejected),
    new RegExp(
      `xattr -dr com\\.apple\\.quarantine '${root.replace(/[\\^$.*+?()[\]{}|]/g, "\\$&")}'`,
    ),
  );
  // A host outside an archive's bin/ names its own directory.
  const alone = join(root, "lib", "alpha2-host");
  await writeFile(alone, "");
  assert.ok(
    (await quarantineRefusal(alone, rejected)).includes(
      `xattr -dr com.apple.quarantine '${join(root, "lib")}'`,
    ),
  );
});

const noCompiler =
  process.platform === "darwin" &&
  spawnSync("cc", ["--version"], { stdio: "ignore" }).status !== 0;

test(
  "a quarantined ad-hoc host is refused before it starts",
  {
    timeout: 60_000,
    skip:
      process.platform !== "darwin"
        ? "macOS only: quarantine and Gatekeeper"
        : noCompiler &&
          missingCapability(
            "c-compiler",
            SUITE,
            "no cc to build an unsigned host",
          ),
  },
  async () => {
    const root = await archiveTree([]);
    const source = join(root, "main.c");
    const host = join(root, "bin", "alpha2-host");
    await writeFile(source, "int main(void) { return 0; }\n");
    execFileSync("cc", ["-o", host, source]);
    await chmod(host, 0o755);
    const value = `0083;${Math.floor(Date.now() / 1000).toString(16)};Safari;${randomUUID().toUpperCase()}`;
    execFileSync("/usr/bin/xattr", ["-w", "com.apple.quarantine", value, host]);
    const started = Date.now();
    const error = await new Alpha2NodeClient({ hostPath: host })
      .getCapabilities()
      .then(
        () => assert.fail("the call resolved"),
        (cause) => cause,
      );
    assert.ok(
      Date.now() - started < 5_000,
      `refused after ${Date.now() - started} ms`,
    );
    assert.equal(error.code, "HOST_QUARANTINED", error.message);
    assert.ok(
      error.message.includes(`xattr -dr com.apple.quarantine '${root}'`),
      error.message,
    );
    assert.equal(failureOf(error)?.code, "INVALID_INPUT");
    const log = spawnSync(
      "/usr/bin/log",
      [
        "show",
        "--last",
        "1m",
        "--predicate",
        'process == "syspolicyd"',
        "--style",
        "compact",
      ],
      { encoding: "utf8" },
    );
    const prompts = (log.stdout ?? "")
      .split("\n")
      .filter((line) => line.includes("Prompt shown") && line.includes(host));
    assert.deepEqual(prompts, []);
  },
);
