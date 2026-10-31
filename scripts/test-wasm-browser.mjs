#!/usr/bin/env node

// Test browser-only wasm paths in a browser: context loss, video conversion and memory, and the
// packaged engine against native pins. Run `npm run build` first. Chrome unless `--browser
// edge|safari|firefox` (or ALPHA2_BROWSER) picks another. Missing Chrome skips the run,
// and a case that cannot run in this browser skips naming its capability; ALPHA2_REQUIRE fails
// the capabilities it names. `--case <name>` selects one case (perf cases, such as export-perf,
// run only this way); `--serve[=port]` serves pages for manual runs. Cases are in
// scripts/lib/wasm-browser/.

import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join, resolve } from "node:path";

import { browserForRun, runBrowserPage } from "./lib/browser-page.mjs";
import { probeRun } from "./lib/browser-probe.mjs";
import { recordSkip } from "./lib/capabilities.mjs";
import {
  gpuProcessEnvironment,
  reportSkippedBrowser,
  skippedCasesSummary,
  fromThisMachine,
  servedFile,
} from "./lib/chrome-cdp.mjs";
import {
  cases,
  pageDrivers,
  caseLaunch,
  perfCases,
  videoImportCases,
} from "./lib/wasm-browser/cases.mjs";
import { page } from "./lib/wasm-browser/page.mjs";
import { runCases } from "./lib/wasm-browser/run-cases.mjs";

const root = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const only = args.includes("--case") ? args[args.indexOf("--case") + 1] : null;
const serveArgument = args.find((arg) => arg.startsWith("--serve"));
const servePort =
  serveArgument === undefined ? null : Number(serveArgument.split("=")[1] ?? 0);

// A perf case runs only when `--case` names it.
const names =
  only === null
    ? Object.keys(cases)
    : [...Object.keys(cases), ...Object.keys(perfCases)].filter(
        (name) => name === only,
      );
if (names.length === 0) {
  process.stdout.write(`FAIL no case named ${only}\n`);
  process.exit(1);
}

let browser = null;
if (servePort === null) {
  try {
    browser = browserForRun({ argv: args });
  } catch (error) {
    process.stdout.write(`FAIL ${error.message}\n`);
    process.exit(1);
  }
  if (browser === null) {
    reportSkippedBrowser("the wasm browser checks", "wasm-browser");
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
  const body = Object.hasOwn(cases, name)
    ? cases[name]
    : Object.hasOwn(perfCases, name)
      ? perfCases[name]
      : undefined;
  if (body !== undefined) {
    response.setHeader("content-type", "text/html; charset=utf-8");
    response.end(page(body(), videoImportCases.has(name)));
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
  for (const name of [...Object.keys(cases), ...Object.keys(perfCases)])
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
  const runPage = async (name) => {
    // A page whose GPU process restarted reports the environment, whatever it reported itself.
    let gpuProcess;
    let encoded;
    try {
      encoded = await runBrowserPage(
        browser,
        `http://127.0.0.1:${address.port}/${name}${process.env.ALPHA2_CASE_QUERY ?? ""}`,
        {
          // A perf case exports for minutes on a slow GPU.
          timeoutMs: Number(
            process.env.ALPHA2_CASE_TIMEOUT_MS ??
              (Object.hasOwn(perfCases, name)
                ? 600_000
                : (caseLaunch[name]?.timeoutMs ?? 60_000)),
          ),
          ...(Object.hasOwn(pageDrivers, name)
            ? { onPage: pageDrivers[name] }
            : {}),
          ...(caseLaunch[name]?.domainBlocking3d
            ? { domainBlocking3d: true }
            : {}),
          onGpuProcess: (record) => (gpuProcess = record),
        },
      );
    } catch (error) {
      const marked = gpuProcessEnvironment(gpuProcess, {
        error: error.message,
      });
      if (marked === null) throw error;
      return marked;
    }
    const result = JSON.parse(decodeURIComponent(encoded));
    // A case that crashes the GPU process itself counts only the exits it did not cause: more of
    // them restarted the GPU process under the page (a Windows session 0 does at launch), which is
    // the environment's, as for any other case. The page's own result says whether it recovered.
    const caused = caseLaunch[name]?.gpuProcessExits;
    if (caused !== undefined && gpuProcess !== undefined) {
      if (gpuProcess.exits <= caused) return { ...result, gpuProcess };
      return (
        gpuProcessEnvironment(
          { ...gpuProcess, exits: gpuProcess.exits - caused },
          result,
        ) ?? result
      );
    }
    return gpuProcessEnvironment(gpuProcess, result) ?? result;
  };
  const report = (name, result) => {
    if (result.skipped && !result.capability) {
      failures += 1;
      process.stdout.write(
        `FAIL ${name}: a case skipped without naming its capability: ${result.skipped}\n`,
      );
    } else if (result.skipped) {
      try {
        recordSkip({
          suite: "wasm-browser",
          test: name,
          capability: result.capability,
          reason: result.skipped,
        });
        skipped.push({ name, reason: result.skipped });
        process.stdout.write(
          `skip ${name}: [${result.capability}] ${result.skipped}${result.page === undefined ? "" : ` (the page: ${JSON.stringify(result.page)})`}\n`,
        );
      } catch (error) {
        failures += 1;
        process.stdout.write(`FAIL ${name}: ${error.message}\n`);
      }
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
