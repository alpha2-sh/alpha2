import type { AudioEffect, MasterAudio } from "./project.js";
import { TICKS_PER_SECOND } from "./project.js";

/**
 * Browser plumbing for engine-mixed timeline audio.
 *
 * The engine mixes blocks inside the worker that owns the wasm module. Those
 * blocks reach the audio thread through {@link AudioRingBuffer}, a lock-free
 * single-producer/single-consumer ring in shared memory, so the worklet never
 * waits on a message and the mixer never runs on the audio thread.
 *
 * `SharedArrayBuffer` requires a cross-origin-isolated document
 * (`Cross-Origin-Opener-Policy: same-origin` and
 * `Cross-Origin-Embedder-Policy: require-corp`). Call
 * {@link canShareAudioMemory} before choosing this path.
 */

/**
 * Control slots held in the ring's shared `Int32Array` header.
 *
 * Each slot has one writing side, which keeps the ring single-producer/single-consumer: the reader
 * owns `READ_INDEX` and `CLEARED_EPOCH`; the writer owns `WRITE_INDEX`, `CLEAR_TO`, and
 * `CLEAR_EPOCH`. A clear is a request (`CLEAR_TO`, then a new `CLEAR_EPOCH`) that the reader
 * carries out at its next read.
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

/**
 * Whether shared memory is usable for the audio ring: `SharedArrayBuffer` exists and the document
 * is cross-origin isolated.
 */
export function canShareAudioMemory(): boolean {
  return (
    typeof SharedArrayBuffer === "function" &&
    (globalThis as { crossOriginIsolated?: boolean }).crossOriginIsolated ===
      true
  );
}

/** Transferable description of one ring, posted to the worklet and worker. */
export interface AudioRingTransfer {
  /** The index header, at least {@link AUDIO_RING_CONTROL_BYTES} long. */
  readonly control: SharedArrayBuffer;
  /** `channels` planes of `capacity` 32-bit floats, one after another. */
  readonly samples: SharedArrayBuffer;
  readonly channels: number;
  /** Samples per channel; a power of two. */
  readonly capacity: number;
}

