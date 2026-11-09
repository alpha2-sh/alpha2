// The extractor contract every surface shares: what an extractor declares, reads and emits.
// Types only, so importing it loads no media library.

import type { DataSet } from "./generated/project-schema.js";

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
