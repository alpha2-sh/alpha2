import type { AudioEffect, MasterAudio } from "./project.js";
import { TICKS_PER_SECOND } from "./project.js";

/**
 * Browser plumbing for engine-mixed timeline audio.
 *
 * The engine mixes in the wasm worker, and blocks reach the audio thread through
 * {@link AudioRingBuffer}, a lock-free SPSC ring in shared memory, so neither side waits on the
 * other.
 *
 * `SharedArrayBuffer` requires a cross-origin-isolated document; check
 * {@link canShareAudioMemory} first.
 */

/**
 * Control slots in the ring's shared header. Each slot has a single writer: the reader owns
 * `READ_INDEX` and `CLEARED_EPOCH`, the writer the rest. A clear is a request (`CLEAR_TO`, then a
 * new `CLEAR_EPOCH`) that the reader carries out at its next read.
 */
const READ_INDEX = 0;
const WRITE_INDEX = 1;
const CLEAR_TO = 2;
const CLEAR_EPOCH = 3;
const CLEARED_EPOCH = 4;
const HEADER_SLOTS = 5;

/** Byte length of an {@link AudioRingTransfer}'s `control` buffer. */
export const AUDIO_RING_CONTROL_BYTES =
  HEADER_SLOTS * Int32Array.BYTES_PER_ELEMENT;

/** Whether `SharedArrayBuffer` exists and the document is cross-origin isolated. */
export function canShareAudioMemory(): boolean {
  return (
    typeof SharedArrayBuffer === "function" &&
    (globalThis as { crossOriginIsolated?: boolean }).crossOriginIsolated ===
      true
  );
}

/** Transferable description of one ring, posted to the worklet and worker. */
export interface AudioRingTransfer {
  /** At least {@link AUDIO_RING_CONTROL_BYTES} long. */
  readonly control: SharedArrayBuffer;
  /** `channels` consecutive planes of `capacity` 32-bit floats. */
  readonly samples: SharedArrayBuffer;
  readonly channels: number;
  /** Samples per channel, a power of two. */
  readonly capacity: number;
}

/**
 * A lock-free planar audio ring for exactly one writer and one reader.
 *
 * Indices are sample counts wrapping at 2^32; a power-of-two capacity keeps slots stable across
 * the wrap. Each side publishes its index with `Atomics.store` after touching the samples, so the
 * other side never sees samples before they are written.
 */
export class AudioRingBuffer {
  readonly #control: Int32Array;
  readonly #planes: readonly Float32Array[];
  readonly #capacity: number;
  readonly #mask: number;

  private constructor(transfer: AudioRingTransfer) {
    if (!isPowerOfTwo(transfer.capacity)) {
      throw new RangeError("audio ring capacity must be a power of two");
    }
    if (transfer.control.byteLength < AUDIO_RING_CONTROL_BYTES) {
      throw new RangeError("audio ring control buffer is too small");
    }
    this.#control = new Int32Array(transfer.control);
    this.#capacity = transfer.capacity;
    this.#mask = transfer.capacity - 1;
    const planes: Float32Array[] = [];
    for (let channel = 0; channel < transfer.channels; channel += 1) {
      planes.push(
        new Float32Array(
          transfer.samples,
          channel * transfer.capacity * Float32Array.BYTES_PER_ELEMENT,
          transfer.capacity,
        ),
      );
    }
    this.#planes = planes;
  }

  /**
   * Allocates a ring of at least `capacity` samples per channel, rounded up to a power of two.
   *
   * @param channels 1 or 2.
   * @param capacity From 2 to 2^30; fractions round up.
   * @throws RangeError for a channel count or capacity out of range.
   * @throws Error when shared memory is unavailable ({@link canShareAudioMemory} is false).
   */
  static create(channels: number, capacity: number): AudioRingBuffer {
    if (!Number.isInteger(channels) || channels < 1 || channels > 2) {
      throw new RangeError("audio ring supports mono or stereo");
    }
    if (!(capacity >= 2 && capacity <= 2 ** 30)) {
      throw new RangeError(
        "audio ring capacity must be between two samples and 2^30",
      );
    }
    const rounded = 2 ** Math.ceil(Math.log2(capacity));
    if (!canShareAudioMemory()) {
      throw new Error(
        "SharedArrayBuffer is unavailable; serve the page cross-origin isolated",
      );
    }
    return new AudioRingBuffer({
      control: new SharedArrayBuffer(AUDIO_RING_CONTROL_BYTES),
      samples: new SharedArrayBuffer(
        channels * rounded * Float32Array.BYTES_PER_ELEMENT,
      ),
      channels,
      capacity: rounded,
    });
  }

