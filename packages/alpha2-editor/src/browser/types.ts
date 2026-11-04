import type { Generator, MediaFingerprint, Project } from "alpha2/project";
import type { LayoutBounds } from "alpha2/layout";
import type {
  AssetBinding,
  AudioStreamFacts,
  ConformReport,
  MediaPurpose,
} from "alpha2/assets";
import type { BuiltinEffectCatalog, EffectDefinition } from "alpha2/effects";
import type { TrackDecodeFacts } from "alpha2/hdr-frames";
import type { EditorSession } from "../core/session.js";
import type { CapabilityReport, PlaybackPolicy } from "alpha2";
import type { Capability } from "../ports/index.js";
import type { Json } from "../core/document.js";
import type { ExtensionWasmUrls } from "alpha2/extensions";
import type { ExportJob } from "./jobs.js";
/** Bundled locations of the runtime's code and data. Each URL defaults to the packaged one. */
export interface RuntimeAssets {
  /**
   * Creates a worker running `installEditorWorker`. Without one the preview renders on the main
   * thread.
   */
  readonly workerFactory?: () => Worker;
  /**
   * Creates a worker running the `@alpha2-sh/editor/hash-worker` entry for import hashing. Without
   * one, files hash on the page's thread at about 5 ms per megabyte.
   */
  readonly hashWorkerFactory?: () => Worker;
  /**
   * Creates a worker running the `@alpha2-sh/editor/extensions-worker` entry, which probes, opens
   * and thumbnails files read by extension providers (SVG, animated image, Lottie, OpenEXR). A
   * cancelled import terminates it, as does a call past `EXTENSIONS_CALL_TIMEOUT_MS` (40 s, failing
   * the import as `provider_stalled`). Without one, or if it cannot start, those calls block the
   * page's thread.
   */
  readonly extensionsWorkerFactory?: () => Worker;
  /** The engine's wasm binary (`alpha2/wasm`). */
  readonly wasmUrl?: string | URL;
  /**
   * `alpha2_extensions_bg.wasm`, the core provider module, loaded when an SVG, animated image,
   * Lottie, OpenEXR, image sequence or LaTeX source opens.
   */
  readonly extensionsWasmUrl?: string | URL;
  /**
   * Other provider modules' `.wasm` files by module name, such as `pdf` for
   * `alpha2_extensions_pdf_bg.wasm`, each loaded when a file it reads is probed or opened.
   */
  readonly extensionModuleUrls?: ExtensionWasmUrls;
  /** The audio worklet's wasm binary, loaded on the first Play. */
  readonly audioWasmUrl?: string | URL;
  /** The audio worklet processor module. */
  readonly workletUrl?: string | URL;
  /**
   * Fonts the runtime provides beyond the document's font assets. They follow those in the
   * fallback order for captions and SVG text, and native exports receive them too.
   */
  readonly fonts?: readonly RuntimeFont[];
}
/** A font file the runtime registers with the engine. */
export interface RuntimeFont {
  readonly id: string;
  readonly name: string;
  readonly bytes: Uint8Array;
  /** The face to read in a collection file (`.ttc`/`.otc`); 0 for a single-face file. */
  readonly faceIndex: number;
}
/** A source's media as this browser session holds it; never part of the document or its history. */
export interface RuntimeAsset {
  readonly id: string;
  /** `video`, `image`, `audio`, `text`, `font`, `lut3d`, or `generated`. */
  readonly kind: string;
  readonly name?: string;
  /**
   * What the source is read from. A still is decoded from it on each raster cache miss; no decoded
   * picture is kept with the asset.
   */
  readonly file?: File;
  /** Small preview canvas; `dataUrl` is a ready-to-use image URL. */
  readonly thumbnail?: {
    readonly dataUrl?: string;
    readonly width: number;
    readonly height: number;
  } | null;
  /** Display size in pixels, where the media has one. */
  readonly width?: number;
  readonly height?: number;
  /** Seconds; 0 for a still. */
  readonly duration?: number;
  /** Whether the file has an audio stream, as the document records in `has_audio`. */
  readonly hasAudio?: boolean;
  /**
   * `false` when this browser cannot decode the file's audio (Safari before 26 has no
   * `AudioDecoder`). It then plays and exports silent in this session, while the document still
   * records the audio for native hosts and other browsers.
   */
  readonly audioDecodable?: boolean;
  /** When `audioDecodable` is `false`, what the trial decode at import found. */
  readonly audioDecodeFacts?: TrackDecodeFacts;
  /**
   * Every audio stream of the file, which a clip's `audio.stream` counts over. Conform refuses a
   * clip choosing a stream or channel not listed here.
   */
  readonly audioStreams?: readonly AudioStreamFacts[];
  /**
   * For a video, `unavailable` when this browser decodes it only to frames the engine cannot read
   * (HDR, see `BrowserBridgeOptions.hdrWithoutPlanes`). It then previews degraded and cannot export
   * in this session.
   */
  readonly planarDecode?: "available" | "unavailable";
  /** Per-bucket peak magnitudes across the whole file, for audio sources. */
  readonly peaks?: Float32Array | null;
  /** `native`: drawn centred at its own pixel size (text); otherwise contained. */
  readonly fit?: "native" | "contain";
}
/** A file decoded and probed for import, before it enters the document. */
export interface StagedAsset {
  readonly runtime: RuntimeAsset;
  /** The asset's authoring description, recorded in the document. */
  readonly description: Json;
  /** Evidence of the file, recorded with the asset and its project source. */
  readonly fingerprint?: MediaFingerprint;
  /** Releases what staging opened. Called once the bridge no longer holds the runtime. */
  dispose(): void;
}
/** Decodes and probes files for import. `createMediaAdapter` makes the default one. */
export interface BrowserMediaAdapter {
  /**
   * Stages one file. The bridge stages at most four files of one import at once.
   *
   * Rejects with an `EditorError` coded `unsupported_media` for a file the adapter does not import,
   * `unreadable_media` when this browser or an extension provider cannot read it (the reason is the
   * message and `cause`), `too_large` for Lottie, dotLottie or SVG past the engine's parse limits
   * (`MAX_LOTTIE_DOCUMENT_BYTES`, `MAX_PROVIDER_DOCUMENT_BYTES`, `MAX_SVG_DOCUMENT_BYTES`),
   * `provider_stalled` past `EXTENSIONS_CALL_TIMEOUT_MS`, and when `signal` aborts.
   */
  stage(
    file: File,
    signal: AbortSignal,
    options?: {
      /** Pass `false` to skip computing the fingerprint. Default `true`. */
      readonly fingerprint?: boolean;
    },
  ): Promise<StagedAsset>;
  /** Stages numbered PNGs or OpenEXR images as one image sequence. */
  stageSequence?(
    files: readonly File[],
    options: {
      readonly frameRate: { readonly num: number; readonly den: number };
      readonly missing: "reject" | "hold" | "blank";
      /** Source ID to stage the frames under: the asset's own for a relink, else a new one. */
      readonly id?: string;
    },
    signal: AbortSignal,
  ): Promise<StagedAsset & { readonly name?: string }>;
}
/** The parts of a dedicated worker's global (`self`) that `installEditorWorker` uses. */
export interface EditorWorkerScope {
  onmessage: ((event: MessageEvent) => void) | null;
  postMessage(message: unknown, transfer?: Transferable[]): void;
}
/** Options of `createBrowserBridge`. */
export interface BrowserBridgeOptions {
  /** Session the bridge previews, imports into and exports. Disposing it disposes the bridge. */
  readonly editor: EditorSession;
  readonly runtimeAssets?: RuntimeAssets;
  /** Defaults to `createMediaAdapter(runtimeAssets)`. */
  readonly mediaAdapter?: BrowserMediaAdapter;
  /**
   * The most runtime resources (imported media, rasterized text, proxies) held at once,
   * default 128. An import past it is refused with `EditorError` `resource_budget`.
   */
  readonly resourceLimit?: number;
  /** `false` renders picture only: no AudioContext opens and playback runs on the wall clock. */
  readonly audio?: boolean;
  /**
   * Receives errors no caller awaits, such as a failed preview render or audio that did not start.
   * The bridge keeps running after each.
   */
  readonly onError?: (error: unknown) => void;
  /**
   * The engine's asset decisions (fingerprint comparison, conform). Defaults to the packaged wasm
   * module, loaded on first use.
   */
  readonly assetDecisions?: AssetBinding;
  /**
   * Preview resolution scale relative to the document's size, above 0 and at most 8, default 0.5.
   * At 1, text and effects match export pixel for pixel. See `BrowserBridge.setPreviewScale`.
   */
  readonly previewScale?: number;
  /**
   * Who converts decoded SDR video frames to the engine's pixels, in preview and export (an export
   * may override it with `BrowserExportOptions.videoConversion`). `engine` (the default) copies the
   * planes out and converts them with the engine's shader, as a native export does. `browser`
   * saves that copy (1 to 4 ms per frame per video layer) but ignores the stream's chroma siting,
   * so sharp chroma edges can differ from a native export (Chrome places H.264 and HEVC chroma half
   * a sample off). HDR frames and sources whose color the project declares always use the engine.
   * Another value is refused when the bridge is made, with an `EditorError`
   * `invalid_video_conversion`.
   */
  readonly videoConversion?: "engine" | "browser";
  /**
   * `"auto"` (the default) draws with a hardware GPU through WebGPU or WebGL2, else with the
   * engine's CPU renderer (also when only a software adapter exists). `"gpu"` accepts any adapter,
   * software included. `"cpu"` uses the CPU renderer, which cannot draw runtime effects. GPU-loss
   * recovery may still move the preview, or an export retry that draws no runtime effect, to the
   * CPU. `capabilities.renderer`
   * reports the preview's. Another value is refused when the bridge is made, with a `RangeError`.
   */
  readonly renderer?: "auto" | "gpu" | "cpu";
  /**
   * Handling of HDR video this browser decodes only to frames the engine cannot read and has no
   * software decoder for (10-bit HEVC HDR in Chrome). `preview-degraded` (the default) imports it,
   * previews it through the browser's tone mapping with `previewIssue` set, and makes `startExport`
   * refuse exports that show it (`unsupported_media`, its cause a `PlanarDecodeUnavailableError`).
   * The native host (`@alpha2-sh/editor/native-client`) can export such a project. `refuse`
   * refuses the import (`unsupported_media`). Another value is refused when the bridge is made
   * (`RangeError`).
   */
  readonly hdrWithoutPlanes?: "refuse" | "preview-degraded";
}
/** Options of `BrowserBridge.startExport`. */
export interface BrowserExportOptions {
  /**
   * File format, default `mp4`. It limits the codecs (see `alpha2/containers`): `mov` takes no
   * VP9, AV1 or Opus, and `webm` takes only VP9, AV1 and Opus. Other codecs are refused before the
   * export renders, as `INVALID_INPUT`.
   */
  readonly container?: "mp4" | "mov" | "mkv" | "webm";
  /**
   * Defaults to the container's (`avc`, or `vp9` for `webm`). A codec this browser does not encode
   * ({@link BrowserBridge.videoEncoders}) is refused before the export renders, as `INVALID_INPUT`.
   */
  readonly videoCodec?: "avc" | "hevc" | "vp9" | "av1";
  /**
   * Defaults to the first of the container's audio codecs this browser encodes (AAC then Opus for
   * `mp4` and `mkv`, AAC for `mov`, Opus for `webm`). A codec this browser cannot encode is refused
   * before the export renders, as `INVALID_INPUT`.
   */
  readonly audioCodec?: "aac" | "opus";
  /**
   * Engine quality level (`very-low`, `low`, `medium`, `high`, `very-high`), default `high`. The
   * engine resolves it to a bitrate for the codec, size and frame rate, as for a native export.
   */
  readonly videoQuality?: string;
  /** An engine quality level for the audio, default `high`. */
  readonly audioQuality?: string;
  /**
   * Default `true`. Audio is written only when an audible source is shown. An export with audio is
   * refused before it renders if this browser encodes none of the container's audio codecs.
   */
  readonly includeAudio?: boolean;
  /**
   * Exact export frame rate as `num / den` (`{ num: 30000, den: 1001 }` for 29.97), instead of the
   * project's. `num` and `den` must be positive 32-bit integers giving a frame at least one tick
   * long, else the export is refused when it starts with an `EditorError` `invalid_frame_rate`.
   */
  readonly frameRate?: { readonly num: number; readonly den: number };
  /**
   * `original` (the default) reads originals and refuses without them; `proxies` reads fresh
   * proxies where there are some.
   */
  readonly media?: "original" | "proxies";
  /**
   * Where the file is written. `opfs` (the default where supported) streams to a temporary
   * origin-private file that the job's artifact holds until released. `file` streams to a file the
   * user picked (`showSaveFilePicker`, obtained during the click that started the export). `memory`
   * is for small outputs: the export is refused before it starts when its estimated size, with
   * room for buffer growth and copies, passes `maxBytes` (default 512 MiB). Without a disk API the
   * export falls back to `memory` under the same limit.
   */
  readonly output?: ExportOutputTarget;
  /**
   * `inFlightBytes` bounds the rendered frames held at once (GPU readbacks and copies queued for
   * the encoder), default 128 MiB. The export is refused before it starts when one frame's readback
   * and copy cannot fit. The result's `performance.memory` reports the budget, the least one frame
   * needed, the most held, and how often the budget waited.
   */
  readonly memory?: { readonly inFlightBytes?: number };
  /**
   * `auto` (the default) lets the browser pick, a hardware decoder where it has one. `software`
   * decodes on the CPU so no frame is read back from the GPU; a source the browser has no software
   * decoder for decodes as `auto` and is listed in the result's `report.decodeFallbacks`
   * (`kind: "software-unavailable"`). Another value is refused when the export starts, with an
   * `EditorError` `invalid_video_decode`.
   */
  readonly videoDecode?: "auto" | "software";
  /**
   * Overrides `BrowserBridgeOptions.videoConversion` for this export; the result's
   * `report.videoConversion` says which ran. Another value is refused when the export starts, with
   * an `EditorError` `invalid_video_conversion`.
   */
  readonly videoConversion?: "engine" | "browser";
}
/** Where a browser export writes its file. */
export type ExportOutputTarget =
  | { readonly kind: "opfs" }
  | { readonly kind: "file"; readonly handle: FileSystemFileHandle }
  | { readonly kind: "memory"; readonly maxBytes?: number };
