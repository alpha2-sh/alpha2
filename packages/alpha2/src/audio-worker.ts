import type { BrowserAudioBlockSink, OfflineAudioRenderer } from "./audio.js";
import { renderOfflineAudio } from "./audio.js";
import { type SerializedAlpha2Error, serializeError } from "./worker-error.js";

/**
 * A message to {@link AudioWorkerHost.handle}: render `totalFrames` frames in
 * blocks cycling through `blockSizes`, or cancel the render with request `id`.
 */
export type AudioWorkerRequest =
  | {
      readonly type: "render-audio";
      readonly id: number;
      readonly totalFrames: bigint;
      readonly blockSizes: readonly number[];
    }
  | { readonly type: "cancel-audio"; readonly id: number };

/** The answer to a `render-audio` request; a `cancel-audio` request has none. */
export type AudioWorkerResponse =
  | { readonly type: "audio-complete"; readonly id: number }
  | {
      readonly type: "audio-error";
      readonly id: number;
      /**
       * The failure as the render worker reports one: its code, name, fatality, and classified
       * `failure`. `new Alpha2WasmError(error)` (`alpha2/worker`) rebuilds it for
       * `classifyBrowserFailure`.
       */
      readonly error: SerializedAlpha2Error;
      /** Whether the render failed because a `cancel-audio` request aborted it. */
      readonly cancelled: boolean;
    };

/** What {@link AudioWorkerHost} asks for when a `render-audio` request starts. */
export type AudioWorkerSinkFactory = (request: {
  readonly id: number;
  readonly totalFrames: bigint;
}) => BrowserAudioBlockSink | Promise<BrowserAudioBlockSink>;

/**
 * The worker side of the offline audio protocol: runs {@link renderOfflineAudio} per request, one
 * at a time, each into a sink of its own, and cancels by request id.
 *
 * Renders never overlap. A cancelled render still finishes the block it is writing and aborts its
 * sink, so a request made meanwhile (the usual cancel-then-export-again) waits for that, then gets
 * a fresh sink from `sinkFor`: no block of one render reaches another's sink, and no late `abort`
 * reaches a sink after it finished.
 */
export class AudioWorkerHost {
  readonly #render: OfflineAudioRenderer;
  readonly #sinkFor: AudioWorkerSinkFactory;
  readonly #controllers = new Map<number, AbortController>();
  // Settles when the last render asked for has finished or aborted its sink.
  #queue: Promise<unknown> = Promise.resolve();

  constructor(render: OfflineAudioRenderer, sinkFor: AudioWorkerSinkFactory) {
    this.#render = render;
    this.#sinkFor = sinkFor;
  }

  /**
   * Handles one request. A render resolves with its response once its sink has finished or
   * aborted; it never rejects, since a failure is answered with `audio-error`. It starts after the
   * renders asked for before it have settled, and a cancel that arrives first answers it as
   * cancelled without opening a sink. A render whose id is still rendering or waiting is refused
   * with `audio-error`, leaving the first one as it was. A cancel resolves with `undefined` at
   * once, and cancelling an id that is not rendering or waiting does nothing.
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
