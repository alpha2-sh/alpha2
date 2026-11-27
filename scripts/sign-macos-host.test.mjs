import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const sign = join(root, "scripts/sign-macos-host.sh");
const entitlements = (name) =>
  join(root, "release/macos", `${name}.entitlements`);
const notMacos = process.platform !== "darwin" && "codesign runs on macOS only";

function run(command, args, options = {}) {
  return spawnSync(command, args, { encoding: "utf8", ...options });
}

function built(command, args) {
  const result = run(command, args);
  assert.equal(
    result.status,
    0,
    `${command} ${args.join(" ")}: ${result.stderr}`,
  );
}

function archiveWithStandInHost(t) {
  const scratch = mkdtempSync(join(tmpdir(), "alpha2 sign "));
  t.after(() => rmSync(scratch, { recursive: true, force: true }));
  const archive = join(scratch, "alpha2 archive");
  mkdirSync(join(archive, "bin"), { recursive: true });
  mkdirSync(join(archive, "lib"));
  writeFileSync(
    join(scratch, "standin.c"),
    'const char *standin_answer(void) { return "\\"requestId\\":\\"signed\\",\\"ok\\":true"; }\n',
  );
  writeFileSync(
    join(scratch, "host.c"),
    `#include <stdio.h>
const char *standin_answer(void);
int main(void) {
  while (getchar() != EOF) {}
  printf("%s\\n", standin_answer());
  return 0;
}
`,
  );
  const library = join(archive, "lib", "libalpha2-standin.dylib");
  built("cc", [
    "-dynamiclib",
    "-install_name",
    "@rpath/libalpha2-standin.dylib",
    "-o",
    library,
    join(scratch, "standin.c"),
  ]);
  for (const executable of ["alpha2-host", "alpha2"])
    built("cc", [
      "-o",
      join(archive, "bin", executable),
      join(scratch, "host.c"),
      library,
      "-Wl,-rpath,@executable_path/../lib",
    ]);
  return archive;
}

function signature(file) {
  const details = run("codesign", ["--display", "--verbose=2", file]);
  const granted = run("codesign", [
    "--display",
    "--entitlements",
    "-",
    "--xml",
    file,
  ]);
  return { details: details.stderr, entitlements: granted.stdout };
}

test(
  "the host signed with library validation off loads the archive's libraries and answers",
  { skip: notMacos },
  (t) => {
    const archive = archiveWithStandInHost(t);
    const signed = run(sign, [
      archive,
      "-",
      entitlements("disable-library-validation"),
    ]);
    assert.equal(signed.status, 0, signed.stderr);
    assert.match(signed.stdout, /alpha2-host answers capabilities/);
    for (const file of [
      "lib/libalpha2-standin.dylib",
      "bin/alpha2",
      "bin/alpha2-host",
    ]) {
      const { details } = signature(join(archive, file));
      assert.match(
        details,
        /flags=0x10002\(adhoc,runtime\)/,
        `${file}: ${details}`,
      );
    }
    assert.match(
      signature(join(archive, "bin/alpha2-host")).entitlements,
      /com\.apple\.security\.cs\.disable-library-validation/,
    );
    assert.doesNotMatch(
      signature(join(archive, "bin/alpha2")).entitlements,
      /disable-library-validation/,
    );
  },
);

test(
  "with library validation on, a library of another team does not load and the script fails",
  { skip: notMacos },
  (t) => {
    for (const set of [[], [entitlements("library-validation")]]) {
      const archive = archiveWithStandInHost(t);
      const signed = run(sign, [archive, "-", ...set]);
      assert.notEqual(signed.status, 0, signed.stdout);
      assert.match(signed.stderr, /gave no capability report/);
      assert.match(signed.stderr, /different Team IDs/);
      const verified = run("codesign", [
        "--verify",
        "--deep",
        "--strict",
        join(archive, "bin/alpha2-host"),
      ]);
      assert.equal(verified.status, 0, verified.stderr);
    }
  },
);

test(
  "a directory that is not an unpacked archive is refused before anything is signed",
  { skip: process.platform === "win32" && "the script runs under POSIX sh" },
  (t) => {
    const scratch = mkdtempSync(join(tmpdir(), "alpha2-sign-"));
    t.after(() => rmSync(scratch, { recursive: true, force: true }));
    const refused = run(sign, [scratch, "-"]);
    assert.notEqual(refused.status, 0);
    assert.match(
      refused.stderr,
      process.platform === "darwin"
        ? /bin\/alpha2-host is not there/
        : /macOS only/,
    );
  },
);
