#!/usr/bin/env node
// An HTTP forward proxy for install journeys and installer tests: plain requests in absolute form
// are forwarded, CONNECT opens a tunnel. It prints its URL, then logs every request line (to
// --log when given).
//
// usage: forward-proxy.mjs [--port n] [--log file]
import { appendFileSync } from "node:fs";
import { createServer, request } from "node:http";
import { connect } from "node:net";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

/**
 * Starts the proxy on 127.0.0.1 (`port` 0 picks one) and resolves with its port, the request lines
 * seen so far and a `close`. `onLine` also receives each line.
 */
export async function startForwardProxy({ port = 0, onLine } = {}) {
  const lines = [];
  const record = (line) => {
    lines.push(line);
    onLine?.(line);
  };
  const sockets = new Set();
  const server = createServer((incoming, outgoing) => {
    record(`${incoming.method} ${incoming.url}`);
    let target;
    try {
      target = new URL(incoming.url);
    } catch {
      outgoing.writeHead(400).end();
      return;
    }
    const upstream = request(
      {
        host: target.hostname,
        port: target.port || 80,
        path: `${target.pathname}${target.search}`,
        method: incoming.method,
        headers: { ...incoming.headers, host: target.host },
      },
      (response) => {
        outgoing.writeHead(response.statusCode ?? 502, response.headers);
        response.pipe(outgoing);
      },
    );
    upstream.on("error", () => outgoing.destroy());
    incoming.pipe(upstream);
  });
  server.on("connect", (incoming, client, head) => {
    record(`CONNECT ${incoming.url}`);
    const separator = incoming.url.lastIndexOf(":");
    const upstream = connect(
      Number(incoming.url.slice(separator + 1)),
      incoming.url.slice(0, separator).replace(/^\[|\]$/g, ""),
      () => {
        client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
        upstream.write(head);
        upstream.pipe(client);
        client.pipe(upstream);
      },
    );
    for (const socket of [upstream, client]) {
      sockets.add(socket);
      socket.on("close", () => sockets.delete(socket));
    }
    upstream.on("error", () => client.destroy());
    client.on("error", () => upstream.destroy());
  });
  await new Promise((resolve) => server.listen(port, "127.0.0.1", resolve));
  return {
    port: server.address().port,
    lines,
    close: () =>
      new Promise((resolve) => {
        for (const socket of sockets) socket.destroy();
        server.closeAllConnections();
        server.close(resolve);
      }),
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const { values } = parseArgs({
    options: { port: { type: "string" }, log: { type: "string" } },
  });
  const proxy = await startForwardProxy({
    port: Number(values.port ?? 0),
    onLine: (line) => {
      if (values.log) appendFileSync(values.log, `${line}\n`);
      else console.log(line);
    },
  });
  console.log(`http://127.0.0.1:${proxy.port}`);
}
