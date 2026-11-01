import { throwIfAborted } from "./abort.js";
import type { AudioRingTransfer } from "./audio-timeline.js";
import type { MasterAudio } from "./project.js";
import { encodeMasterAudio } from "./audio-timeline.js";

/** Options for {@link Alpha2AudioWorkletHost.create}. */
export interface AudioWorkletHostOptions {
  /** Processor module URL. Defaults to {@link alpha2AudioWorkletProcessorUrl}. */
  readonly processorUrl?: string | URL;
  /** Name the processor module passes to `registerProcessor` (default `alpha2-processor`). */
  readonly processorName?: string;
  /** Default 1. */
  readonly numberOfInputs?: number;
  /** Default 1. */
  readonly numberOfOutputs?: number;
  /** Channels per output (default 2 each). The first entry sets the DSP module's channel count. */
  readonly outputChannelCount?: readonly number[];
  /**
   * Sample rate of ring audio ({@link useAudioRing}), normally the project's. Defaults to the
   * context's. When it differs from the context's, ring audio is resampled with the engine's
   * band-limited converter and `resampled` is reported through `onDiagnostic`. WebAudio input is
   * never resampled. Must be a positive integer.
   */
  readonly projectSampleRate?: number;
  /**
   * Called once at startup when playback will differ from an export (currently only `resampled`).
   * Defaults to `console.warn`.
   */
  readonly onDiagnostic?: (diagnostic: AudioWorkletDiagnostic) => void;
  /** Aborts creation; `create` rejects with the signal's reason and disposes the node. */
  readonly signal?: AbortSignal;
  /**
   * Called for every error the running node reports, such as a processor exception, a DSP trap or
   * an unfillable output. Defaults to `console.error`.
   */
  readonly onError?: (error: Error) => void;
}

/**
 * See {@link AudioWorkletHostOptions.onDiagnostic}. `resampled` means the context rate differs
 * from the project rate.
 */
export interface AudioWorkletDiagnostic {
  readonly kind: "resampled";
  readonly projectSampleRate: number;
  readonly contextSampleRate: number;
}

type WorkletInitResponse =
  | { readonly type: "alpha2-ready" }
  | { readonly type: "alpha2-error"; readonly message: string }
  | { readonly type: "alpha2-stage"; readonly stage: string }
  | { readonly type: "alpha2-latency"; readonly latencySamples: number }
  | {
      readonly type: "alpha2-applied";
      readonly id: number;
      readonly error?: string;
    };

/**
 * Node output delay in context-rate samples, for `AudioPlaybackClock`'s `processing` option.
 */
export interface AudioProcessingDelay {
  readonly latencySamples: number;
}

/** Live delay of each node created by {@link Alpha2AudioWorkletHost.create}. */
const delays = new WeakMap<AudioWorkletNode, { latencySamples: number }>();

const NO_DELAY: AudioProcessingDelay = Object.freeze({ latencySamples: 0 });

/** URL of the processor module distributed with the alpha2 package. */
export const alpha2AudioWorkletProcessorUrl = new URL(
  "./audio-worklet-processor.js",
  import.meta.url,
);

/** URL of the C-ABI DSP wasm module distributed with the alpha2 package. */
export const alpha2AudioWorkletWasmUrl = new URL(
  "../wasm/alpha2_audio_worklet.wasm",
  import.meta.url,
);

/** Fetches the packaged DSP wasm bytes. */
export async function loadAlpha2AudioWorkletWasm(
  url: string | URL = alpha2AudioWorkletWasmUrl,
): Promise<ArrayBuffer> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(
      `failed to fetch alpha2 AudioWorklet wasm: ${response.status} ${response.statusText}`,
    );
  }
  return response.arrayBuffer();
}

