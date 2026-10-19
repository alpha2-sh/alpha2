declare abstract class AudioWorkletProcessor {
  readonly port: MessagePort;
  constructor(options?: AudioWorkletNodeOptions);
  abstract process(
    inputs: Float32Array[][],
    outputs: Float32Array[][],
  ): boolean;
}
declare function registerProcessor(
  name: string,
  constructor: typeof AudioWorkletProcessor,
): void;
declare const sampleRate: number;

/** The most frames the DSP module processes in one call (`MAX_FRAMES` in its source). */
const MODULE_MAX_FRAMES = 4096;
/** The render quantum of every context that cannot choose another. */
const DEFAULT_QUANTUM = 128;

/**
 * Frames per module call: the scope's render quantum where it says one the module can take whole,
 * so a quantum is one call, and the default quantum otherwise. `process` splits any longer quantum
 * into calls of this size.
 */
function moduleFrames(): number {
  const quantum = (globalThis as { renderQuantumSize?: unknown })
    .renderQuantumSize;
  return typeof quantum === "number" &&
    Number.isInteger(quantum) &&
    quantum >= 1 &&
    quantum <= MODULE_MAX_FRAMES
    ? quantum
    : DEFAULT_QUANTUM;
}

interface WorkletExports extends WebAssembly.Exports {
  readonly memory: WebAssembly.Memory;
  alpha2_worklet_create(rate: number, frames: number, channels: number): number;
  alpha2_worklet_input(handle: number): number;
  alpha2_worklet_output(handle: number): number;
  alpha2_worklet_frames(handle: number): number;
  alpha2_worklet_channels(handle: number): number;
  alpha2_worklet_process(
    handle: number,
    frames: number,
    channels: number,
  ): number;
  alpha2_worklet_config_buffer(handle: number): number;
  alpha2_worklet_config_capacity(): number;
  alpha2_worklet_configure(handle: number, length: number): number;
  alpha2_worklet_reset(handle: number): number;
  alpha2_worklet_transparent(handle: number): number;
  alpha2_worklet_latency(handle: number): number;
  alpha2_worklet_set_source_rate(handle: number, rate: number): number;
  alpha2_worklet_source(handle: number): number;
  alpha2_worklet_source_frames(handle: number): number;
  alpha2_worklet_source_need(handle: number, frames: number): number;
  alpha2_worklet_source_latency(handle: number): number;
  alpha2_worklet_resample(
    handle: number,
    written: number,
    frames: number,
  ): number;
  alpha2_worklet_panic_message(): number;
  alpha2_worklet_panic_length(): number;
  alpha2_worklet_destroy(handle: number): number;
}

/** UTF-8 to a string without `TextDecoder`, which worklet scopes lack. */
function decodeUtf8(bytes: Uint8Array): string {
  let text = "";
  for (let index = 0; index < bytes.length;) {
    const lead = bytes[index] ?? 0;
    const length = lead < 0x80 ? 1 : lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4;
    let code = length === 1 ? lead : lead & (0x7f >> length);
    for (let next = 1; next < length; next += 1)
      code = (code << 6) | ((bytes[index + next] ?? 0) & 0x3f);
    text += String.fromCodePoint(code);
    index += length;
  }
  return text;
}

/**
 * Reader half of the package's shared audio ring.
 *
 * The index arithmetic is inlined here because a worklet module cannot rely on bundler-resolved
 * imports. `AudioRingBuffer` in `alpha2/audio-timeline` is the writer half and the tested reference
 * for this layout.
 */
class RingReader {
  readonly #control: Int32Array;
  readonly #planes: Float32Array[];
  readonly #capacity: number;
  readonly #mask: number;