/** A source's proxy, as the bridge keeps it. */
export interface ProxyStatus {
  /** Whether a proxy made from the recorded media is kept. */
  readonly available: boolean;
  /** Progress of one being made, from 0 to 1, or `null`. */
  readonly generating: number | null;
  /** What preview reads for the source now. */
  readonly reading: "original" | "proxy" | "unavailable";
}
/** A proxy kept in this browser's storage, by its original's SHA-256. */
export interface KeptProxy {
  readonly hash: string;
  readonly bytes: number;
}
/** What a proxy removal did: the hashes removed, and those left because a page reads them. */
export interface ProxyRemoval {
  readonly removed: readonly string[];
  readonly inUse: readonly string[];
}
/** A rectangle in fractions of the canvas, y down, as a text style's `position` is. */
export interface CanvasRect {
  readonly left: number;
  readonly top: number;
  readonly right: number;
  readonly bottom: number;
}

/**
 * Where a text generator's text lies on its picture, before the clip's transform and effects.
 * Returned by {@link BrowserBridge.measureText}.
 */
export interface TextBounds {
  /**
   * The box the style's `anchor` and `position` place: `max_width` wide, every line's em box tall,
   * plus outline and background padding.
   */
  readonly layout: CanvasRect;
  /**
   * From the leftmost line start to the rightmost line end, as tall as `layout`, with the same
   * padding. It moves only when line advances change, not glyph ink, so it gives steady handles.
   */
  readonly content: CanvasRect;
  /**
   * What the text draws (glyphs, outline, background boxes), within about a pixel of sampling
   * spread. `null` when the text draws nothing (only spaces).
   */
  readonly ink: CanvasRect | null;
  readonly lineCount: number;
}

