// The first launch of every browser run: one page that reports what the browser is and what GPU it
// gave the page, printed as one `browser-run:` line and kept as JSON under target/browser-reports.
// A browser that cannot start fails here, before any case.

import { mkdirSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join, resolve } from "node:path";

import { runBrowserPage } from "./browser-page.mjs";
import {
  browserSettingsMode,
  chromeGpuMode,
  fromThisMachine,
  servedFile,
} from "./chrome-cdp.mjs";

const root = resolve(import.meta.dirname, "..", "..");

/**
 * The probe page. With `engineModule` (a served path of the packaged wasm engine) it also opens an
 * engine on an `OffscreenCanvas` and reports the adapter the engine chose.
 *
 * @param {{ engineModule?: string }} [options]
 * @returns {string}
 */
export function probePage({ engineModule } = {}) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>pending</title></head><body>
<script type="module">
const report = (value) => {
  document.title = encodeURIComponent(JSON.stringify(value));
};
const ENGINE_MODULE = ${JSON.stringify(engineModule ?? null)};
const result = { ua: navigator.userAgent, crossOriginIsolated: globalThis.crossOriginIsolated === true };
try {
  result.webgpu = { present: "gpu" in navigator, adapter: null };
  const adapter = await navigator.gpu?.requestAdapter();
  if (adapter) {
    const info = adapter.info ?? {};
    result.webgpu.adapter = {
      vendor: info.vendor ?? "",
      architecture: info.architecture ?? "",
      device: info.device ?? "",
      description: info.description ?? "",
      isFallbackAdapter: Boolean(info.isFallbackAdapter ?? adapter.isFallbackAdapter),
      features: [...adapter.features].sort(),
      limits: {
        maxTextureDimension2D: adapter.limits.maxTextureDimension2D,
        maxBufferSize: adapter.limits.maxBufferSize,
        maxStorageBufferBindingSize: adapter.limits.maxStorageBufferBindingSize,
        maxBindGroups: adapter.limits.maxBindGroups,
      },
    };
  }
} catch (error) {
  result.webgpu = { present: "gpu" in navigator, adapter: null, error: String(error?.message ?? error) };
}
try {
  const gl = new OffscreenCanvas(1, 1).getContext("webgl2");
  const debug = gl?.getExtension("WEBGL_debug_renderer_info");
  result.webgl2 = { renderer: gl ? String(gl.getParameter(debug ? debug.UNMASKED_RENDERER_WEBGL : gl.RENDERER)) : null };
} catch (error) {
  result.webgl2 = { renderer: null, error: String(error?.message ?? error) };
}
const supported = async (probe) => {
  try {
    return (await probe())?.supported === true;
  } catch {
    return false;
  }
};
result.webcodecs = {
  h264Decode: await supported(() => globalThis.VideoDecoder?.isConfigSupported({ codec: "avc1.640028", hardwareAcceleration: "prefer-hardware" })),
  aacEncode: await supported(() => globalThis.AudioEncoder?.isConfigSupported({ codec: "mp4a.40.2", sampleRate: 48000, numberOfChannels: 2 })),
};
if (ENGINE_MODULE === null) {
  result.engine = "not-loaded";
} else {
  try {
    const { default: init, WebEffectRegistry } = await import(ENGINE_MODULE);
    await init();
    const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
    const adapter = JSON.parse(engine.diagnostics()).adapter ?? {};
    result.engine = { backend: adapter.backend ?? "", name: adapter.name ?? "", driver: adapter.driver ?? "" };
  } catch (error) {
    result.engine = { failed: String(error?.message ?? error) };
  }
}
report(result);
</script></body></html>`;
}

/**
 * Runs the probe page in `browser` (from `browserForRun`) and returns its report with the run's
 * browser settings. The page is served by a server of its own, with cross-origin isolation and the
 * repository's files for the engine module. Rejects with a `BrowserStartError` when the browser
 * cannot start.
 *
 * @param {NonNullable<ReturnType<import("./browser-page.mjs").browserForRun>>} browser
 * @param {{ engineModule?: string, env?: NodeJS.ProcessEnv }} [options]
 */
export async function probeBrowser(
  browser,
  { engineModule, env = process.env } = {},
) {
  const server = createServer(async (request, response) => {
    if (!fromThisMachine(request.headers.host, server.address().port)) {
      response.writeHead(403).end();
      return;
    }
    const url = new URL(request.url ?? "/", "http://localhost");
    response.setHeader("cross-origin-opener-policy", "same-origin");
    response.setHeader("cross-origin-embedder-policy", "require-corp");
    if (url.pathname === "/__probe") {
      response.setHeader("content-type", "text/html; charset=utf-8");
      response.end(probePage({ engineModule }));
      return;
    }
    const path = servedFile(root, url.pathname);
    if (path === null) {
      response.writeHead(403).end();
      return;
    }
    try {
      const bytes = await readFile(path);
      response.setHeader(
        "content-type",
        path.endsWith(".wasm")
          ? "application/wasm"
          : /\.m?js$/.test(path)
            ? "text/javascript"
            : "application/octet-stream",
      );
      response.end(bytes);
    } catch {
      response.writeHead(404).end();
    }
  });
  await new Promise((resolveListen) =>
    server.listen(0, "127.0.0.1", resolveListen),
  );
  let version = "";
  try {
    const { port } = server.address();
    const encoded = await runBrowserPage(
      browser,
      `http://127.0.0.1:${port}/__probe`,
      {
        timeoutMs: 60_000,
        onBrowser: (found) => {
          version =
            found.product?.replace(/^[^/]*\//, "") ??
            found.browserVersion ??
            "";
        },
      },
    );
    const page = JSON.parse(decodeURIComponent(encoded));
    return { ...page, browser: browserSettings(browser, version, env) };
  } finally {
    await new Promise((resolveClose) => server.close(resolveClose));
  }
}

