// The framed messages of `alpha2-host serve`, for scripts that talk to it without a client.

export const PROTOCOL_VERSION = 2;

/** What `serve` writes before its first frame. */
export const HOST_MAGIC = Buffer.from([
  0x89, 0x41, 0x32, 0x48, 0x4f, 0x53, 0x54, 0x0a,
]);

/** The bytes of one message, a request or a fixture host's answer; `protocolVersion` is added. */
export function encodeFrame(message) {
  const header = Buffer.from(
    JSON.stringify({ protocolVersion: PROTOCOL_VERSION, ...message }),
    "utf8",
  );
  const prefix = Buffer.alloc(8);
  prefix.writeUInt32LE(header.length, 0);
  return Buffer.concat([prefix, header]);
}

/**
 * A reader to feed chunks of a host's stdout to, or with `magic: false` a host's stdin. It calls
 * `onMessage(header, attachments)` for each frame, with `attachments` as `{ type, data }`, and
 * keeps what came before the magic and any unfinished frame left over.
 */
export function frameReader(onMessage, { magic = true } = {}) {
  let pending = Buffer.alloc(0);
  let preamble = Buffer.alloc(0);
  let magicSeen = !magic;
  return {
    push(chunk) {
      pending = Buffer.concat([pending, chunk]);
      if (!magicSeen) {
        const at = pending.indexOf(HOST_MAGIC);
        if (at < 0) return;
        preamble = pending.subarray(0, at);
        pending = pending.subarray(at + HOST_MAGIC.length);
        magicSeen = true;
      }
      while (pending.length >= 8) {
        const headerBytes = pending.readUInt32LE(0);
        const end = 8 + headerBytes + pending.readUInt32LE(4);
        if (pending.length < end) return;
        const header = JSON.parse(pending.subarray(8, 8 + headerBytes));
        let offset = 8 + headerBytes;
        const attachments = (header.attachments ?? []).map(
          ({ type, bytes }) => {
            const data = pending.subarray(offset, offset + bytes);
            offset += bytes;
            return { type, data };
          },
        );
        pending = pending.subarray(end);
        onMessage(header, attachments);
      }
    },
    get preamble() {
      return magicSeen ? preamble : pending;
    },
    get leftover() {
      return magicSeen ? pending : Buffer.alloc(0);
    },
  };
}

/** The messages in all of a host's stdout (or stdin), as `{ header, attachments }`. */
export function readFrames(bytes, options) {
  const messages = [];
  const reader = frameReader(
    (header, attachments) => messages.push({ header, attachments }),
    options,
  );
  reader.push(Buffer.from(bytes));
  return { messages, preamble: reader.preamble, leftover: reader.leftover };
}