/** The renderer that draws the bound preview, in {@link BrowserBridge.capabilities}. */
export interface PreviewRenderer {
  /** `"gpu"`, or `"cpu"`: the engine's CPU renderer in wasm. */
  readonly renderer: "gpu" | "cpu";
  /**
   * Why it draws on the CPU: no hardware GPU (a refused software adapter is named), or a GPU loss
   * moved the preview there. `null` on a GPU.
   */
  readonly rendererReason: string | null;
  readonly backend: "webgpu" | "webgl2" | "cpu" | null;
  /** The GPU adapter as the browser names it; `null` on the CPU or when it names none. */
  readonly adapter: string | null;
  /**
   * Whether runtime effects draw. On the CPU renderer each one a loaded project draws is reported
   * once as `effect_unavailable`, and an export that draws one fails with `EFFECT_ERROR`.
   */
  readonly runtimeEffects: boolean;
}

/**
 * The browser runtime of one editor session, made by `createBrowserBridge`. Times are integer
 * ticks (120000 per second). Methods that change the document dispatch through the session, so
 * they are undoable.
 */
export interface BrowserBridge {
  /** Where the preview renders: nowhere until `bindPreview`, then a worker or the main thread. */
  readonly previewBackend: "unbound" | "worker" | "main-thread";
  /** How many times the preview was made again after its GPU device or WebGL context was lost. */
  readonly previewRecoveries: number;
  /**
   * What this page can do; `gpu`, `color` and `renderer` are `null` until a preview is bound.
   * `features.preview` and `features.export` need no GPU: without one the engine draws on the CPU.
   */
  readonly capabilities: CapabilityReport<null, null> & {
    readonly renderer: PreviewRenderer | null;
    readonly features: Readonly<
      Record<
        "preview" | "audioPreview" | "transportRate" | "worker" | "export",
        Capability
      >
    >;
  };
  /** The playhead, in ticks. */
  readonly tick: number;
  readonly playing: boolean;
  /** Runtime resources currently held (see `BrowserBridgeOptions.resourceLimit`). */
  readonly resourceCount: number;
  /**
   * Preview output volume from 0 to 1, a monitor control outside the project and exports. Kept by
   * the bridge across bindings. Throws for a value outside that range.
   */
  setMonitorVolume(value: number): void;
  /** The monitor volume `setMonitorVolume` last set; 1 until then. */
  readonly monitorVolume: number;
  /**
   * Shifts the preview picture against its sound by `ms` milliseconds (positive shows it later),
   * for output delay the browser does not report, such as a wireless headset's. Kept across
   * bindings. Throws for a value that is not finite or beyond 2000 ms either way.
   */
  setAudioOffset(ms: number): void;
  /** The offset `setAudioOffset` last set; 0 until then. */
  readonly audioOffsetMs: number;
  /** Logs every source upload of the preview renderer to the console. Kept across bindings. */
  setTraceUploads(value: boolean): void;
  /**
   * Turns per-pass GPU and CPU timing of the preview renderer on or off (off by default); the
   * passes then appear in `diagnostics`. Kept across bindings.
   */
  setPassTiming(enabled: boolean): void;
  /** The preview resolution scale (see `BrowserBridgeOptions.previewScale`). */
  readonly previewScale: number;
  /**
   * Renders the preview at `scale` times the document's size and redraws the current frame;
   * resolves once drawn. The bound canvas's backing store takes the render size, so size the canvas
   * on the page with CSS (as `EditorPreview` does). Rejects with `invalid_preview_scale` unless the
   * scale is above 0 and at most 8.
   */
  setPreviewScale(scale: number): Promise<void>;
  /**
   * The latest diagnostics sample of the preview renderer (taken at most every 250 ms), or `null`.
   * Its `host` block adds to the renderer's timing:
   * - `renders`: frames the player drew;
   * - `unchanged`: frames equal to the one shown, so neither drawn nor presented;
   * - `skippedRequests`: playback refreshes that needed no new frame;
   * - `bitmapTransfers`: frames copied to the bound canvas from another surface;
   * - `renderSize`: `[width, height]` frames render at;
   * - `previewScale`, and `backend` (`worker` or `main-thread`);
   * - `audioLatencySource`: `output-latency`, `output-timestamp`, `base-only`, or `null` if silent.
   * Counters are cumulative (`renders` and `unchanged` per renderer, the others since
   * `bindPreview`): subtract two samples for a rate.
   */
  readonly diagnostics: Readonly<Record<string, unknown>> | null;
  /** The engine's built-in effects and transitions; `null` until a preview is bound. */
  readonly catalog: BuiltinEffectCatalog | null;
  /**
   * Adds runtime effects and transitions beyond the built-in catalog, addressable by `id@version`.
   * They belong to the bridge, not the document: every renderer the bridge makes registers them
   * (preview, a preview remade after GPU loss, each export attempt), and loading another project
   * keeps them.
   *
   * With a preview bound, the definitions compile on its device all or none, and the call rejects
   * with `invalid_effect` naming the first one the engine refuses. Without one they are kept
   * unchecked; `bindPreview` compiles them and drops any refused one, reporting it to `onError` as
   * `invalid_effect`. An export attempt in which the engine refuses one fails with
   * `invalid_effect`.
   *
   * Registering the same definition under an `id@version` again is skipped; a different one is
   * refused with `effect_conflict` (change the version instead). Also rejects with `invalid_effect`
   * for a definition without a string `id` and an integer `version`, and with `disposed` after the
   * bridge is disposed.
   */
  registerEffects(definitions: readonly EffectDefinition[]): Promise<void>;
  /**
   * Source `id` of the draft: its original while online, even where preview reads a proxy (see
   * {@link BrowserBridge.setPreviewMedia}), else the kept proxy standing in for it. Use
   * {@link BrowserBridge.hasOriginal} to ask whether the original itself is linked.
   */
  getAsset(id: string): RuntimeAsset | null;
  /** Notifies when derived runtime data (such as waveform peaks) arrives for an imported asset. */
  subscribeAssets(listener: () => void): () => void;
  /**
   * Why the viewer holds its last frame, or `null` when it is current (a composition at the
   * playhead cannot draw its output yet). Also set while an HDR source shows through the browser's
   * own tone mapping (see `BrowserBridgeOptions.hdrWithoutPlanes`), which cannot export here.
   */
  readonly previewIssue: string | null;
  /** Notifies when {@link previewIssue} changes. */
  subscribePreview(listener: () => void): () => void;
  /**
   * Draws the preview on `canvas`. Rejects only when the surface cannot be bound (the GPU would not
   * start); a first frame that cannot render, such as one with unlinked media, goes to `onError`
   * and is drawn by the next edit or seek. If the preview worker fails, the main-thread renderer
   * takes over the same surface: `previewBackend` becomes `main-thread` and `onError` receives one
   * `EditorError` `preview_worker_failed` (a worker that loses its GPU recovers without a report).
   * The worker is not restarted until the next `bindPreview`.
   */
  bindPreview(canvas: HTMLCanvasElement | OffscreenCanvas): Promise<{
    readonly canvas: HTMLCanvasElement | OffscreenCanvas;
    dispose(): Promise<void>;
  }>;
  /**
   * Stages `files` and adds them to the document in one undoable transaction. With `relinkId`,
   * the single file replaces that asset's media, keeping its ID and clips.
   *
   * Rejects when staging fails, when `signal` aborts, when the session loads another project
   * meanwhile (`stale_import`), for an unknown relink target (`missing_asset`), and for a relink
   * that is not one file of the same kind and recorded media (`relink_conflict`). Nothing is added
   * when any file fails.
   */
  importFiles(
    files: readonly File[],
    options?: { signal?: AbortSignal; relinkId?: string },
  ): Promise<{ assetIds: string[] }>;
  /** `importFiles([file], { relinkId: id })`. */
  relink(
    id: string,
    file: File,
    options?: { signal?: AbortSignal },
  ): Promise<{ assetIds: string[] }>;
  /**
   * Imports numbered PNGs as one image sequence at `frameRate` (the project's by default). A gap in
   * the numbering is refused unless `missing` holds or blanks it.
   */
  importSequence(
    files: readonly File[],
    options?: {
      frameRate?: { readonly num: number; readonly den: number };
      missing?: "reject" | "hold" | "blank";
      signal?: AbortSignal;
    },
  ): Promise<{ assetIds: string[] }>;
  /**
   * Relinks an image sequence to its frames at the recorded frame rate and gap rule, keeping its ID
   * and clips; the engine decides whether the frames match the recorded sequence. Refused for an
   * asset that is not an image sequence.
   */
  relinkSequence(
    id: string,
    files: readonly File[],
    options?: { signal?: AbortSignal },
  ): Promise<{ assetIds: string[] }>;
  /**
   * Moves the playhead to `tick`, pausing playback, and renders that frame. Resolves `true` when
   * the frame was drawn and `false` when a later seek or edit superseded it or nothing is bound.
   * Rejects for a tick that is not a non-negative safe integer (`invalid_time`) and when the
   * frame fails to render.
   */
  seek(tick: number): Promise<boolean>;
  /**
   * Plays from the playhead at `rate` (negative plays backwards) under a playback `policy`
   * (default `drop_video_keep_audio`), stopping at the end of the export range (else of the
   * content), or looping with `loop`. Sound plays only at rate 1 under a policy that allows it;
   * when it cannot start within 3 s the picture plays silently and `onError` hears why. Rejects
   * for a zero or non-finite rate (`invalid_rate`) and an unknown policy (`invalid_policy`).
   */
  play(
    rate?: number,
    options?: { loop?: boolean; policy?: PlaybackPolicy },
  ): Promise<void>;
  /** Stops playback and audio; the playhead stays where it was. */
  pause(): void;
  /**
   * Reads a subtitle file (`.srt`, or `.vtt` for WebVTT) into a captions asset in the bin;
   * `skipped` counts blocks that were not cues. Nothing reaches the timeline until the asset is
   * inserted (`captionTrackCommands`).
   *
   * Rejects with an `EditorError` `invalid_captions` for a file larger than
   * `MAX_CAPTION_FILE_BYTES` (not read), a WebVTT file without its header, more than
   * `MAX_CAPTION_FILE_CUES` cues, or cues that need more caption tracks than a project holds, and
   * with `invalid_text` for a file that is neither UTF-8 nor UTF-16 with a byte order mark.
   */
  importCaptions(file: File): Promise<{ assetId: string; skipped: number }>;
  /**
   * The font families caption styles can name in `font_family`: the document's font assets and
   * the runtime's fonts, in fallback order, deduplicated, as read from the fonts' name tables.
   */
  captionFontFamilies(): Promise<string[]>;
  /**
   * Where the engine draws a text generator's text (see {@link TextBounds}), laid out as preview
   * does at the draft's canvas size, without rendering. The clip's transform maps these rectangles
   * like the rest of its picture.
   *
   * Rejects an invalid text generator (`INVALID_INPUT`), one with no fonts to draw it, and after
   * the bridge is disposed (`disposed`).
   */
  measureText(
    generator: Extract<Generator, { type: "text" }>,
  ): Promise<TextBounds>;
  /**
   * Where every box of a layout generator lies at picture time `time` (whole ticks) and whether
   * anything overflows (see `alpha2/layout`'s `LayoutBounds`), laid out as preview does at the
   * draft's canvas size, without rendering. The clip's transform maps these rectangles like the
   * rest of its picture.
   *
   * Rejects an invalid layout generator (`INVALID_INPUT`), a layout with text and no fonts to draw
   * it, and after the bridge is disposed (`disposed`).
   */
  measureLayout(
    generator: Extract<Generator, { type: "layout" }>,
    time?: number,
  ): Promise<LayoutBounds>;
  /**
   * The files a native export of `document` (the current one by default) needs, for upload:
   * shown media (or their proxies with `media: "proxies"`), an image sequence's frames, font
   * files, LUTs, and text rendered to PNG. Rejects as `startExport` does when the media is not
   * ready (`ConformError`) or the document does not compile.
   */
  nativeAssets(
    document?: import("../core/document.js").EditorDocument,
    options?: Pick<BrowserExportOptions, "media">,
  ): Promise<import("../native-client/index.js").NativeAsset[]>;
  /**
   * Whether the document's media is ready for `purpose`, as the engine decides from this session's
   * originals and kept proxies.
   */
  conform(
    purpose: MediaPurpose,
    document?: import("../core/document.js").EditorDocument,
  ): Promise<ConformReport>;
  /**
   * Makes and keeps a proxy of a source whose original this session holds. Proxies are made one at
   * a time and never during an export: this waits for a running export or another proxy, and an
   * export that starts restarts the conversion from 0 once it ends. Rejects for a source without a
   * single video file (`unsupported_media`, including HDR video), without its original in this
   * session (`missing_asset`), with a proxy already being made (`proxy_busy`), when the conversion
   * makes no progress for 30 s (`proxy_stalled`; preview keeps reading the original), or when
   * `signal` aborts.
   */
  generateProxy(id: string, options?: { signal?: AbortSignal }): Promise<void>;
  /** Whether a source has a kept proxy, the progress of one being made, and what preview reads. */
  proxyStatus(id: string): ProxyStatus;
  /**
   * Which video codecs this browser encodes for an export of the current document (its size and
   * frame rate) at `videoQuality` (default `high`). An export with a codec answered `false` is
   * refused before it renders (`INVALID_INPUT`).
   */
  videoEncoders(options?: {
    readonly videoQuality?: string;
  }): Promise<
    Readonly<Record<NonNullable<BrowserExportOptions["videoCodec"]>, boolean>>
  >;
  /** Proxies this browser keeps for all projects: each original's SHA-256 and its proxy's bytes. */
  listProxies(): Promise<KeptProxy[]>;
  /**
   * Removes the kept proxies of `hashes` (originals' SHA-256). This session releases its own first
   * and refreshes the preview; a proxy still read afterwards (by the preview for an offline
   * original, by an export, or by another page of the origin) stays and is listed in `inUse`.
   */
  removeProxies(hashes: Iterable<string>): Promise<ProxyRemoval>;
  /**
   * Whether this session holds the original media of asset `id` at the document's content revision;
   * a proxy standing in for it does not count.
   */
  hasOriginal(id: string): boolean;
  /**
   * Shows a composition node in the viewer instead of the composition's output, for preview only;
   * `null` shows the outputs.
   */
  setPreviewOutput(
    target: { readonly compositionId: string; readonly nodeId: string } | null,
  ): void;
  /** The node {@link BrowserBridge.setPreviewOutput} shows, or `null`. */
  readonly previewOutput: {
    readonly compositionId: string;
    readonly nodeId: string;
  } | null;
  /**
   * Makes preview read a fresh proxy wherever a source has one (`proxies`), or the original
   * wherever it is online (`original`, the default). Preview only: an export reads what its own
   * `media` says. {@link BrowserBridge.proxyStatus} reports what each source reads.
   */
  setPreviewMedia(media: "original" | "proxies"): void;
  /** What {@link BrowserBridge.setPreviewMedia} last set. */
  readonly previewMedia: "original" | "proxies";
  /**
   * Shows another animation of a dotLottie source, keeping its clips. Throws `EditorError`
   * `unsupported_media` for another kind of source or an animation the package lacks.
   */
  selectAnimation(id: string, animation: string): void;
  /**
   * Reads an OpenEXR source's linear RGB in `primaries`, overriding what the file declares; `null`
   * goes back to the file's. Throws `EditorError` `unsupported_media` for a source whose primaries
   * cannot be chosen.
   */
  setLinearPrimaries(
    id: string,
    primaries: import("alpha2").LinearPrimaries | null,
  ): void;
  /**
   * Exports the document as it is now; later edits do not affect the job. The engine's asset
   * decisions must be loaded: call {@link BrowserBridge.conform} first, or pass `assetDecisions`.
   *
   * Throws synchronously when the bridge is disposed (`disposed`), another export runs
   * (`export_busy`), `frameRate` is not one the engine reads (`invalid_frame_rate`), `videoDecode`
   * is not `auto` or `software` (`invalid_video_decode`), `videoConversion` is not `engine` or
   * `browser` (`invalid_video_conversion`), the asset decisions are not loaded (`conform_pending`),
   * the media is not ready (`ConformError`), the document does not compile, or it shows an HDR
   * source this browser previews only degraded (`unsupported_media`, its `cause` a
   * `PlanarDecodeUnavailableError` naming the sources). Later failures arrive through the job's
   * `result`.
   */
  startExport(options?: BrowserExportOptions): ExportJob;
  /**
   * Cancels exports and imports, stops playback, and releases the preview, audio, and every
   * runtime resource. Idempotent; later calls reject or throw `disposed`.
   */
  dispose(): Promise<void>;
}
/** Options of `createWorkerPreview`. */
export interface WorkerPreviewOptions {
  /** `seek` returns the rendered frame as an `ImageBitmap`, default `false`. */
  readonly bitmapOutput?: boolean;
  /** Initial size of the worker's own canvas when no `canvas` is given, default 16x16. */
  readonly width?: number;
  readonly height?: number;
  /** Tags every message; replies for another session are dropped. A random UUID by default. */
  readonly sessionId?: string;
  readonly runtimeAssets?: RuntimeAssets;
  /** A canvas to render into, transferred to the worker. */
  readonly canvas?: OffscreenCanvas;
  /** `engine` (the default) or `browser`; see `BrowserBridgeOptions.videoConversion`. */
  readonly videoConversion?: "engine" | "browser";
  /**
   * `present-degraded` shows an HDR source without planes through the browser's own conversion
   * (`FrameSupply`'s `opaqueEngineFrames`); `refuse` (the default) fails its frames.
   */
  readonly opaqueEngineFrames?: "refuse" | "present-degraded";
  /** `"auto"` (the default: a hardware GPU, else the CPU renderer), `"gpu"` or `"cpu"`. */
  readonly renderer?: "auto" | "gpu" | "cpu";
}
/** The main-thread side of a preview renderer running in a worker. */
export interface WorkerPreview {
  /** Settles when the worker's engine has started; rejects if it cannot start. */
  readonly ready: Promise<void>;
  /**
   * Loads `project`. `assets` are the runtime assets it adds or changes; `keep` names those the
   * previous call sent that stay as they were, so their decoders stay open. Any other asset is
   * released. `fonts: null` keeps the registered fonts.
   */
  replaceProject(
    project: Project,
    revision: number,
    assets?: readonly RuntimeAsset[],
    fonts?: readonly RuntimeFont[] | null,
    keep?: readonly string[],
  ): Promise<{
    /** `[width, height]` frames render at, which the worker's canvas now is; `null` when empty. */
    renderSize: readonly [number, number] | null;
  }>;
  /**
   * Renders the frame at `tick`. `rendered` is `false` for an empty project or a render cancelled
   * meanwhile. `unchanged` means the frame equals the one already shown, so nothing was drawn and
   * no `bitmap` comes. `bitmap` is present only with `bitmapOutput`, and the caller owns it.
   * `diagnostics` are the source decoders'; `engineDiagnostics` is the renderer's sample, present
   * when newer than the one the last reply carried.
   */
  seek(tick: number): Promise<{
    rendered: boolean;
    unchanged: boolean;
    /** Sources the frame showed through the browser's own conversion. */
    degradedSources: readonly string[];
    bitmap?: ImageBitmap;
    diagnostics: Readonly<Record<string, number>>;
    engineDiagnostics?: Readonly<Record<string, unknown>>;
  }>;
  /**
   * Sets the preview resolution scale (see `BrowserBridgeOptions.previewScale`) and sizes the
   * worker's canvas to it.
   */
  setPreviewScale(scale: number): Promise<{
    renderSize: readonly [number, number] | null;
  }>;
  /** Turns per-pass timing and upload logging of the worker's renderer on or off. */
  setTracing(options: {
    passTiming?: boolean;
    traceUploads?: boolean;
  }): Promise<void>;
  /**
   * Compiles and registers runtime effects on the worker's renderer, all or none (see
   * {@link BrowserBridge.registerEffects}); an `id@version` it holds already is skipped. Rejects
   * with `invalid_effect` naming the first one the engine refuses.
   */
  registerEffects(effects: readonly EffectDefinition[]): Promise<void>;
  /** Mixes `frames` project-rate samples from `startSample`, one array per channel. */
  mix(startSample: number, frames: number): Promise<Float32Array[]>;
  /** Drops the render in flight; its `seek` resolves with `rendered: false`. */
  cancel(): Promise<void>;
  /** Releases the worker's engine and terminates the worker. Idempotent. */
  dispose(): Promise<void>;
}
