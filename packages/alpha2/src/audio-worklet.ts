import { throwIfAborted } from "./abort.js";
import type { AudioRingTransfer } from "./audio-timeline.js";
import type { MasterAudio } from "./project.js";
import { encodeMasterAudio } from "./audio-timeline.js";

/** Options for {@link Alpha2AudioWorkletHost.create}. */
export interface AudioWorkletHostOptions {
  /**
   * URL of the worklet processor module added to `audioWorklet`. Defaults to
   * {@link alpha2AudioWorkletProcessorUrl}.
   */
  readonly processorUrl?: string | URL;
  /** Name the processor module passes to `registerProcessor` (default `alpha2-processor`). */
  readonly processorName?: string;
  /** Default 1. */
  readonly numberOfInputs?: number;
  /** Default 1. */
  readonly numberOfOutputs?: number;
  /**
   * Channels of each output. Defaults to two for every output, which is also what the DSP module
   * mixes; the module's channel count is the first entry. Leaving it to WebAudio would size the
   * output from whatever is connected.
   */
  readonly outputChannelCount?: readonly number[];
  /**
   * The sample rate of the audio the node plays from a ring ({@link useAudioRing}): the project's,
   * which an `AudioPump` mixes at. Defaults to the context's. A context at another rate (one that
   * did not grant the rate a host asked for) plays the ring converted to its own rate with the
   * engine's band-limited converter, so the audio keeps its speed and pitch, and reports
   * `resampled` through `onDiagnostic`. A node's WebAudio input is at the context's rate already
   * and is never converted. A positive integer; any other value is refused with a `RangeError`.
   */
  readonly projectSampleRate?: number;
  /**
   * Called once the node runs with what it does differently from the export: `resampled` when it
   * converts the ring from `projectSampleRate` to the context's rate, so what is heard is not
   * bit-identical to what an export renders. Defaults to `console.warn`.
   */
  readonly onDiagnostic?: (diagnostic: AudioWorkletDiagnostic) => void;
  /** Aborts creation; `create` then rejects with the signal's reason and disposes the node. */
  readonly signal?: AbortSignal;
  /**
   * Called for every error the running node reports: a processor exception, a rejected
   * configuration, a trap in the DSP module, or an output it cannot fill. Defaults to
   * `console.error`, so none of them is silent.
   */
  readonly onError?: (error: Error) => void;
}

/**
 * What a node does differently from the export ({@link AudioWorkletHostOptions.onDiagnostic}).
 * `resampled`: the context runs at another rate than the project, so the node converts the ring's
 * audio to the context's rate.
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
 * How late a node's output is behind the samples it takes, in samples at the context's rate. An
 * `AudioPlaybackClock` subtracts it (its `processing` argument).
 */
export interface AudioProcessingDelay {
  readonly latencySamples: number;
}

/** The live delay of every node {@link Alpha2AudioWorkletHost.create} made. */
const delays = new WeakMap<AudioWorkletNode, { latencySamples: number }>();

/** The delay of a node this module did not make: none. */
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

/** Fetches the packaged DSP bytes for transfer into the AudioWorklet realm. */
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
   * Adds the processor module to `context`, creates the node and starts the DSP module in it from
   * a copy of `wasm` (the caller's buffer is not detached). Resolves once the processor reports
   * ready; later errors go to `options.onError`.
   *
   * Rejects when the module cannot be added, the processor fails or reports an error while
   * starting, it is not ready within 5 seconds, or `options.signal` aborts. A node that fails to
   * start is disposed.
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
        // Mix the input to the module's channels by WebAudio's speaker rules; the default "max"
        // would pass a mono source as one channel.
        channelCount: channels,
        channelCountMode: "explicit",
        channelInterpretation: "speakers",
      },
    );
    // Listening before the processor starts, so the delay it reports on starting is not missed.
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
      // The processor may still be starting; without this it would finish and run until the
      // context closes.
      disposeAudioWorklet(node);
      untrack();
      node.port.close();
      throw error;
    }
  }

  /**
   * The node's delay, live, in samples at the context's rate: the samples its master chain holds
   * its input back by (the lookahead of the master's compressors; the limiter has none), and, while
   * it converts a ring at the project's rate to the context's, the source samples the converter
   * reads past the one it plays (16 to 18 at the usual rates), so its output is that much behind
   * what it takes from the ring or its input. Zero without either (an engine-mixed node at the
   * project's rate), and for a node `create` did not make.
   *
   * The engine reads ahead by its own latency, so its mix is never late; the worklet cannot read
   * ahead of live input. The value follows each configuration once the processor has installed it,
   * which it reports on its port; pass the object to `AudioPlaybackClock` so the clock says
   * what is being heard.
   */
  static latency(node: AudioWorkletNode): AudioProcessingDelay {
    return delays.get(node) ?? NO_DELAY;
  }
}

