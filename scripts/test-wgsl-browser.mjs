#!/usr/bin/env node

// Compile built-in pipelines, effects and transitions with a browser's WGSL compiler. Browser
// fragment-uniformity rules can reject shaders accepted by native naga. Chrome unless `--browser
// edge|safari|firefox` (or ALPHA2_BROWSER) picks another. Missing Chrome skips the run;
// ALPHA2_REQUIRE naming `browser` fails it.
//
// It also times what compiling every built-in costs the browser: one shader module and one render
// pipeline per built-in effect and transition, created one after another. The renderer makes each
// the first time it is drawn, so this is the most a project that draws all of them waits in total.
// Run it on the machine whose browser is in question.

import { spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { resolve } from "node:path";

import { browserForRun, runBrowserPage } from "./lib/browser-page.mjs";
import { probeRun } from "./lib/browser-probe.mjs";
import { reportSkippedBrowser } from "./lib/chrome-cdp.mjs";

const root = resolve(import.meta.dirname, "..");
let browser;
try {
  browser = browserForRun({ argv: process.argv.slice(2) });
} catch (error) {
  process.stdout.write(`FAIL ${error.message}\n`);
  process.exit(1);
}
if (browser === null) {
  reportSkippedBrowser("the WGSL compile check", "wgsl-browser");
  process.exit(0);
}
await probeRun("wgsl-browser", browser);

const listed = spawnSync(
  "cargo",
  ["run", "--quiet", "-p", "alpha2-render", "--example", "wgsl_modules"],
  { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
);
if (listed.status !== 0) {
  process.stdout.write(`FAIL listing WGSL modules: ${listed.stderr}\n`);
  process.exit(1);
}
const modules = listed.stdout;

const server = createServer((request, response) => {
  const url = new URL(request.url ?? "/", "http://localhost");
  if (url.pathname === "/modules.json") {
    response.setHeader("content-type", "application/json");
    response.end(modules);
    return;
  }
  if (url.pathname === "/") {
    response.setHeader("content-type", "text/html; charset=utf-8");
    response.end(page(browser.name));
    return;
  }
  response.writeHead(404).end();
});
await new Promise((resolveListen) =>
  server.listen(0, "127.0.0.1", resolveListen),
);
let result;
try {
  const address = server.address();
  if (typeof address !== "object" || address === null)
    throw new Error("missing server address");
  const encoded = await runBrowserPage(
    browser,
    `http://127.0.0.1:${address.port}/`,
    { timeoutMs: 120_000 },
  );
  result = JSON.parse(decodeURIComponent(encoded));
} finally {
  await new Promise((resolveClose) => server.close(resolveClose));
}

if (result.error) {
  process.stdout.write(`FAIL ${result.error}\n`);
  process.exit(1);
}
for (const failure of result.failures) {
  process.stdout.write(`FAIL ${failure.label}\n`);
  for (const message of failure.messages)
    process.stdout.write(`  ${message}\n`);
}
process.stdout.write(
  `${result.count - result.failures.length}/${result.count} WGSL modules compile in ${result.adapter}\n`,
);
if (result.registration)
  process.stdout.write(
    `compiling ${result.registration.count} built-in effects and transitions took ${result.registration.ms.toFixed(0)} ms (shader modules and pipelines, made at first use)\n`,
  );
process.exit(result.failures.length === 0 ? 0 : 1);

function page(browserName) {
  return `<!doctype html>
<meta charset="utf-8">
<title>pending</title>
<script type="module">
const report = (value) => {
  document.title = encodeURIComponent(JSON.stringify(value));
};
try {
  const modules = await (await fetch("/modules.json")).json();
  const adapter = await navigator.gpu?.requestAdapter();
  if (!adapter) throw new Error("this browser exposes no WebGPU adapter");
  const device = await adapter.requestDevice();
  const failures = [];
  for (const { label, source } of modules) {
    device.pushErrorScope("validation");
    const module = device.createShaderModule({ label, code: source });
    const info = await module.getCompilationInfo();
    const scoped = await device.popErrorScope();
    const messages = info.messages
      .filter((message) => message.type === "error")
      .map((message) => message.lineNum + ":" + message.linePos + " " + message.message);
    if (scoped && messages.length === 0) messages.push(scoped.message);
    if (messages.length > 0) failures.push({ label, messages });
  }
  // Every built-in as the renderer compiles it, timed once every module above compiled.
  let registration = null;
  if (failures.length === 0) {
    const effects = modules.filter(({ label }) => label.startsWith("alpha2."));
    const start = performance.now();
    for (const { label, source } of effects) {
      const module = device.createShaderModule({ label, code: source });
      await device.createRenderPipelineAsync({
        label,
        layout: "auto",
        vertex: { module, entryPoint: "alpha2_vs" },
        fragment: { module, entryPoint: "alpha2_fs", targets: [{ format: "rgba16float" }] },
        primitive: { topology: "triangle-list" },
      });
    }
    registration = { count: effects.length, ms: performance.now() - start };
  }
  const description = adapter.info
    ? [adapter.info.vendor, adapter.info.architecture, adapter.info.description]
        .filter(Boolean)
        .join(" ")
    : "";
  report({
    count: modules.length,
    failures,
    registration,
    adapter: ${JSON.stringify(browserName)} + " WebGPU" + (description ? " (" + description + ")" : ""),
  });
} catch (error) {
  report({ error: String(error?.stack ?? error) });
}
</script>`;
}
