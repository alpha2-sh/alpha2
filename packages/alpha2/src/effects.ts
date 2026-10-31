import type { AudioSpeaker } from "./media.js";
import type { ChromaSiting } from "./hdr-frames.js";
import type { EffectResourceKind } from "./project.js";
import type { EngineVideoCodec } from "./rate-control.js";

/** An effect filters one input; a transition blends two. */
export type EffectKind = "effect" | "transition";
/**
 * GPU tier: `a` is a full `WebGPU` adapter, `b` a downlevel one. Effects with `tierMin: "a"` do not
 * compile on tier `b`.
 */
export type CapabilityTier = "a" | "b";

/** A resource an effect reads; `name` is its shader binding name. */
export interface EffectResourceSpec {
  readonly name: string;
  readonly kind: EffectResourceKind;
}

/** An extra image an effect samples; an effect node binds it under `inputs.<name>`. */
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
    /** Whether keyframes interpolate; enum and bool keyframes only hold. */
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

/** Four non-color numbers, such as per-channel thresholds; keyframed per component. */
export interface Vec4Parameter extends ParameterBase {
  readonly type: "vec4";
  readonly default: readonly [number, number, number, number];
  readonly min?: number;
  readonly max?: number;
  readonly step?: number;
  readonly unit: "normalized";
}

/** Straight sRGB RGBA in 0..1; keyframes interpolate premultiplied. */
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
   * Up to 8 extra images, for `kind: "effect"` only. The shader reads each with
   * `sample_<name>(uv) -> vec4f` (premultiplied, sRGB-encoded, edge-clamped like `sample_src`) and
   * `size_<name>() -> vec2f` in texels. The body must not redeclare `input_<name>`, `sample_<name>`
   * or `size_<name>`.
   */
  readonly inputs?: readonly EffectInputSpec[];
  readonly params?: readonly EffectParameter[];
  readonly resources?: readonly EffectResourceSpec[];
  /** WGSL inserted into the engine's effect shader; compile errors reject registration. */
  readonly wgslBody: string;
  readonly tierMin?: CapabilityTier;
  readonly license?: string;
}

/** A compiled effect awaiting registration. */
export interface WasmCompiledEffect {
  /** `id@version`. */
  readonly id: string;
}

/** Minimal shape of the generated wasm-bindgen class. */
export interface WasmEffectRegistry {
  /**
   * Compiles one effect definition without holding the binding while the browser compiles.
   * Rejects with `EFFECT_ERROR` and the shader's diagnostics.
   */
  compileEffect(definitionJson: string): Promise<WasmCompiledEffect>;
  /** Returns `id@version`; throws `EFFECT_ERROR` if that id was registered meanwhile. */
  registerCompiledEffect(compiled: WasmCompiledEffect): string;
  unregisterEffect(id: string, version: number): boolean;
  clearEffects(): void;
  builtinCatalog(): string;
  registerLut3d(id: string, cube: string): void;
  unregisterEffectResource(id: string): boolean;
  clearEffectResources(): void;
}

/** See `WasmCanvasRenderer.setAudioContinuity`. */
export type AudioContinuity = "exact" | "continue-small-gaps";

