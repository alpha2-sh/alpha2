// Extractors in the page or a worker: the descriptor and step types every surface shares, and
// `runExtractor`, which drives the engine's session in wasm over media decoded by the browser.

import {
  ALL_FORMATS,
  BlobSource,
  BufferSource,
  EncodedPacketSink,
  Input,
  type VideoSample,
  VideoSampleSink,
} from "mediabunny";

import type { ChromaSiting } from "./chroma-siting.js";
import type { DataSet, Project } from "./generated/project-schema.js";
import { copyPlanarYuvFrame } from "./hdr-frames.js";
import { microsecondsToTicks, readVideoOrientation } from "./media.js";
import {
  type AudioResamplerBinding,
  MediabunnyAudioDecoder,
  type MediabunnyInput,
  streamChromaSiting,
} from "./mediabunny.js";

/** What an extractor reads and emits; the same object `describe()` returns on every surface. */
export interface ExtractorDescriptor {
  /** Reverse-DNS id; `alpha2.` is reserved for first-party plugins. */
  readonly id: string;
  /** The output contract's version. */
  readonly version: number;
  readonly input:
    | {
        readonly media: "video";
        readonly from: readonly ("source" | "timeline")[];
        readonly format: "rgba8" | "luma8";
        readonly max_size?: number;
      }
    | {
        readonly media: "audio";
        readonly from: readonly ("source" | "timeline")[];
        readonly format: "f32";
        readonly sample_rate?: number;
        readonly channels?: "mono" | "source";
        readonly block_frames?: number;
      };
  /** JSON Schema of the request's `params`; checked before the request is sent. */
  readonly params_schema?: unknown;
  readonly outputs: readonly {
    readonly name: string;
    readonly kind: "points" | "scalars" | "similarity" | "words" | "intervals";
    readonly channels?: readonly string[];
    readonly interpolation?: "hold" | "linear";
    readonly suggest?: "bind" | "remove_ranges" | "split_at";
  }[];
}

/** One run: the descriptor's id and version, its params, the input read and the data set ids. */
export interface ExtractRun {
  readonly extractor: string;
  readonly version: number;
  readonly params?: Readonly<Record<string, unknown>>;
  readonly input: {
    readonly from: "source" | "timeline";
    readonly source_id?: string;
    /** Ticks on the input's clock. */
    readonly range: { readonly start: number; readonly end: number };
    readonly passes?: readonly {
      readonly start: number;
      readonly direction: "forward" | "backward";
    }[];
    readonly size?: number;
    readonly frame_rate?: { readonly num: number; readonly den: number };
    readonly stream?: number;
    readonly sample_rate?: number;
    readonly channels?: "mono" | "source";
  };
  /** Output name to the id of the data set it becomes. */
  readonly outputs: Readonly<Record<string, string>>;
}

/** What `begin` is told: the range, passes, and the delivered video size or audio layout. */
export interface ExtractContext {
  readonly range: { readonly start: number; readonly end: number };
  readonly passes: readonly {
    readonly start: number;
    readonly direction: "forward" | "backward";
  }[];
  readonly from: "source" | "timeline";
  readonly video: { readonly size: number; readonly format: string } | null;
  readonly audio: {
    readonly rate: number;
    readonly channels: "mono" | "source";
    readonly blockFrames: number;
  } | null;
}

/** A delivered frame: tightly packed `rgba8` (straight alpha, sRGB) or `luma8`. */
export interface ExtractFrame {
  /** Presentation time in ticks on the input's clock. */
  readonly time: number;
  readonly width: number;
  readonly height: number;
  readonly format: "rgba8" | "luma8";
  /** Owned by the call; copy what must outlive it. */
  readonly data: Uint8Array;
}

/** A delivered audio block: planar f32, channel after channel. */
export interface ExtractAudio {
  /** The first sample's time, in ticks (the tick at or before it). */
  readonly start: number;
  /** The first sample, at `rate` from the clock's zero. */
  readonly startSample: number;
  readonly frames: number;
  readonly rate: number;
  readonly channels: number;
  readonly samples: Float32Array;
}

