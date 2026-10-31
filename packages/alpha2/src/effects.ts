import type { AudioSpeaker } from "./media.js";
import type { ChromaSiting } from "./hdr-frames.js";
import type { EffectResourceKind } from "./project.js";
import type { EngineVideoCodec } from "./rate-control.js";

/** An effect filters one input; a transition blends two. */
export type EffectKind = "effect" | "transition";
/**
 * GPU capability tier: `a` is a full WebGPU-capable adapter, `b` a conservative downlevel one. An
 * effect whose `tierMin` is `a` does not compile on a tier `b` adapter.
 */
export type CapabilityTier = "a" | "b";

/** A resource an effect reads, by the name its shader binds it under. */
export interface EffectResourceSpec {
  readonly name: string;
  readonly kind: EffectResourceKind;
}

/** An image an effect samples besides its source; an effect node binds it under `inputs.<name>`. */
export interface EffectInputSpec {
  readonly name: string;
}

/** Host-loaded bytes for one project effect resource. */
export type EffectResourceData = {
  readonly id: string;
  readonly kind: EffectResourceKind;
  /** UTF-8 `.cube` contents. */
  readonly data: string;
};

/** Engine-owned authoring metadata for one built-in effect. */
export interface BuiltinEffectCatalogEntry {
  readonly id: string;
  readonly version: number;
  readonly kind: EffectKind;
  readonly inputs: readonly EffectInputSpec[];
  readonly label: string;
  readonly group: string;
  readonly params: readonly (EffectParameter & {
    readonly label: string;
    readonly step?: number;
    /**
     * Whether a project may keyframe this parameter with an interpolating curve. Enum and bool
     * keyframes only hold.
     */
    readonly animatable: boolean;
  })[];
  readonly resources: readonly EffectResourceSpec[];
}

/** Discoverable catalog shared by native and WebAssembly hosts. */
export interface BuiltinEffectCatalog {
  readonly effects: readonly BuiltinEffectCatalogEntry[];
  readonly transitions: readonly BuiltinEffectCatalogEntry[];
}

interface ParameterBase {
  readonly name: string;
}

export interface FloatParameter extends ParameterBase {
  readonly type: "float";
  readonly default: number;
  readonly min?: number;
  readonly max?: number;
  readonly step?: number;
  readonly unit: "normalized" | "degrees";
}

export interface Vec2Parameter extends ParameterBase {
  readonly type: "vec2";
  readonly default: readonly [number, number];
  readonly min?: number;
  readonly max?: number;
  readonly step?: number;
  readonly unit: "normalized";
}

/**
 * Four numbers that are not a color, such as per-channel thresholds; they keyframe component by
 * component.
 */
export interface Vec4Parameter extends ParameterBase {
  readonly type: "vec4";
  readonly default: readonly [number, number, number, number];
  readonly min?: number;
  readonly max?: number;
  readonly step?: number;
  readonly unit: "normalized";
}

/** A straight sRGB RGBA color in 0..1; keyframes interpolate premultiplied. */
export interface ColorParameter extends ParameterBase {
  readonly type: "color";
  readonly default: readonly [number, number, number, number];
  readonly unit: "color";
}

export interface EnumParameter extends ParameterBase {
  readonly type: "enum";
  readonly default: number;
  readonly unit: "enum";
  readonly enumValues: readonly string[];
}

export interface BoolParameter extends ParameterBase {
  readonly type: "bool";
  readonly default: boolean;
  readonly unit: "bool";
}

export type EffectParameter =
  | FloatParameter
  | Vec2Parameter
  | Vec4Parameter
  | ColorParameter
  | EnumParameter
  | BoolParameter;

/** Data-only runtime shader contract accepted by the wasm binding. */
export interface EffectDefinition {
  /** Registered and referenced from projects as `id@version`. */
  readonly id: string;
  readonly version: number;
  readonly kind: EffectKind;
  /**
   * Images the effect samples besides its source, at most 8; only `kind: "effect"` declares them.
   * The shader reads each as `sample_<name>(uv) -> vec4f` (premultiplied, sRGB-encoded, clamped at
   * the edges, as `sample_src`) and `size_<name>() -> vec2f` (the bound texture's size in texels).
   * The wrapper declares `input_<name>`, `sample_<name>` and `size_<name>`, so a body must not.
   */
  readonly inputs?: readonly EffectInputSpec[];
  readonly params?: readonly EffectParameter[];
  readonly resources?: readonly EffectResourceSpec[];
  /** WGSL the engine places into its effect shader; compile errors reject registration. */
  readonly wgslBody: string;
  readonly tierMin?: CapabilityTier;
  readonly license?: string;
}

