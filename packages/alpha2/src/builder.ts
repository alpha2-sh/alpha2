/**
 * A fluent builder for project documents: sources, tracks, clips, and caption cues, with times in
 * seconds (or exact ticks) and ids generated deterministically.
 *
 * ```ts
 * const project = new ProjectBuilder({ name: "Intro", width: 1920, height: 1080, frameRate: 30 });
 * const video = project.source("video", "asset://intro.mp4");
 * project.videoTrack().clip({ source: video, at: 0, duration: 5, sourceIn: 12 });
 * const document = project.build(); // passes validateProject, or throws
 * ```
 */

import type {
  CaptionCue,
  TextContent,
  TextStyle,
  TextStylePatch,
  Clip,
  ClipEffect,
  ClipTransition,
  Composition,
  CompositionNode,
  Generator,
  MediaSource,
  Project,
  ProjectSettings,
  Rgb,
  Track,
} from "./generated/project-schema.js";
import {
  CURRENT_SCHEMA_VERSION,
  secondsToTicks,
  validateProject,
} from "./project.js";

/** An exact time in ticks, where seconds would round. */
export interface Ticks {
  readonly ticks: number;
}

/** Seconds, or exact {@link ticks}. */
export type Time = number | Ticks;

/** `count` ticks: an exact time. Throws a `RangeError` unless `count` is a safe integer. */
export function ticks(count: number): Ticks {
  if (!Number.isSafeInteger(count)) {
    throw new RangeError(`ticks must be a safe integer, not ${count}`);
  }
  return { ticks: count };
}

/**
 * A time in ticks: seconds rounded to the nearest tick ({@link secondsToTicks}).
 *
 * @throws RangeError For seconds that are not finite.
 */
export function toTicks(time: Time): number {
  if (typeof time === "number") {
    if (!Number.isFinite(time)) throw new RangeError(`${time} is not a time`);
    return secondsToTicks(time);
  }
  return time.ticks;
}

/** What a generated id names. */
export type IdKind = "source" | "track" | "clip" | "composition";

/**
 * Makes ids: called with what the id names, in build order. A builder skips an id it already holds
 * of that kind, given explicitly or generated, and calls again.
 */
export type IdGenerator = (kind: IdKind) => string;

/** `source-1`, `track-1`, `clip-1`, ..., counted per kind. */
export function sequentialIds(): IdGenerator {
  const counts = new Map<string, number>();
  return (kind) => {
    const next = (counts.get(kind) ?? 0) + 1;
    counts.set(kind, next);
    return `${kind}-${next}`;
  };
}

/** The settings a {@link ProjectBuilder} starts from. */
export interface ProjectBuilderOptions {
  readonly name: string;
  /** Project id (default `project`). */
  readonly id?: string;
  readonly width: number;
  readonly height: number;
  /** Frames per second, or an exact rational such as `{ num: 30000, den: 1001 }`. */
  readonly frameRate: number | { readonly num: number; readonly den: number };
  /** Audio sample rate (default 48 000). */
  readonly sampleRate?: number;
  /** Canvas color behind every track (the engine's default when absent). */
  readonly background?: Rgb;
  /**
   * How generated ids are made (default {@link sequentialIds}). An id the builder already holds
   * is skipped, so explicit ids such as `clip-2` and generated ones mix freely.
   */
  readonly ids?: IdGenerator;
}

/** A clip to place: where on the timeline, how long, and from where in the source. */
export interface ClipOptions {
  readonly source: string;
  /** Timeline start. */
  readonly at: Time;
  readonly duration: Time;
  /** Source start (default 0). */
  readonly sourceIn?: Time;
  /**
   * Source duration (default `duration`: untouched speed). Twice `duration` plays twice as fast.
   */
  readonly sourceDuration?: Time;
  /**
   * Keeps the sound's pitch at a speed other than 1: it is time-stretched instead of resampled
   * (sets `audio.preserve_pitch`).
   */
  readonly preservePitch?: boolean;
  /** An explicit id instead of a generated one. */
  readonly id?: string;
  /**
   * Effects applied in order to the clip's picture: a composition of the clip node through one
   * effect node each, added for this clip. The clip has one composition, so this and a
   * `with.composition` together are refused.
   */
  readonly effects?: readonly ClipEffect[];
  /** Any other clip fields: visual, composition, blend mode, masks, and so on. */
  readonly with?: Partial<
    Omit<Clip, "id" | "source_id" | "timeline_range" | "source_range">
  >;
}