/** Where an extractor's values go; the host validates them as the session does. */
export interface ExtractEmitter {
  /** A sample of a `points`, `scalars` or `similarity` output; `null` marks a lost channel. */
  sample(
    output: string,
    time: number,
    values: readonly (number | null)[],
  ): void;
  word(
    output: string,
    start: number,
    end: number,
    text: string,
    speaker?: string,
  ): void;
  interval(
    output: string,
    start: number,
    end: number,
    label?: string,
    score?: number,
  ): void;
}

/** A client-side extractor. Every callback may return a promise; the host waits for it. */
export interface Extractor {
  describe(): ExtractorDescriptor;
  begin?(context: ExtractContext): void | Promise<void>;
  pass?(pass: {
    readonly pass: number;
    readonly direction: "forward" | "backward";
  }): void | Promise<void>;
  video?(frame: ExtractFrame, out: ExtractEmitter): void | Promise<void>;
  audio?(block: ExtractAudio, out: ExtractEmitter): void | Promise<void>;
  /** Whole-range work (speech recognition) belongs here and may emit anywhere in the range. */
  end?(out: ExtractEmitter): void | Promise<void>;
}

/** Progress of one pass; `total` is `null` for source frames, whose count is not known ahead. */
export interface ExtractProgress {
  readonly phase: "extract";
  readonly pass: number;
  readonly completed: number;
  readonly total: number | null;
}

/** What an extraction returns: the data sets, ready to put in the project. */
export interface ExtractResult {
  readonly dataSets: readonly DataSet[];
  readonly timings: Readonly<Record<string, number>>;
}

/** An emitter that keeps the emits, in the host protocol's JSON form. */
export function collectEmits(): {
  emits: Record<string, unknown>[];
  out: ExtractEmitter;
} {
  const emits: Record<string, unknown>[] = [];
  return {
    emits,
    out: {
      sample(output, time, values) {
        emits.push({ output, time, values: [...values] });
      },
      word(output, start, end, text, speaker) {
        emits.push({
          output,
          start,
          end,
          text,
          ...(speaker === undefined ? {} : { speaker }),
        });
      },
      interval(output, start, end, label, score) {
        emits.push({
          output,
          start,
          end,
          ...(label === undefined ? {} : { label }),
          ...(score === undefined ? {} : { score }),
        });
      },
    },
  };
}

/**
 * Where `value` breaks `schema`, as a path and reason, or `undefined`. Covers the JSON Schema
 * keywords descriptors use: `type`, `enum`, `const`, `properties`, `required`,
 * `additionalProperties`, `items`, `minItems`, `maxItems`, `minimum`, `maximum`, `minLength`,
 * `maxLength`.
 */