/** An effect the binding compiled, waiting to be registered on it. */
export interface WasmCompiledEffect {
  /** The `id@version` it registers under. */
  readonly id: string;
}

/** Minimal shape of the generated wasm-bindgen class. */
export interface WasmEffectRegistry {
  /**
   * Compiles one effect definition. The call returns once the shader and pipeline exist; the
   * promise settles when the browser's compiler answers, and holds nothing of the binding
   * meanwhile. It rejects with `EFFECT_ERROR` and the shader's diagnostics.
   */
  compileEffect(definitionJson: string): Promise<WasmCompiledEffect>;
  /**
   * Adds an effect this binding compiled and returns its `id@version`; `EFFECT_ERROR` for one
   * registered meanwhile.
   */
  registerCompiledEffect(compiled: WasmCompiledEffect): string;
  unregisterEffect(id: string, version: number): boolean;
  clearEffects(): void;
  builtinCatalog(): string;
  registerLut3d(id: string, cube: string): void;
  unregisterEffectResource(id: string): boolean;
  clearEffectResources(): void;
}

/**
 * What `renderAudio` does with a block that does not start where the previous one ended:
 * `"exact"` replays the settling history and mixes what a fresh renderer would;
 * `"continue-small-gaps"` (live preview only) continues across a step of at most `audioMaxBlock`
 * samples either way without a replay.
 */
export type AudioContinuity = "exact" | "continue-small-gaps";

/**
 * Generated wasm surface used by the concrete render-worker host: the GPU renderer
 * (`createForCanvas`) or the CPU one (`createCpu`), which answer the same calls.
 */
