// Where cargo writes its build outputs, which CARGO_TARGET_DIR or a cargo config can move, and the
// paths of the executables built there.

import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..", "..");

const directories = new Map();

/**
 * The cargo target directory of the workspace at `cwd` (default: this repository), as `cargo
 * metadata` reports it. Cached per `cwd`.
 */
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

/**
 * The path of executable `name` built with `profile` ("debug" or "release"), with `.exe` on
 * Windows. `target` defaults to the repository's cargo target directory.
 */
export function executable(
  name,
  profile,
  { target = cargoTargetDirectory(), platform = process.platform } = {},
) {
  return join(target, profile, name + (platform === "win32" ? ".exe" : ""));
}