export function schemaProblem(
  schema: unknown,
  value: unknown,
  at = "params",
): string | undefined {
  if (typeof schema !== "object" || schema === null) return undefined;
  const rules = schema as Record<string, unknown>;
  const kind = (item: unknown): string =>
    item === null
      ? "null"
      : Array.isArray(item)
        ? "array"
        : Number.isInteger(item)
          ? "integer"
          : typeof item;
  if (rules.type !== undefined) {
    const types = Array.isArray(rules.type) ? rules.type : [rules.type];
    const actual = kind(value);
    const fits = types.some(
      (type) => type === actual || (type === "number" && actual === "integer"),
    );
    if (!fits) return `${at} must be ${types.join(" or ")}`;
  }
  if (
    Array.isArray(rules.enum) &&
    !rules.enum.some((item) => JSON.stringify(item) === JSON.stringify(value))
  )
    return `${at} must be one of ${JSON.stringify(rules.enum)}`;
  if ("const" in rules && JSON.stringify(rules.const) !== JSON.stringify(value))
    return `${at} must be ${JSON.stringify(rules.const)}`;
  if (typeof value === "number") {
    if (typeof rules.minimum === "number" && value < rules.minimum)
      return `${at} must be at least ${rules.minimum}`;
    if (typeof rules.maximum === "number" && value > rules.maximum)
      return `${at} must be at most ${rules.maximum}`;
  }
  if (typeof value === "string") {
    if (typeof rules.minLength === "number" && value.length < rules.minLength)
      return `${at} is too short`;
    if (typeof rules.maxLength === "number" && value.length > rules.maxLength)
      return `${at} is too long`;
  }
  if (Array.isArray(value)) {
    if (typeof rules.minItems === "number" && value.length < rules.minItems)
      return `${at} has too few items`;
    if (typeof rules.maxItems === "number" && value.length > rules.maxItems)
      return `${at} has too many items`;
    for (const [index, item] of value.entries()) {
      const problem = schemaProblem(rules.items, item, `${at}[${index}]`);
      if (problem !== undefined) return problem;
    }
  }
  if (typeof value === "object" && value !== null && !Array.isArray(value)) {
    const record = value as Record<string, unknown>;
    const properties = (rules.properties ?? {}) as Record<string, unknown>;
    for (const name of Array.isArray(rules.required) ? rules.required : []) {
      if (!(String(name) in record)) return `${at}.${String(name)} is required`;
    }
    for (const [name, item] of Object.entries(record)) {
      if (name in properties) {
        const problem = schemaProblem(properties[name], item, `${at}.${name}`);
        if (problem !== undefined) return problem;
      } else if (rules.additionalProperties === false) {
        return `${at}.${name} is not allowed`;
      } else if (typeof rules.additionalProperties === "object") {
        const problem = schemaProblem(
          rules.additionalProperties,
          item,
          `${at}.${name}`,
        );
        if (problem !== undefined) return problem;
      }
    }
  }
  return undefined;
}

/** The wasm `ExtractSession`: the session, driven push style. */
export interface WasmExtractSession {
  context(): string;
  next(): string;
  videoBytes(): Uint8Array;
  audioSamples(): Float32Array;
  supplyYuvFrame(
    ptsTicks: bigint,
    format: string,
    codedWidth: number,
    codedHeight: number,
    data: Uint8Array,
    layout: Uint32Array,
    visibleRect: Uint32Array,
    displayWidth: number,
    displayHeight: number,
    colorJson: string,
    chromaSiting: string,
    rotationDegrees: number,
    mirror: boolean,
  ): void;
  supplyRgbaFrame(
    ptsTicks: bigint,
    width: number,
    height: number,
    rgba: Uint8Array,
  ): void;
  supplyEnd(): void;
  supplyAudio(channels: number, samples: Float32Array, speakers?: string): void;
  emit(emits: string): void;
  progress(): string;
  finish(madeFrom?: string): string;
  free(): void;
}

/** The part of the `alpha2/wasm` module {@link runExtractor} uses. */
export interface ExtractBinding {
  readonly ExtractSession: new (
    project: string,
    descriptor: string,
    request: string,
  ) => WasmExtractSession;
}

/** A rendered timeline frame: straight-alpha sRGB RGBA8 at the size asked for. */
export interface TimelineFrame {
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

/** What {@link runExtractor} runs over. */
export interface RunExtractorRequest {
  readonly binding: ExtractBinding;
  readonly project: Project;
  readonly request: ExtractRun;
  /** The media file of each source the request reads. */
  readonly media?: Readonly<Record<string, MediabunnyInput>>;
  /** Resamples source audio to the rate the extractor reads. */
  readonly audioBinding?: AudioResamplerBinding;
  /**
   * Draws the timeline at `time` (ticks), `width` x `height`, for a timeline input; the CPU
   * renderer's `readRgbaAt` does, once the frames it shows are uploaded.
   */
  readonly timelineFrame?: (
    time: bigint,
    width: number,
    height: number,
  ) => Promise<TimelineFrame> | TimelineFrame;
  /** The original source's fingerprint, recorded as `made_from` on source-anchored data. */
  readonly madeFrom?: unknown;
  /** The plugin `extractor` came from (`loadPlugin`), recorded as `made_by.plugin`. */
  readonly plugin?: {
    readonly version: string;
    readonly artifactSha256: string;
  };
  readonly signal?: AbortSignal;
  readonly onProgress?: (progress: ExtractProgress) => void;
}

const TICKS_PER_SECOND = 120_000;

/** An error with the code the native host answers with. */
function coded(code: string, message: string, cause?: unknown): Error {
  return Object.assign(new Error(message, { cause }), { code });
}

/**
 * One source's frames, decoded in software so they carry planes: the frame shown at a time, the
 * one after it in either direction.
 */
class SourceFrames {
  readonly #sink: VideoSampleSink;
  readonly #input: Input;
  readonly #orientation: { rotationDegrees: number; mirror: boolean };
  readonly #chromaSiting: ChromaSiting;
  #forward: AsyncGenerator<VideoSample, void, unknown> | undefined;

