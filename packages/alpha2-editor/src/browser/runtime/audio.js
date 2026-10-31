// Timeline audio. The engine owns the mix (gain, fades, panning, buses, limiter); this module only
// moves samples, so preview and export cannot drift apart.
//
// Preview needs `SharedArrayBuffer`, so the page must be cross-origin isolated (COOP/COEP).
// Without isolation, preview is silent and export still writes engine-mixed audio.

import {
  AudioPlaybackClock,
  AudioPump,
  AudioRingBuffer,
  canShareAudioMemory,
} from "alpha2/audio-timeline";
import {
  Alpha2AudioWorkletHost,
  configureEngineMixedAudio,
  disposeAudioWorklet,
  loadAlpha2AudioWorkletWasm,
  resetAudioWorklet,
  useAudioRing,
} from "alpha2/audio-worklet";
import { MediabunnyAudioDecoder } from "alpha2/mediabunny";
import * as engineBinding from "alpha2/wasm";

const TICKS_PER_SECOND = 120_000n;
const PREFETCH_TICKS = 2n * TICKS_PER_SECOND;
/** How much of a source is kept behind the earliest tick a mix still reads (backward prefetch). */
const KEEP_BEHIND_TICKS = PREFETCH_TICKS;
/** How far that point moves forward before the source is trimmed again. */
const TRIM_STEP_TICKS = PREFETCH_TICKS;

/**
 * Sorts one source's needed tick ranges and joins the ones less than a prefetch apart, which are
 * cheaper to decode as one window than as two.
 */
function coalesceRanges(ranges) {
  const sorted = [...ranges].sort((a, b) =>
    a.start < b.start ? -1 : a.start > b.start ? 1 : 0,
  );
  const merged = [];
  for (const range of sorted) {
    const last = merged.at(-1);
    if (last && range.start - last.end <= PREFETCH_TICKS) {
      if (range.end > last.end) last.end = range.end;
    } else {
      merged.push({ ...range });
    }
  }
  return merged;
}

function minBigInt(a, b) {
  return a < b ? a : b;
}

/**
 * Sources that contribute to the mix, paired with their browser-side asset.
 *
 * A video file with a sound track counts: it declares `has_audio`, so its clip on a video track is
 * mixed without a second clip on an audio lane.
 */
export function audibleSources(project, library) {
  const items = [];
  for (const source of project.sources) {
    if (source.kind !== "audio" && source.has_audio !== true) continue;
    const asset = library.get(source.id);
    if (asset?.hasAudio || asset?.audioBuffer)
      items.push({ id: source.id, asset });
  }
  return items;
}

/**
 * Supplies bounded source windows requested by the engine. Page-synthesized audio keeps its
 * AudioBuffer, but only the requested slice enters wasm.
 */
export class AudioWindowSupplier {
  #library;
  #engine = null;
  #sampleRate = 48_000;
  #generation = 0;
  #controller = new AbortController();
  #decoders = new Map();
  /** Per source, the earliest tick the previous request needed. */
  #lastStarts = new Map();
  /** Per source, the tick the engine's audio was last trimmed below, or would have been. */
  #trimmed = new Map();
  /** The earliest tick each running `supply` needs, per source. */
  #inFlight = new Set();

  constructor(library) {
    this.#library = library;
  }

