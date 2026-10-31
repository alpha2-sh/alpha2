#!/usr/bin/env node
// A static HTTP mirror of release downloads for install journeys: <dir> is laid out as the release
// base is (`v<version>/checksums.sha256`, `v<version>/<archive>`), so clients take its URL as
// ALPHA2_HOST_DOWNLOAD_URL. It prints its URL, then logs every request path (to --log when given).
//
// usage: serve-mirror.mjs <dir> [--port n] [--log file]
import { createReadStream, statSync, appendFileSync } from "node:fs";
import { createServer } from "node:http";
import { resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

/**
 * Serves the files under `root` on 127.0.0.1 (`port` 0 picks one) and resolves with its URL, the
 * paths requested so far and a `close`. `onRequest` also receives each path.
 */
export async function serveMirror(root, { port = 0, onRequest } = {}) {
  const base = resolve(root);
  const requests = [];
  const server = createServer((request, response) => {
    const path = decodeURIComponent(
      new URL(request.url, "http://mirror").pathname,
    );
    requests.push(path);
    onRequest?.(path);
    const file = resolve(base, `.${path}`);
    let size;
    try {
      if (!file.startsWith(base + sep)) throw new Error("outside the mirror");
      const info = statSync(file);
      if (!info.isFile()) throw new Error("not a file");
      size = info.size;
    } catch {
      response.writeHead(404, { "content-length": 0 }).end();
      return;
    }
    response.writeHead(200, { "content-length": size });
    createReadStream(file).pipe(response);
  });
  await new Promise((done) => server.listen(port, "127.0.0.1", done));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    requests,
    close: () =>
      new Promise((done) => {
        server.closeAllConnections();
        server.close(done);
      }),
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: { port: { type: "string" }, log: { type: "string" } },
  });
  if (positionals.length !== 1) {
    console.error("usage: serve-mirror.mjs <dir> [--port n] [--log file]");
    process.exit(2);
  }
  const mirror = await serveMirror(positionals[0], {
    port: Number(values.port ?? 0),
    onRequest: (path) => {
      if (values.log) appendFileSync(values.log, `${path}\n`);
      else console.log(path);
    },
  });
  console.log(mirror.url);
}
