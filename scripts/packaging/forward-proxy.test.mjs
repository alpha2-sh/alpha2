// The packaging journeys' helpers: the forward proxy (plain forwarding and CONNECT tunnels) and
// the static mirror, against servers on 127.0.0.1; no network.
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, request } from "node:http";
import { connect } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { startForwardProxy } from "./forward-proxy.mjs";
import { serveMirror } from "./serve-mirror.mjs";

async function origin(t) {
  const server = createServer((incoming, outgoing) =>
    outgoing.end(`origin saw ${incoming.method} ${incoming.url}`),
  );
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  t.after(() => {
    server.closeAllConnections();
    server.close();
  });
  return server.address().port;
}

function read(stream) {
  return new Promise((done, fail) => {
    let text = "";
    stream.setEncoding("utf8");
    stream.on("data", (chunk) => (text += chunk));
    stream.on("end", () => done(text));
    stream.on("error", fail);
  });
}

test("the proxy forwards a request in absolute form and logs it", async (t) => {
  const port = await origin(t);
  const proxy = await startForwardProxy();
  t.after(proxy.close);
  const target = `http://127.0.0.1:${port}/v1/file?x=1`;
  const body = await new Promise((done, fail) => {
    request({ host: "127.0.0.1", port: proxy.port, path: target }, (response) =>
      read(response).then(done, fail),
    )
      .on("error", fail)
      .end();
  });
  assert.equal(body, "origin saw GET /v1/file?x=1");
  assert.deepEqual(proxy.lines, [`GET ${target}`]);
});

test("the proxy tunnels CONNECT to the named host and port", async (t) => {
  const port = await origin(t);
  const proxy = await startForwardProxy();
  t.after(proxy.close);
  const socket = connect(proxy.port, "127.0.0.1");
  socket.write(
    `CONNECT 127.0.0.1:${port} HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\n\r\n`,
  );
  const established = await new Promise((done) =>
    socket.once("data", (chunk) => done(chunk.toString())),
  );
  assert.match(established, /^HTTP\/1\.1 200/);
  socket.write("GET /through HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
  assert.match(await read(socket), /origin saw GET \/through$/);
  assert.deepEqual(proxy.lines, [`CONNECT 127.0.0.1:${port}`]);
});

test("the mirror serves files under its root and nothing outside it", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "alpha2-mirror-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, "v1.2.3"));
  writeFileSync(join(root, "v1.2.3", "checksums.sha256"), "sums\n");
  writeFileSync(join(root, "..secret"), "no");
  const mirror = await serveMirror(join(root));
  t.after(mirror.close);
  const fetched = await fetch(`${mirror.url}/v1.2.3/checksums.sha256`);
  assert.equal(fetched.status, 200);
  assert.equal(await fetched.text(), "sums\n");
  assert.equal((await fetch(`${mirror.url}/v1.2.3/missing`)).status, 404);
  assert.equal((await fetch(`${mirror.url}/%2e%2e/etc/passwd`)).status, 404);
  assert.equal((await fetch(`${mirror.url}/v1.2.3`)).status, 404);
  assert.deepEqual(mirror.requests.slice(0, 2), [
    "/v1.2.3/checksums.sha256",
    "/v1.2.3/missing",
  ]);
});
