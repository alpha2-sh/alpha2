#!/usr/bin/env node

// Test browser-only wasm paths in a browser: context loss, video conversion and memory, and the
// packaged engine against native pins. Run `npm run build` first. Chrome unless `--browser
// safari|firefox` (or ALPHA2_BROWSER) picks a WebDriver browser. Missing Chrome skips the run;
// `--require-browser` or ALPHA2_REQUIRE_BROWSER=1 fails it. `--case <name>` selects one case;
// `--serve[=port]` serves pages for manual runs. Cases are in scripts/lib/wasm-browser/.

import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join, resolve } from "node:path";

import { browserForRun, runBrowserPage } from "./lib/browser-page.mjs";
import { probeRun } from "./lib/browser-probe.mjs";
import {
  reportSkippedBrowser,
  skippedCasesSummary,
  fromThisMachine,
  servedFile,
} from "./lib/chrome-cdp.mjs";
import { cases, videoImportCases } from "./lib/wasm-browser/cases.mjs";
import { page } from "./lib/wasm-browser/page.mjs";
import { runCases } from "./lib/wasm-browser/run-cases.mjs";

const root = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const only = args.includes("--case") ? args[args.indexOf("--case") + 1] : null;
const serveArgument = args.find((arg) => arg.startsWith("--serve"));
// A skip is the machine's (headless or SwiftShader cannot import a VideoFrame); `--no-skips` or
// ALPHA2_NO_SKIPS=1 makes one a failure.
const noSkips =
  args.includes("--no-skips") || process.env.ALPHA2_NO_SKIPS === "1";
const servePort =
  serveArgument === undefined ? null : Number(serveArgument.split("=")[1] ?? 0);

let browser = null;
if (servePort === null) {
  try {
    browser = browserForRun({ argv: args });
  } catch (error) {
    process.stdout.write(`FAIL ${error.message}\n`);
    process.exit(1);
  }
  if (browser === null) {
    reportSkippedBrowser("the wasm browser checks");
    process.exit(0);
  }
}
if (!existsSync(join(root, "packages/alpha2/wasm/alpha2_wasm_bg.wasm"))) {
  process.stdout.write(
    "FAIL the packaged wasm is missing; run npm run build\n",
  );
  process.exit(1);
}

const server = createServer(async (request, response) => {
  if (!fromThisMachine(request.headers.host, server.address().port)) {
    response.writeHead(403).end();
    return;
  }
  const url = new URL(request.url ?? "/", "http://localhost");
  response.setHeader("cross-origin-opener-policy", "same-origin");
  response.setHeader("cross-origin-embedder-policy", "require-corp");
  const name = url.pathname.slice(1);
  if (Object.hasOwn(cases, name)) {
    response.setHeader("content-type", "text/html; charset=utf-8");
    response.end(page(cases[name](), videoImportCases.has(name)));
    return;
  }
  const path = servedFile(root, url.pathname);
  if (path === null) {
    response.writeHead(403).end();
    return;
  }
  try {
    const bytes = await readFile(path);
    const type = {
      ".wasm": "application/wasm",
      ".js": "text/javascript",
      ".mjs": "text/javascript",
      ".json": "application/json",
      ".png": "image/png",
      ".mp4": "video/mp4",
      ".webm": "video/webm",
    }[path.slice(path.lastIndexOf("."))];
    response.setHeader("content-type", type ?? "application/octet-stream");
    response.end(bytes);
  } catch {
    response.writeHead(404).end();
  }
});
await new Promise((resolveListen) =>
  server.listen(servePort ?? 0, "127.0.0.1", resolveListen),
);
if (servePort !== null) {
  // Cross-origin isolation, which the audio cases need, requires a secure context: localhost.
  const { port } = server.address();
  for (const name of Object.keys(cases))
    process.stdout.write(`http://localhost:${port}/${name}\n`);
  process.stdout.write("serving; stop with Ctrl-C\n");
  await new Promise(() => {});
}
let failures = 0;
const skipped = [];
try {
  const address = server.address();
  if (typeof address !== "object" || address === null)
    throw new Error("missing server address");
  // The run's first launch: a browser that cannot start ends the run here.
  await probeRun("wasm-browser", browser, {
    engineModule: "/packages/alpha2/wasm/alpha2_wasm.js",
  });
  const names = Object.keys(cases).filter(
    (name) => only === null || name === only,
  );
  const runPage = async (name) => {
    const encoded = await runBrowserPage(
      browser,
      `http://127.0.0.1:${address.port}/${name}${process.env.ALPHA2_CASE_QUERY ?? ""}`,
      { timeoutMs: Number(process.env.ALPHA2_CASE_TIMEOUT_MS ?? 60_000) },
    );
    return JSON.parse(decodeURIComponent(encoded));
  };
  const report = (name, result) => {
    if (result.skipped && noSkips) {
      failures += 1;
      process.stdout.write(
        `FAIL ${name}: skipped where every case must run: ${result.skipped}\n`,
      );
    } else if (result.skipped) {
      skipped.push({ name, reason: result.skipped });
      process.stdout.write(`skip ${name}: ${result.skipped}\n`);
    } else if (result.error || result.ok !== true) {
      failures += 1;
      process.stdout.write(
        `FAIL ${name}: ${result.error ?? JSON.stringify(result)}\n`,
      );
    } else {
      const { ok: _, ...details } = result;
      process.stdout.write(
        `ok ${name}${Object.keys(details).length ? ` ${JSON.stringify(details)}` : ""}\n`,
      );
    }
  };
  const stopped = await runCases(names, runPage, report);
  for (const line of stopped) process.stdout.write(`${line}\n`);
  if (stopped.length > 0) failures += 1;
} finally {
  await new Promise((resolveClose) => server.close(resolveClose));
}
process.stdout.write(skippedCasesSummary(skipped));
process.exit(failures === 0 ? 0 : 1);