/**
 * A lock-free planar audio ring shared by one writer and one reader.
 *
 * Indices are monotonic sample counts that wrap at 2^32. The capacity is a power of two, so it
 * divides 2^32 and an index keeps its slot across the wrap. Each side publishes its index with a
 * sequentially consistent `Atomics.store` after writing or reading the samples, and the other side
 * loads it with `Atomics.load`, so neither side blocks and neither sees samples before they are
 * written. This holds only for one writer and one reader.
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
   * Allocates a ring holding at least `capacity` samples per channel, rounded up to a power of two.
   *
   * @param channels 1 or 2.
   * @param capacity From 2 to 2^30. A fraction is rounded up with the rest, so a ring sized from
   *   an odd sample rate (`sampleRate / 2` at 11 025 Hz) holds at least that much.
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
   * Attaches to a ring described by a transferred handle, sharing its memory.
   *
   * @throws RangeError when the capacity is not a power of two or the control buffer is too small.
   */
  static attach(transfer: AudioRingTransfer): AudioRingBuffer {
    return new AudioRingBuffer(transfer);
  }

  /** The handle to post to the other thread; posting it shares, not copies, the ring. */
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

  /**
   * Samples per channel the reader will still play: everything written since the last clear, even
   * before the reader has carried that clear out.
   */
  get available(): number {
    const write = Atomics.load(this.#control, WRITE_INDEX) >>> 0;
    const from = this.#clearPending()
      ? Atomics.load(this.#control, CLEAR_TO) >>> 0
      : Atomics.load(this.#control, READ_INDEX) >>> 0;
    const available = (write - from) >>> 0;
    return available <= this.#capacity ? available : 0;
  }

  /**
   * Samples per channel the writer may still push.
   *
   * Audio a pending clear discards still occupies its slots until the reader moves past it, so it
   * counts against the space here.
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
   * Writes the first `frames` samples of each plane. Writer side only.
   *
   * A channel with no plane of its own takes `planes[0]`; a non-finite sample is written as 0.
   * Returns how many samples per channel were accepted, which is less than `frames` when the reader
   * has not drained enough.
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
   * Reads `frames` samples per channel into caller-owned planes, carrying out a pending clear
   * first. Reader side only.
   *
   * Returns how many samples per channel were produced; the rest of each plane is filled with
   * silence, so an underrun is heard as a gap rather than as stale audio. Channels are mixed as
   * WebAudio's speaker rules mix them: a plane past the ring's channel count receives channel 0,
   * and a single plane read from a stereo ring receives the average of both.
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
   * Reader side: carries out a pending clear and returns the read index.
   *
   * The read index only moves forward, to where the writer stood when it asked, so the writer's
   * view of free space is never too large. The worklet processor's reader repeats this logic, so a
   * change here must be made there too.
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
   * Writer side: discards everything written so far, as a seek needs. The reader carries it out at
   * its next read, so the writer may keep writing the new position's audio at once.
   */
  clear(): void {
    const write = Atomics.load(this.#control, WRITE_INDEX);
    Atomics.store(this.#control, CLEAR_TO, write);
    Atomics.add(this.#control, CLEAR_EPOCH, 1);
  }
}

/** A frame count a ring can move by: a whole number, never NaN, which would reset an index. */
function checkFrames(frames: number): void {
  if (!Number.isSafeInteger(frames) || frames < 0) {
    throw new RangeError(
      "audio frame count must be a non-negative whole number",
    );
  }
}

/** A pump option that sizes blocks or fill, both counted in whole samples. */
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

/** The engine surface the pump drives. A worker implements it over the wasm module. */
export interface TimelineAudioSource {
  /** Project sample rate of every mixed block. */
  readonly sampleRate: number;
  /** Channel count of every mixed block. */
  readonly channels: number;
  /**
   * Mixes `frames` samples per channel starting at absolute sample index `startSample`. Resolves
   * with at least `frames` samples in each of `channels` planes, or with the sources whose media
   * must be supplied first.
   *
   * `signal` aborts when the pump seeks away from `startSample`, so a source may stop decoding
   * for it. The pump drops whatever a mix it aborted resolves or rejects with.
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
   * Samples per channel mixed per step, a positive whole number. Defaults to 1024. A source that
   * mixes with the engine's `renderAudio` mixes at most its `audioMaxBlock` per call, so keep this
   * within that. Round a block computed from a duration (`Math.ceil(sampleRate * 0.02)`).
   */
  readonly blockFrames?: number;
  /**
   * Ring fill the pump maintains, in samples per channel, a positive whole number. Defaults to
   * half the ring capacity.
   */
  readonly targetFrames?: number;
  /**
   * Called with the missing source IDs when a mix step needs media the host has not supplied.
   * {@link AudioPump.fill} awaits it, then stops; the host calls `fill` again once the media is in.
   */
  readonly onRefill?: (
    missingSources: readonly string[],
  ) => Promise<void> | void;
}

/**
 * Keeps an {@link AudioRingBuffer} ahead of the audio thread.
 *
 * The pump is driven by the host's own scheduler (a timer, a message, or the
 * worklet's own fill notifications) so this package never owns a clock.
 *
 * The constructor throws `RangeError` when `startSample` is not a non-negative integer, or a
 * `blockFrames` or `targetFrames` option is not a positive one.
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
  // Replaced by every seek and aborted with it, so a mix asked for before the seek is known stale
  // when it settles, and its source may stop working on it.
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

  /** The rate the pump mixes at: its source's, the project's. */
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
   * Moves the pump to `startSample` and clears buffered audio, as a seek needs.
   *
   * A mix still running for the old position is aborted, and what it returns is dropped: a fill
   * in progress goes on from `startSample`, so the first sample written after a seek is always
   * the target's.
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
   * Mixes until the ring holds its target fill or the engine needs media.
   *
   * Resolves with the number of samples per channel written. A call made while another is running
   * resolves with 0 at once, so a host may call this from several events; after a seek, the
   * running call carries on from the new position. Rejects when the source's `mix` or `onRefill`
   * rejects, except for a mix that a seek made stale.
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
        // Mixed for a position the pump has left: its audio would play before the target's.
        if (position.signal.aborted) continue;
        if (step.kind === "waiting") {
          await this.#onRefill?.(step.missingSources);
          // A seek made while the host supplied media: the target may need none.
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

/**
 * What the audio output reports about its own delay. An `AudioContext` satisfies it directly.
 */
export interface AudioOutputTiming {
  readonly sampleRate: number;
  /** Seconds from the audio thread to the speaker, where the browser knows. */
  readonly outputLatency?: number;
  /** Seconds of the audio thread's own processing delay. */
  readonly baseLatency?: number;
}

/**
 * The timeline position a listener is hearing now, for presenting video against the audio (closed
 * loop).
 *
 * `Alpha2Player.present(time)` shows whatever time the host passes. Deriving that time from a wall
 * clock (`performance.now()`) is open loop: the audio device runs on its own crystal, so video and
 * sound drift apart by the difference of the two clocks (tens of milliseconds over ten minutes at
 * 50 ppm), and every audio underrun or seek adds a step. This clock instead counts what the audio
 * thread has taken from the ring (everything the pump mixed, less what is still buffered) and
 * subtracts the output's latency, so a host that presents `timelineTicks` on each animation frame
 * shows the picture that belongs to the sound being heard, without drift.
 *
 * Before playback reaches the speaker, the position holds at the start or seek target instead of
 * going earlier. Positions are the pump's samples, at the project's rate; the output's latency is
 * converted from seconds and `processing` from the output's samples, so a context at another rate
 * (whose worklet converts the ring to it) keeps time. The constructor throws `RangeError` for an
 * output or pump sample rate that is not positive.
 */
export class AudioPlaybackClock {
  readonly #pump: AudioPump;
  readonly #ring: AudioRingBuffer;
  readonly #output: AudioOutputTiming;
  readonly #processing: { readonly latencySamples: number } | undefined;

  /**
   * `processing` is the delay of what plays the ring before the output, read at every query: for a
   * worklet that runs the master itself, `Alpha2AudioWorkletHost.latency(node)` (its compressors'
   * lookahead). An engine-mixed node delays nothing.
   */
  constructor(
    pump: AudioPump,
    ring: AudioRingBuffer,
    output: AudioOutputTiming,
    processing?: { readonly latencySamples: number },
  ) {
    if (!(output.sampleRate > 0) || !(pump.sampleRate > 0)) {
      throw new RangeError("audio clock needs a positive sample rate");
    }
    this.#pump = pump;
    this.#ring = ring;
    this.#output = output;
    this.#processing = processing;
  }

  /** Absolute sample index being heard now, at the pump's (the project's) rate. */
  get sample(): number {
    const rate = this.#pump.sampleRate;
    const latency =
      ((this.#output.outputLatency ?? 0) + (this.#output.baseLatency ?? 0)) *
        rate +
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

/** Record version and effect node identifiers of the worklet's master-chain record. */
const EFFECT_EQ_BAND = 0;
const EFFECT_COMPRESSOR = 1;
const EFFECT_REVERB = 2;
const CONFIG_VERSION = 1;

const BAND_KINDS: Readonly<Record<string, number>> = {
  peaking: 0,
  low_pass: 1,
  high_pass: 2,
};

/** The limiter a master bus without a `limiter` field has (the schema default). */
const DEFAULT_LIMITER = { threshold: 1, release_ms: 50 } as const;

/**
 * Encodes a project master bus into the worklet's fixed-width record.
 *
 * The worklet carries no JSON parser, so the host writes this record into the module's
 * configuration region and calls `alpha2_worklet_configure`. The layout is little-endian and
 * versioned (version u32, gain f32, limiter flag u8, limiter threshold f32, limiter release f32,
 * node count u32, then the nodes); the module rejects anything it cannot read without disturbing
 * the chain it is already running.
 *
 * Fields the master leaves out take the project schema's defaults, as the engine reads them: unity
 * gain and the 0 dBFS, 50 ms limiter. Only `limiter: null` removes the limiter. An EQ effect
 * becomes one node per band.
 *
 * @throws RangeError for a keyframed master gain, which the record's one static gain cannot play.
 *   Preview such a master through engine-mixed audio (`configureEngineMixedAudio`), which applies
 *   it as the export does, or pass the gain evaluated at the time being heard.
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