export interface WasmCanvasRenderer extends WasmEffectRegistry {
  /**
   * Which renderer draws: `"gpu"` (WebGPU or WebGL2) or `"cpu"` (the engine's CPU renderer in
   * wasm, which refuses runtime effects with `EFFECT_ERROR`). Absent means `"gpu"`.
   */
  readonly renderer?: "gpu" | "cpu";
  /** The graphics API a GPU renderer draws through; absent on the CPU renderer. */
  readonly backend?: "webgpu" | "webgl2";
  /** The adapter a GPU renderer draws on, as the browser names it (may be empty). */
  readonly adapter?: string;
  /**
   * After a loss, what the renderer's submission times say about it, as JSON `{ hung, gpuMs,
   * watchdogMs, timeTicks, frameIndex, effects }` (`timeTicks` a decimal string); `undefined`
   * before any loss and on the CPU renderer.
   */
  readonly lossReport?: string;
  /**
   * Whether the GPU device is gone: a WebGPU device-lost callback, or a WebGL canvas's
   * `webglcontextlost` event. A lost renderer rejects export readbacks and must be recreated. The
   * CPU renderer is never lost.
   */
  readonly deviceLost: boolean;
  readonly uploadedFrameCount: number;
  readonly uploadedFrameBytes: bigint;
  readonly supportsExternalVideoFrame: boolean;
  loadProject(projectJson: string): void;
  uploadVideoFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    frame: VideoFrame,
  ): FrameUploadReceipt;
  uploadPresentedVideoFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    frame: VideoFrame,
    outputWidth: number,
    outputHeight: number,
    rotationDegrees: number,
    mirror: boolean,
  ): FrameUploadReceipt;
  /** `"direct"`, `"bitmap"`, `"untried"`, or `"canvas"` (WebGL2): see `uploadPresentedVideoFrame`. */
  readonly videoFrameCopy: string;
  /** `uploadPresentedVideoFrame` for a bitmap made with `premultiplyAlpha: "none"`. */
  uploadPresentedImageBitmap(
    sourceId: string,
    mediaTimeTicks: bigint,
    bitmap: ImageBitmap,
    outputWidth: number,
    outputHeight: number,
    rotationDegrees: number,
    mirror: boolean,
  ): FrameUploadReceipt;
  uploadRgbaFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    width: number,
    height: number,
    pixels: Uint8Array,
  ): FrameUploadReceipt;
  /**
   * Planar YUV (a WebCodecs format) converted with the project's color pipeline: how HDR frames
   * arrive, so the project, not the browser, decides how they are tone-mapped.
   */
  uploadYuvFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    format: string,
    codedWidth: number,
    codedHeight: number,
    data: Uint8Array,
    layout: Uint32Array,
    visibleRect: Uint32Array,
    /**
     * Size the visible picture is shown at before it turns (a `VideoFrame`'s display size): its
     * pixel aspect ratio.
     */
    displayWidth: number,
    displayHeight: number,
    colorJson: string,
    chromaSiting: ChromaSiting,
    outputWidth: number,
    outputHeight: number,
    rotationDegrees: number,
    mirror: boolean,
  ): FrameUploadReceipt;
  /** Whether the loaded project declares `sourceId` HDR. */
  hdrSource(sourceId: string): boolean;
  /** The color the loaded project declares for `sourceId`, as JSON. */
  sourceColorMetadata(sourceId: string): string | undefined;
  uploadRgbaImage(
    sourceId: string,
    width: number,
    height: number,
    pixels: Uint8Array,
  ): FrameUploadReceipt;
  aliasVideoFrame(
    sourceId: string,
    existingMediaTimeTicks: bigint,
    requestedMediaTimeTicks: bigint,
  ): boolean;
  releaseFrame(sourceId: string, mediaTimeTicks: bigint): boolean;
  releaseFrameGeneration(generation: bigint): boolean;
  /**
   * Whether the still `sourceId` is still the upload `generation` names: the engine drops stills on
   * its own, by its byte budget and a count limit.
   */
  holdsImage(sourceId: string, generation: bigint): boolean;
  /** Releases the frames and still of one source whose media changed. */
  releaseSourceFrames(sourceId: string): boolean;
  clearFrames(): void;
  /**
   * Releases one source's frames before a time on that source's own timeline; other sources keep
   * theirs.
   */
  trimFramesBefore(sourceId: string, mediaTimeTicks: bigint): void;
  readonly retiredFrameCount: number;
  /** JSON clip/source/timestamp needs; decoder cursors are scoped by clip ID. */
  frameNeeds(timeTicks: bigint): string;
  renderAt(timeTicks: bigint): string;
  /**
   * Project-rate planar audio upload. `startSample` is the source sample index of the first sample,
   * at the project rate. `layout` names the speaker each plane feeds, in plane order (a block's
   * `speakers`), or is a channel count for the count's default layout (`FFmpeg`'s: 3 is 2.1, 4 is
   * 4.0), for a source that declares none. The source's 1 to 24 planes are supplied end to end; the
   * engine folds them into the project's channels by speaker.
   *
   * The mix reads a window only where it holds every sample a block needs; a sample the store does
   * not hold is audio that has not arrived, not silence. Upload zeros past a source's end, or pass
   * `endsSource` with an upload that runs to the source's end, such as a whole decoded source.
   *
   * An upload replaces the windows it overlaps and joins the ones it meets. It throws
   * `RESOURCE_LIMIT` (not fatal) when no window can hold it (over 600 s, or over the store's 512
   * MiB) and `INVALID_INPUT` when it is malformed.
   */
  uploadAudioBlock(
    sourceId: string,
    startSample: bigint,
    sampleRate: number,
    layout: number | readonly AudioSpeaker[],
    samples: Float32Array,
    endsSource?: boolean,
  ): void;
  /** The most samples per channel one {@link renderAudio} call mixes. */
  readonly audioMaxBlock: number;
  /**
   * Mixes one block of at most {@link audioMaxBlock} samples per channel and returns its channels
   * end to end. A longer block throws `INVALID_INPUT` (not fatal) naming the limit: mix it as
   * consecutive calls.
   */
  renderAudio(
    startSample: bigint,
    frames: number,
    playing: boolean,
  ): Float32Array;
  /** JSON source intervals required before a sample window can be mixed. */
  audioNeeds(startSample: bigint, frames: number): string;
  /**
   * Starts (from empty) or stops measuring the mix before the master limiter, over the complete
   * blocks `renderAudio` renders from now on. Starting removes the normalization gain, so it
   * measures the mix as authored.
   */
  setLoudnessMetering(enabled: boolean): void;
  /** JSON `LoudnessReport` (`alpha2/loudness`) since metering started, or `null`. */
  loudnessReport(): string;
  /**
   * Static gain before the master limiter, dB; zero removes it, and so does starting to meter. It
   * stays until it is set again.
   */
  setNormalizationGainDb(gainDb: number): void;
  /**
   * Appends an OpenType font to the fallback stack caption tracks and text generators draw with.
   * The same font files as the export host give identical text.
   */
  addFont(family: string, bytes: Uint8Array, faceIndex: number): void;
  /**
   * Appends every face of an OpenType font file, in the file's order: each weight of a collection
   * (`.ttc`), as the export host registers a font file.
   */
  addFontFile(family: string, bytes: Uint8Array): void;
  /** Drops every font. */
  clearFonts(): void;
  /** JSON source ids the most recent renderAudio call could not mix. */
  audioMissingSources(): string;
  audioChannels(): number;
  audioSampleRate(): number;
  audioRetainedFrames(): number;
  audioRetainedBytes(): number;
  /**
   * JSON `[{startSample, endSample}]`: one source's retained windows as half-open project-rate
   * sample intervals, most recently uploaded first.
   */
  audioRetainedWindows(sourceId: string): string;
  releaseAudioSource(sourceId: string): boolean;
  clearAudioSources(): void;
  /**
   * Drops one source's audio that ends before a time on that source's own timeline; other sources
   * keep theirs. A host calls it as playback moves on, a few seconds behind the earliest time a mix
   * still reads of the source, so what the store holds stays seconds rather than every window
   * decoded since a seek.
   */
  trimAudioBefore(sourceId: string, ticks: bigint): void;
  resetAudio(): void;
  /**
   * Chooses what `renderAudio` does outside an export with a block that does not start where the
   * previous one ended. `"exact"` (the default) resets and replays up to 30 s of settling history,
   * so the block is what a fresh renderer mixes. `"continue-small-gaps"` is for live preview: a
   * step of at most {@link audioMaxBlock} samples either way continues from the state the last
   * block left at the cost of one block, and only a longer jump, or the first block after
   * {@link resetAudio}, replays. Loudness measurements and other output that must repeat stay
   * exact; from `beginExport` to `endExport` the renderer is exact whatever is chosen. Choosing
   * another policy resets the audio. Any other name throws `INVALID_INPUT` (not fatal).
   */
  setAudioContinuity(policy: AudioContinuity): void;
  /**
   * Starts a deterministic export over a half-open timeline range.
   *
   * Export renders offscreen at the engine's export quality and never touches the presentation
   * surface, so its output does not depend on the canvas, its size, or the browser compositor. Pass
   * a zero rate to use the project's. Frames are packed for `codec`: their chroma sits where that
   * codec implies.
   *
   * Until `endExport`, an audio block with a missing source is silence and advances nothing, so
   * rendering it again once the source is uploaded continues exactly. `loadProject` ends the
   * export.
   */
  beginExport(
    startTicks: bigint,
    endTicks: bigint,
    rateNum: number,
    rateDen: number,
    codec: EngineVideoCodec,
  ): void;
  /** Renders the next export frame. Waiting never advances the cursor. */
  exportFrame(): Promise<ExportFrameResult>;
  /** Starts a frame and exposes its readback promise without awaiting it. */
  beginExportFrame(): ExportFrameTicket;
  /**
   * Starts one exact timestamp, packed for `codec`, and exposes its readback separately. It belongs
   * to no export, so neither its frame nor a readback failure carries a frame index: the caller
   * numbers it. An odd 4:2:0 size is refused with `INVALID_INPUT` naming the nearest even size,
   * before anything renders. `pixels` names the output pictures by their FFmpeg pixel format
   * (`yuva420p` keeps alpha and draws no background, `yuv420p10le` is 10-bit); absent, I420. Such a
   * frame's planes are tight, and its `format` is the `WebCodecs` one (`I420A`, `I420P10`).
   */
  beginExportAt(
    timeTicks: bigint,
    codec: EngineVideoCodec,
    pixels?: string,
  ): ExportAtTicket;
  /**
   * JSON `{ completed, total, effectiveRange: { startTicks, endTicks }, audioWindow: { startSample,
   * frames } }` for the running export. Ticks and `startSample` are decimal strings.
   * `effectiveRange` is the request widened to the frame grid; its start is the output's zero.
   * `audioWindow` is the project-rate samples the export's audio covers, from that range.
   */
  exportProgress(): string;
  endExport(): void;
  diagnostics?(): string;
  /**
   * Turns per-pass timing collection on or off. Off by default, because timing reads a clock per
   * pass and allocates GPU timestamp resources; every other diagnostic field is always available.
   */
  setPassTiming?(enabled: boolean): void;
  resizeSurface?(width: number, height: number): void;
  resetPlayer(): void;
  free?(): void;
}

