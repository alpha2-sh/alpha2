// Protocol I/O for the fake alpha2-host processes the tests run; the fake hosts import it.
import { createInterface } from "node:readline";

export function send(message, callback) {
  sendRaw(JSON.stringify({ protocolVersion: 1, ...message }), callback);
}

/** Writes `body` where a message goes, whether or not it is one. */
export function sendRaw(body, callback) {
  process.stdout.write(`${body}\n`, callback);
}

export function ready(callback) {
  send(
    { type: "state", state: "ready", reason: "started", details: {} },
    callback,
  );
}

export function drain(reason, callback) {
  send({ type: "state", state: "draining", reason, details: {} }, callback);
}

/** Answers `request`; `fields` may set `hostState` and `requestId` over the defaults. */
export function respond(request, fields, callback) {
  send(
    { requestId: request.requestId, hostState: "ready", ...fields },
    callback,
  );
}

export function progress(request, fields, callback) {
  send({ type: "progress", requestId: request.requestId, ...fields }, callback);
}

/**
 * Calls `handle` with each request and `onCancel` with each cancel line. When stdin ends it calls
 * `onEnd`, which exits by default, as `serve` does.
 */
export function requests(
  handle,
  { onCancel = () => {}, onEnd = () => process.exit(0) } = {},
) {
  const lines = createInterface({ input: process.stdin });
  lines.on("line", (line) => {
    const request = JSON.parse(line);
    if (request.operation === "cancel") onCancel(request);
    else handle(request);
  });
  lines.on("close", onEnd);
}

/** Sends the ready state, then serves as {@link requests} does. */
export function serve(handle, options) {
  ready();
  requests(handle, options);
}
