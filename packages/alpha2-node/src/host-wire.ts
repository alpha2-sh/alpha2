import { createInterface, type Interface } from "node:readline";
import type { Readable, Writable } from "node:stream";

import { type HostMessage, parseHostMessage } from "./protocol.js";

/** One message as written to the host. */
export interface Frame {
  readonly data: string;
  /** Bytes of the message itself, which the host's request size limit counts. */
  readonly bodyBytes: number;
}

/** Throws what `JSON.stringify` throws for a value it cannot write, such as a BigInt. */
export function encodeMessage(
  message: Readonly<Record<string, unknown>>,
): Frame {
  const body = JSON.stringify(message);
  return { data: `${body}\n`, bodyBytes: Buffer.byteLength(body) };
}

export interface WireEvents {
  readonly message: (message: HostMessage) => void;
  readonly invalid: (cause: unknown) => void;
  /** Output on stdout that is not a message, such as a loaded library's diagnostics. */
  readonly diagnostic: (text: string) => void;
}

/** Messages to and from one host as JSON lines on its standard streams. */
export class HostWire {
  readonly #input: Writable;
  readonly #reader: Interface;

  constructor(input: Writable, output: Readable, events: WireEvents) {
    this.#input = input;
    this.#reader = createInterface({ input: output });
    this.#reader.on("line", (line) => {
      // A Vulkan validation layer prints to stdout.
      if (!line.startsWith("{")) {
        events.diagnostic(`${line}\n`);
        return;
      }
      let message: HostMessage;
      try {
        message = parseHostMessage(line);
      } catch (cause) {
        events.invalid(cause);
        return;
      }
      events.message(message);
    });
  }

  write(frame: Frame, callback?: (error: Error | null | undefined) => void) {
    this.#input.write(frame.data, callback);
  }

  /** Ends the host's input, which asks it to exit once it has answered. */
  end(): void {
    this.#input.end();
  }

  close(): void {
    this.#reader.close();
  }
}
