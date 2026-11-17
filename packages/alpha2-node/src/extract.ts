// Client-side extractors over the host protocol's `extract` operation. The host supplies frames
// and audio step by step, and the extractor's emits go back in acknowledgements.

import {
  type ExtractContext,
  type Extractor,
  collectEmits,
} from "alpha2/extractor";
import type { Attachment } from "./host-wire.js";
import type { ExtractStepMessage } from "./protocol.js";

export {
  type ExtractAudio,
  type ExtractContext,
  type ExtractEmitter,
  type ExtractFrame,
  type ExtractProgress,
  type ExtractResult,
  type ExtractRun,
  type Extractor,
  type ExtractorDescriptor,
  schemaProblem,
} from "alpha2/extractor";

/** The acknowledgement payload for one step: emits, or the extractor's failure. */
export type StepAck =
  | { readonly emits: readonly Readonly<Record<string, unknown>>[] }
  | { readonly error: { readonly message: string } };

function direction(value: unknown): "forward" | "backward" {
  return value === "backward" ? "backward" : "forward";
}

/** Takes one step with `extractor` and returns its acknowledgement; never rejects. */
export async function takeStep(
  extractor: Extractor,
  message: ExtractStepMessage,
  attachments: readonly Attachment[],
): Promise<StepAck> {
  const { emits, out } = collectEmits();
  try {
    switch (message.step) {
      case "begin":
        await extractor.begin?.(message.context as ExtractContext);
        break;
      case "pass":
        await extractor.pass?.({
          pass: Number(message.pass),
          direction: direction(message.direction),
        });
        break;
      case "video": {
        const bytes = attachments[0]?.data ?? Buffer.alloc(0);
        await extractor.video?.(
          {
            time: Number(message.time),
            width: Number(message.width),
            height: Number(message.height),
            format: message.format === "luma8" ? "luma8" : "rgba8",
            data: new Uint8Array(
              bytes.buffer,
              bytes.byteOffset,
              bytes.byteLength,
            ),
          },
          out,
        );
        break;
      }
      case "audio": {
        const bytes = attachments[0]?.data ?? Buffer.alloc(0);
        const samples = new Float32Array(bytes.byteLength / 4);
        const view = new DataView(
          bytes.buffer,
          bytes.byteOffset,
          bytes.byteLength,
        );
        for (let index = 0; index < samples.length; index += 1)
          samples[index] = view.getFloat32(index * 4, true);
        await extractor.audio?.(
          {
            start: Number(message.start),
            startSample: Number(message.startSample),
            frames: Number(message.frames),
            rate: Number(message.rate),
            channels: Number(message.channels),
            samples,
          },
          out,
        );
        break;
      }
      case "end":
        await extractor.end?.(out);
        break;
    }
  } catch (cause) {
    return {
      error: {
        message: cause instanceof Error ? cause.message : String(cause),
      },
    };
  }
  return { emits };
}
