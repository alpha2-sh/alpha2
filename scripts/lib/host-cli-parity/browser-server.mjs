// Serves one parity page, the native frames it compares with and the repository's files, opens it
// in Chrome, and returns what the page reported.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";

import { fromThisMachine, runChromePage, servedFile } from "../chrome-cdp.mjs";
import { root } from "./fixtures.mjs";
import { complexPage } from "./pages/complex.mjs";
import { extensionsPage } from "./pages/extensions.mjs";
import { hdrPage } from "./pages/hdr.mjs";
import { orientationPage } from "./pages/orientation.mjs";
import { parityPage } from "./pages/parity.mjs";
import { sinkPage } from "./pages/sink.mjs";

export async function browserParity(
  chrome,
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
    const encoded = await runChromePage(chrome, url);
    const result = JSON.parse(decodeURIComponent(encoded));
    if (result.error) throw new Error(`browser parity failed: ${result.error}`);
    return result;
  } finally {
    await new Promise((resolveClose, reject) =>
      server.close((error) => (error ? reject(error) : resolveClose())),
    );
  }
}