/** Browser host for the C-ABI DSP wasm module; no wasm-bindgen glue is used. */
export class Alpha2AudioWorkletHost {
  /**
   * Adds the processor module, creates the node and starts the DSP module from a copy of `wasm`.
   * Resolves once the processor is ready; later errors go to `options.onError`.
   *
   * Rejects (and disposes the node) when the module cannot be added, the processor fails while
   * starting, it is not ready within 5 seconds, or `options.signal` aborts.
   *
   * @throws RangeError When `options.projectSampleRate` is not a positive integer.
   */
  static async create(
    context: BaseAudioContext,
    wasm: BufferSource,
    options: AudioWorkletHostOptions,
  ): Promise<AudioWorkletNode> {
    const projectSampleRate = options.projectSampleRate ?? context.sampleRate;
    if (!Number.isInteger(projectSampleRate) || projectSampleRate <= 0) {
      throw new RangeError(
        "the project sample rate must be a positive integer",
      );
    }
    throwIfAborted(options.signal);
    await context.audioWorklet.addModule(
      String(options.processorUrl ?? alpha2AudioWorkletProcessorUrl),
    );
    throwIfAborted(options.signal);
    const payload = copyWasmBytes(wasm);
    const channels = options.outputChannelCount?.[0] ?? 2;
    const numberOfOutputs = options.numberOfOutputs ?? 1;
    const node = new AudioWorkletNode(
      context,
      options.processorName ?? "alpha2-processor",
      {
        numberOfInputs: options.numberOfInputs ?? 1,
        numberOfOutputs,
        outputChannelCount: options.outputChannelCount
          ? [...options.outputChannelCount]
          : Array.from({ length: numberOfOutputs }, () => channels),
        // Up-mix by speaker rules; the default "max" would pass mono through as one channel.
        channelCount: channels,
        channelCountMode: "explicit",
        channelInterpretation: "speakers",
      },
    );
    // Listen before starting so the initial latency report is not missed.
    const untrack = trackLatency(node);
    try {
      await initializeNode(
        node,
        payload,
        channels,
        projectSampleRate,
        options.signal,
      );
      reportErrors(node, options.onError ?? ((error) => console.error(error)));
      if (projectSampleRate !== context.sampleRate)
        (
          options.onDiagnostic ??
          ((diagnostic) =>
            console.warn(
              `alpha2: the AudioContext runs at ${diagnostic.contextSampleRate} Hz, not the project's ${diagnostic.projectSampleRate} Hz; preview converts the project's audio to it`,
            ))
        )({
          kind: "resampled",
          projectSampleRate,
          contextSampleRate: context.sampleRate,
        });
      return node;
    } catch (error) {
      // The processor may still be starting and would otherwise run until the context closes.
      disposeAudioWorklet(node);
      untrack();
      node.port.close();
      throw error;
    }
  }

  /**
   * The node's live output delay in context-rate samples: master compressor lookahead plus, when
   * resampling ring audio, the converter's lookahead (16 to 18 samples at common rates). Zero for an
   * engine-mixed node at the project rate and for nodes `create` did not make.
   *
   * The returned object updates as configurations are applied; pass it to `AudioPlaybackClock` so
   * the clock reports what is being heard.
   */
  static latency(node: AudioWorkletNode): AudioProcessingDelay {
    return delays.get(node) ?? NO_DELAY;
  }
}

/** Tracks the processor's reported delay; returns a function that stops tracking. */
function trackLatency(node: AudioWorkletNode): () => void {
  const delay = { latencySamples: 0 };
  delays.set(node, delay);
  const listener = (event: MessageEvent<WorkletInitResponse>): void => {
    if (event.data.type === "alpha2-latency")
      delay.latencySamples = event.data.latencySamples;
  };
  node.port.addEventListener("message", listener);
  return () => {
    node.port.removeEventListener("message", listener);
    delays.delete(node);
  };
}

/** Forwards the running node's errors to `report`. */
function reportErrors(
  node: AudioWorkletNode,
  report: (error: Error) => void,
): void {
  node.addEventListener("processorerror", () => {
    report(new Error("AudioWorklet processor threw; the node has stopped"));
  });
  node.port.addEventListener(
    "message",
    (event: MessageEvent<WorkletInitResponse>) => {
      if (event.data.type === "alpha2-error")
        report(new Error(event.data.message));
    },
  );
}