/**
 * Keeps the delay the processor reports for {@link Alpha2AudioWorkletHost.latency}. Returns what
 * stops it, for a node that failed to start.
 */
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

/** Keeps the node's error channels open for as long as it runs. */
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

/** Makes a transferable, tightly bounded copy without detaching caller data. */
function copyWasmBytes(source: BufferSource): ArrayBuffer {
  if (source instanceof ArrayBuffer) return source.slice(0);
  return new Uint8Array(
    source.buffer,
    source.byteOffset,
    source.byteLength,
  ).slice().buffer;
}

/**
 * The configurations a node's processor has yet to answer. Each request carries an id the processor
 * answers with `alpha2-applied` once it is in place, or with why it is not.
 */
interface Requests {
  next: number;
  readonly pending: Map<
    number,
    { resolve: () => void; reject: (error: Error) => void }
  >;
  /** Why the node will answer nothing more: disposed, or its processor threw. */
  closed?: Error;
}

const requests = new WeakMap<AudioWorkletNode, Requests>();

/** The node's requests, listening for their answers from the first one on. */
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

/** Rejects every request the node has not answered, and every later one, with `reason`. */
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
 * Posts `message` to the node's processor. Resolves once the processor has applied it, so the
 * quantum after the promise resolves runs with it; rejects with why the processor could not apply
 * it, or when the node is disposed or its processor throws first.
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
 * Points the node at a shared audio ring instead of its WebAudio input.
 *
 * Pass `undefined` to return the node to input processing. Both sides must agree to clear the ring
 * at a seek.
 *
 * Resolves once the processor plays from the ring (and {@link Alpha2AudioWorkletHost.latency}
 * includes a conversion's delay): the processor takes messages between render quanta, so a quantum
 * rendered before then plays the input. Await it before an `OfflineAudioContext`'s
 * `startRendering()`, whose first quanta may otherwise render without the ring. Rejects when the
 * node is disposed or its processor throws before it answers.
 */
export function useAudioRing(
  node: AudioWorkletNode,
  ring: AudioRingTransfer | undefined,
): Promise<void> {
  return request(node, { type: "alpha2-ring", ring });
}

/**
 * Installs a project master bus in the running worklet, as an edit: the effects it keeps, and those
 * it changes in place, keep their history, so moving a master control does not restart the reverb's
 * tail or empty the compressor's lookahead.
 *
 * The worklet applies the master as the engine does (effects, then gain, then limiter), except the
 * loudness normalization a host sets on the engine, which the master bus does not carry.
 *
 * Resolves once the processor runs the new master, with {@link Alpha2AudioWorkletHost.latency}
 * already its delay. Rejects with a `RangeError`, posting nothing, for a keyframed master gain
 * ({@link encodeMasterAudio}); with the processor's reason when it refuses the record (the chain it
 * was running keeps running); and when the node is disposed or its processor throws before it
 * answers.
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
 * Configures the worklet as a transparent transport for engine-mixed audio.
 *
 * `renderAudio` has already applied the project master gain, effects, and limiter. Ring-buffer
 * preview must therefore bypass the worklet master chain or preview would process it twice while
 * offline export processes it once.
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
 * Clears the worklet's DSP history and transport position. Resolves once the processor has cleared
 * them; rejects when the node is disposed or its processor throws before it answers.
 */
export function resetAudioWorklet(node: AudioWorkletNode): Promise<void> {
  return request(node, { type: "alpha2-reset" });
}

/**
 * Stops the node for good and disconnects it.
 *
 * A processor runs until its `AudioContext` closes unless it is told to stop: disconnecting alone
 * leaves it processing every quantum and holding its DSP module's memory. This frees the module and
 * retires the processor, including one still starting. Call it for every node a host stops using
 * while the context lives on. A configuration the processor has not answered yet, and any made
 * afterwards, rejects.
 */
export function disposeAudioWorklet(node: AudioWorkletNode): void {
  closeRequests(node, new Error("the AudioWorklet node was disposed"));
  node.port.postMessage({ type: "alpha2-dispose" });
  node.disconnect();
}

/**
 * The reason a signal carries, as an `Error` a caller can reject with.
 *
 * `AbortSignal.reason` is typed `any` and a caller may abort with a value that is not an `Error`,
 * which would reject without a stack. An `Error` passes through untouched, so an `AbortError` keeps
 * its name.
 */
function abortError(signal: AbortSignal | undefined): Error {
  const reason: unknown = signal?.reason;
  if (reason instanceof Error) return reason;
  if (typeof reason === "string") return new DOMException(reason, "AbortError");
  return new DOMException("Aborted", "AbortError");
}