  configure(engine, project) {
    this.#controller.abort(new DOMException("Project reloaded", "AbortError"));
    this.#controller = new AbortController();
    this.#engine = engine;
    this.#sampleRate = project.settings.sample_rate;
    this.#generation += 1;
    this.#lastStarts.clear();
    this.#trimmed.clear();

    const audible = new Set(
      audibleSources(project, this.#library).map(({ id }) => id),
    );
    for (const [sourceId, entry] of this.#decoders) {
      const asset = this.#library.get(sourceId);
      if (audible.has(sourceId) && asset?.file === entry.file) continue;
      closeDecoder(entry.promise);
      this.#decoders.delete(sourceId);
    }
  }

  async supply(startSample, frames, { signal, forceSources = [] } = {}) {
    const engine = this.#engine;
    if (!engine) throw new Error("audio supplier has no loaded project");
    throwIfAborted(signal);
    const generation = this.#generation;
    const combined = combineSignals(this.#controller.signal, signal);
    let running = null;
    try {
      const needs = JSON.parse(engine.audioNeeds(BigInt(startSample), frames));
      const forced = new Set(forceSources);
      const bySource = new Map();
      for (const need of needs) {
        const ranges = bySource.get(need.sourceId) ?? [];
        ranges.push({
          start: BigInt(need.startTicks),
          end: BigInt(need.endTicks),
        });
        bySource.set(need.sourceId, ranges);
      }
      const earliest = new Map(
        [...bySource].map(([sourceId, ranges]) => [
          sourceId,
          ranges.reduce(
            (least, { start }) => minBigInt(least, start),
            ranges[0].start,
          ),
        ]),
      );
      running = earliest;
      this.#inFlight.add(running);
      for (const [sourceId, start] of earliest) this.#trim(sourceId, start);
      // The engine replaces any window an upload overlaps, so one source's requests run in order
      // and each prefetch stops short of the next request.
      await Promise.all(
        [...bySource].map(async ([sourceId, ranges]) => {
          const requests = coalesceRanges(ranges);
          // A reversed clip or ping-pong backward pass reads backwards, so prefetch goes that way.
          const previous = this.#lastStarts.get(sourceId);
          const backward =
            previous !== undefined &&
            requests[0].start < previous &&
            previous - requests[0].start <= TICKS_PER_SECOND;
          this.#lastStarts.set(sourceId, requests[0].start);
          for (let index = 0; index < requests.length; index += 1) {
            const { start, end } = requests[index];
            const neighbour = requests[backward ? index - 1 : index + 1];
            const gap = neighbour
              ? backward
                ? start - neighbour.end
                : neighbour.start - end
              : null;
            const prefetch =
              gap === null
                ? PREFETCH_TICKS
                : minBigInt(PREFETCH_TICKS, gap / 2n);
            const window = backward
              ? { start: start > prefetch ? start - prefetch : 0n, end }
              : { start, end: end + prefetch };
            await this.#supplyRange(sourceId, start, end, window, {
              forced: forced.has(sourceId),
              generation,
              signal: combined.signal,
            });
          }
        }),
      );
    } finally {
      if (running) this.#inFlight.delete(running);
      combined.dispose();
    }
  }

  /**
   * Drops what the engine holds of `sourceId` well before `start`, stopping short of the earliest
   * tick any running mix needs. Runs once per `TRIM_STEP_TICKS` of progress; a seek back resets it.
   */
  #trim(sourceId, start) {
    let floor = start;
    for (const running of this.#inFlight) {
      const other = running.get(sourceId);
      if (other !== undefined) floor = minBigInt(floor, other);
    }
    const before = floor - KEEP_BEHIND_TICKS;
    if (before <= 0n) return;
    const last = this.#trimmed.get(sourceId);
    if (last !== undefined && before < last) {
      this.#trimmed.set(sourceId, before);
      return;
    }
    if (last !== undefined && before - last < TRIM_STEP_TICKS) return;
    this.#engine.trimAudioBefore(sourceId, before);
    this.#trimmed.set(sourceId, before);
  }

  async #supplyRange(
    sourceId,
    startTicks,
    endTicks,
    decodeWindow,
    { forced, generation, signal },
  ) {
    const engine = this.#engine;
    const requiredStart = Math.max(
      0,
      ticksToSampleFloor(startTicks, this.#sampleRate) - 2,
    );
    const requiredEnd = ticksToSampleCeil(endTicks, this.#sampleRate) + 2;
    // Ask the engine what it holds: it extends, replaces and drops windows on its own terms.
    if (
      !forced &&
      JSON.parse(engine.audioRetainedWindows(sourceId)).some(
        (window) =>
          window.startSample <= requiredStart &&
          window.endSample >= requiredEnd,
      )
    )
      return;

    const asset = this.#library.get(sourceId);
    if (!asset) throw new Error(`audio source ${sourceId} is unavailable`);
    if (asset.audioDecodable === false && !asset.audioBuffer) {
      // Undecodable here (`RuntimeAsset.audioDecodable`): silent, while the document keeps it.
      engine.uploadAudioBlock(
        sourceId,
        BigInt(requiredStart),
        this.#sampleRate,
        1,
        new Float32Array(Math.max(1, requiredEnd - requiredStart)),
      );
      return;
    }
    const block = asset.audioBuffer
      ? blockFromAudioBuffer(
          sourceId,
          asset.audioBuffer,
          decodeWindow.start,
          decodeWindow.end,
          this.#sampleRate,
          generation,
        )
      : await this.#decode(
          sourceId,
          asset,
          decodeWindow.start,
          decodeWindow.end,
          generation,
          signal,
        );
    throwIfAborted(signal);
    if (generation !== this.#generation) return;
    engine.uploadAudioBlock(
      sourceId,
      block.startSample,
      block.sampleRate,
      block.speakers ?? block.channels.length,
      flattenPlanes(block.channels),
    );
  }

  async close() {
    this.#controller.abort();
    this.#generation += 1;
    const closing = [...this.#decoders.values()].map((entry) =>
      entry.promise.then((decoder) => decoder.close()).catch(() => {}),
    );
    this.#decoders.clear();
    this.#engine = null;
    await Promise.allSettled(closing);
  }

  async #decode(sourceId, asset, startTicks, endTicks, generation, signal) {
    if (!asset.file)
      throw new Error(`audio source ${sourceId} has no decodable file`);
    let entry = this.#decoders.get(sourceId);
    if (!entry || entry.file !== asset.file) {
      if (entry) closeDecoder(entry.promise);
      entry = {
        file: asset.file,
        // The engine's own resampler, as on native; alpha2/wasm is loaded by the time this decodes.
        promise: MediabunnyAudioDecoder.open(
          sourceId,
          asset.file,
          engineBinding,
        ),
      };
      this.#decoders.set(sourceId, entry);
    }
    const decoder = await entry.promise;
    return decoder.refill(
      {
        sourceId,
        startMediaTimeTicks: startTicks,
        endMediaTimeTicks: endTicks,
        generation,
      },
      this.#sampleRate,
      signal,
    );
  }
}