  private constructor(
    input: Input,
    sink: VideoSampleSink,
    orientation: { rotationDegrees: number; mirror: boolean },
    chromaSiting: ChromaSiting,
  ) {
    this.#input = input;
    this.#sink = sink;
    this.#orientation = orientation;
    this.#chromaSiting = chromaSiting;
  }

  static async open(resource: MediabunnyInput): Promise<SourceFrames> {
    const source =
      resource instanceof Blob
        ? new BlobSource(resource)
        : new BufferSource(resource);
    const input = new Input({ source, formats: ALL_FORMATS });
    const track = await input.getPrimaryVideoTrack();
    if (track === null) {
      input.dispose();
      throw coded("INVALID_INPUT", "the source has no video track");
    }
    if (!(await track.canDecode())) {
      input.dispose();
      throw coded(
        "DECODE_UNAVAILABLE",
        `this browser cannot decode the source's ${track.codec ?? "video"} track`,
      );
    }
    const orientation = (await readVideoOrientation(resource, track.id)) ?? {
      rotationDegrees: track.rotation,
      mirror: false,
    };
    const chromaSiting = await streamChromaSiting(
      track,
      new EncodedPacketSink(track),
    );
    // Software frames carry planes, which present alike on every surface.
    const sink = new VideoSampleSink(track, {
      hardwareAcceleration: "prefer-software",
    });
    return new SourceFrames(input, sink, orientation, chromaSiting);
  }

  /** The frame shown at `ticks`, or the first frame when none is; a forward pass continues. */
  async start(ticks: bigint, forward: boolean): Promise<VideoSample | null> {
    await this.#forward?.return();
    const seconds = Number(ticks) / TICKS_PER_SECOND;
    this.#forward = this.#sink.samples(seconds);
    const first = await this.#forward.next();
    if (!forward) {
      await this.#forward.return();
      this.#forward = undefined;
    }
    return first.done === true ? null : first.value;
  }

  /** The frame after the one at `pts` in the pass's direction, or `null` past the end. */
  async after(pts: bigint, forward: boolean): Promise<VideoSample | null> {
    if (forward) {
      const next = await this.#forward?.next();
      return next === undefined || next.done === true ? null : next.value;
    }
    if (pts <= 0n) return null;
    const sample = await this.#sink.getSample(
      Number(pts - 1n) / TICKS_PER_SECOND,
    );
    if (sample === null) return null;
    if (ticksOf(sample) >= pts) {
      sample.close();
      return null;
    }
    return sample;
  }

  async supply(
    session: WasmExtractSession,
    sample: VideoSample,
  ): Promise<void> {
    const frame = sample.toVideoFrame();
    sample.close();
    const planes = await copyPlanarYuvFrame(frame, null, {
      rotation: this.#orientation.rotationDegrees as 0 | 90 | 180 | 270,
      mirror: this.#orientation.mirror,
      chromaSiting: this.#chromaSiting,
    });
    session.supplyYuvFrame(
      microsecondsToTicks(planes.physicalTimeMicroseconds),
      planes.format,
      planes.codedWidth,
      planes.codedHeight,
      planes.data,
      Uint32Array.from(planes.layout),
      Uint32Array.from(planes.visibleRect),
      planes.displayWidth,
      planes.displayHeight,
      JSON.stringify(planes.color),
      planes.chromaSiting,
      planes.rotation ?? 0,
      planes.mirror ?? false,
    );
  }