/** Generated wasm renderer surface, shared by the GPU and CPU renderers. */
export interface WasmCanvasRenderer extends WasmEffectRegistry {
  /** Absent means `"gpu"`. The CPU renderer rejects runtime effects with `EFFECT_ERROR`. */
  readonly renderer?: "gpu" | "cpu";
  /** Absent on the CPU renderer. */
  readonly backend?: "webgpu" | "webgl2";
  /** Browser-reported adapter name; may be empty. */
  readonly adapter?: string;
  /**
   * After a loss, JSON `{ hung, gpuMs, watchdogMs, timeTicks, frameIndex, effects }` with
   * `timeTicks` as a decimal string; `undefined` before any loss and on the CPU renderer.
   */
  readonly lossReport?: string;
  /** A lost renderer rejects export readbacks and must be recreated. Never set on the CPU. */
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
  /** `"direct"`, `"bitmap"`, `"untried"` or `"canvas"` (WebGL2). */
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
  /** Planar YUV converted with the project's color pipeline, which controls tone mapping. */
  uploadYuvFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    format: string,
    codedWidth: number,
    codedHeight: number,
    data: Uint8Array,
    layout: Uint32Array,
    visibleRect: Uint32Array,
    /** Display size before rotation; carries the pixel aspect ratio. */
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
  /** Project-declared color for `sourceId`, as JSON. */
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
  /** Whether the still for `sourceId` is still upload `generation`; the engine may evict stills. */
  holdsImage(sourceId: string, generation: bigint): boolean;
  /** Releases a source's frames and still, such as after its media changed. */
  releaseSourceFrames(sourceId: string): boolean;
  clearFrames(): void;
  /** Releases one source's frames before a time in that source's media timeline. */
  trimFramesBefore(sourceId: string, mediaTimeTicks: bigint): void;
  readonly retiredFrameCount: number;
  /** JSON clip/source/timestamp needs; decoder cursors are scoped by clip ID. */
  frameNeeds(timeTicks: bigint): string;
  renderAt(timeTicks: bigint): string;
  /**
   * Uploads project-rate planar audio; `startSample` is the source sample index of the first
   * sample. `layout` is the speaker per plane, or a channel count for `FFmpeg`'s default layout
   * (3 is 2.1, 4 is 4.0). The 1 to 24 planes are concatenated; the engine folds them into the
   * project's channels by speaker.
   *
   * Missing samples mean audio not yet arrived, not silence, so a mix needs every sample of its
   * block. Upload zeros past a source's end, or pass `endsSource` when the upload reaches it.
   *
   * An upload replaces overlapping windows and joins adjacent ones. Throws `RESOURCE_LIMIT` (not
   * fatal) over 600 s or the store's 512 MiB, and `INVALID_INPUT` when malformed.
   */
  uploadAudioBlock(
    sourceId: string,
    startSample: bigint,
    sampleRate: number,
    layout: number | readonly AudioSpeaker[],
    samples: Float32Array,
    endsSource?: boolean,
  ): void;
  /** Maximum samples per channel per {@link renderAudio} call. */
  readonly audioMaxBlock: number;
  /**
   * Mixes up to {@link audioMaxBlock} samples per channel and returns the channels concatenated.
   * A longer block throws `INVALID_INPUT` (not fatal).
   */
  renderAudio(
    startSample: bigint,
    frames: number,
    playing: boolean,
  ): Float32Array;
  /** JSON source intervals required before a sample window can be mixed. */
  audioNeeds(startSample: bigint, frames: number): string;
  /**
   * Starts (from empty) or stops loudness metering before the master limiter. Starting resets the
   * normalization gain so the mix is measured as authored.
   */
  setLoudnessMetering(enabled: boolean): void;
  /** JSON `LoudnessReport` (`alpha2/loudness`) since metering started, or `null`. */
  loudnessReport(): string;
  /** Static gain in dB before the master limiter; 0 removes it, as does starting metering. */
  setNormalizationGainDb(gainDb: number): void;
  /**
   * Appends an OpenType font to the text fallback stack. The same fonts as the export host give
   * identical text.
   */
  addFont(family: string, bytes: Uint8Array, faceIndex: number): void;
  /** Appends every face of an OpenType file (including `.ttc` collections) in file order. */
  addFontFile(family: string, bytes: Uint8Array): void;
  clearFonts(): void;
  /** JSON source ids the last `renderAudio` call could not mix. */
  audioMissingSources(): string;
  audioChannels(): number;
  audioSampleRate(): number;
  audioRetainedFrames(): number;
  audioRetainedBytes(): number;
  /** JSON `[{startSample, endSample}]` of a source's retained half-open windows, newest first. */
  audioRetainedWindows(sourceId: string): string;
  releaseAudioSource(sourceId: string): boolean;
  clearAudioSources(): void;
  /**
   * Drops one source's audio ending before a time in its media timeline. Call it during playback,
   * a few seconds behind the earliest time still mixed, to bound memory.
   */
  trimAudioBefore(sourceId: string, ticks: bigint): void;
  resetAudio(): void;
  /**
   * How `renderAudio` handles a block that does not continue the previous one, outside an export.
   * `"exact"` (default) replays up to 30 s of settling history, matching a fresh renderer.
   * `"continue-small-gaps"` (live preview) continues across steps of up to {@link audioMaxBlock}
   * samples and replays only on larger jumps or after {@link resetAudio}. Exports are always
   * exact. Changing the policy resets the audio; an unknown name throws `INVALID_INPUT`.
   */
  setAudioContinuity(policy: AudioContinuity): void;
  /**
   * Starts a deterministic offscreen export over a half-open timeline range. A zero rate uses the
   * project's. Frames are packed with the chroma siting `codec` implies.
   *
   * Until `endExport`, an audio block with a missing source returns silence without advancing, so
   * it can be rendered again after the upload. `loadProject` ends the export.
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
  beginExportFrame(): ExportFrameTicket;
  /**
   * Renders one timestamp outside any export, packed for `codec`; results carry no frame index.
   * An odd 4:2:0 size throws `INVALID_INPUT`. `pixels` is an `FFmpeg` pixel format (`yuva420p`
   * keeps alpha with no background, `yuv420p10le` is 10-bit); default I420.
   */
  beginExportAt(
    timeTicks: bigint,
    codec: EngineVideoCodec,
    pixels?: string,
  ): ExportAtTicket;
  /**
   * JSON `{ completed, total, effectiveRange: { startTicks, endTicks }, audioWindow: { startSample,
   * frames } }`; ticks and `startSample` are decimal strings. See `Alpha2Exporter.effectiveRange`
   * and `audioWindow`.
   */
  exportProgress(): string;
  endExport(): void;
  diagnostics?(): string;
  /** Per-pass timing, off by default because it costs a clock read and GPU timestamp resources. */
  setPassTiming?(enabled: boolean): void;
  resizeSurface?(width: number, height: number): void;
  resetPlayer(): void;
  free?(): void;
}