/** The settings a run launches `browser` with, as the report records them. */
export function browserSettings(browser, version, env = process.env) {
  const chrome = browser.kind === "chrome";
  const gpu = chrome ? chromeGpuMode({ env }) : { mode: "n/a", flags: [] };
  return {
    name: browser.name,
    version,
    path: chrome ? browser.command : browser.webdriver.driver,
    // Safari has no headless mode.
    headed: browser.name === "safari" || env.ALPHA2_BROWSER_HEADED === "1",
    gpu: gpu.mode,
    settings: browserSettingsMode(env),
    flags: gpu.flags,
  };
}

/** A value of the adapter line: JSON-quoted when it holds spaces or quotes or is empty. */
function lineValue(value) {
  const text = String(value);
  return text === "" || /[\s"]/.test(text) ? JSON.stringify(text) : text;
}

/**
 * The one line a run prints about its browser, every field in a fixed order.
 *
 * @param {Awaited<ReturnType<typeof probeBrowser>>} report
 * @returns {string}
 */
export function adapterLine(report) {
  const { browser } = report;
  const adapter = report.webgpu?.adapter ?? null;
  const webgpu =
    adapter === null
      ? "none"
      : [
          adapter.vendor,
          adapter.architecture,
          adapter.device,
          adapter.description,
        ]
          .filter(Boolean)
          .join(" ");
  const engine = report.engine;
  const engineName =
    engine === "not-loaded" || engine === undefined
      ? "not-loaded"
      : engine.failed !== undefined
        ? "failed"
        : engine.backend || "failed";
  const engineAdapter =
    typeof engine === "object" && engine !== null && engine.failed === undefined
      ? `${engine.name} (${engine.driver})`
      : "";
  const fields = [
    ["browser", browser.name],
    ["version", browser.version],
    ["mode", browser.headed ? "headed" : "headless"],
    ["gpu", browser.gpu],
    ["settings", browser.settings],
    ["flags", JSON.stringify(browser.flags.join(" "))],
    ["webgpu", webgpu],
    [
      "fallback",
      adapter === null ? "n/a" : String(adapter.isFallbackAdapter === true),
    ],
    ["webgl2", JSON.stringify(report.webgl2?.renderer ?? "none")],
    ["engine", engineName],
    ["engine-adapter", JSON.stringify(engineAdapter)],
  ];
  return `browser-run: ${fields
    .map(([key, value]) =>
      key === "flags" || key === "webgl2" || key === "engine-adapter"
        ? `${key}=${value}`
        : `${key}=${lineValue(value)}`,
    )
    .join(" ")}`;
}

/** Hints for a report whose browser gave no WebGPU adapter; empty when it gave one. */
export function adapterHints(report, platform = process.platform) {
  if (report.webgpu?.adapter) return [];
  if (report.browser.gpu === "swiftshader")
    return [
      "SwiftShader gave no WebGPU adapter; set ALPHA2_CHROME_GPU=machine on this OS",
    ];
  if (platform === "win32" && report.browser.gpu === "machine")
    return [
      "no WebGPU adapter: a session-0 (SSH) run with stock settings withholds WebGPU; run in a console session",
    ];
  return [];
}

/**
 * Writes the report as `<harness>-<browser>.json` under `ALPHA2_BROWSER_REPORT_DIR`, by default
 * target/browser-reports. Returns the path.
 */
export function writeBrowserReport(harness, report, env = process.env) {
  const directory =
    env.ALPHA2_BROWSER_REPORT_DIR ?? join(root, "target", "browser-reports");
  mkdirSync(directory, { recursive: true });
  const path = join(directory, `${harness}-${report.browser.name}.json`);
  writeFileSync(path, `${JSON.stringify(report, null, 2)}\n`);
  return path;
}

/**
 * The probe as a harness's first step: prints the adapter line and its hints, writes the report,
 * and returns it. A browser that cannot start prints `FAIL browser could not start: ...` and ends
 * the process with status 1, before any case.
 */
export async function probeRun(harness, browser, options = {}) {
  let report;
  try {
    report = await probeBrowser(browser, options);
  } catch (error) {
    process.stdout.write(`FAIL browser could not start: ${error.message}\n`);
    process.exit(1);
  }
  process.stdout.write(`${adapterLine(report)}\n`);
  for (const hint of adapterHints(report)) process.stdout.write(`  ${hint}\n`);
  writeBrowserReport(harness, report);
  return report;
}
