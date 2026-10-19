// Host CLI parity: the native host's encoded outputs agree byte for byte across CLI, `serve`,
// pipelined/synchronous and every decode concurrency, and the packaged browser engine draws what
// the native one does. Without Chrome only the native checks run; ALPHA2_REQUIRE_BROWSER=1 fails
// instead. The checks are in scripts/lib/host-cli-parity/.

import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { browserForCheck, reportSkippedBrowser } from "./lib/chrome-cdp.mjs";
import { browserChecks } from "./lib/host-cli-parity/browser-checks.mjs";
import { projectPath } from "./lib/host-cli-parity/fixtures.mjs";
import { nativeFrames } from "./lib/host-cli-parity/native-frames.mjs";
import { nativeOutputChecks } from "./lib/host-cli-parity/native-outputs.mjs";

async function main() {
  // Decided first, so a run requiring the browser fails before the native checks.
  const chrome = browserForCheck();
  const scratch = await mkdtemp(join(tmpdir(), "alpha2-host-parity-"));
  const project = JSON.parse(await readFile(projectPath, "utf8"));
  try {
    const cliOutput = await nativeOutputChecks(scratch, project);
    const frames = await nativeFrames(scratch, cliOutput);
    if (chrome !== null) await browserChecks(chrome, frames);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
  // Reported last, so the skip is the final line of the run's output.
  if (chrome === null)
    reportSkippedBrowser(
      "browser parity (golden, complex, effects, HDR, extensions, orientation, sink); only the native checks ran",
    );
}

await main();