  async close(): Promise<void> {
    await this.#forward?.return();
    this.#input.dispose();
  }
}

function ticksOf(sample: VideoSample): bigint {
  return microsecondsToTicks(sample.microsecondTimestamp);
}

/** Calls an extractor step, turning its failure into `EXTRACTOR_FAILED`. */
async function step<T>(call: () => T | Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (cause) {
    // A plugin's failure already carries the code every surface reports it with.
    if (
      cause instanceof Error &&
      typeof (cause as { code?: unknown }).code === "string"
    )
      throw cause;
    const message = cause instanceof Error ? cause.message : String(cause);
    throw coded("EXTRACTOR_FAILED", `the extractor failed: ${message}`, cause);
  }
}

interface Need {
  readonly need:
    "source_frame" | "timeline_frame" | "source_audio" | "timeline_audio";
  readonly source?: string;
  readonly from?: "start" | "after";
  readonly time?: number;
  readonly direction?: "forward" | "backward";
  readonly size?: number;
  readonly width?: number;
  readonly height?: number;
  readonly stream?: number;
  readonly startSample?: number;
  readonly frames?: number;
  readonly rate?: number;
  readonly channels?: "mono" | "source";
}

/**
 * Runs `extractor` over `request.request` in the page or a worker: source frames are decoded in
 * software and presented by the engine exactly as the native hosts present them, so the data
 * matches theirs byte for byte; timeline frames come from `request.timelineFrame`. Resolves with
 * the data sets the session validated.
 *
 * Rejects with `INVALID_INPUT` for a request the session refuses or params the descriptor's
 * `params_schema` does not take, `INVALID_EMIT` (with `details`) for output the session refuses,
 * `EXTRACTOR_FAILED` when a step throws, and `ABORTED` on `request.signal`.
 */