/** One engine-owned source texture generation and its physical byte cost. */
export interface FrameUploadReceipt {
  readonly generation: bigint;
  readonly retainedBytes: number;
}

/**
 * A source frame a render needed and did not have, from the snapshot it rendered. A trimmed,
 * offset, or retimed clip needs its source at a time other than the timeline's, so this is what to
 * decode.
 */
export interface WasmMissingFrame {
  readonly clipId: string;
  /** The composition step that reads the source; `null` when the clip reads it itself. */
  readonly readerId: string | null;
  readonly sourceId: string;
  /** Source time in ticks, as a decimal string so every i64 bit survives. */
  readonly mediaTimeTicks: string;
}

/** One step of an engine-owned export. */
export type ExportFrameResult =
  | { readonly kind: "complete" }
  | {
      readonly kind: "waiting";
      readonly timeTicks: string;
      readonly missingSources: readonly string[];
      readonly missingFrames: readonly WasmMissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly index: number;
      readonly timeTicks: string;
      readonly width: number;
      readonly height: number;
      /** `I420`, or for an export in other pictures `I420A` or `I420P10`, with tight planes. */
      readonly format: "I420" | "I420A" | "I420P10" | "I420AP10";
      /**
       * What the planes are coded as: the engine's export coding (BT.709, limited range), which a
       * `VideoFrame` built from them carries so the encoder tags it, as the native encoder does.
       */
      readonly colorSpace: VideoColorSpaceInit;
      /**
       * Where the Y, U, and V planes sit in `pixels`. Rows are padded, so read them through this,
       * as a `VideoFrame` given it as its `layout` does, never as if the planes were tightly
       * packed.
       */
      readonly layout: readonly ExportPlaneLayout[];
      /** The planes, at the offsets and strides of `layout`: Y, U, V, then alpha. */
      readonly pixels: Uint8Array;
    };

