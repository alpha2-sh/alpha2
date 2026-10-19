// The release compiler check prints the pinned `rustc -vV` from any directory and refuses others.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "../..");
const checkToolchain = join(root, "scripts/lib/check-rust-toolchain.sh");
const onWindows = process.platform === "win32";

function read(path) {
  return readFileSync(join(root, path), "utf8").replaceAll("\r\n", "\n");
}

const pin = /^channel = "(.+)"$/m.exec(read("rust-toolchain.toml"))?.[1];

function check(options) {
  return onWindows
    ? spawnSync("bash", [checkToolchain], { encoding: "utf8", ...options })
    : spawnSync(checkToolchain, [], { encoding: "utf8", ...options });
}

test("the toolchain check prints rustc -vV of the pinned release, even started outside the checkout", () => {
  const elsewhere = mkdtempSync(join(tmpdir(), "alpha2-rust-toolchain-"));
  try {
    const env = { ...process.env };
    delete env.RUSTUP_TOOLCHAIN;
    const result = check({ cwd: elsewhere, env });
    assert.equal(result.status, 0, result.stderr);
    assert.match(
      result.stdout,
      new RegExp(`^release: ${pin.replaceAll(".", "\\.")}$`, "m"),
    );
    assert.match(result.stdout, /^host: \S+$/m);
    assert.match(result.stdout, /^commit-hash: \S+$/m);
  } finally {
    rmSync(elsewhere, { recursive: true, force: true });
  }
});

test(
  "the toolchain check refuses a compiler of another release",
  { skip: onWindows && "the stand-in rustc is a shell script" },
  () => {
    const bin = mkdtempSync(join(tmpdir(), "alpha2-rust-toolchain-"));
    try {
      const rustc = join(bin, "rustc");
      writeFileSync(
        rustc,
        '#!/bin/sh\nprintf "rustc 1.100.0 (0000000 2026-11-05)\\nbinary: rustc\\nrelease: 1.100.0\\n"\n',
      );
      chmodSync(rustc, 0o755);
      const result = check({
        env: { ...process.env, PATH: `${bin}${delimiter}${process.env.PATH}` },
      });
      assert.notEqual(result.status, 0, result.stdout);
      assert.equal(result.stdout, "");
      assert.match(
        result.stderr,
        new RegExp(
          `rustc 1\\.100\\.0 is not the pinned Rust ${pin.replaceAll(".", "\\.")}`,
        ),
      );
    } finally {
      rmSync(bin, { recursive: true, force: true });
    }
  },
);