function initializeNode(
  node: AudioWorkletNode,
  wasm: ArrayBuffer,
  channels: number,
  sourceRate: number,
  signal?: AbortSignal,
): Promise<void> {
  return new Promise((resolve, reject) => {
    let timeout: ReturnType<typeof setTimeout> | undefined;
    let lastStage = "node-created";
    const cleanup = (): void => {
      if (timeout !== undefined) clearTimeout(timeout);
      node.port.removeEventListener("message", onMessage);
      node.removeEventListener("processorerror", onProcessorError);
      signal?.removeEventListener("abort", onAbort);
    };
    const onMessage = (event: MessageEvent<WorkletInitResponse>): void => {
      if (event.data.type === "alpha2-ready") {
        cleanup();
        resolve();
      } else if (event.data.type === "alpha2-error") {
        cleanup();
        reject(new Error(event.data.message));
      } else if (event.data.type === "alpha2-stage") {
        lastStage = event.data.stage;
      }
    };
    const onProcessorError = (): void => {
      cleanup();
      reject(new Error("AudioWorklet processor failed during initialization"));
    };
    const onAbort = (): void => {
      cleanup();
      reject(abortError(signal));
    };
    node.port.addEventListener("message", onMessage);
    node.addEventListener("processorerror", onProcessorError, { once: true });
    node.port.start();
    signal?.addEventListener("abort", onAbort, { once: true });
    timeout = setTimeout(() => {
      cleanup();
      reject(
        new Error(
          `AudioWorklet initialization timed out (last stage: ${lastStage})`,
        ),
      );
    }, 5_000);
    try {
      const message = { type: "alpha2-init", wasm, channels, sourceRate };
      node.port.postMessage(message, [wasm]);
    } catch (error) {
      cleanup();
      reject(error instanceof Error ? error : new Error(String(error)));
    }
  });
}

/** Copies the bytes so the transfer does not detach the caller's buffer. */
function copyWasmBytes(source: BufferSource): ArrayBuffer {
  if (source instanceof ArrayBuffer) return source.slice(0);
  return new Uint8Array(
    source.buffer,
    source.byteOffset,
    source.byteLength,
  ).slice().buffer;
}

/** Requests awaiting the processor's `alpha2-applied` answer, by id. */
interface Requests {
  next: number;
  readonly pending: Map<
    number,
    { resolve: () => void; reject: (error: Error) => void }
  >;
  /** Set once the node is disposed or its processor threw. */
  closed?: Error;
}

const requests = new WeakMap<AudioWorkletNode, Requests>();

/** Request state for `node`, created with its answer listener on first use. */
function requestsOf(node: AudioWorkletNode): Requests {
  const existing = requests.get(node);
  if (existing !== undefined) return existing;
  const created: Requests = { next: 1, pending: new Map() };
  requests.set(node, created);
  node.port.addEventListener(
    "message",
    (event: MessageEvent<WorkletInitResponse>) => {
      const data = event.data;
      if (data.type !== "alpha2-applied") return;
      const request = created.pending.get(data.id);
      if (request === undefined) return;
      created.pending.delete(data.id);
      if (data.error === undefined) request.resolve();
      else request.reject(new Error(data.error));
    },
  );
  node.addEventListener("processorerror", () =>
    closeRequests(
      node,
      new Error("AudioWorklet processor threw; the node has stopped"),
    ),
  );
  node.port.start();
  return created;
}

/** Rejects pending and future requests with `reason`. */
function closeRequests(node: AudioWorkletNode, reason: Error): void {
  const state = requests.get(node);
  if (state === undefined) {
    requests.set(node, { next: 1, pending: new Map(), closed: reason });
    return;
  }
  state.closed ??= reason;
  const pending = [...state.pending.values()];
  state.pending.clear();
  for (const request of pending) request.reject(reason);
}

