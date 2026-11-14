// The extractor contract every surface shares and its pure helpers; imports no media library.

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

/**
 * Where an extractor's values go; the host validates them as the session does. Times are integer
 * tick counts (numbers, not bigints) and values and scores finite; anything else throws.
 */
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

function text(value: unknown, what: string): string {
  if (typeof value !== "string")
    throw new TypeError(`${what} must be a string, not ${String(value)}`);
  return value;
}

function ticks(value: unknown, what: string): number {
  if (typeof value === "bigint")
    throw new TypeError(`${what} is a bigint; pass a number of ticks`);
  if (!Number.isSafeInteger(value))
    throw new TypeError(
      `${what} must be an integer tick count, not ${String(value)}`,
    );
  return value as number;
}

function finite(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isFinite(value))
    throw new TypeError(
      `${what} must be a finite number, not ${String(value)}`,
    );
  return value;
}

/**
 * An emitter that keeps the emits, in the host protocol's JSON form. An emit the wire cannot
 * carry throws a `TypeError` naming it, which fails the step.
 */
export function collectEmits(): {
  emits: Record<string, unknown>[];
  out: ExtractEmitter;
} {
  const emits: Record<string, unknown>[] = [];
  return {
    emits,
    out: {
      sample(output, time, values) {
        const what = `sample "${String(output)}"`;
        emits.push({
          output: text(output, "an emit's output"),
          time: ticks(time, `${what}: time`),
          values: [...values].map((value, index) =>
            value === null ? null : finite(value, `${what}: values[${index}]`),
          ),
        });
      },
      word(output, start, end, words, speaker) {
        const what = `word "${String(output)}"`;
        emits.push({
          output: text(output, "an emit's output"),
          start: ticks(start, `${what}: start`),
          end: ticks(end, `${what}: end`),
          text: text(words, `${what}: text`),
          ...(speaker === undefined
            ? {}
            : { speaker: text(speaker, `${what}: speaker`) }),
        });
      },
      interval(output, start, end, label, score) {
        const what = `interval "${String(output)}"`;
        emits.push({
          output: text(output, "an emit's output"),
          start: ticks(start, `${what}: start`),
          end: ticks(end, `${what}: end`),
          ...(label === undefined
            ? {}
            : { label: text(label, `${what}: label`) }),
          ...(score === undefined
            ? {}
            : { score: finite(score, `${what}: score`) }),
        });
      },
    },
  };
}

/**
 * Where `value` breaks `schema`, as a path and reason, or `undefined`. Covers the JSON Schema
 * keywords descriptors use, which are `type`, `enum`, `const`, `properties`, `required`,
 * `additionalProperties`, `items`, `minItems`, `maxItems`, `minimum`, `maximum`, `minLength` and
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