  /**
   * Attaches to a transferred ring, sharing its memory.
   *
   * @throws RangeError when the capacity is not a power of two or the control buffer is too small.
   */
  static attach(transfer: AudioRingTransfer): AudioRingBuffer {
    return new AudioRingBuffer(transfer);
  }

  /** Handle to post to the other thread; the memory is shared, not copied. */
  get transfer(): AudioRingTransfer {
    return {
      control: this.#control.buffer as SharedArrayBuffer,
      samples: this.#planes[0]?.buffer as SharedArrayBuffer,
      channels: this.#planes.length,
      capacity: this.#capacity,
    };
  }

  get channels(): number {
    return this.#planes.length;
  }

  get capacity(): number {
    return this.#capacity;
  }

  /** Samples per channel written since the last clear, even if the reader has not applied it. */
  get available(): number {
    const write = Atomics.load(this.#control, WRITE_INDEX) >>> 0;
    const from = this.#clearPending()
      ? Atomics.load(this.#control, CLEAR_TO) >>> 0
      : Atomics.load(this.#control, READ_INDEX) >>> 0;
    const available = (write - from) >>> 0;
    return available <= this.#capacity ? available : 0;
  }

  /**
   * Samples per channel the writer may still push. Audio a pending clear discards still occupies
   * its slots until the reader applies the clear.
   */
  get writable(): number {
    const write = Atomics.load(this.#control, WRITE_INDEX) >>> 0;
    const read = Atomics.load(this.#control, READ_INDEX) >>> 0;
    const held = (write - read) >>> 0;
    return held <= this.#capacity ? this.#capacity - held : 0;
  }

  #clearPending(): boolean {
    return (
      Atomics.load(this.#control, CLEAR_EPOCH) !==
      Atomics.load(this.#control, CLEARED_EPOCH)
    );
  }

  /**
   * Writer side: writes up to `frames` samples per channel and returns how many were accepted.
   * Missing planes take `planes[0]`; non-finite samples become 0.
   *
   * @throws RangeError when `frames` is not a non-negative whole number, before any index moves.
   */
  write(planes: readonly Float32Array[], frames: number): number {
    checkFrames(frames);
    const accepted = Math.min(frames, this.writable);
    if (accepted <= 0) return 0;
    const write = Atomics.load(this.#control, WRITE_INDEX) >>> 0;
    for (let channel = 0; channel < this.#planes.length; channel += 1) {
      const target = this.#planes[channel];
      const source = (planes[channel] ?? planes[0]) as Float32Array | undefined;
      if (source === undefined) continue;
      for (let index = 0; index < accepted; index += 1) {
        const sample = source[index];
        target[(write + index) & this.#mask] = Number.isFinite(sample)
          ? sample
          : 0;
      }
    }
    Atomics.store(this.#control, WRITE_INDEX, write + accepted);
    return accepted;
  }

  /**
   * Reader side: applies a pending clear, then reads up to `frames` samples per channel and returns
   * how many were produced. The rest is filled with silence. Channels fold as WebAudio's speaker
   * rules do: extra planes take channel 0, and a mono read of a stereo ring averages both.
   *
   * @throws RangeError when `frames` is not a non-negative whole number, before any index moves.
   */
  read(planes: readonly Float32Array[], frames: number): number {
    checkFrames(frames);
    const read = this.#applyClear();
    const write = Atomics.load(this.#control, WRITE_INDEX) >>> 0;
    const available = (write - read) >>> 0;
    const produced = Math.min(
      frames,
      available <= this.#capacity ? available : 0,
    );
    const left = this.#planes[0];
    const right = planes.length === 1 ? this.#planes[1] : undefined;
    for (let channel = 0; channel < planes.length; channel += 1) {
      const target = planes[channel];
      const source = (this.#planes[channel] ?? left) as
        Float32Array | undefined;
      for (let index = 0; index < frames; index += 1) {
        if (index >= produced || source === undefined) {
          target[index] = 0;
          continue;
        }
        const slot = (read + index) & this.#mask;
        target[index] =
          right === undefined ? source[slot] : (source[slot] + right[slot]) / 2;
      }
    }
    if (produced > 0) Atomics.store(this.#control, READ_INDEX, read + produced);
    return produced;
  }

  /**
   * Reader side: applies a pending clear and returns the read index. The index only moves forward,
   * so the writer never overestimates free space. Keep in sync with the worklet processor's reader.
   */
  #applyClear(): number {
    let read = Atomics.load(this.#control, READ_INDEX) >>> 0;
    const epoch = Atomics.load(this.#control, CLEAR_EPOCH);
    if (epoch !== Atomics.load(this.#control, CLEARED_EPOCH)) {
      const to = Atomics.load(this.#control, CLEAR_TO) >>> 0;
      if ((to - read) >>> 0 <= this.#capacity) read = to;
      Atomics.store(this.#control, READ_INDEX, read);
      Atomics.store(this.#control, CLEARED_EPOCH, epoch);
    }
    return read;
  }

  /**
   * Writer side: discards everything written so far. The reader applies it at its next read, so the
   * writer may immediately write audio for the new position.
   */
  clear(): void {
    const write = Atomics.load(this.#control, WRITE_INDEX);
    Atomics.store(this.#control, CLEAR_TO, write);
    Atomics.add(this.#control, CLEAR_EPOCH, 1);
  }
}

/** Rejects non-integer counts; NaN would corrupt an index. */
function checkFrames(frames: number): void {
  if (!Number.isSafeInteger(frames) || frames < 0) {
    throw new RangeError(
      "audio frame count must be a non-negative whole number",
    );
  }
}

function positiveFrames(value: number, name: string): number {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new RangeError(
      `audio pump ${name} must be a positive whole number of samples`,
    );
  }
  return value;
}

function isPowerOfTwo(value: number): boolean {
  return Number.isInteger(value) && value >= 2 && (value & (value - 1)) === 0;
}

/** Engine surface the pump drives, implemented by a worker over the wasm module. */
export interface TimelineAudioSource {
  /** Project sample rate. */
  readonly sampleRate: number;
  readonly channels: number;
  /**
   * Mixes `frames` samples per channel from absolute sample `startSample`, or reports the sources
   * whose media must be supplied first. `signal` aborts when the pump seeks away; the result of an
   * aborted mix is dropped.
   */
  mix(
    startSample: number,
    frames: number,
    signal: AbortSignal,
  ): Promise<
    | { readonly kind: "mixed"; readonly planes: readonly Float32Array[] }
    | { readonly kind: "waiting"; readonly missingSources: readonly string[] }
  >;
}

/** Tuning for an {@link AudioPump}. */
export interface AudioPumpOptions {
  /**
   * Samples per channel per mix step (default 1024). Keep it within the engine's `audioMaxBlock`
   * when mixing with `renderAudio`. Must be a whole number.
   */
  readonly blockFrames?: number;
  /** Ring fill to maintain, in samples per channel. Defaults to half the ring capacity. */
  readonly targetFrames?: number;
  /**
   * Called with missing source IDs. {@link AudioPump.fill} awaits it and then stops; call `fill`
   * again once the media is supplied.
   */
  readonly onRefill?: (
    missingSources: readonly string[],
  ) => Promise<void> | void;
}

/**
 * Keeps an {@link AudioRingBuffer} ahead of the audio thread. The host drives it from its own
 * scheduler; this package owns no clock.
 *
 * The constructor throws `RangeError` when `startSample` is not a non-negative integer, or
 * `blockFrames` or `targetFrames` is not a positive one.
 */
export class AudioPump {
  readonly #source: TimelineAudioSource;
  readonly #ring: AudioRingBuffer;
  readonly #blockFrames: number;
  readonly #targetFrames: number;
  readonly #onRefill?: (
    missingSources: readonly string[],
  ) => Promise<void> | void;
  #nextSample: number;
  #startSample: number;
  #running = false;
  // Aborted and replaced on every seek, marking in-flight mixes stale.
  #position = new AbortController();

  constructor(
    source: TimelineAudioSource,
    ring: AudioRingBuffer,
    startSample = 0,
    options: AudioPumpOptions = {},
  ) {
    if (!Number.isInteger(startSample) || startSample < 0) {
      throw new RangeError("audio pump must start at a non-negative sample");
    }
    this.#source = source;
    this.#ring = ring;
    this.#nextSample = startSample;
    this.#startSample = startSample;
    this.#blockFrames = positiveFrames(
      options.blockFrames ?? 1_024,
      "blockFrames",
    );
    this.#targetFrames = positiveFrames(
      options.targetFrames ?? Math.floor(ring.capacity / 2),
      "targetFrames",
    );
    this.#onRefill = options.onRefill;
  }

  /** The project sample rate. */
  get sampleRate(): number {
    return this.#source.sampleRate;
  }

  /** Absolute sample index the next mixed block starts at. */
  get position(): number {
    return this.#nextSample;
  }

  /** Absolute sample index playback last started at or was sought to. */
  get startSample(): number {
    return this.#startSample;
  }

  /**
   * Moves the pump to `startSample` and clears buffered audio. An in-flight mix is aborted and
   * dropped; a running fill continues from the new position.
   *
   * @throws RangeError when `startSample` is not a non-negative integer.
   */
  seek(startSample: number): void {
    if (!Number.isInteger(startSample) || startSample < 0) {
      throw new RangeError("audio pump must seek to a non-negative sample");
    }
    this.#position.abort(
      new DOMException("Audio pump sought elsewhere", "AbortError"),
    );
    this.#position = new AbortController();
    this.#nextSample = startSample;
    this.#startSample = startSample;
    this.#ring.clear();
  }

  /**
   * Mixes until the ring reaches its target fill or the engine needs media, resolving with the
   * samples per channel written. A concurrent call resolves with 0 immediately. Rejects when `mix`
   * or `onRefill` rejects, unless a seek made the mix stale.
   */
  async fill(): Promise<number> {
    if (this.#running) return 0;
    this.#running = true;
    let written = 0;
    try {
      while (this.#ring.available < this.#targetFrames) {
        const frames = Math.min(this.#blockFrames, this.#ring.writable);
        if (frames <= 0) break;
        const position = this.#position;
        let step: Awaited<ReturnType<TimelineAudioSource["mix"]>>;
        try {
          step = await this.#source.mix(
            this.#nextSample,
            frames,
            position.signal,
          );
        } catch (error) {
          if (position.signal.aborted) continue;
          throw error;
        }
        // Stale mix from before a seek.
        if (position.signal.aborted) continue;
        if (step.kind === "waiting") {
          await this.#onRefill?.(step.missingSources);
          // A seek happened meanwhile; the new position may need no media.
          if (position.signal.aborted) continue;
          break;
        }
        const accepted = this.#ring.write(step.planes, frames);
        this.#nextSample += accepted;
        written += accepted;
        if (accepted < frames) break;
      }
    } finally {
      this.#running = false;
    }
    return written;
  }
}

/** Audio output timing; an `AudioContext` satisfies it. */
export interface AudioOutputTiming {
  readonly sampleRate: number;
  /** Seconds from the audio thread to the speaker, when known. */
  readonly outputLatency?: number;
  readonly baseLatency?: number;
  readonly currentTime?: number;
  getOutputTimestamp?(): AudioTimestamp;
}

/**
 * Source of an {@link AudioPlaybackClock}'s output latency, in order of preference: a positive
 * `outputLatency`, the output timestamp, or `baseLatency` alone (which omits device and wireless
 * delay).
 */
export type AudioLatencySource =
  "output-latency" | "output-timestamp" | "base-only";

export interface AudioPlaybackClockOptions {
  /**
   * Delay of the ring's player, read at every query; pass `Alpha2AudioWorkletHost.latency(node)`
   * for a worklet that runs the master chain.
   */
  readonly processing?: { readonly latencySamples: number };
  /**
   * Seconds added to the measured latency for unreported delay, such as a wireless link. Positive
   * shows the picture later. Default 0.
   */
  readonly latencyOffsetSeconds?: number;
}

/**
 * The timeline position being heard now, for presenting video against the audio clock.
 *
 * A wall clock drifts from the audio device's crystal and ignores underruns. This clock counts
 * samples the audio thread has consumed from the ring and subtracts output latency and the manual
 * offset, so presenting `timelineTicks` each animation frame stays in sync without drift.
 *
 * The position never goes before the start or seek target. Positions are at the project rate;
 * latency is converted from seconds and `processing` from output-rate samples, so a resampling
 * context keeps time.
 *
 * The constructor throws `RangeError` for a non-positive sample rate or a non-finite offset.
 */
export class AudioPlaybackClock {
  readonly #pump: AudioPump;
  readonly #ring: AudioRingBuffer;
  readonly #output: AudioOutputTiming;
  readonly #processing: { readonly latencySamples: number } | undefined;
  #offset: number;

  constructor(
    pump: AudioPump,
    ring: AudioRingBuffer,
    output: AudioOutputTiming,
    { processing, latencyOffsetSeconds = 0 }: AudioPlaybackClockOptions = {},
  ) {
    if (!(output.sampleRate > 0) || !(pump.sampleRate > 0)) {
      throw new RangeError("audio clock needs a positive sample rate");
    }
    this.#pump = pump;
    this.#ring = ring;
    this.#output = output;
    this.#processing = processing;
    this.#offset = checkedOffset(latencyOffsetSeconds);
  }

  /** Manual offset in seconds; a non-finite value throws `RangeError`. */
  get latencyOffsetSeconds(): number {
    return this.#offset;
  }

  set latencyOffsetSeconds(seconds: number) {
    this.#offset = checkedOffset(seconds);
  }

  #latency(): { seconds: number; source: AudioLatencySource } {
    const output = this.#output;
    const base = output.baseLatency ?? 0;
    const reported = output.outputLatency ?? 0;
    if (reported > 0)
      return { seconds: reported + base, source: "output-latency" };
    const stamp = output.getOutputTimestamp?.();
    if (
      stamp?.contextTime !== undefined &&
      stamp.performanceTime !== undefined &&
      output.currentTime !== undefined
    ) {
      // Difference between the next render time and the time heard now is the full output delay.
      const heard =
        stamp.contextTime + (performance.now() - stamp.performanceTime) / 1000;
      const seconds = output.currentTime - heard;
      if (seconds > 0) return { seconds, source: "output-timestamp" };
    }
    return { seconds: base, source: "base-only" };
  }

  get latencySource(): AudioLatencySource {
    return this.#latency().source;
  }

  /** Output latency in seconds, excluding the manual offset. */
  get latencySeconds(): number {
    return this.#latency().seconds;
  }

  /** Absolute sample being heard now, at the project rate. */
  get sample(): number {
    const rate = this.#pump.sampleRate;
    const latency =
      (this.#latency().seconds + this.#offset) * rate +
      ((this.#processing?.latencySamples ?? 0) * rate) /
        this.#output.sampleRate;
    const handedOver = this.#pump.position - this.#ring.available;
    return Math.max(this.#pump.startSample, Math.floor(handedOver - latency));
  }

  /** Timeline ticks being heard now, for `Alpha2Player.present`. */
  get timelineTicks(): bigint {
    return (
      (BigInt(this.sample) * BigInt(TICKS_PER_SECOND)) /
      BigInt(Math.round(this.#pump.sampleRate))
    );
  }
}

function checkedOffset(seconds: number): number {
  if (!Number.isFinite(seconds))
    throw new RangeError(
      "audio latency offset must be a finite number of seconds",
    );
  return seconds;
}

/** Worklet master-chain record version and effect node ids. Must match the DSP module. */
const EFFECT_EQ_BAND = 0;
const EFFECT_COMPRESSOR = 1;
const EFFECT_REVERB = 2;
const CONFIG_VERSION = 1;

const BAND_KINDS: Readonly<Record<string, number>> = {
  peaking: 0,
  low_pass: 1,
  high_pass: 2,
};

/** Schema default limiter. */
const DEFAULT_LIMITER = { threshold: 1, release_ms: 50 } as const;

/**
 * Encodes a master bus into the worklet's binary record, since the worklet has no JSON parser.
 *
 * Little-endian layout: version u32, gain f32, limiter flag u8, limiter threshold f32, limiter
 * release f32, node count u32, then the nodes. Missing fields take schema defaults (unity gain,
 * 0 dBFS 50 ms limiter); only `limiter: null` removes the limiter. Each EQ band becomes one node.
 *
 * @throws RangeError for a keyframed master gain. Use `configureEngineMixedAudio`, or pass the gain
 *   evaluated at the time being heard.
 */
export function encodeMasterAudio(master: MasterAudio | undefined): Uint8Array {
  const gain: NonNullable<MasterAudio["gain"]> = master?.gain ?? {
    mode: "constant",
    value: 1,
  };
  if (gain.mode !== "constant") {
    throw new RangeError(
      "the worklet master record holds one static gain, so a keyframed master gain cannot be encoded; preview it through configureEngineMixedAudio, or pass the gain evaluated at the time being heard",
    );
  }
  const effects = master?.effects ?? [];
  const nodes: number[][] = [];
  for (const effect of effects) {
    nodes.push(...encodeEffect(effect));
  }
  const size =
    4 +
    4 +
    1 +
    4 +
    4 +
    4 +
    nodes.reduce((total, node) => total + node.length, 0);
  const bytes = new Uint8Array(size);
  const view = new DataView(bytes.buffer);
  let at = 0;
  view.setUint32(at, CONFIG_VERSION, true);
  at += 4;
  view.setFloat32(at, gain.value, true);
  at += 4;
  const limiter =
    master?.limiter === undefined ? DEFAULT_LIMITER : master.limiter;
  if (limiter === null) {
    bytes[at] = 0;
    at += 1;
    view.setFloat32(at, 0, true);
    at += 4;
    view.setFloat32(at, 0, true);
    at += 4;
  } else {
    bytes[at] = 1;
    at += 1;
    view.setFloat32(at, limiter.threshold, true);
    at += 4;
    view.setFloat32(at, limiter.release_ms, true);
    at += 4;
  }
  view.setUint32(at, nodes.length, true);
  at += 4;
  for (const node of nodes) {
    bytes.set(node, at);
    at += node.length;
  }
  return bytes;
}

function encodeEffect(effect: AudioEffect): number[][] {
  const float = (value: number): number[] => {
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setFloat32(0, value, true);
    return [...bytes];
  };
  if (effect.type === "eq") {
    return effect.bands.map((band) => [
      EFFECT_EQ_BAND,
      BAND_KINDS[band.kind] ?? 0,
      ...float(band.frequency_hz),
      ...float(band.q),
      ...float(band.gain_db ?? 0),
    ]);
  }
  if (effect.type === "compressor") {
    return [
      [
        EFFECT_COMPRESSOR,
        ...float(effect.threshold_db),
        ...float(effect.ratio),
        ...float(effect.attack_ms),
        ...float(effect.release_ms),
        ...float(effect.lookahead_ms ?? 0),
      ],
    ];
  }
  return [
    [
      EFFECT_REVERB,
      ...float(effect.room_size),
      ...float(effect.damping),
      ...float(effect.mix),
    ],
  ];
}
