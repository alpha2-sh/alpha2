// Native outputs must match byte for byte across CLI, `serve`, the Python client and decode
// concurrency, and the packaged browser engine must draw what the native one does. Checks are in
// scripts/lib/host-cli-parity/.

import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { browserForRun } from "./lib/browser-page.mjs";
import { probeRun } from "./lib/browser-probe.mjs";
import { reportSkippedBrowser } from "./lib/chrome-cdp.mjs";
import { browserChecks } from "./lib/host-cli-parity/browser-checks.mjs";
import { projectPath } from "./lib/host-cli-parity/fixtures.mjs";
import { nativeFrames } from "./lib/host-cli-parity/native-frames.mjs";
import { nativeOutputChecks } from "./lib/host-cli-parity/native-outputs.mjs";
import { pythonClientCheck } from "./lib/host-cli-parity/python-client.mjs";

async function main() {
  const browser = browserForRun({ argv: process.argv.slice(2) });
  if (browser !== null)
    await probeRun("host-parity", browser, {
      engineModule: "/packages/alpha2/wasm/alpha2_wasm.js",
    });
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-host-parity-"));
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  let pythonChecked;
  try {
    const cliOutput = await nativeOutputChecks(scratch, project);
    pythonChecked = await pythonClientCheck(scratch, cliOutput);
    const frames = await nativeFrames(scratch, cliOutput);
    if (browser !== null) await browserChecks(browser, frames);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
  if (!pythonChecked)
    process.stderr.write(
      "skipped the Python client's parity: no Python imports alpha2; build python/alpha2 with maturin, or set ALPHA2_PARITY_PYTHON\n",
    );
  if (browser === null)
    reportSkippedBrowser(
      "browser parity (golden, complex, shapes, effects, HDR, extensions, orientation, sink); only the native checks ran",
      "host-cli-parity",
    );
}

await main();