/**
 * Posts `message` and resolves once the processor has applied it, so the next quantum uses it.
 * Rejects with the processor's refusal, or when the node is disposed or its processor throws.
 */
function request(
  node: AudioWorkletNode,
  message: { readonly type: string } & Record<string, unknown>,
): Promise<void> {
  const state = requestsOf(node);
  if (state.closed !== undefined) return Promise.reject(state.closed);
  const id = state.next;
  state.next += 1;
  return new Promise<void>((resolve, reject) => {
    state.pending.set(id, { resolve, reject });
    try {
      node.port.postMessage({ ...message, id });
    } catch (error) {
      state.pending.delete(id);
      reject(error instanceof Error ? error : new Error(String(error)));
    }
  });
}

/**
 * Points the node at a shared audio ring instead of its WebAudio input; `undefined` switches back.
 * Both sides must agree to clear the ring at a seek.
 *
 * Resolves once the processor plays from the ring and {@link Alpha2AudioWorkletHost.latency} is
 * updated. Await it before `OfflineAudioContext.startRendering()`, or the first quanta may render
 * without the ring. Rejects when the node is disposed or its processor throws first.
 */
export function useAudioRing(
  node: AudioWorkletNode,
  ring: AudioRingTransfer | undefined,
): Promise<void> {
  return request(node, { type: "alpha2-ring", ring });
}

/**
 * Installs a project master bus in the running worklet as an edit. Kept and changed effects keep
 * their state, so adjusting a control does not cut a reverb tail or reset compressor lookahead.
 *
 * Applied as the engine does (effects, gain, limiter), without the engine's loudness normalization
 * gain, which the master bus does not carry.
 *
 * Resolves once the new master runs and {@link Alpha2AudioWorkletHost.latency} reflects it.
 * Rejects with a `RangeError` for a keyframed master gain (nothing is posted), with the processor's
 * reason when it refuses the record (the old chain keeps running), or when the node is disposed or
 * its processor throws first.
 */
export function configureMasterAudio(
  node: AudioWorkletNode,
  master: MasterAudio | undefined,
): Promise<void> {
  let config: Uint8Array;
  try {
    config = encodeMasterAudio(master);
  } catch (error) {
    return Promise.reject(
      error instanceof Error ? error : new Error(String(error)),
    );
  }
  return request(node, { type: "alpha2-config", config });
}

/**
 * Makes the worklet a transparent transport for engine-mixed audio, since `renderAudio` has already
 * applied the master chain.
 *
 * Resolves once the processor is transparent; rejects as {@link configureMasterAudio} does.
 */
export function configureEngineMixedAudio(
  node: AudioWorkletNode,
): Promise<void> {
  return request(node, {
    type: "alpha2-config",
    config: encodeMasterAudio({ limiter: null }),
  });
}

/**
 * Clears the worklet's DSP state and transport position. Rejects when the node is disposed or its
 * processor throws first.
 */
export function resetAudioWorklet(node: AudioWorkletNode): Promise<void> {
  return request(node, { type: "alpha2-reset" });
}

/**
 * Permanently stops and disconnects the node.
 *
 * Disconnecting alone leaves the processor running and holding DSP memory until the context
 * closes, so call this for every node no longer used. Pending and later configuration requests
 * reject.
 */
export function disposeAudioWorklet(node: AudioWorkletNode): void {
  closeRequests(node, new Error("the AudioWorklet node was disposed"));
  node.port.postMessage({ type: "alpha2-dispose" });
  node.disconnect();
}

/** The signal's reason as an `Error`; non-`Error` reasons become an `AbortError`. */
function abortError(signal: AbortSignal | undefined): Error {
  const reason: unknown = signal?.reason;
  if (reason instanceof Error) return reason;
  if (typeof reason === "string") return new DOMException(reason, "AbortError");
  return new DOMException("Aborted", "AbortError");
}