export async function runExtractor(
  request: RunExtractorRequest,
  extractor: Extractor,
): Promise<ExtractResult> {
  const started = performance.now();
  const descriptor = extractor.describe();
  const problem = schemaProblem(
    descriptor.params_schema,
    request.request.params ?? {},
  );
  if (problem !== undefined)
    throw coded("INVALID_INPUT", `invalid params: ${problem}`);
  const session = new request.binding.ExtractSession(
    JSON.stringify(request.project),
    JSON.stringify(descriptor),
    JSON.stringify(request.request),
  );
  const sources = new Map<string, SourceFrames>();
  const audio = new Map<string, MediabunnyAudioDecoder>();
  const resource = (id: string): MediabunnyInput => {
    const found = request.media?.[id];
    if (found === undefined)
      throw coded("MISSING_INPUT", `no media for source ${id}`);
    return found;
  };
  try {
    const context = JSON.parse(session.context()) as ExtractContext;
    await step(() => extractor.begin?.(context));
    let pass = 0;
    for (;;) {
      if (request.signal?.aborted === true)
        throw coded("ABORTED", "the extraction was cancelled");
      const next = JSON.parse(session.next()) as {
        step: "need" | "pass" | "video" | "audio" | "done";
        need?: Need;
        pass?: number;
        direction?: "forward" | "backward";
        time?: number;
        width?: number;
        height?: number;
        format?: "rgba8" | "luma8";
        start?: number;
        startSample?: number;
        frames?: number;
        rate?: number;
        channels?: number;
      };
      if (next.step === "done") break;
      if (next.step === "need" && next.need !== undefined) {
        const need = next.need;
        if (need.need === "source_frame") {
          const id = need.source ?? "";
          let frames = sources.get(id);
          if (frames === undefined) {
            frames = await SourceFrames.open(resource(id));
            sources.set(id, frames);
          }
          const forward = need.direction !== "backward";
          const time = BigInt(need.time ?? 0);
          const sample =
            need.from === "start"
              ? await frames.start(time, forward)
              : await frames.after(time, forward);
          if (sample === null) session.supplyEnd();
          else await frames.supply(session, sample);
        } else if (need.need === "timeline_frame") {
          if (request.timelineFrame === undefined)
            throw coded(
              "INVALID_INPUT",
              "a timeline input needs request.timelineFrame",
            );
          const time = BigInt(need.time ?? 0);
          const frame = await request.timelineFrame(
            time,
            need.width ?? 0,
            need.height ?? 0,
          );
          session.supplyRgbaFrame(time, frame.width, frame.height, frame.rgba);
        } else if (need.need === "source_audio") {
          const id = need.source ?? "";
          const binding = request.audioBinding;
          if (binding === undefined)
            throw coded(
              "INVALID_INPUT",
              "an audio input needs request.audioBinding",
            );
          let decoder = audio.get(id);
          if (decoder === undefined) {
            decoder = await MediabunnyAudioDecoder.open(
              id,
              resource(id),
              binding,
              { stream: need.stream ?? 0 },
            );
            audio.set(id, decoder);
          }
          const rate = need.rate ?? 48_000;
          const first = BigInt(need.startSample ?? 0);
          const frames = need.frames ?? 0;
          const toTicks = (sample: bigint) =>
            (sample * BigInt(TICKS_PER_SECOND)) / BigInt(rate);
          const block = await decoder.refill(
            {
              sourceId: id,
              startMediaTimeTicks: toTicks(first),
              endMediaTimeTicks: toTicks(first + BigInt(frames)) + 1n,
              generation: 0,
            },
            rate,
            request.signal,
          );
          const offset = Number(first - block.startSample);
          const samples = new Float32Array(frames * block.channels.length);
          block.channels.forEach((plane, channel) => {
            for (let index = 0; index < frames; index += 1) {
              const at = offset + index;
              samples[channel * frames + index] =
                at >= 0 && at < plane.length ? (plane[at] ?? 0) : 0;
            }
          });
          session.supplyAudio(
            block.channels.length,
            samples,
            block.speakers === undefined
              ? undefined
              : JSON.stringify(block.speakers),
          );
        } else {
          throw coded(
            "INVALID_INPUT",
            "timeline audio is not supplied in the browser yet; extract from the source",
          );
        }
        continue;
      }
      const { emits, out } = collectEmits();
      if (next.step === "pass") {
        pass = next.pass ?? 0;
        await step(() =>
          extractor.pass?.({
            pass,
            direction: next.direction === "backward" ? "backward" : "forward",
          }),
        );
      } else if (next.step === "video") {
        const data = session.videoBytes();
        await step(() =>
          extractor.video?.(
            {
              time: next.time ?? 0,
              width: next.width ?? 0,
              height: next.height ?? 0,
              format: next.format === "luma8" ? "luma8" : "rgba8",
              data,
            },
            out,
          ),
        );
      } else {
        const samples = session.audioSamples();
        await step(() =>
          extractor.audio?.(
            {
              start: next.start ?? 0,
              startSample: next.startSample ?? 0,
              frames: next.frames ?? 0,
              rate: next.rate ?? 0,
              channels: next.channels ?? 0,
              samples,
            },
            out,
          ),
        );
      }
      session.emit(JSON.stringify(emits));
      const progress = JSON.parse(session.progress()) as {
        completed: number;
        total: number | null;
      }[];
      const current = progress[pass];
      if (current !== undefined && next.step !== "pass")
        request.onProgress?.({ phase: "extract", pass, ...current });
    }
    const { emits, out } = collectEmits();
    await step(() => extractor.end?.(out));
    session.emit(JSON.stringify(emits));
    const dataSets = JSON.parse(
      session.finish(
        request.madeFrom === undefined
          ? undefined
          : JSON.stringify(request.madeFrom),
      ),
    ) as DataSet[];
    const plugin = request.plugin;
    const recorded =
      plugin === undefined
        ? dataSets
        : dataSets.map((set) => ({
            ...set,
            made_by: {
              ...set.made_by,
              plugin: {
                version: plugin.version,
                artifact_sha256: plugin.artifactSha256,
              },
            },
          }));
    return {
      dataSets: recorded as DataSet[],
      timings: { totalMs: performance.now() - started },
    };
  } finally {
    await Promise.all([...sources.values()].map((frames) => frames.close()));
    for (const decoder of audio.values()) decoder.close();
    session.free();
  }
}