  constructor(transfer: {
    control: SharedArrayBuffer;
    samples: SharedArrayBuffer;
    channels: number;
    capacity: number;
  }) {
    this.#control = new Int32Array(transfer.control);
    this.#capacity = transfer.capacity;
    // `AudioRingBuffer` guarantees a power of two, so slots survive the wrap.
    this.#mask = transfer.capacity - 1;
    this.#planes = [];
    for (let channel = 0; channel < transfer.channels; channel += 1) {
      this.#planes.push(
        new Float32Array(
          transfer.samples,
          channel * transfer.capacity * Float32Array.BYTES_PER_ELEMENT,
          transfer.capacity,
        ),
      );
    }
  }

  /**
   * Fills the first `frames` samples of each target channel and returns how many real samples were
   * produced. Channels fold as `#readInput` describes.
   */
  read(targets: readonly Float32Array[], frames: number): number {
    // Slots: 0 read, 1 write, 2 clear target, 3 clear epoch, 4 cleared epoch. A clear is the
    // writer's request; the reader carries it out.
    let read = Atomics.load(this.#control, 0) >>> 0;
    const epoch = Atomics.load(this.#control, 3);
    if (epoch !== Atomics.load(this.#control, 4)) {
      const to = Atomics.load(this.#control, 2) >>> 0;
      if ((to - read) >>> 0 <= this.#capacity) read = to;
      Atomics.store(this.#control, 0, read);
      Atomics.store(this.#control, 4, epoch);
    }
    const write = Atomics.load(this.#control, 1) >>> 0;
    const available = (write - read) >>> 0;
    const produced = Math.min(
      frames,
      available <= this.#capacity ? available : 0,
    );
    const left = this.#planes[0];
    const right = targets.length === 1 ? this.#planes[1] : undefined;
    for (let channel = 0; channel < targets.length; channel += 1) {
      const source = this.#planes[channel] ?? left;
      const target = targets[channel];
      if (target === undefined) continue;
      for (let index = 0; index < frames; index += 1) {
        let sample = 0;
        if (index < produced && source !== undefined) {
          const slot = (read + index) & this.#mask;
          sample =
            right === undefined
              ? (source[slot] ?? 0)
              : ((source[slot] ?? 0) + (right[slot] ?? 0)) / 2;
        }
        target[index] = Number.isFinite(sample) ? sample : 0;
      }
    }
    if (produced > 0) Atomics.store(this.#control, 0, read + produced);
    return produced;
  }
}

/** No input connected: WebAudio passes no channels. */
const NO_CHANNELS: readonly Float32Array[] = Object.freeze([]);

/**
 * Per-channel views of the module's input and output buffers, `frames` long.
 *
 * `process` runs a few hundred times a second on the audio thread, and a typed-array view made per
 * call there is garbage the thread has to collect. These are made once and kept while they stay
 * valid: a wasm memory that grows detaches its old `ArrayBuffer` and hands out a new one, so the
 * buffer's identity is the key, checked on every call. A quantum longer than the module's runs as
 * full calls and one shorter remainder, so two lengths are kept.
 */
interface ChannelViews {
  readonly buffer: ArrayBuffer;
  readonly frames: number;
  readonly input: readonly Float32Array[];
  readonly output: readonly Float32Array[];
}

class Alpha2Processor extends AudioWorkletProcessor {
  #exports?: WorkletExports;
  #handle = 0;
  #inputPointer = 0;
  #outputPointer = 0;
  #maxFrames = DEFAULT_QUANTUM;
  #channels = 0;
  #ring?: RingReader;
  #underruns = 0;
  #shapeReported = false;
  #views: ChannelViews[] = [];
  // Once set, `process` returns false so the audio thread retires the node.
  #disposed = false;
  // The master chain is the identity, so the input is copied straight through without the module.
  #transparent = false;
  // The samples the master chain delays its input by, as last posted to the host.
  #latency = 0;
  // Where the module takes ring samples at the project's rate to convert to the context's, and
  // their per-channel stride; 0 when the ring carries the context's rate.
  #sourcePointer = 0;
  #sourceFrames = 0;
  #sourceViews?: { buffer: ArrayBuffer; planes: Float32Array[] };

  constructor() {
    super();
    this.port.onmessage = (
      event: MessageEvent<{
        type: string;
        wasm?: ArrayBuffer;
        channels?: number;
        sourceRate?: number;
        ring?: {
          control: SharedArrayBuffer;
          samples: SharedArrayBuffer;
          channels: number;
          capacity: number;
        };
        config?: Uint8Array;
        id?: number;
      }>,
    ) => {
      const message = event.data;
      if (this.#disposed) return;
      if (message.type === "alpha2-dispose") {
        this.#dispose();
      } else if (message.type === "alpha2-init" && message.wasm !== undefined) {
        this.port.postMessage({ type: "alpha2-stage", stage: "init-received" });
        void this.#initialize(
          message.wasm,
          message.channels ?? 2,
          message.sourceRate ?? sampleRate,
        );
      } else if (
        message.type === "alpha2-ring" ||
        message.type === "alpha2-config" ||
        message.type === "alpha2-reset"
      ) {
        let error: string | undefined;
        try {
          error = this.#apply(message);
        } catch (thrown) {
          error = thrown instanceof Error ? thrown.message : String(thrown);
        }
        // The answer follows everything applying the request posted (`alpha2-latency` included), so
        // the host knows the outcome before the next quantum resolves. Without an id, a refusal is
        // an error.
        if (message.id !== undefined)
          this.port.postMessage(
            error === undefined
              ? { type: "alpha2-applied", id: message.id }
              : { type: "alpha2-applied", id: message.id, error },
          );
        else if (error !== undefined)
          this.port.postMessage({ type: "alpha2-error", message: error });
      }
    };
    this.port.postMessage({ type: "alpha2-stage", stage: "processor-created" });
  }

  /**
   * Starts the module for `channels` at the context's rate. A ring carrying `sourceRate` (the
   * project's) is converted to the context's rate when the two differ.
   */
  async #initialize(
    wasm: ArrayBuffer,
    channels: number,
    sourceRate: number,
  ): Promise<void> {
    try {
      this.port.postMessage({
        type: "alpha2-stage",
        stage: "wasm-instantiating",
      });
      const instance = (await WebAssembly.instantiate(wasm, {})).instance;
      // Disposed while the module compiled: it never gets an instance to leak.
      if (this.#disposed) return;
      this.port.postMessage({
        type: "alpha2-stage",
        stage: "wasm-instantiated",
      });
      const exports = instance.exports as WorkletExports;
      this.#maxFrames = moduleFrames();
      const handle = exports.alpha2_worklet_create(
        sampleRate,
        this.#maxFrames,
        channels,
      );
      if (handle === 0)
        throw new Error("invalid AudioWorklet channel configuration");
      const preparedFrames = exports.alpha2_worklet_frames(handle);
      const preparedChannels = exports.alpha2_worklet_channels(handle);
      if (preparedFrames !== this.#maxFrames || preparedChannels !== channels)
        throw new Error("AudioWorklet wasm buffer shape mismatch");
      this.#exports = exports;
      this.#handle = handle;
      this.#channels = preparedChannels;
      this.#inputPointer = exports.alpha2_worklet_input(handle);
      this.#outputPointer = exports.alpha2_worklet_output(handle);
      if (sourceRate !== sampleRate) {
        if (exports.alpha2_worklet_set_source_rate(handle, sourceRate) !== 0)
          throw new Error(
            `cannot convert ${sourceRate} Hz audio to the context's ${sampleRate} Hz`,
          );
        this.#sourcePointer = exports.alpha2_worklet_source(handle);
        this.#sourceFrames = exports.alpha2_worklet_source_frames(handle);
      }
      this.#refreshTransparency();
      this.#reportLatency();
      this.port.postMessage({ type: "alpha2-ready" });
    } catch (error) {
      this.#disposed = true;
      this.port.postMessage({
        type: "alpha2-error",
        message: error instanceof Error ? error.message : String(error),
      });
    }
  }

  /** Frees the module's instance and retires the node at its next quantum. */
  #dispose(): void {
    this.#disposed = true;
    if (this.#exports !== undefined && this.#handle !== 0)
      this.#exports.alpha2_worklet_destroy(this.#handle);
    this.#exports = undefined;
    this.#handle = 0;
    this.#ring = undefined;
    this.#views = [];
    this.#sourceViews = undefined;
  }

  /** The message the module recorded for its last panic, if any. */
  #panicMessage(exports: WorkletExports): string | undefined {
    const length = exports.alpha2_worklet_panic_length();
    if (length === 0) return undefined;
    return decodeUtf8(
      new Uint8Array(
        exports.memory.buffer,
        exports.alpha2_worklet_panic_message(),
        length,
      ),
    );
  }

  /**
   * Applies a ring, master-chain or reset message. Returns why it could not, or undefined once it
   * is in place.
   */
  #apply(message: {
    type: string;
    ring?: {
      control: SharedArrayBuffer;
      samples: SharedArrayBuffer;
      channels: number;
      capacity: number;
    };
    config?: Uint8Array;
  }): string | undefined {
    if (message.type === "alpha2-ring") {
      // Without a ring the node processes whatever WebAudio feeds its input.
      this.#ring =
        message.ring === undefined ? undefined : new RingReader(message.ring);
      this.#underruns = 0;
      // The conversion delays only what comes through the ring.
      this.#reportLatency();
      return undefined;
    }
    if (message.type === "alpha2-config") {
      if (message.config === undefined)
        return "the master configuration carries no record";
      return this.#configure(message.config);
    }
    this.#exports?.alpha2_worklet_reset(this.#handle);
    this.#underruns = 0;
    this.#refreshTransparency();
    return undefined;
  }

  /** Installs a master-chain record. Returns the module's verdict: why it refused, if it did. */
  #configure(config: Uint8Array): string | undefined {
    const exports = this.#exports;
    if (exports === undefined || this.#handle === 0)
      return "the AudioWorklet processor has not started";
    const capacity = exports.alpha2_worklet_config_capacity();
    if (config.length > capacity)
      return `master configuration exceeds ${capacity} bytes`;
    const pointer = exports.alpha2_worklet_config_buffer(this.#handle);
    new Uint8Array(exports.memory.buffer, pointer, capacity).set(config);
    const result = exports.alpha2_worklet_configure(
      this.#handle,
      config.length,
    );
    this.#refreshTransparency();
    this.#reportLatency();
    return result === 0
      ? undefined
      : `master configuration was rejected (${result})`;
  }

  /**
   * Posts `alpha2-latency` when the master chain's delay changed: the host's clock subtracts it, as
   * the sound comes out that much after the samples the processor took.
   */
  #reportLatency(): void {
    const exports = this.#exports;
    if (exports === undefined || this.#handle === 0) return;
    const latency =
      exports.alpha2_worklet_latency(this.#handle) +
      (this.#ring !== undefined && this.#sourceFrames > 0
        ? exports.alpha2_worklet_source_latency(this.#handle)
        : 0);
    if (latency === this.#latency) return;
    this.#latency = latency;
    this.port.postMessage({ type: "alpha2-latency", latencySamples: latency });
  }

  #refreshTransparency(): void {
    const exports = this.#exports;
    this.#transparent =
      exports !== undefined &&
      this.#handle !== 0 &&
      exports.alpha2_worklet_transparent(this.#handle) === 1;
  }

  /** The cached channel views, remade when memory grew or `frames` changed. */
  #viewsFor(exports: WorkletExports, frames: number): ChannelViews {
    const buffer = exports.memory.buffer;
    for (const views of this.#views) {
      if (views.buffer === buffer && views.frames === frames) return views;
    }
    const channel = (pointer: number, index: number): Float32Array =>
      new Float32Array(
        buffer,
        pointer + index * this.#maxFrames * Float32Array.BYTES_PER_ELEMENT,
        frames,
      );
    const indices = Array.from({ length: this.#channels }, (_, index) => index);
    const views = {
      buffer,
      frames,
      input: indices.map((index) => channel(this.#inputPointer, index)),
      output: indices.map((index) => channel(this.#outputPointer, index)),
    };
    // Views over a detached buffer are dropped; otherwise the older length goes.
    this.#views = this.#views.filter((kept) => kept.buffer === buffer);
    if (this.#views.length >= 2) this.#views.shift();
    this.#views.push(views);
    return views;
  }

  /**
   * Reads `frames` samples into the start of each of `targets`: the next ones from the ring, or
   * WebAudio's input from `offset` in the quantum. A sample that is not a number is read as silence
   * on either path.
   *
   * Both paths fold channels as WebAudio's speaker rules do: a target past the source's channels
   * takes channel 0, and a single target fed two channels takes their average. The node asks
   * WebAudio for exactly the module's channels, so its input needs this only when a host changed
   * the node's channel mode.
   */
  #readInput(
    inputs: Float32Array[][],
    targets: readonly Float32Array[],
    frames: number,
    offset: number,
  ): void {
    if (this.#ring === undefined) {
      const source = inputs[0] ?? NO_CHANNELS;
      const right = targets.length === 1 ? source[1] : undefined;
      for (let channel = 0; channel < targets.length; channel += 1) {
        const target = targets[channel];
        const channelInput = source[channel] ?? source[0];
        if (target === undefined) continue;
        if (channelInput === undefined) {
          target.fill(0, 0, frames);
          continue;
        }
        for (let index = 0; index < frames; index += 1) {
          const sample =
            right === undefined
              ? (channelInput[offset + index] ?? 0)
              : ((channelInput[offset + index] ?? 0) +
                  (right[offset + index] ?? 0)) /
                2;
          target[index] = Number.isFinite(sample) ? sample : 0;
        }
      }
      return;
    }
    this.#readRing(this.#ring, targets, frames);
  }

  /** Reads the next `frames` ring samples into `targets`, reporting a gap. */
  #readRing(
    ring: RingReader,
    targets: readonly Float32Array[],
    frames: number,
  ): void {
    const produced = ring.read(targets, frames);
    if (produced < frames) {
      this.#underruns += 1;
      // Report the first gap of a run so a host can widen its buffer.
      if (this.#underruns === 1)
        this.port.postMessage({ type: "alpha2-underrun" });
    } else {
      this.#underruns = 0;
    }
  }

  /** The module's source planes, remade when its memory grew. */
  #sourcePlanes(exports: WorkletExports): readonly Float32Array[] {
    const buffer = exports.memory.buffer;
    if (this.#sourceViews?.buffer !== buffer) {
      this.#sourceViews = {
        buffer,
        planes: Array.from(
          { length: this.#channels },
          (_, index) =>
            new Float32Array(
              buffer,
              this.#sourcePointer +
                index * this.#sourceFrames * Float32Array.BYTES_PER_ELEMENT,
              this.#sourceFrames,
            ),
        ),
      };
    }
    return this.#sourceViews.planes;
  }

  /**
   * Plays the next `frames` of the ring, which carries the project's rate, converted to the
   * context's: the module asks for the ring samples the outputs weigh, converts them into its input
   * planes, and runs the master over them (or they are copied out as they are, for a transparent
   * master). Returns false once the module failed, after reporting why.
   */
  #processConverted(
    exports: WorkletExports,
    ring: RingReader,
    output: Float32Array[],
    frames: number,
  ): boolean {
    for (let start = 0; start < frames; start += this.#maxFrames) {
      const length = Math.min(this.#maxFrames, frames - start);
      let status: number;
      try {
        const need = exports.alpha2_worklet_source_need(this.#handle, length);
        this.#readRing(ring, this.#sourcePlanes(exports), need);
        status = exports.alpha2_worklet_resample(this.#handle, need, length);
        if (status === 0 && !this.#transparent)
          status = exports.alpha2_worklet_process(
            this.#handle,
            length,
            this.#channels,
          );
      } catch (error) {
        this.port.postMessage({
          type: "alpha2-error",
          message: `AudioWorklet DSP module trapped (${
            this.#panicMessage(exports) ?? String(error)
          }); the node has stopped`,
        });
        return false;
      }
      if (status !== 0) {
        this.port.postMessage({
          type: "alpha2-error",
          message: `AudioWorklet processing failed (${status}); the node has stopped`,
        });
        return false;
      }
      const views = this.#viewsFor(exports, length);
      const produced = this.#transparent ? views.input : views.output;
      for (let channel = 0; channel < this.#channels; channel += 1) {
        const view = produced[channel];
        if (view !== undefined) output[channel]?.set(view, start);
      }
    }
    return true;
  }

  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    if (this.#disposed) return false;
    const exports = this.#exports;
    const output = outputs[0];
    if (exports === undefined || output === undefined || this.#handle === 0)
      return true;
    const frames = output[0]?.length ?? 0;
    if (output.length !== this.#channels) {
      // WebAudio sizes the output from the node's options, so a mismatch is a host configuration
      // error.
      if (!this.#shapeReported) {
        this.#shapeReported = true;
        this.port.postMessage({
          type: "alpha2-error",
          message:
            `the node's output has ${output.length} channel(s), but the DSP ` +
            `module mixes ${this.#channels}; create the node with ` +
            `outputChannelCount [${this.#channels}]`,
        });
      }
      return true;
    }
    this.#shapeReported = false;
    if (this.#ring !== undefined && this.#sourceFrames > 0)
      return this.#processConverted(exports, this.#ring, output, frames);
    if (this.#transparent) {
      this.#readInput(inputs, output, frames, 0);
      return true;
    }
    // The context chooses the quantum; the module takes at most its prepared size per call.
    for (let start = 0; start < frames; start += this.#maxFrames) {
      const length = Math.min(this.#maxFrames, frames - start);
      this.#readInput(
        inputs,
        this.#viewsFor(exports, length).input,
        length,
        start,
      );
      let status: number;
      try {
        status = exports.alpha2_worklet_process(
          this.#handle,
          length,
          this.#channels,
        );
      } catch (error) {
        // A Rust panic traps, and the trap carries no message of its own.
        this.port.postMessage({
          type: "alpha2-error",
          message: `AudioWorklet DSP module trapped (${
            this.#panicMessage(exports) ?? String(error)
          }); the node has stopped`,
        });
        return false;
      }
      if (status !== 0) {
        // Returning false retires this node for good; this message is the host's only signal.
        this.port.postMessage({
          type: "alpha2-error",
          message: `AudioWorklet processing failed (${status}); the node has stopped`,
        });
        return false;
      }
      // Recheck the views anyway: a stale one would read a detached buffer as empty.
      const produced = this.#viewsFor(exports, length).output;
      for (let channel = 0; channel < this.#channels; channel += 1) {
        const view = produced[channel];
        if (view !== undefined) output[channel]?.set(view, start);
      }
    }
    return true;
  }
}

registerProcessor("alpha2-processor", Alpha2Processor);
