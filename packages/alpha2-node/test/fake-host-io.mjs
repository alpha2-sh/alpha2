// Protocol I/O for the fake alpha2-host processes the tests run; the fake hosts import it.

export const MAGIC = Buffer.from([
  0x89, 0x41, 0x32, 0x48, 0x4f, 0x53, 0x54, 0x0a,
]);

let magicWritten = false;

/** Writes the magic that comes before the first frame; `send` writes it when it has not been. */
export function writeMagic(callback) {
  magicWritten = true;
  process.stdout.write(MAGIC, callback);
}

/** The bytes of one frame: `header` (an object, or text as it is) and its attachments. */
export function frame(header, attachments = []) {
  const text = typeof header === "string" ? header : JSON.stringify(header);
  const head = Buffer.from(text, "utf8");
  const body = Buffer.concat(attachments.map(({ data }) => Buffer.from(data)));
  const prefix = Buffer.alloc(8);
  prefix.writeUInt32LE(head.length, 0);
  prefix.writeUInt32LE(body.length, 4);
  return Buffer.concat([prefix, head, body]);
}

/** Writes raw bytes on stdout, after the magic. */
export function sendBytes(bytes, callback) {
  if (!magicWritten) writeMagic();
  process.stdout.write(bytes, callback);
}

/**
 * Sends `message`. `attachments` are `{ type, data }` and are listed in the header unless
 * `message.attachments` already lists them.
 */
export function send(message, callback, attachments = []) {
  const listed =
    attachments.length === 0 || message.attachments !== undefined
      ? message
      : {
          ...message,
          attachments: attachments.map(({ type, data }) => ({
            type,
            bytes: data.length,
          })),
        };
  sendBytes(frame({ protocolVersion: 2, ...listed }, attachments), callback);
}

/** Writes `header` text where a message goes, whether or not it is one. */
export function sendRaw(header, callback) {
  sendBytes(frame(header), callback);
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
export function respond(request, fields, callback, attachments) {
  send(
    { requestId: request.requestId, hostState: "ready", ...fields },
    callback,
    attachments,
  );
}

export function progress(request, fields, callback) {
  send({ type: "progress", requestId: request.requestId, ...fields }, callback);
}

/**
 * Calls `handle` with each request and `onCancel` with each cancel. When stdin ends it calls
 * `onEnd`, which exits by default, as `serve` does.
 */
export function requests(
  handle,
  { onCancel = () => {}, onEnd = () => process.exit(0) } = {},
) {
  let chunks = [];
  let buffered = 0;
  let needed = 8;
  process.stdin.on("data", (chunk) => {
    chunks.push(chunk);
    buffered += chunk.length;
    if (buffered < needed) return;
    let pending = Buffer.concat(chunks);
    needed = 8;
    while (pending.length >= 8) {
      const headerBytes = pending.readUInt32LE(0);
      const end = 8 + headerBytes + pending.readUInt32LE(4);
      if (pending.length < end) {
        needed = end;
        break;
      }
      const request = JSON.parse(pending.subarray(8, 8 + headerBytes));
      pending = pending.subarray(end);
      if (request.operation === "cancel") onCancel(request);
      else handle(request);
    }
    chunks = pending.length === 0 ? [] : [pending];
    buffered = pending.length;
  });
  process.stdin.on("end", onEnd);
}

/** Sends the ready state, then serves as {@link requests} does. */
export function serve(handle, options) {
  ready();
  requests(handle, options);
}
