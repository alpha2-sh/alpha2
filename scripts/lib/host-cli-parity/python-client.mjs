// The Python client's check: the `alpha2` Python package, asked for the parity export, writes the
// one-shot CLI's bytes. It needs a Python that imports `alpha2` (built from python/alpha2 with
// maturin): ALPHA2_PARITY_PYTHON, else python/alpha2/.venv's. Without one the check is skipped;
// ALPHA2_REQUIRE_PYTHON=1 fails instead.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";

import {
  assetPath,
  executable,
  PINNED_RATE,
  projectPath,
  root,
} from "./fixtures.mjs";

// The request the `serve` check sends, in the Python client's words.
const SCRIPT = `
import json, sys
import alpha2
host, project_path, asset, output, bitrate = sys.argv[1:]
project = json.load(open(project_path, encoding="utf-8"))
alpha2.HostClient(host_path=host).export(
    project,
    output,
    assets={"image-source": asset},
    start_ticks=0,
    end_ticks=5_000,
    frame_rate=(24, 1),
    include_audio=False,
    video_codec="mpeg4-part2",
    video_quality={"bitrate": int(bitrate)},
    audio_codec="aac",
)
`;

/** The Python to run the client with, or `null` when there is none that imports `alpha2`. */
function pythonForCheck() {
  const venv = join(
    root,
    "python",
    "alpha2",
    ".venv",
    process.platform === "win32" ? "Scripts\\python.exe" : "bin/python",
  );
  const python =
    process.env.ALPHA2_PARITY_PYTHON ?? (existsSync(venv) ? venv : null);
  if (python !== null) {
    const probe = spawnSync(python, ["-c", "import alpha2"], { cwd: root });
    if (probe.status === 0) return python;
  }
  if (process.env.ALPHA2_REQUIRE_PYTHON === "1")
    throw new Error(
      "no Python imports alpha2; build python/alpha2 (maturin develop) or set ALPHA2_PARITY_PYTHON",
    );
  return null;
}

/**
 * Exports the parity project through the Python client and compares it with `cliOutput`. Returns
 * false when the check was skipped for want of a Python.
 */
export async function pythonClientCheck(scratch, cliOutput) {
  const python = pythonForCheck();
  if (python === null) return false;
  const output = join(scratch, "python-client.mp4");
  const run = spawnSync(
    python,
    [
      "-c",
      SCRIPT,
      executable,
      projectPath,
      assetPath,
      output,
      String(PINNED_RATE.bitrate),
    ],
    { cwd: root, stdio: ["ignore", "inherit", "inherit"] },
  );
  assert.equal(run.status, 0, `the Python client exited ${run.status}`);
  assert.deepEqual(
    await readFile(output),
    await readFile(cliOutput),
    "the Python client and the one-shot CLI must produce byte-identical output",
  );
  console.log("verified byte-identical Python client/export output");
  return true;
}