/** Mixes one block through the engine, returning planar channels. */
export function mixBlock(engine, startSample, frames, playing = true) {
  const flat = engine.renderAudio(BigInt(startSample), frames, playing);
  const channels = engine.audioChannels();
  const planes = [];
  for (let channel = 0; channel < channels; channel += 1) {
    planes.push(flat.subarray(channel * frames, (channel + 1) * frames));
  }
  return planes;
}

/** Sources the most recent mix could not read, as reported by the engine. */
export function missingAudioSources(engine) {
  try {
    return JSON.parse(engine.audioMissingSources());
  } catch {
    return [];
  }
}

const RING_SECONDS = 0.5;
const PUMP_INTERVAL_MS = 40;

/**
 * Preview transport for engine-mixed audio. A timer keeps the shared ring ahead of the audio
 * thread; the worklet applies no second master chain.
 */
export class EngineAudioMixer {
  /**
   * `onInterrupted` is called when the output stops under playing audio (the context suspended or
   * interrupted by the system, as when another app takes the device); the mixer has stopped by then.
   */
  constructor(runtimeAssets = {}, { enabled = true, onInterrupted } = {}) {
    this.runtimeAssets = runtimeAssets;
    this.#enabled = enabled;
    this.#onInterrupted = onInterrupted ?? null;
  }
  #enabled = true;
  #clock = null;
  #onInterrupted = null;
  /** The user's A/V offset in milliseconds, for a delay the output does not report. */
  #offsetMs = 0;
  get tick() {
    return this.#clock ? Number(this.#clock.timelineTicks) : null;
  }

  /** Where the playing clock's output latency comes from (`AudioLatencySource`), else null. */
  get latencySource() {
    return this.#clock?.latencySource ?? null;
  }

