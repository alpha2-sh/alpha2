// Host CLI parity: the native host's encoded outputs agree byte for byte across CLI, `serve`, the
// Python client, pipelined/synchronous and every decode concurrency, and the packaged browser
// engine draws what the native one does. Without Chrome only the native checks run, and without a
// Python that imports `alpha2` the Python client's is skipped; ALPHA2_REQUIRE_BROWSER=1 and
// ALPHA2_REQUIRE_PYTHON=1 fail instead. The checks are in scripts/lib/host-cli-parity/.

import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { probeRun } from "./lib/browser-probe.mjs";
import { browserForCheck, reportSkippedBrowser } from "./lib/chrome-cdp.mjs";
import { browserChecks } from "./lib/host-cli-parity/browser-checks.mjs";
import { projectPath } from "./lib/host-cli-parity/fixtures.mjs";
import { nativeFrames } from "./lib/host-cli-parity/native-frames.mjs";
import { nativeOutputChecks } from "./lib/host-cli-parity/native-outputs.mjs";
import { pythonClientCheck } from "./lib/host-cli-parity/python-client.mjs";

async function main() {
  // Decided first, so a run requiring the browser fails before the native checks.
  const chrome = browserForCheck();
  if (chrome !== null)
    await probeRun(
      "host-parity",
      { kind: "chrome", name: "chrome", command: chrome },
      { engineModule: "/packages/alpha2/wasm/alpha2_wasm.js" },
    );
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-host-parity-"));
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  let pythonChecked;
  try {
    const cliOutput = await nativeOutputChecks(scratch, project);
    pythonChecked = await pythonClientCheck(scratch, cliOutput);
    const frames = await nativeFrames(scratch, cliOutput);
    if (chrome !== null) await browserChecks(chrome, frames);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
  // Reported last, so the skips are the final lines of the run's output.
  if (!pythonChecked)
    process.stderr.write(
      "skipped the Python client's parity: no Python imports alpha2; build python/alpha2 with maturin, or set ALPHA2_PARITY_PYTHON\n",
    );
  if (chrome === null)
    reportSkippedBrowser(
      "browser parity (golden, complex, shapes, effects, HDR, extensions, orientation, sink); only the native checks ran",
    );
}

await main();
