import type { Readable, Writable } from "node:stream";

import { type HostMessage, parseHostMessage } from "./protocol.js";

/** What `serve` writes before its first frame; output before it is diagnostics. */
export const HOST_MAGIC = Buffer.from([
  0x89, 0x41, 0x32, 0x48, 0x4f, 0x53, 0x54, 0x0a,
]);

/** Output before the magic past this means stdout is not a host's. */
const MAX_PREAMBLE_BYTES = 1024 * 1024;

/** A larger header length from the host is a broken frame, not a message. */
const MAX_HEADER_BYTES = 128 * 1024 * 1024;

/** The header length and the attachments length, each a little-endian u32. */
const PREFIX_BYTES = 8;

/** One message as written to the host. */
export interface Frame {
  readonly data: Buffer;
  /** Bytes of the header, which the host's request size limit counts. */
  readonly headerBytes: number;
}

/** One attachment of a host message, in the order its header lists them. */
export interface Attachment {
  /** A media type, such as `image/png`. */
  readonly type: string;
  readonly data: Buffer;
}

/** Throws what `JSON.stringify` throws for a value it cannot write, such as a BigInt. */
export function encodeMessage(
  message: Readonly<Record<string, unknown>>,
): Frame {
  const header = Buffer.from(JSON.stringify(message), "utf8");
  const prefix = Buffer.alloc(PREFIX_BYTES);
  prefix.writeUInt32LE(header.length, 0);
  prefix.writeUInt32LE(0, 4);
  return { data: Buffer.concat([prefix, header]), headerBytes: header.length };
}

export interface WireEvents {
  readonly message: (
    message: HostMessage,
    attachments: readonly Attachment[],
  ) => void;
  /** The host wrote something that is not a frame; nothing more is read from it. */
  readonly invalid: (cause: Error) => void;
  /** Output on stdout before the magic, such as a loaded library's diagnostics. */
  readonly diagnostic: (bytes: Buffer) => void;
}

/** Frames to and from one host on its standard streams. */
export class HostWire {
  readonly #input: Writable;
  readonly #output: Readable;
  readonly #events: WireEvents;
  readonly #receive = (chunk: Buffer): void => this.#read(chunk);
  readonly #ended = (): void => this.#flushPreamble();
  readonly #decoder = new TextDecoder("utf-8", { fatal: true });
  /** Unread chunks, oldest first. */
  #pending: Buffer[] = [];
  #pendingBytes = 0;
  #magicSeen = false;
  #preambleBytes = 0;
  #closed = false;

  constructor(input: Writable, output: Readable, events: WireEvents) {
    this.#input = input;
    this.#output = output;
    this.#events = events;
    output.on("data", this.#receive);
    output.on("end", this.#ended);
  }

  write(frame: Frame, callback?: (error: Error | null | undefined) => void) {
    this.#input.write(frame.data, callback);
  }

  /** Ends the host's input, which asks it to exit once it has answered. */
  end(): void {
    this.#input.end();
  }

  close(): void {
    this.#closed = true;
    this.#pending = [];
    this.#output.off("data", this.#receive);
    this.#output.off("end", this.#ended);
  }

  #read(chunk: Buffer): void {
    if (this.#closed) return;
    this.#pending.push(chunk);
    this.#pendingBytes += chunk.length;
    if (!this.#magicSeen && !this.#skipPreamble()) return;
    while (!this.#closed && this.#pendingBytes >= PREFIX_BYTES) {
      const prefix = this.#take(PREFIX_BYTES, false);
      const headerBytes = prefix.readUInt32LE(0);
      const attachmentBytes = prefix.readUInt32LE(4);
      if (headerBytes > MAX_HEADER_BYTES) {
        this.#fail(new Error(`a frame header of ${headerBytes} bytes`));
        return;
      }
      if (this.#pendingBytes < PREFIX_BYTES + headerBytes + attachmentBytes)
        return;
      this.#take(PREFIX_BYTES);
      const header = this.#take(headerBytes);
      const body = this.#take(attachmentBytes);
      let message: HostMessage;
      let attachments: Attachment[];
      try {
        message = parseHostMessage(this.#decoder.decode(header));
        attachments = splitAttachments(message, body);
      } catch (cause) {
        this.#fail(cause instanceof Error ? cause : new Error(String(cause)));
        return;
      }
      this.#events.message(message, attachments);
    }
  }

  /** Reports what came before the magic; the last 7 bytes wait for a magic split across reads. */
  #skipPreamble(): boolean {
    const bytes = this.#take(this.#pendingBytes);
    const at = bytes.indexOf(HOST_MAGIC);
    const kept =
      at >= 0 ? at : Math.max(0, bytes.length - (HOST_MAGIC.length - 1));
    if (kept > 0) this.#events.diagnostic(bytes.subarray(0, kept));
    this.#preambleBytes += kept;
    const rest = bytes.subarray(at >= 0 ? at + HOST_MAGIC.length : kept);
    this.#pending = rest.length > 0 ? [rest] : [];
    this.#pendingBytes = rest.length;
    if (at >= 0) {
      this.#magicSeen = true;
      return true;
    }
    if (this.#preambleBytes > MAX_PREAMBLE_BYTES)
      this.#fail(
        new Error(
          `alpha2-host wrote ${this.#preambleBytes} bytes without the protocol's magic`,
        ),
      );
    return false;
  }

  /** Reports the bytes held back for a split magic once stdout ends without one. */
  #flushPreamble(): void {
    if (this.#closed || this.#magicSeen || this.#pendingBytes === 0) return;
    this.#events.diagnostic(this.#take(this.#pendingBytes));
  }

  /** The next `count` bytes, copied only when they span chunks. */
  #take(count: number, consume = true): Buffer {
    const first = this.#pending[0];
    if (count === 0) return Buffer.alloc(0);
    if (first !== undefined && first.length >= count) {
      if (consume) {
        this.#pendingBytes -= count;
        if (first.length === count) this.#pending.shift();
        else this.#pending[0] = first.subarray(count);
      }
      return first.subarray(0, count);
    }
    const bytes = Buffer.allocUnsafe(count);
    let filled = 0;
    let index = 0;
    while (filled < count) {
      const piece = this.#pending[index];
      const used = Math.min(piece.length, count - filled);
      piece.copy(bytes, filled, 0, used);
      filled += used;
      if (!consume) index += 1;
      else if (used === piece.length) this.#pending.shift();
      else this.#pending[0] = piece.subarray(used);
    }
    if (consume) this.#pendingBytes -= count;
    return bytes;
  }

  #fail(cause: Error): void {
    this.close();
    this.#events.invalid(cause);
  }
}

/** Throws unless the header lists exactly the attachment bytes that follow it. */
function splitAttachments(message: HostMessage, body: Buffer): Attachment[] {
  const listed = message.attachments ?? [];
  const total = listed.reduce((sum, entry) => sum + entry.bytes, 0);
  if (total !== body.length)
    throw new Error(
      `the header lists ${total} attachment bytes and the frame has ${body.length}`,
    );
  let offset = 0;
  return listed.map((entry) => {
    const data = body.subarray(offset, offset + entry.bytes);
    offset += entry.bytes;
    return { type: entry.type, data };
  });
}
