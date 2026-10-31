// CARGO_TARGET_DIR or a cargo config can move the target directory.

import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..", "..");

const directories = new Map();

export function cargoTargetDirectory({ cwd = root } = {}) {
  if (!directories.has(cwd))
    directories.set(
      cwd,
      JSON.parse(
        execFileSync(
          "cargo",
          ["metadata", "--format-version", "1", "--no-deps"],
          { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] },
        ),
      ).target_directory,
    );
  return directories.get(cwd);
}

export function executable(
  name,
  profile,
  { target = cargoTargetDirectory(), platform = process.platform } = {},
) {
  return join(target, profile, name + (platform === "win32" ? ".exe" : ""));
}
