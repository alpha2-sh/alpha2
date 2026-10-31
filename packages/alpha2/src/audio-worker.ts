import type { BrowserAudioBlockSink, OfflineAudioRenderer } from "./audio.js";
import { renderOfflineAudio } from "./audio.js";
import { type SerializedAlpha2Error, serializeError } from "./worker-error.js";

/**
 * A request to {@link AudioWorkerHost.handle}. `render-audio` renders `totalFrames` frames in
 * blocks cycling through `blockSizes`; `cancel-audio` cancels the render with that `id`.
 */
export type AudioWorkerRequest =
  | {
      readonly type: "render-audio";
      readonly id: number;
      readonly totalFrames: bigint;
      readonly blockSizes: readonly number[];
    }
  | { readonly type: "cancel-audio"; readonly id: number };

/** The response to a `render-audio` request (`cancel-audio` has none). */
export type AudioWorkerResponse =
  | { readonly type: "audio-complete"; readonly id: number }
  | {
      readonly type: "audio-error";
      readonly id: number;
      /** Rebuild with `new Alpha2WasmError(error)` from `alpha2/worker`. */
      readonly error: SerializedAlpha2Error;
      /** Whether the render failed because a `cancel-audio` request aborted it. */
      readonly cancelled: boolean;
    };

/** Creates the sink for each `render-audio` request. */
export type AudioWorkerSinkFactory = (request: {
  readonly id: number;
  readonly totalFrames: bigint;
}) => BrowserAudioBlockSink | Promise<BrowserAudioBlockSink>;

/**
 * Worker side of the offline audio protocol. Runs {@link renderOfflineAudio} per request into its
 * own sink, one request at a time, and cancels by request id.
 *
 * A request made while a cancelled render is still winding down waits for that render to abort
 * its sink, so no block or late `abort` reaches the wrong sink.
 */
export class AudioWorkerHost {
  readonly #render: OfflineAudioRenderer;
  readonly #sinkFor: AudioWorkerSinkFactory;
  readonly #controllers = new Map<number, AbortController>();
  // Settles when the last queued render has finished or aborted its sink.
  #queue: Promise<unknown> = Promise.resolve();

  constructor(render: OfflineAudioRenderer, sinkFor: AudioWorkerSinkFactory) {
    this.#render = render;
    this.#sinkFor = sinkFor;
  }

  /**
   * Handles one request. A render resolves with its response after its sink has finished or
   * aborted and never rejects; failures become `audio-error`, including reuse of an id that is still
   * active. A cancel resolves with `undefined` immediately; unknown ids are ignored.
   */
  async handle(
    request: AudioWorkerRequest,
  ): Promise<AudioWorkerResponse | undefined> {
    if (request.type === "cancel-audio") {
      this.#controllers
        .get(request.id)
        ?.abort(new DOMException("Cancelled", "AbortError"));
      return undefined;
    }
    if (this.#controllers.has(request.id)) {
      return {
        type: "audio-error",
        id: request.id,
        error: serializeError(
          new RangeError(
            `audio request ${request.id} is already rendering; use a new id`,
          ),
          false,
          "AUDIO_WORKER_ERROR",
        ),
        cancelled: false,
      };
    }
    const controller = new AbortController();
    this.#controllers.set(request.id, controller);
    const previous = this.#queue;
    const run = (async (): Promise<AudioWorkerResponse> => {
      await previous;
      try {
        controller.signal.throwIfAborted();
        const sink = await this.#sinkFor({
          id: request.id,
          totalFrames: request.totalFrames,
        });
        await renderOfflineAudio(
          request.totalFrames,
          request.blockSizes,
          this.#render,
          sink,
          controller.signal,
        );
        return { type: "audio-complete", id: request.id };
      } catch (error) {
        return {
          type: "audio-error",
          id: request.id,
          error: serializeError(error, false, "AUDIO_WORKER_ERROR"),
          cancelled: controller.signal.aborted,
        };
      } finally {
        this.#controllers.delete(request.id);
      }
    })();
    this.#queue = run;
    return run;
  }
}