/** One plane of an exported frame: its first byte and its row pitch. */
export interface ExportPlaneLayout {
  readonly offset: number;
  readonly stride: number;
}

/** Synchronous export status whose GPU mapping can remain in flight. */
export type ExportFrameTicket =
  | { readonly kind: "complete" }
  | {
      readonly kind: "waiting";
      readonly timeTicks: string;
      readonly missingSources: readonly string[];
      readonly missingFrames: readonly WasmMissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly index: number;
      readonly timeTicks: string;
      /** GPU readback buffer bytes retained by this ticket. */
      readonly retainedBytes: number;
      readonly readback: Promise<
        Extract<ExportFrameResult, { kind: "rendered" }>
      >;
    };

/** A frame `beginExportAt` rendered: an export frame without an index. */
export type ExportAtFrame = Omit<
  Extract<ExportFrameResult, { kind: "rendered" }>,
  "index"
>;

/**
 * What `beginExportAt` answers: waiting for sources, or a frame whose GPU mapping can remain in
 * flight.
 */
export type ExportAtTicket =
  | {
      readonly kind: "waiting";
      readonly timeTicks: string;
      readonly missingSources: readonly string[];
      readonly missingFrames: readonly WasmMissingFrame[];
    }
  | {
      readonly kind: "rendered";
      readonly timeTicks: string;
      /** GPU readback buffer bytes retained by this ticket. */
      readonly retainedBytes: number;
      readonly readback: Promise<ExportAtFrame>;
    };