/** A cue to show on a caption track. */
export interface CueOptions {
  readonly at: Time;
  readonly duration: Time;
  /**
   * A string, or runs whose texts joined are the text; a run's `time` is in ticks from the cue's
   * start, for Timing selectors (karaoke).
   */
  readonly text: TextContent;
  /** Who says it; a track style of that name styles the cue when it names none. */
  readonly speaker?: string;
  /** The name of one of the track's styles, applied over its style. */
  readonly style?: string;
}

/**
 * Builds one project document. Nothing is checked until {@link ProjectBuilder.build}
 * validates the whole document.
 */
export class ProjectBuilder {
  readonly #settings: ProjectSettings;
  readonly #id: string;
  readonly #name: string;
  readonly #ids: IdGenerator;
  readonly #used = new Map<IdKind, Set<string>>();
  readonly #sources: MediaSource[] = [];
  readonly #compositions: Composition[] = [];
  readonly #tracks: { build(): Track }[] = [];

  constructor(options: ProjectBuilderOptions) {
    const rate =
      typeof options.frameRate === "number"
        ? { num: options.frameRate, den: 1 }
        : options.frameRate;
    this.#settings = {
      width: options.width,
      height: options.height,
      frame_rate: { num: rate.num, den: rate.den },
      sample_rate: options.sampleRate ?? 48_000,
      ...(options.background === undefined
        ? {}
        : { background: options.background }),
    };
    this.#id = options.id ?? "project";
    this.#name = options.name;
    this.#ids = options.ids ?? sequentialIds();
  }

  /** Adds a media source the host resolves; returns its id. */
  source(
    kind: "video" | "audio" | "image",
    locator: string,
    options: { readonly id?: string; readonly hasAudio?: boolean } = {},
  ): string {
    const id = this.#claim("source", options.id);
    this.#sources.push({
      id,
      kind,
      locator,
      ...(options.hasAudio === undefined
        ? {}
        : { has_audio: options.hasAudio }),
    });
    return id;
  }

  /**
   * Each media source's id mapped to its locator: the `assets` a native export (`@alpha2-sh/node`)
   * reads, when the locators are file paths. A relative path resolves against the exporting
   * process's working directory. Sources added to the built document by hand are not in it.
   */
  assets(): Record<string, string> {
    return Object.fromEntries(
      this.#sources.flatMap((source) =>
        source.locator === undefined ? [] : [[source.id, source.locator]],
      ),
    );
  }

  /** Adds a source the engine draws itself; returns its id. */
  generated(
    generator: Generator,
    options: { readonly id?: string } = {},
  ): string {
    const id = this.#claim("source", options.id);
    this.#sources.push({ id, kind: "generated", generator });
    return id;
  }

  /**
   * Adds a composition graph for clips to show (with `ClipOptions.with`'s `composition`); returns
   * its id.
   */
  composition(
    nodes: readonly CompositionNode[],
    output: string | null,
    options: { readonly id?: string } = {},
  ): string {
    const id = this.#claim("composition", options.id);
    this.#compositions.push({
      id,
      nodes: [...nodes],
      ...(output === null ? {} : { output }),
    });
    return id;
  }

  /** Adds a visual track, drawn over the tracks added before it. */
  videoTrack(
    options: { readonly id?: string; readonly name?: string } = {},
  ): TrackBuilder {
    return this.#track("video", options);
  }

  /** Adds an audio track. */
  audioTrack(
    options: { readonly id?: string; readonly name?: string } = {},
  ): TrackBuilder {
    return this.#track("audio", options);
  }

  /**
   * Adds a caption track in `style` (the defaults where absent), with named `styles` its cues take
   * by `style`, or by `speaker` when a style has the speaker's name.
   */
  captionTrack(
    options: {
      readonly id?: string;
      readonly name?: string;
      readonly style?: TextStyle;
      readonly styles?: Readonly<Record<string, TextStylePatch>>;
    } = {},
  ): CaptionTrackBuilder {
    const id = this.#claim("track", options.id);
    const track = new CaptionTrackBuilder(
      id,
      options.name ?? id,
      options.style ?? {},
      options.styles ?? {},
    );
    this.#tracks.push(track);
    return track;
  }

  /**
   * The document.
   *
   * @throws {RangeError} Listing every problem when it does not validate.
   */
  build(): Project {
    const project: Project = {
      schema_version: CURRENT_SCHEMA_VERSION,
      id: this.#id,
      name: this.#name,
      settings: this.#settings,
      sources: [...this.#sources],
      ...(this.#compositions.length === 0
        ? {}
        : { compositions: [...this.#compositions] }),
      timeline: { tracks: this.#tracks.map((track) => track.build()) },
    };
    const problems = validateProject(project);
    if (problems.length > 0) {
      throw new RangeError(`invalid project: ${problems.join("; ")}`);
    }
    return project;
  }

  #track(
    kind: "video" | "audio",
    options: { readonly id?: string; readonly name?: string },
  ): TrackBuilder {
    const id = this.#claim("track", options.id);
    const track = new TrackBuilder(
      id,
      options.name ?? id,
      kind,
      (explicit) => this.#claim("clip", explicit),
      (effects) => {
        const nodes: CompositionNode[] = [{ id: "clip", op: { type: "clip" } }];
        effects.forEach((effect, index) =>
          nodes.push({
            id: `effect-${index}`,
            op: {
              type: "effect",
              input: index === 0 ? "clip" : `effect-${index - 1}`,
              effect,
            },
          }),
        );
        return this.composition(nodes, nodes[nodes.length - 1].id);
      },
    );
    this.#tracks.push(track);
    return track;
  }

  /**
   * `explicit`, or the generator's next id of `kind` that this builder does not hold yet; either
   * way the id is held from now on. An explicit id is taken as given: a repeat is for `build` to
   * report.
   *
   * @throws RangeError When the generator offers only ids already held.
   */
  #claim(kind: IdKind, explicit: string | undefined): string {
    let used = this.#used.get(kind);
    if (used === undefined) {
      used = new Set();
      this.#used.set(kind, used);
    }
    if (explicit !== undefined) {
      used.add(explicit);
      return explicit;
    }
    // A generator that counts reaches a free id within one call per id held.
    let id = "";
    for (let attempt = 0; attempt <= used.size; attempt += 1) {
      id = this.#ids(kind);
      if (!used.has(id)) {
        used.add(id);
        return id;
      }
    }
    throw new RangeError(
      `the id generator offered only ${kind} ids already in use, last ${id}`,
    );
  }
}

/** Clips and transitions of one visual or audio track. */
export class TrackBuilder {
  readonly #id: string;
  readonly #name: string;
  readonly #kind: "video" | "audio";
  readonly #clipId: (explicit: string | undefined) => string;
  readonly #chain: (effects: readonly ClipEffect[]) => string;
  readonly #clips: Clip[] = [];
  readonly #transitions: ClipTransition[] = [];

  /** @internal */
  constructor(
    id: string,
    name: string,
    kind: "video" | "audio",
    clipId: (explicit: string | undefined) => string,
    chain: (effects: readonly ClipEffect[]) => string,
  ) {
    this.#id = id;
    this.#name = name;
    this.#kind = kind;
    this.#clipId = clipId;
    this.#chain = chain;
  }

  /** The track's id. */
  get id(): string {
    return this.#id;
  }

  /**
   * Places a clip; returns its id. With `effects`, a composition chaining them
   * is added to the project and the clip shows it.
   *
   * @throws RangeError For `effects` together with a `with.composition`, or a
   *   time that is not finite; nothing is added then.
   */
  clip(options: ClipOptions): string {
    const effects = options.effects ?? [];
    if (effects.length > 0 && options.with?.composition != null) {
      throw new RangeError(
        "a clip shows one composition: give effects or with.composition, not both",
      );
    }
    const start = toTicks(options.at);
    const duration = toTicks(options.duration);
    const sourceStart = toTicks(options.sourceIn ?? 0);
    const sourceDuration = toTicks(options.sourceDuration ?? options.duration);
    const id = this.#clipId(options.id);
    this.#clips.push({
      ...options.with,
      ...(options.preservePitch === undefined
        ? {}
        : {
            audio: {
              ...options.with?.audio,
              preserve_pitch: options.preservePitch,
            },
          }),
      ...(effects.length === 0
        ? {}
        : { composition: { id: this.#chain(effects) } }),
      id,
      source_id: options.source,
      timeline_range: { start, end: start + duration },
      source_range: { start: sourceStart, end: sourceStart + sourceDuration },
    });
    return id;
  }

  /**
   * A transition from `from` to `to` over the time the two clips overlap, which must be where they
   * meet.
   *
   * @throws RangeError When either clip is not on this track. An overlap that
   *   is not valid is reported by {@link ProjectBuilder.build}.
   */
  transition(
    from: string,
    to: string,
    effect: ClipTransition["effect"],
    easing: ClipTransition["easing"] = { type: "linear" },
  ): this {
    const first = this.#clips.find((clip) => clip.id === from);
    const second = this.#clips.find((clip) => clip.id === to);
    if (first === undefined || second === undefined) {
      throw new RangeError(
        `transition ${from} -> ${to} names a clip not on track ${this.#id}`,
      );
    }
    this.#transitions.push({
      from_clip: from,
      to_clip: to,
      timeline_range: {
        start: second.timeline_range.start,
        end: first.timeline_range.end,
      },
      easing,
      effect,
    });
    return this;
  }

  /** @internal */
  build(): Track {
    return {
      id: this.#id,
      name: this.#name,
      kind: this.#kind,
      clips: [...this.#clips],
      transitions: [...this.#transitions],
    };
  }
}

/** Cues of one caption track. */
export class CaptionTrackBuilder {
  readonly #id: string;
  readonly #name: string;
  readonly #style: TextStyle;
  readonly #styles: Readonly<Record<string, TextStylePatch>>;
  readonly #cues: CaptionCue[] = [];

  /** @internal */
  constructor(
    id: string,
    name: string,
    style: TextStyle,
    styles: Readonly<Record<string, TextStylePatch>>,
  ) {
    this.#id = id;
    this.#name = name;
    this.#style = style;
    this.#styles = styles;
  }

  /** The track's id. */
  get id(): string {
    return this.#id;
  }

  /** Shows `text` from `at` for `duration`. Cues may be added in any order. */
  cue(options: CueOptions): this {
    const start = toTicks(options.at);
    this.#cues.push({
      timeline_range: { start, end: start + toTicks(options.duration) },
      text: options.text,
      ...(options.speaker === undefined ? {} : { speaker: options.speaker }),
      ...(options.style === undefined ? {} : { style: options.style }),
    });
    return this;
  }

  /** @internal */
  build(): Track {
    const cues = [...this.#cues].sort(
      (a, b) => a.timeline_range.start - b.timeline_range.start,
    );
    return {
      id: this.#id,
      name: this.#name,
      kind: "caption",
      clips: [],
      captions: {
        style: this.#style,
        ...(Object.keys(this.#styles).length > 0
          ? { styles: { ...this.#styles } }
          : {}),
        cues,
      },
    };
  }
}
