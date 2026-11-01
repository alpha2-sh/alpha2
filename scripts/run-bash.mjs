#!/usr/bin/env node
// Runs a bash script from an npm script on every OS: `bash` on POSIX, Git for Windows' bash on
// Windows (never the WSL `bash.exe` that PATH may find first).
// usage: run-bash.mjs <script> [args...]

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";

const argv = process.argv.slice(2);
if (argv.length === 0) {
  process.stderr.write("usage: run-bash.mjs <script> [args...]\n");
  process.exit(2);
}

let bash = "bash";
if (process.platform === "win32") {
  bash = join(
    process.env.PROGRAMFILES ?? "C:\\Program Files",
    "Git",
    "bin",
    "bash.exe",
  );
  if (!existsSync(bash)) {
    process.stderr.write(
      `run-bash.mjs: ${bash} does not exist; install Git for Windows\n`,
    );
    process.exit(1);
  }
}
const result = spawnSync(bash, argv, { stdio: "inherit" });
if (result.error) throw result.error;
process.exit(result.status ?? 1);
