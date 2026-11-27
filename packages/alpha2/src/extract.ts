// `runExtractor` drives the engine's session in wasm over media the page or a worker decodes.
// Re-exports the shared contract from `alpha2/extractor`.

import {
  ALL_FORMATS,
  EncodedPacketSink,
  Input,
  type InputVideoTrack,
  type VideoSample,
  VideoSampleSink,
} from "mediabunny";

import type { ChromaSiting } from "./chroma-siting.js";
import { decodeFailureOf, withDecoderFallback } from "./decoder-fallback.js";
import {
  collectEmits,
  type ExtractContext,
  type ExtractProgress,
  type ExtractResult,
  type ExtractRun,
  type ExtractSourceDecode,
  type Extractor,
  schemaProblem,
} from "./extractor.js";
export type {
  ExtractAudio,
  ExtractContext,
  ExtractEmitter,
  ExtractFrame,
  ExtractProgress,
  ExtractResult,
  ExtractRun,
  ExtractSourceDecode,
  Extractor,
  ExtractorDescriptor,
} from "./extractor.js";
export { collectEmits, schemaProblem } from "./extractor.js";
import type { DataSet, Project } from "./generated/project-schema.js";
import { isUnreadableFrame, withReadbackRetries } from "./frame-intake.js";
import {
  codecName,
  DecodeUnavailableError,
  decodeRefusalMessage,
  NonPlanarVideoFrameError,
  PlanarDecodeUnavailableError,
  type PlanarYuvFrame,
  readPlanarYuvFrame,
  type TrackDecodeFacts,
} from "./hdr-frames.js";
import { microsecondsToTicks, readVideoOrientation } from "./media.js";
import { mediaSource } from "./media-source.js";
import {
  type AudioResamplerBinding,
  MediabunnyAudioDecoder,
  type MediabunnyInput,
  streamChromaSiting,
  videoTrackFacts,
} from "./mediabunny.js";

/** The wasm `ExtractSession`, driven push style. */
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
  readonly signal?: AbortSignal;
  readonly onProgress?: (progress: ExtractProgress) => void;
}

const TICKS_PER_SECOND = 120_000;

/** An error with the code the native host answers with. */
function coded(code: string, message: string, cause?: unknown): Error {
  return Object.assign(new Error(message, { cause }), { code });
}

/**
 * One source's frames as planes: the frame shown at a time, the one after it in either direction.
 * Decoded in software where this browser's software decoder reads the source, and by the
 * browser's own choice of decoder otherwise.
 */
class SourceFrames {
  readonly #id: string;
  readonly #input: Input;
  readonly #track: InputVideoTrack;
  readonly #orientation: { rotationDegrees: number; mirror: boolean };
  readonly #chromaSiting: ChromaSiting;
  #sink: VideoSampleSink;
  #acceleration: ExtractSourceDecode["hardwareAcceleration"];
  #softwareRefusal: string | undefined;
  #decodedAny = false;
  #frameFormat: string | undefined;
  #forward: AsyncGenerator<VideoSample, void, unknown> | undefined;

