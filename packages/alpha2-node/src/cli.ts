#!/usr/bin/env node
import { parseArgs } from "node:util";

import { Alpha2HostInstallError, installHost } from "./install.js";

const USAGE = `usage: alpha2-node install [--thin] [--force]

Downloads alpha2-host for this package's version and platform into the cache
(ALPHA2_HOST_CACHE_DIR, else the platform's cache directory), where
@alpha2-sh/node finds it.

  --thin   the archive that loads the system's FFmpeg 8 instead of its own
  --force  download and unpack again when it is already installed`;

async function main(): Promise<number> {
  let parsed;
  try {
    parsed = parseArgs({
      allowPositionals: true,
      options: {
        thin: { type: "boolean" },
        force: { type: "boolean" },
        help: { type: "boolean", short: "h" },
      },
    });
  } catch (error) {
    console.error(`${(error as Error).message}\n\n${USAGE}`);
    return 2;
  }
  const { values, positionals } = parsed;
  if (values.help) {
    console.log(USAGE);
    return 0;
  }
  if (positionals.length !== 1 || positionals[0] !== "install") {
    console.error(USAGE);
    return 2;
  }

  // Progress goes to standard error, in whole percents, and only to a terminal.
  let shown = -1;
  try {
    const result = await installHost({
      kind: values.thin ? "thin" : "bundled",
      ...(values.force ? { force: true } : {}),
      onProgress: ({ received, total }) => {
        if (!process.stderr.isTTY || total === undefined) return;
        const percent = Math.floor((received / total) * 100);
        if (percent === shown) return;
        shown = percent;
        process.stderr.write(`\rdownloading alpha2-host ${percent}%`);
      },
    });
    if (shown >= 0) process.stderr.write("\n");
    console.log(
      result.downloaded
        ? `installed ${result.hostPath}`
        : `already installed: ${result.hostPath}`,
    );
    return 0;
  } catch (error) {
    if (shown >= 0) process.stderr.write("\n");
    if (!(error instanceof Alpha2HostInstallError)) throw error;
    const cause =
      error.cause instanceof Error ? ` (${error.cause.message})` : "";
    console.error(`alpha2-node install: ${error.message}${cause}`);
    return 1;
  }
}

process.exitCode = await main();