/** What the binding's canvas constructors accept besides the canvas. */
export interface CanvasRendererOptions {
  /**
   * `"allow"` (the default) or `"refuse"`: refused, a software adapter (a WebGPU fallback adapter,
   * SwiftShader, llvmpipe, WARP) is not drawn on, and the constructor rejects `GPU_INIT_FAILED`
   * before binding the canvas when the browser offers nothing else.
   */
  readonly softwareAdapter?: "allow" | "refuse";
}

/** Constructor shape injected by a browser host after loading wasm. */
export interface WasmEffectRegistryBinding {
  create(): Promise<WasmEffectRegistry>;
  /**
   * The GPU renderer on `canvas`: WebGPU, else WebGL2. On WebGPU the device is opened before the
   * canvas is bound, so a rejection leaves the canvas free for the CPU renderer.
   */
  createForCanvas?(
    canvas: OffscreenCanvas,
    options?: CanvasRendererOptions,
  ): Promise<WasmCanvasRenderer>;
  createForHtmlCanvas?(
    canvas: HTMLCanvasElement,
    options?: CanvasRendererOptions,
  ): Promise<WasmCanvasRenderer>;
  /**
   * The renderer that draws on the CPU, for a browser with no WebGPU adapter and no WebGL2
   * context: it presents on `canvas`'s 2D context (nowhere without a canvas) and needs no GPU. It
   * throws `INVALID_INPUT` for a canvas that already holds a WebGL or WebGPU context.
   */
  createCpu?(canvas?: OffscreenCanvas | HTMLCanvasElement): WasmCanvasRenderer;
}

/** Typed adapter that keeps JSON serialization out of product integrations. */
export class RuntimeEffectRegistry {
  readonly #wasm: WasmEffectRegistry;

  private constructor(wasm: WasmEffectRegistry) {
    this.#wasm = wasm;
  }

  /** Creates a registry on a new binding instance; rejects when the binding cannot be created. */
  static async create(
    binding: WasmEffectRegistryBinding,
  ): Promise<RuntimeEffectRegistry> {
    return new RuntimeEffectRegistry(await binding.create());
  }

  /**
   * Compiles and registers one effect and resolves to its `id@version`. Registrations may overlap:
   * neither holds the binding while the browser compiles.
   *
   * Rejects with `EFFECT_ERROR` and the shader's diagnostics when the definition does not compile,
   * or when the same `id@version` was registered while it compiled.
   */
  async register(definition: EffectDefinition): Promise<string> {
    const compiled = await this.#wasm.compileEffect(JSON.stringify(definition));
    return this.#wasm.registerCompiledEffect(compiled);
  }

  /** Removes a host-registered effect; `false` when none was registered under `id@version`. */
  unregister(id: string, version: number): boolean {
    return this.#wasm.unregisterEffect(id, version);
  }

  /** Removes every host-registered effect; built-in effects stay. */
  clearHostEffects(): void {
    this.#wasm.clearEffects();
  }

  /** The engine's built-in effects and transitions with their authoring metadata. */
  builtinCatalog(): BuiltinEffectCatalog {
    return JSON.parse(this.#wasm.builtinCatalog()) as BuiltinEffectCatalog;
  }

  /**
   * Registers a 3D LUT under `resource.id` for projects to reference. Throws when the `.cube` text
   * is invalid or its edge length does not fit the device; a failed registration changes nothing.
   */
  registerResource(resource: EffectResourceData): void {
    this.#wasm.registerLut3d(resource.id, resource.data);
  }

  /** Removes a registered resource; `false` when none had `id`. */
  unregisterResource(id: string): boolean {
    return this.#wasm.unregisterEffectResource(id);
  }

  /** Removes every registered resource. */
  clearResources(): void {
    this.#wasm.clearEffectResources();
  }
}