/** An uploaded source texture generation and its byte cost. */
export interface FrameUploadReceipt {
  readonly generation: bigint;
  readonly retainedBytes: number;
}

/** A source frame a render needed but did not have; `mediaTimeTicks` is the time to decode. */
export interface WasmMissingFrame {
  readonly clipId: string;
  /** Composition step that reads the source; `null` when the clip reads it directly. */
  readonly readerId: string | null;
  readonly sourceId: string;
  /** Decimal string to preserve full i64 precision. */
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
      readonly format: "I420" | "I420A" | "I420P10" | "I420AP10";
      /** BT.709 limited range; pass it to the `VideoFrame` so the encoder tags the stream. */
      readonly colorSpace: VideoColorSpaceInit;
      /** Plane offsets and strides in `pixels`. Rows may be padded. */
      readonly layout: readonly ExportPlaneLayout[];
      /** Y, U, V, then alpha, as described by `layout`. */
      readonly pixels: Uint8Array;
    };

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
      /** GPU readback bytes held by this ticket. */
      readonly retainedBytes: number;
      readonly readback: Promise<
        Extract<ExportFrameResult, { kind: "rendered" }>
      >;
    };

/** A frame rendered by `beginExportAt`. */
export type ExportAtFrame = Omit<
  Extract<ExportFrameResult, { kind: "rendered" }>,
  "index"
>;

/** Result of `beginExportAt`; a rendered frame's GPU mapping can remain in flight. */
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
      /** GPU readback bytes held by this ticket. */
      readonly retainedBytes: number;
      readonly readback: Promise<ExportAtFrame>;
    };

/** Options for the binding's canvas constructors. */
export interface CanvasRendererOptions {
  /**
   * `"refuse"` rejects software adapters (fallback adapter, SwiftShader, llvmpipe, WARP) with
   * `GPU_INIT_FAILED` before binding the canvas. Default `"allow"`.
   */
  readonly softwareAdapter?: "allow" | "refuse";
}

/** Constructor shape injected by a browser host after loading wasm. */
export interface WasmEffectRegistryBinding {
  create(): Promise<WasmEffectRegistry>;
  /**
   * GPU renderer on `canvas` (WebGPU, else WebGL2). On WebGPU a rejection happens before the canvas
   * is bound, leaving it free for the CPU renderer.
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
   * CPU renderer presenting on `canvas`'s 2D context, for browsers with no GPU path. Throws
   * `INVALID_INPUT` for a canvas that already holds a WebGL or WebGPU context.
   */
  createCpu?(canvas?: OffscreenCanvas | HTMLCanvasElement): WasmCanvasRenderer;
}

/** Typed wrapper over the effect registry binding. */
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
   * Compiles and registers one effect, resolving to its `id@version`. Registrations may overlap.
   *
   * Rejects with `EFFECT_ERROR` and the shader's diagnostics when it does not compile, or when the
   * same `id@version` was registered meanwhile.
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

  /** Built-in effects and transitions with authoring metadata. */
  builtinCatalog(): BuiltinEffectCatalog {
    return JSON.parse(this.#wasm.builtinCatalog()) as BuiltinEffectCatalog;
  }

  /**
   * Registers a 3D LUT under `resource.id`. Throws, changing nothing, when the `.cube` text is
   * invalid or too large for the device.
   */
  registerResource(resource: EffectResourceData): void {
    this.#wasm.registerLut3d(resource.id, resource.data);
  }

  /** Removes a registered resource; `false` when none had `id`. */
  unregisterResource(id: string): boolean {
    return this.#wasm.unregisterEffectResource(id);
  }

  clearResources(): void {
    this.#wasm.clearEffectResources();
  }
}
