#!/usr/bin/env node
import { parseArgs } from "node:util";

import {
  Alpha2HostInstallError,
  OPENH264_LICENSE_URL,
  installHost,
  installOpenH264,
} from "./install.js";

const USAGE = `usage: alpha2-node install [--openh264] [--force]

Downloads alpha2-host for this package's version and platform into the cache
(ALPHA2_HOST_CACHE_DIR, else the platform's cache directory), where
@alpha2-sh/node finds it.

  --openh264  download Cisco's OpenH264 library instead, the software H.264
              encoder alpha2-host loads when no hardware encoder is available
  --force     download and unpack again when it is already installed`;

async function main(): Promise<number> {
  let parsed;
  try {
    parsed = parseArgs({
      allowPositionals: true,
      options: {
        openh264: { type: "boolean" },
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
  const what = values.openh264 ? "OpenH264" : "alpha2-host";
  let shown = -1;
  const onProgress = ({
    received,
    total,
  }: {
    received: number;
    total: number | undefined;
  }) => {
    if (!process.stderr.isTTY || total === undefined) return;
    const percent = Math.floor((received / total) * 100);
    if (percent === shown) return;
    shown = percent;
    process.stderr.write(`\rdownloading ${what} ${percent}%`);
  };
  try {
    if (values.openh264) {
      const result = await installOpenH264({
        ...(values.force ? { force: true } : {}),
        onProgress,
      });
      if (shown >= 0) process.stderr.write("\n");
      console.log(
        result.downloaded
          ? `installed OpenH264 ${result.version}: ${result.libraryPath}`
          : `already installed: ${result.libraryPath}`,
      );
      // Cisco's licence asks for its notice wherever the user controls the codec.
      console.log(result.notice);
      console.log(
        `It is Cisco's binary, under Cisco's terms: ${OPENH264_LICENSE_URL}`,
      );
      console.log(
        "alpha2-host uses it for H.264 when no hardware encoder is available; " +
          "set ALPHA2_OPENH264=0 to turn it off.",
      );
      return 0;
    }
    const result = await installHost({
      ...(values.force ? { force: true } : {}),
      onProgress,
    });
    if (shown >= 0) process.stderr.write("\n");
    console.log(
      result.downloaded
        ? `installed ${result.hostPath} (checked against ` +
            (result.verifiedBy === "release-pin"
              ? "the SHA-256 this package pins)"
              : "the release's checksums.sha256)")
        : `already installed: ${result.hostPath}`,
    );
    return 0;
  } catch (error) {
    if (shown >= 0) process.stderr.write("\n");
    if (!(error instanceof Alpha2HostInstallError)) throw error;
    // The message already carries the cause chain and the remedy.
    console.error(`alpha2-node install: ${error.message}`);
    return 1;
  }
}

process.exitCode = await main();
