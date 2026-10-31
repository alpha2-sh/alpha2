// Serves one parity page, the native frames it compares with and the repository's files, opens it
// in the run's browser, and returns what the page reported.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";

import { runBrowserPage } from "../browser-page.mjs";
import { recordSkip } from "../capabilities.mjs";
import {
  fromThisMachine,
  gpuProcessEnvironment,
  servedFile,
} from "../chrome-cdp.mjs";
import { root } from "./fixtures.mjs";
import { complexPage } from "./pages/complex.mjs";
import { extensionsPage } from "./pages/extensions.mjs";
import { hdrPage } from "./pages/hdr.mjs";
import { orientationPage } from "./pages/orientation.mjs";
import { parityPage } from "./pages/parity.mjs";
import { sinkPage } from "./pages/sink.mjs";

/**
 * Opens `page` in `browser` (from `browserForRun`) with the native frame and `files` served beside
 * it. A page has ALPHA2_CASE_TIMEOUT_MS, 120 s by default: the effects page draws over 600 frames.
 * A page whose engine ran on another backend than WebGPU is recorded as a `webgpu` skip (a failure
 * when the run requires it); its checks still run.
 */
export async function browserParity(
  browser,
  nativeFramePath,
  page = "/parity",
  files = {},
) {
  const routes = { "/native.png": nativeFramePath, ...files };
  const pages = {
    "/parity": parityPage,
    "/sink": sinkPage,
    "/complex": () =>
      complexPage("/tests/cases/host-parity-complex-project.json"),
    "/shapes": () =>
      complexPage("/tests/cases/host-parity-shapes-project.json"),
    "/layout": () =>
      complexPage("/tests/cases/host-parity-layout-project.json", [
        "/tests/fonts/Alpha2Fixture.ttf",
      ]),
    "/effects": () =>
      complexPage("/tests/cases/host-parity-effects-project.json"),
    "/hdr": hdrPage,
    "/extensions": extensionsPage,
    "/orientation": orientationPage,
  };
  const server = createServer(async (request, response) => {
    if (!fromThisMachine(request.headers.host, server.address().port)) {
      response.writeHead(403).end();
      return;
    }
    const url = new URL(request.url ?? "/", "http://localhost");
    response.setHeader("cross-origin-opener-policy", "same-origin");
    response.setHeader("cross-origin-embedder-policy", "require-corp");
    if (pages[url.pathname] !== undefined) {
      response.setHeader("content-type", "text/html; charset=utf-8");
      response.end(pages[url.pathname]());
      return;
    }
    const routed = routes[url.pathname];
    const path = routed ?? servedFile(root, url.pathname);
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
          : path.endsWith(".js")
            ? "text/javascript"
            : path.endsWith(".png")
              ? "image/png"
              : path.endsWith(".mjs")
                ? "text/javascript"
                : path.endsWith(".mp4")
                  ? "video/mp4"
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
  try {
    const address = server.address();
    if (typeof address !== "object" || address === null)
      throw new Error("missing parity server address");
    const url = `http://127.0.0.1:${address.port}${page}`;
    let gpuProcess;
    let result;
    try {
      const encoded = await runBrowserPage(browser, url, {
        timeoutMs: Number(process.env.ALPHA2_CASE_TIMEOUT_MS ?? 120_000),
        onGpuProcess: (record) => (gpuProcess = record),
      });
      result = JSON.parse(decodeURIComponent(encoded));
    } catch (error) {
      if (!gpuProcess?.restarted) throw error;
      result = { error: error.message };
    }
    // A page that lost its GPU process to the environment says nothing about the engine.
    const environment = gpuProcessEnvironment(gpuProcess, result);
    if (environment !== null) {
      recordSkip({
        suite: "host-cli-parity",
        test: page,
        capability: environment.capability,
        reason: environment.skipped,
      });
      throw new Error(`browser parity did not run: ${environment.skipped}`);
    }
    if (result.error) throw new Error(`browser parity failed: ${result.error}`);
    const backend = result.adapter?.backend;
    if (result.adapter !== undefined && backend !== "BrowserWebGpu")
      recordSkip({
        suite: "host-cli-parity",
        test: page,
        capability: "webgpu",
        reason: `the ${page} page's engine ran on ${backend || "no backend"}, not WebGPU`,
      });
    return result;
  } finally {
    await new Promise((resolveClose, reject) =>
      server.close((error) => (error ? reject(error) : resolveClose())),
    );
  }
}