  private constructor(
    id: string,
    input: Input,
    track: InputVideoTrack,
    softwareRefusal: string | undefined,
    orientation: { rotationDegrees: number; mirror: boolean },
    chromaSiting: ChromaSiting,
  ) {
    this.#id = id;
    this.#input = input;
    this.#track = track;
    this.#softwareRefusal = softwareRefusal;
    this.#acceleration =
      softwareRefusal === undefined ? "prefer-software" : "no-preference";
    this.#sink = new VideoSampleSink(track, {
      hardwareAcceleration: this.#acceleration,
    });
    this.#orientation = orientation;
    this.#chromaSiting = chromaSiting;
  }

  static async open(
    resource: MediabunnyInput,
    id: string,
  ): Promise<SourceFrames> {
    const input = new Input({
      source: mediaSource(resource),
      formats: ALL_FORMATS,
    });
    try {
      const track = await input.getPrimaryVideoTrack();
      if (track === null)
        throw coded("INVALID_INPUT", "the source has no video track");
      if (!(await track.canDecode())) {
        const facts = await videoTrackFacts(track);
        throw new DecodeUnavailableError(
          decodeRefusalMessage(`source ${id}`, facts),
          facts,
        );
      }
      const config = await track.getDecoderConfig();
      const softwareListed =
        typeof VideoDecoder === "undefined" ||
        (config !== null &&
          (await VideoDecoder.isConfigSupported({
            ...config,
            hardwareAcceleration: "prefer-software",
          }).then(
            (support) => support.supported === true,
            () => false,
          )));
      const orientation = (await readVideoOrientation(resource, track.id)) ?? {
        rotationDegrees: track.rotation,
        mirror: false,
      };
      const chromaSiting = await streamChromaSiting(
        track,
        new EncodedPacketSink(track),
      );
      return new SourceFrames(
        id,
        input,
        track,
        softwareListed
          ? undefined
          : `no software decoder for ${codecName(config?.codec ?? track.codec ?? null)}`,
        orientation,
        chromaSiting,
      );
    } catch (error) {
      input.dispose();
      throw error;
    }
  }

  get decode(): ExtractSourceDecode {
    return {
      hardwareAcceleration: this.#acceleration,
      ...(this.#softwareRefusal === undefined
        ? {}
        : { softwareRefusal: this.#softwareRefusal }),
      ...(this.#frameFormat === undefined
        ? {}
        : { frameFormat: this.#frameFormat }),
    };
  }

  async #read<T>(read: () => Promise<T>): Promise<T> {
    if (this.#decodedAny) return read();
    try {
      return await withDecoderFallback(
        read,
        this.#acceleration === "no-preference"
          ? undefined
          : (cause) => {
              this.#softwareRefusal = `${cause.name}: ${cause.message}`;
              this.#acceleration = "no-preference";
              this.#sink = new VideoSampleSink(this.#track, {
                hardwareAcceleration: "no-preference",
              });
              return Promise.resolve();
            },
      );
    } catch (error) {
      throw decodeFailureOf(
        error,
        `source ${this.#id}`,
        await videoTrackFacts(this.#track),
        false,
      );
    }
  }

  #decoded(sample: VideoSample | null): VideoSample | null {
    if (sample !== null) this.#decodedAny = true;
    return sample;
  }

  /** The frame shown at `ticks`, or the first frame when none is; a forward pass continues. */
  async start(ticks: bigint, forward: boolean): Promise<VideoSample | null> {
    const first = await this.#read(async () => {
      await this.#forward?.return();
      this.#forward = this.#sink.samples(Number(ticks) / TICKS_PER_SECOND);
      return this.#forward.next();
    });
    if (!forward) {
      await this.#forward?.return();
      this.#forward = undefined;
    }
    return this.#decoded(first.done === true ? null : first.value);
  }

  /** The frame after the one at `pts` in the pass's direction, or `null` past the end. */
  async after(pts: bigint, forward: boolean): Promise<VideoSample | null> {
    if (forward) {
      const next = await this.#forward?.next();
      return next === undefined || next.done === true ? null : next.value;
    }
    if (pts <= 0n) return null;
    const sample = this.#decoded(
      await this.#read(() =>
        this.#sink.getSample(Number(pts - 1n) / TICKS_PER_SECOND),
      ),
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
    let planes: PlanarYuvFrame;
    try {
      planes = await withReadbackRetries(() =>
        readPlanarYuvFrame(frame, null, {
          rotation: this.#orientation.rotationDegrees as 0 | 90 | 180 | 270,
          mirror: this.#orientation.mirror,
          chromaSiting: this.#chromaSiting,
        }),
      );
    } catch (error) {
      if (error instanceof NonPlanarVideoFrameError) {
        const facts: TrackDecodeFacts = {
          ...(await videoTrackFacts(this.#track)),
          frameFormat: error.format,
          softwareDecoder:
            this.#acceleration === "prefer-software" ? "opaque" : "none",
        };
        throw new PlanarDecodeUnavailableError(
          decodeRefusalMessage(`source ${this.#id}`, facts),
          facts,
          { cause: error },
        );
      }
      if (isUnreadableFrame(error))
        throw coded(
          "FRAME_UNREADABLE",
          `the browser could not read back the frame of source ${this.#id} at ${microsecondsToTicks(frame.timestamp)}`,
          error,
        );
      throw error;
    } finally {
      frame.close();
    }
    this.#frameFormat ??= planes.format;
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
 * software where the browser's software decoder reads the source, by the browser's own choice of
 * decoder otherwise, and presented by the engine exactly as the native hosts present them, so the
 * data matches theirs byte for byte; timeline frames come from `request.timelineFrame`. Resolves
 * with the data sets the session validated and, in `sourceDecode`, how each source was decoded.
 *
 * Rejects with `INVALID_INPUT` for a request the session refuses or params the descriptor's
 * `params_schema` does not take, `INVALID_EMIT` (with `details`) for output the session refuses,
 * `EXTRACTOR_FAILED` when a step throws, `DECODE_UNAVAILABLE` when no decoder gives a source's
 * first frame (`facts` and `cause` say why), `PLANAR_DECODE_UNAVAILABLE` for frames without
 * planes, `FRAME_UNREADABLE` for planes that cannot be read back, and `ABORTED` on
 * `request.signal`. A decoder failure after the first frame rejects with the browser's error.
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
  const report = (): void => {
    if (request.onProgress === undefined) return;
    request.onProgress({
      phase: "extract",
      ...(JSON.parse(session.progress()) as Omit<ExtractProgress, "phase">),
    });
  };
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
    report();
    await step(() => extractor.begin?.(context));
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
            frames = await SourceFrames.open(resource(id), id);
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
        report();
        const pass = next.pass ?? 0;
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
      if (next.step !== "pass") report();
    }
    report();
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
    const plugin = extractor.madeByPlugin?.();
    const recorded =
      plugin === undefined
        ? dataSets
        : dataSets.map((set) => ({
            ...set,
            made_by: { ...set.made_by, plugin },
          }));
    return {
      dataSets: recorded as DataSet[],
      timings: { totalMs: performance.now() - started },
      ...(sources.size === 0
        ? {}
        : {
            sourceDecode: Object.fromEntries(
              [...sources].map(([id, frames]) => [id, frames.decode]),
            ),
          }),
    };
  } finally {
    await Promise.all([...sources.values()].map((frames) => frames.close()));
    for (const decoder of audio.values()) decoder.close();
    session.free();
  }
}
