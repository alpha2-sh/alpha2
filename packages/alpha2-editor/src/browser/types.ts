import type { Generator, MediaFingerprint, Project } from "alpha2/project";
import type { LayoutBounds } from "alpha2/layout";
import type { AssetBinding, ConformReport, MediaPurpose } from "alpha2/assets";
import type { BuiltinEffectCatalog, EffectDefinition } from "alpha2/effects";
import type { EditorSession } from "../core/session.js";
import type { CapabilityReport, PlaybackPolicy } from "alpha2";
import type { Capability } from "../ports/index.js";
import type { Json } from "../core/document.js";
import type { ExportJob } from "./jobs.js";
/**
 * Where a bundler put the runtime's code and data. Each URL defaults to the location the
 * packaged module resolves on its own.
 */
export interface RuntimeAssets {
  /**
   * Creates a worker running `installEditorWorker`. With one, the preview renders in that
   * worker; without, it renders on the main thread.
   */
  readonly workerFactory?: () => Worker;
  /**
   * Creates a worker running the `@alpha2-sh/editor/hash-worker` entry. With one, each imported or
   * relinked file is hashed in a worker of its own; without, on the page's thread, which lets
   * other tasks run every few milliseconds but spends the page's CPU (about 5 ms a megabyte).
   */
  readonly hashWorkerFactory?: () => Worker;
  /**
   * Creates a worker running the `@alpha2-sh/editor/extensions-worker` entry. With one, each file an
   * extension provider reads (SVG, animated image, Lottie, OpenEXR) is probed, opened and drawn for
   * its thumbnail at import, and an SVG opened again for its font report, in a worker of its own,
   * which a cancelled import terminates, as does a call running past `EXTENSIONS_CALL_TIMEOUT_MS`
   * (40 s; the import fails as `provider_stalled`). Without one, or when the worker cannot start,
   * those calls run on the page's thread, which waits for each: an animated image's open decodes
   * every frame.
   */
  readonly extensionsWorkerFactory?: () => Worker;
  /** The engine's wasm binary (`alpha2/wasm`). */
  readonly wasmUrl?: string | URL;
  /**
   * Where a bundler put `alpha2_extensions_bg.wasm`, loaded when an SVG, animated image, or image
   * sequence is opened.
   */
  readonly extensionsWasmUrl?: string | URL;
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
/**
 * A source's media as this browser session holds it. Never part of the project document or its
 * history.
 */
export interface RuntimeAsset {
  readonly id: string;
  /** `video`, `image`, `audio`, `text`, `font`, `lut3d`, or `generated`. */
  readonly kind: string;
  readonly name?: string;
  /**
   * What the source is read from. A still is decoded from it each time a frame needs its raster and
   * the cache does not hold it: no decoded picture stays with the asset.
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
  /**
   * Whether the file has an audio stream: what the document records as the source's `has_audio`,
   * whichever browser imported it.
   */
  readonly hasAudio?: boolean;
  /**
   * `false` when this browser cannot decode the file's audio (Safari before 26 has no
   * `AudioDecoder`): it plays and exports silent in this session, and the document still says the
   * source has audio, which a native host and other browsers hear.
   */
  readonly audioDecodable?: boolean;
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
   * Stages one file. The bridge may stage several files of one import at once, at most four.
   * Rejects with an `EditorError`: `unsupported_media` for a file that is no media the adapter
   * imports, `unreadable_media` for media this browser could not read (a codec it does not decode,
   * a damaged file, or one an extension provider refuses, such as one whose drawing is past its
   * budgets; the decoder's or provider's reason is in the message and is the `cause`), `too_large`
   * for a Lottie, dotLottie or SVG file larger than the engine parses (`MAX_LOTTIE_DOCUMENT_BYTES`,
   * `MAX_PROVIDER_DOCUMENT_BYTES` for a dotLottie package, `MAX_SVG_DOCUMENT_BYTES`),
   * `provider_stalled` for one the extensions worker read past `EXTENSIONS_CALL_TIMEOUT_MS`, and
   * when `signal` aborts.
   */
  stage(
    file: File,
    signal: AbortSignal,
    options?: {
      /**
       * `false` when the caller has no use for the file's fingerprint and it need not be computed:
       * the bridge adopts a kept proxy by its original's hash. Default `true`.
       */
      readonly fingerprint?: boolean;
    },
  ): Promise<StagedAsset>;
  /** Stages numbered PNGs or OpenEXR images as one image sequence. */
  stageSequence?(
    files: readonly File[],
    options: {
      readonly frameRate: { readonly num: number; readonly den: number };
      readonly missing: "reject" | "hold" | "blank";
      /**
       * The source ID the frames are staged under: the asset's own for a relink, a new one
       * otherwise.
       */
      readonly id?: string;
    },
    signal: AbortSignal,
  ): Promise<StagedAsset & { readonly name?: string }>;
}
/**
 * What `installEditorWorker` uses of a dedicated worker's global (`self` in the worker): its
 * message handler and `postMessage`.
 */
export interface EditorWorkerScope {
  onmessage: ((event: MessageEvent) => void) | null;
  postMessage(message: unknown, transfer?: Transferable[]): void;
}
/** Options of `createBrowserBridge`. */
export interface BrowserBridgeOptions {
  /**
   * The session the bridge previews, imports into, and exports. Disposing it disposes the bridge.
   */
  readonly editor: EditorSession;
  readonly runtimeAssets?: RuntimeAssets;
  /** Defaults to `createMediaAdapter(runtimeAssets)`. */
  readonly mediaAdapter?: BrowserMediaAdapter;
  /**
   * The most runtime resources (imported media, rasterized text, proxies) held at once,
   * default 128. An import past it is refused with `EditorError` `resource_budget`.
   */
  readonly resourceLimit?: number;
  /**
   * `false` renders picture only: no AudioContext is opened and playback runs on the wall clock
   * (thumbnails and other silent previews).
   */
  readonly audio?: boolean;
  /**
   * Receives errors no caller awaits: a preview render that failed, audio that did not start,
   * a refresh after an edit. The bridge keeps running after each.
   */
  readonly onError?: (error: unknown) => void;
  /**
   * The engine's asset decisions (fingerprint comparison, conform). Defaults to the packaged wasm
   * module, loaded on first use.
   */
  readonly assetDecisions?: AssetBinding;
  /**
   * The preview resolution scale, default 0.5: frames render at the document's size times it
   * (rounded as the engine rounds), above 0 and at most 8. At 1 the preview renders at the
   * document's resolution, and text and effects match export pixel for pixel. See
   * `BrowserBridge.setPreviewScale`.
   */
  readonly previewScale?: number;
  /**
   * Who converts decoded SDR video frames to the engine's pixels, in the preview and in exports
   * (an export may choose otherwise, see `BrowserExportOptions.videoConversion`). `engine` (the
   * default) copies each frame's planes out and converts them with the engine's shader, as a
   * native export converts the same stream. `browser` hands the frame to the browser instead,
   * which saves that copy (1 to 4 ms a frame per video layer) but leaves the conversion to the
   * browser, which does not read the stream's chroma siting: Chrome places H.264 and HEVC chroma
   * half a sample off (27 codes on a chroma stripe), so sharp chroma edges differ from a native
   * export's. HDR frames, and frames of a source whose color the project declares, are the
   * engine's either way. Another value is refused when the bridge is made, with an `EditorError`
   * `invalid_video_conversion`.
   */
  readonly videoConversion?: "engine" | "browser";
}
/** Options of `BrowserBridge.startExport`. */
export interface BrowserExportOptions {
  /**
   * The file format, default `mp4`. It names the file and decides which codecs an export may ask
   * for (see `alpha2/containers`): `mov` takes no VP9, AV1 or Opus, and `webm` takes only VP9, AV1
   * and Opus. A codec the container does not offer is refused before the export renders, as
   * `INVALID_INPUT`.
   */
  readonly container?: "mp4" | "mov" | "mkv" | "webm";
  /**
   * Default: the container's (`avc`, or `vp9` for `webm`). Whether this browser encodes it is the
   * browser's to say ({@link BrowserBridge.videoEncoders}); an export with one it does not is
   * refused before it renders, as `INVALID_INPUT`.
   */
  readonly videoCodec?: "avc" | "hevc" | "vp9" | "av1";
  /**
   * Default: the first of the container's audio codecs this browser encodes, in the container's
   * order (AAC, then Opus, for `mp4` and `mkv`; AAC for `mov`; Opus for `webm`). A codec asked for
   * that this browser cannot encode is refused before the export renders, as `INVALID_INPUT`.
   */
  readonly audioCodec?: "aac" | "opus";
  /**
   * One of the engine's quality levels (`very-low`, `low`, `medium`, `high`, `very-high`),
   * default `high`. The engine resolves it to a bitrate for the codec, size, and frame rate,
   * as it does for a native export.
   */
  readonly videoQuality?: string;
  /** An engine quality level for the audio, default `high`. */
  readonly audioQuality?: string;
  /**
   * Default `true`. Audio is written only when an audible source is shown. An export with
   * audio is refused before it renders on a browser that encodes none of the container's audio
   * codecs.
   */
  readonly includeAudio?: boolean;
  /**
   * The exact rate to export at instead of the project's, frames per second as `num / den`
   * (`{ num: 30000, den: 1001 }` for 29.97), as the project states its own: both backends export
   * exactly this rate. One the engine cannot read (`num` and `den` positive 32-bit integers, a
   * frame at least one tick long) is refused when the export starts, with an `EditorError`
   * `invalid_frame_rate`.
   */
  readonly frameRate?: { readonly num: number; readonly den: number };
  /**
   * `original` (the default) reads originals and refuses without them; `proxies` reads fresh
   * proxies where there are some.
   */
  readonly media?: "original" | "proxies";
  /**
   * Where the file is written. `opfs` (the default where the browser supports it) streams it to a
   * temporary origin-private file, which the job's artifact holds until released; `file` streams it
   * into a file the user picked (`showSaveFilePicker`, obtained during the click that started the
   * export); `memory` keeps it in memory, for small outputs only: the export is refused before it
   * starts when its estimated size, with room for the buffer's growth and copies, passes `maxBytes`
   * (default 512 MiB). Without a disk API the export falls back to `memory` under the same limit,
   * never to an unbounded buffer.
   */
  readonly output?: ExportOutputTarget;
  /**
   * `inFlightBytes` bounds the rendered frames the export holds at once (GPU readbacks and the
   * copies queued for the encoder), default 128 MiB. The export submits a frame only when it fits,
   * gives back the renderer's idle GPU memory when the budget stops it, and is refused before it
   * starts when one frame's readback and copy cannot fit. The result's `performance.memory` reports
   * the budget, the least one frame needed, the most held, and how often the budget waited.
   */
  readonly memory?: { readonly inFlightBytes?: number };
  /**
   * How source video decodes: `auto` (the default) lets the browser pick, a hardware decoder where
   * it has one; `software` decodes on the CPU, so no frame is read back from the GPU, except a
   * source the browser has no software decoder for, which decodes as `auto` and is reported in the
   * result's `report.decodeFallbacks` (`kind: "software-unavailable"`). Either way the engine
   * converts the same decoded values; a frame the browser cannot read back is decoded again in
   * software. Another value is refused when the export starts, with an `EditorError`
   * `invalid_video_decode`.
   */
  readonly videoDecode?: "auto" | "software";
  /**
   * Who converts decoded SDR video frames for this export, `engine` or `browser`; default the
   * bridge's (see `BrowserBridgeOptions.videoConversion`). The result's `report.videoConversion`
   * says which ran. Another value is refused when the export starts, with an `EditorError`
   * `invalid_video_conversion`.
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
/**
 * The browser runtime of one editor session, made by `createBrowserBridge`. Times are integer
 * ticks (120000 per second). Methods that change the document dispatch through the session, so
 * they are undoable.
 */
/** A rectangle in fractions of the canvas, y down, as a text style's `position` is. */
export interface CanvasRect {
  readonly left: number;
  readonly top: number;
  readonly right: number;
  readonly bottom: number;
}

/**
 * Where a text generator's text lies on the picture it draws, before its clip's transform and
 * effects: what {@link BrowserBridge.measureText} answers.
 */
export interface TextBounds {
  /**
   * The box the style's `anchor` and `position` place: `max_width` wide, every line's em box tall,
   * and the whole pixels of outline and background padding around that.
   */
  readonly layout: CanvasRect;
  /**
   * From the start of the line that starts farthest left to the end of the one that ends farthest
   * right, as tall as `layout`, with the same decorations around it. It moves only as the lines'
   * advances do, not as glyphs with more or less ink are typed: steady handles for a text clip.
   */
  readonly content: CanvasRect;
  /**
   * What the text draws: its glyphs, their outline, and the background's boxes. Every pixel the
   * frame gives the text meets it, give or take the pixel the texture's sampling spreads it by;
   * `null` when the text draws nothing (only spaces).
   */
  readonly ink: CanvasRect | null;
  readonly lineCount: number;
}

export interface BrowserBridge {
  /** Where the preview renders: nowhere until `bindPreview`, then a worker or the main thread. */
  readonly previewBackend: "unbound" | "worker" | "main-thread";
  /** How many times the preview was made again after its GPU device or WebGL context was lost. */
  readonly previewRecoveries: number;
  /** What this page can do; `gpu` and `color` are `null` until a preview is bound. */
  readonly capabilities: CapabilityReport<null, null> & {
    readonly features: Readonly<
      Record<
        "preview" | "audioPreview" | "transportRate" | "worker" | "export",
        Capability
      >
    >;
  };
  /** The playhead. */
  readonly tick: number;
  readonly playing: boolean;
  /** Runtime resources currently held (see `BrowserBridgeOptions.resourceLimit`). */
  readonly resourceCount: number;
  /**
   * Preview output volume from 0 to 1; a monitor control, not part of the project or exports.
   * The bridge keeps it: set before a preview is bound, or across bindings, it applies to every
   * preview's sound. Throws for a value outside that range.
   */
  setMonitorVolume(value: number): void;
  /** The monitor volume `setMonitorVolume` last set; 1 until then. */
  readonly monitorVolume: number;
  /**
   * Logs every source upload of the renderer that draws the preview (the worker's in worker mode)
   * to the console, for debugging. Kept for later bindings.
   */
  setTraceUploads(value: boolean): void;
  /**
   * Turns per-pass GPU and CPU timing of the renderer that draws the preview on or off; the
   * passes then appear in `diagnostics`. Off by default, since timing is profiling cost. Kept for
   * later bindings.
   */
  setPassTiming(enabled: boolean): void;
  /** The preview resolution scale (see `BrowserBridgeOptions.previewScale`). */
  readonly previewScale: number;
  /**
   * Renders the preview at `scale` times the document's size from now on, and draws the current
   * frame again; resolves once it is drawn. The bound canvas's backing store is always the size
   * frames render at, not the document's: a host sizes the canvas on the page with CSS (as
   * `EditorPreview` does). Rejects with `invalid_preview_scale` for a scale that is not above 0 and at
   * most 8.
   */
  setPreviewScale(scale: number): Promise<void>;
  /**
   * The latest diagnostics sample of the renderer that draws the preview (taken at most every
   * 250 ms; in worker mode the worker's), or `null`. Its `host` block holds, with the renderer's
   * timing:
   * - `renders`: frames the player drew;
   * - `unchanged`: frames found equal to the one shown, so neither drawn nor presented;
   * - `skippedRequests`: playback refreshes that asked for no frame, the one shown being the
   *   frame the clock asked for;
   * - `bitmapTransfers`: frames copied to the bound canvas from another surface;
   * - `renderSize`: `[width, height]` frames render at;
   * - `previewScale`, and `backend` (`worker` or `main-thread`).
   * Counters are cumulative (`renders` and `unchanged` per renderer, the others since
   * `bindPreview`): subtract two samples for a rate.
   */
  readonly diagnostics: Readonly<Record<string, unknown>> | null;
  /** The engine's built-in effects and transitions; `null` until a preview is bound. */
  readonly catalog: BuiltinEffectCatalog | null;
  /**
   * Adds runtime effects (and transitions) beyond the built-in catalog, so the document can draw
   * them by `id@version`: every renderer the bridge makes registers them, the preview's (in its
   * worker and on the main thread, and again after a lost GPU) and each export attempt's. They
   * belong to the bridge, not the document: loading another project keeps them.
   *
   * With a preview bound, the definitions are compiled on its device, all or none, and the current
   * frame is drawn again; the call rejects with `invalid_effect` naming the first one the engine
   * refuses, and keeps none of them. Without one they are kept unchecked: `bindPreview` compiles
   * them, and one refused then is dropped and reported to `onError` as `invalid_effect`. An export
   * registers those kept when it starts, and an export attempt the engine refuses one in fails with
   * `invalid_effect`.
   *
   * A definition registered already under the same `id@version` is skipped; a different one under
   * it is refused (`effect_conflict`): a changed effect takes another version. Rejects with
   * `invalid_effect` for a definition without a string `id` and an integer `version`, and once the
   * bridge is disposed (`disposed`).
   */
  registerEffects(definitions: readonly EffectDefinition[]): Promise<void>;
  /**
   * Source `id` of the draft: its original while it is online, even where preview reads a proxy
   * (see {@link BrowserBridge.setPreviewMedia}), else the kept proxy standing in for it; opened
   * with the provider settings the document names. Use {@link BrowserBridge.hasOriginal} to ask
   * whether the original itself is linked.
   */
  getAsset(id: string): RuntimeAsset | null;
  /**
   * Notifies when derived runtime data (such as waveform peaks) becomes available for an already
   * imported asset.
   */
  subscribeAssets(listener: () => void): () => void;
  /**
   * Why the viewer holds its last frame instead of the current one, or `null` when it is current: a
   * composition the playhead shows cannot draw its output yet.
   */
  readonly previewIssue: string | null;
  /** Notifies when {@link previewIssue} changes. */
  subscribePreview(listener: () => void): () => void;
  /**
   * Draws the preview on `canvas`. Rejects only when the surface cannot be bound (the GPU would not
   * start); a first frame that cannot render, such as one whose media is not linked yet, goes to
   * `onError` like any later frame and is drawn by the next edit or seek. A preview worker that
   * fails, to start or later, is given up for the main-thread renderer, which draws on the same
   * surface: `previewBackend` then says `main-thread` and `onError` hears why, once, as an
   * `EditorError` `preview_worker_failed` (a worker that lost its GPU is recovered from without a
   * report, as a lost GPU on the main thread is). A given-up worker is not started again until the
   * next `bindPreview`, not even by a preview made again after a lost GPU.
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
   * Relinks an image sequence to its frames, at the frame rate and gap rule the project recorded:
   * the asset keeps its ID and clips, and the engine decides whether the frames are the sequence it
   * recorded. Refused for an asset that is not an image sequence.
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
   * Reads a subtitle file (`.srt`, or `.vtt` for WebVTT) into a captions asset in the bin
   * (`captionsAsset`); `skipped` counts blocks that were not cues. Nothing reaches the timeline
   * until the asset is inserted (`captionTrackCommands`).
   *
   * Throws an `EditorError` (`invalid_captions`) for a file larger than `MAX_CAPTION_FILE_BYTES`,
   * which is not read, and for one the engine refuses: a WebVTT file without its header, more than
   * `MAX_CAPTION_FILE_CUES` cues, or cues that need more caption tracks than a project holds.
   */
  importCaptions(file: File): Promise<{ assetId: string; skipped: number }>;
  /**
   * The font families caption styles can name in `font_family`: those of the document's font assets
   * and the runtime's fonts, in fallback order, each once, as the engine reads them from the fonts'
   * name tables.
   */
  captionFontFamilies(): Promise<string[]>;
  /**
   * Where the engine draws a text generator's text (see {@link TextBounds}): laid out as preview
   * lays it out, with the same fonts, at the draft's canvas size, without rendering. A clip's
   * transform maps these rectangles as it maps the rest of its picture.
   *
   * Rejects a generator that is not a valid text generator (`INVALID_INPUT`), with no fonts to
   * draw it, and once the bridge is disposed (`disposed`).
   */
  measureText(
    generator: Extract<Generator, { type: "text" }>,
  ): Promise<TextBounds>;
  /**
   * Where every box of a layout generator lies at picture time `time` (whole ticks) and whether
   * anything overflows (see `alpha2/layout`'s `LayoutBounds`): laid out as preview lays it out,
   * with the same fonts, at the draft's canvas size, without rendering. A clip's transform maps
   * these rectangles as it maps the rest of its picture.
   *
   * Rejects a generator that is not a valid layout generator (`INVALID_INPUT`), a layout with text
   * and no fonts to draw it, and once the bridge is disposed (`disposed`).
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
   * Makes and keeps a proxy of a source whose original this session holds. Proxies are made one
   * at a time and never beside an export: this waits for a running export and for a proxy of
   * another source being made, and an export that starts stops the conversion, which starts again
   * from the beginning once the export ends (`proxyStatus` shows its progress back at 0). Rejects
   * for a source without a single video file (`unsupported_media`, including HDR video), without
   * its original in this session (`missing_asset`), with a proxy of it already being made
   * (`proxy_busy`), or when `signal` aborts.
   */
  generateProxy(id: string, options?: { signal?: AbortSignal }): Promise<void>;
  /** Whether a source has a kept proxy, the progress of one being made, and what preview reads. */
  proxyStatus(id: string): ProxyStatus;
  /**
   * Which video codecs this browser encodes for an export of the current document (its size and
   * frame rate) at `videoQuality` (default `high`). WebCodecs leaves the codecs to the browser;
   * an export with one answered `false` is refused before it renders (`INVALID_INPUT`).
   */
  videoEncoders(options?: {
    readonly videoQuality?: string;
  }): Promise<
    Readonly<Record<NonNullable<BrowserExportOptions["videoCodec"]>, boolean>>
  >;
  /**
   * The proxies this browser keeps, for every project: each original's SHA-256 and the bytes its
   * proxy takes.
   */
  listProxies(): Promise<KeptProxy[]>;
  /**
   * Removes the kept proxies of `hashes` (originals' SHA-256). This session lets go of its own
   * first and refreshes the preview; a proxy still read after that (by the preview, for a source
   * whose original is offline, by an export, or by another page of the origin) is left and listed
   * in `inUse`.
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
   * goes back to the file's.
   * Throws `EditorError` `unsupported_media` for a source whose primaries cannot be chosen.
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
   * `browser` (`invalid_video_conversion`), the asset
   * decisions are not loaded (`conform_pending`), the media is not ready (`ConformError`), or the
   * document does not compile. Everything later is reported through the job's `result`.
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
  /**
   * Who converts decoded SDR video frames, `engine` (the default) or `browser`; see
   * `BrowserBridgeOptions.videoConversion`.
   */
  readonly videoConversion?: "engine" | "browser";
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
   * Renders the frame at `tick`. `rendered` is `false` for an empty project or a render
   * cancelled meanwhile. `unchanged` says the frame equals the one already shown, so nothing was
   * drawn or presented and no `bitmap` comes: the surface it was copied to still shows it.
   * `bitmap` is present only with `bitmapOutput`, and the caller owns it. `diagnostics` are the
   * source decoders'; `engineDiagnostics` is the renderer's diagnostics sample, present when it
   * is newer than the one the last reply carried.
   */
  seek(tick: number): Promise<{
    rendered: boolean;
    unchanged: boolean;
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