  /** Sets the A/V offset in milliseconds (positive shows the picture later), playing or not. */
  setLatencyOffset(ms) {
    this.#offsetMs = ms;
    if (this.#clock) this.#clock.latencyOffsetSeconds = ms / 1000;
  }
  #context = null;
  #node = null;
  #setup = null;
  #ring = null;
  #pump = null;
  #timer = 0;
  #transport = new AbortController();
  #engine = null;
  #supplier = null;
  #sampleRate = 48000;
  #onUnderrun = null;
  #onError = null;
  #priming = false;
  volume = 1;

  get supported() {
    return this.#enabled && canShareAudioMemory();
  }

  get context() {
    return this.#context;
  }

  /**
   * Associates a loaded project with the preview mixer. Safe to call again.
   *
   * Worklet creation is deferred to `resume()`: a suspended AudioContext may not construct an
   * AudioWorkletProcessor until a user gesture.
   */
  async attach(engine, project, supplier, onUnderrun, onError) {
    this.stop();
    if (!this.supported) return false;
    const sampleRate = project.settings.sample_rate;
    if (this.#context && this.#sampleRate !== sampleRate) await this.close();
    // Continue across a gap of a block or less instead of replaying effect history; seeks reset
    // and export is exact either way.
    engine.setAudioContinuity("continue-small-gaps");
    this.#engine = engine;
    this.#supplier = supplier;
    this.#onUnderrun = onUnderrun ?? null;
    this.#onError = onError ?? null;
    this.#sampleRate = sampleRate;
    if (this.#node) await configureEngineMixedAudio(this.#node);
    return true;
  }

  async #create(context) {
    const wasm = await loadAlpha2AudioWorkletWasm(
      this.runtimeAssets.audioWasmUrl,
    );
    const node = await Alpha2AudioWorkletHost.create(context, wasm, {
      processorUrl: this.runtimeAssets.workletUrl,
      numberOfInputs: 0,
      outputChannelCount: [2],
      projectSampleRate: this.#sampleRate,
    });
    node.port.addEventListener("message", (event) => {
      if (event.data?.type === "alpha2-underrun" && !this.#priming)
        this.#onUnderrun?.();
    });
    node.connect(context.destination);
    this.#ring = AudioRingBuffer.create(
      2,
      Math.ceil(this.#sampleRate * RING_SECONDS),
    );
    // Play only once the node plays the ring, or a quantum runs absent input through the master.
    try {
      await useAudioRing(node, this.#ring.transfer);
      await configureEngineMixedAudio(node);
    } catch (error) {
      disposeAudioWorklet(node);
      this.#ring = null;
      throw error;
    }
    this.#node = node;
  }

  #beginSetup(context) {
    if (this.#setup) return this.#setup;
    const setup = this.#create(context);
    this.#setup = setup;
    // A rejected setup must be retryable on a later Play click, even if its caller was interrupted.
    void setup.catch(() => {
      if (this.#setup === setup) this.#setup = null;
    });
    return setup;
  }

  setMasterVolume(value) {
    this.volume = value;
  }

  async resume() {
    if (!this.supported || !this.#engine) return;
    // Construct and resume before the first await to keep Play's user activation.
    let context = this.#context;
    if (!context) {
      context = new AudioContext({ sampleRate: this.#sampleRate });
      context.addEventListener("statechange", () => {
        // Only while playing: stopping clears the clock, so a stop reports once.
        if (!this.#clock || this.#context !== context) return;
        if (context.state !== "suspended" && context.state !== "interrupted")
          return;
        this.stop();
        this.#onInterrupted?.(context.state);
      });
    }
    this.#context = context;
    const resumed =
      context.state === "suspended" ? context.resume() : Promise.resolve();
    const setup = this.#node ? Promise.resolve() : this.#beginSetup(context);
    await Promise.all([resumed, setup]);
  }

  /** Starts pumping mixed audio from timeline tick `fromTick`, a non-negative integer. */
  async start(fromTick, rate = 1) {
    const startingSignal = this.#transport.signal;
    await this.resume();
    if (startingSignal.aborted) return;
    if (!this.#node || !this.#ring || !this.#engine) return;
    // Playback rate is a preview-only concern; the engine always mixes at 1x.
    if (rate !== 1) {
      this.stop();
      return;
    }
    this.stop();
    const signal = this.#transport.signal;
    const startSample = playbackStartSample(fromTick, this.#sampleRate);
    this.#pump = new AudioPump(this.#source(signal), this.#ring, startSample, {
      blockFrames: 1024,
      targetFrames: Math.floor(this.#ring.capacity * 0.75),
    });
    this.#ring.clear();
    this.#priming = true;
    try {
      // Cleared before the first sample from the new position reaches the worklet.
      await resetAudioWorklet(this.#node);
      if (signal.aborted) return;
      this.#engine.resetAudio();
      await this.#pump.fill();
    } catch (error) {
      // A stop or close while the first block was decoding ends this start quietly.
      if (signal.aborted) return;
      throw error;
    } finally {
      if (this.#transport.signal === signal) this.#priming = false;
    }
    if (signal.aborted) return;
    this.#clock = new AudioPlaybackClock(
      this.#pump,
      this.#ring,
      this.#context,
      {
        processing: Alpha2AudioWorkletHost.latency(this.#node),
        latencyOffsetSeconds: this.#offsetMs / 1000,
      },
    );
    this.#timer = setInterval(() => {
      void this.#pump?.fill().catch((error) => {
        if (error?.name === "AbortError") return;
        this.stop();
        this.#onError?.(error);
      });
    }, PUMP_INTERVAL_MS);
  }

  stop() {
    this.#clock = null;
    this.#transport.abort(
      new DOMException("Audio transport stopped", "AbortError"),
    );
    this.#transport = new AbortController();
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = 0;
    this.#pump = null;
    this.#priming = false;
    this.#ring?.clear();
  }

  async close() {
    this.stop();
    // Let any in-flight setup settle so it cannot resurrect a closed node.
    await this.#setup?.catch(() => {});
    this.#setup = null;
    if (this.#node) disposeAudioWorklet(this.#node);
    this.#node = null;
    this.#ring = null;
    await this.#context?.close();
    this.#context = null;
    this.#supplier = null;
  }

  #source(signal) {
    const engine = this.#engine;
    const supplier = this.#supplier;
    const volume = () => this.volume;
    return {
      sampleRate: this.#sampleRate,
      channels: 2,
      mix: async (startSample, frames) => {
        await supplier.supply(startSample, frames, { signal });
        let planes = mixBlock(engine, startSample, frames, true);
        let missing = missingAudioSources(engine);
        if (missing.length > 0) {
          await supplier.supply(startSample, frames, {
            signal,
            forceSources: missing,
          });
          planes = mixBlock(engine, startSample, frames, true);
          missing = missingAudioSources(engine);
        }
        if (missing.length > 0)
          return { kind: "waiting", missingSources: missing };
        // Preview volume is a monitor control, not part of the project.
        const level = volume();
        if (level !== 1) {
          for (const plane of planes) {
            for (let index = 0; index < plane.length; index += 1)
              plane[index] *= level;
          }
        }
        return { kind: "mixed", planes };
      },
    };
  }
}

function blockFromAudioBuffer(
  sourceId,
  buffer,
  startTicks,
  endTicks,
  sampleRate,
  generation,
) {
  if (buffer.sampleRate !== sampleRate)
    throw new Error(
      `generated audio ${sourceId} uses ${buffer.sampleRate} Hz, expected ${sampleRate} Hz`,
    );
  const first = Math.max(0, ticksToSampleFloor(startTicks, sampleRate) - 2);
  const last = Math.max(first + 1, ticksToSampleCeil(endTicks, sampleRate) + 2);
  const channels = Array.from(
    { length: Math.min(2, buffer.numberOfChannels) },
    (_, channel) => {
      const output = new Float32Array(last - first);
      const sourceStart = Math.min(buffer.length, first);
      const sourceEnd = Math.min(buffer.length, last);
      if (sourceEnd > sourceStart) {
        output.set(
          buffer.getChannelData(channel).subarray(sourceStart, sourceEnd),
          sourceStart - first,
        );
      }
      return output;
    },
  );
  return {
    sourceId,
    startSample: BigInt(first),
    generation,
    sampleRate,
    channels,
  };
}

function flattenPlanes(channels) {
  const frames = channels[0]?.length ?? 0;
  const flat = new Float32Array(channels.length * frames);
  for (let channel = 0; channel < channels.length; channel += 1)
    flat.set(channels[channel], channel * frames);
  return flat;
}

/**
 * The sample playback from `tick` starts at: the first one at or after it. The audio clock reports
 * the tick at or before the sample heard, so rounding to nearest could be heard a frame early.
 */
export function playbackStartSample(tick, sampleRate) {
  return ticksToSampleCeil(BigInt(Math.max(0, tick)), sampleRate);
}

function ticksToSampleFloor(ticks, sampleRate) {
  return Number((ticks * BigInt(sampleRate)) / TICKS_PER_SECOND);
}

function ticksToSampleCeil(ticks, sampleRate) {
  const rate = BigInt(sampleRate);
  return Number((ticks * rate + TICKS_PER_SECOND - 1n) / TICKS_PER_SECOND);
}

function closeDecoder(promise) {
  void promise.then((decoder) => decoder.close()).catch(() => {});
}

function combineSignals(primary, secondary) {
  const controller = new AbortController();
  const abort = (signal) =>
    controller.abort(
      signal.reason ?? new DOMException("Audio decode aborted", "AbortError"),
    );
  const onPrimary = () => abort(primary);
  const onSecondary = () => abort(secondary);
  if (primary.aborted) abort(primary);
  else primary.addEventListener("abort", onPrimary, { once: true });
  if (secondary?.aborted) abort(secondary);
  else secondary?.addEventListener("abort", onSecondary, { once: true });
  return {
    signal: controller.signal,
    dispose() {
      primary.removeEventListener("abort", onPrimary);
      secondary?.removeEventListener("abort", onSecondary);
    },
  };
}

function throwIfAborted(signal) {
  if (signal?.aborted)
    throw (
      signal.reason ?? new DOMException("Audio decode aborted", "AbortError")
    );
}
