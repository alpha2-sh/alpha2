// Browser extension providers (SVG, animated images, Lottie, OpenEXR, image sequences, LaTeX,
// PDF), running the native host's Rust code in a lazily loaded wasm module.

import { WasmModule, type WasmBindgenGlue } from "./wasm-module.js";
import type {
  LinearPrimaries,
  ProviderSource,
} from "./generated/project-schema.js";

export type ExtensionFormat =
  | "svg"
  | "gif"
  | "apng"
  | "webp"
  | "png_sequence"
  | "lottie"
  | "dot_lottie"
  | "exr"
  | "exr_sequence"
  | "latex"
  | "pdf";

/** Frame timing in source ticks. */
export type ExtensionTiming =
  | { readonly kind: "static" }
  | { readonly kind: "frames"; readonly durations: readonly number[] }
  | {
      readonly kind: "rate";
      readonly rate: { readonly num: number; readonly den: number };
      readonly frames: number;
    };

/** A provider notice worth showing the user. */
export type ExtensionDiagnostic =
  | {
      readonly kind: "normalized_delay";
      readonly frame: number;
      readonly declared_ms: number;
    }
  | { readonly kind: "missing_font"; readonly families: string }
  /** Fonts a document does not embed, drawn with the nearest standard font. */
  | { readonly kind: "substituted_font"; readonly families: string }
  /** A referenced image is not registered or is too large; it is not drawn. */
  | { readonly kind: "missing_image"; readonly href: string }
  | {
      readonly kind: "unsupported_feature";
      readonly feature: string;
      /** JSON pointer to the first occurrence. */
      readonly path: string;
      readonly count: number;
    }
  /** A referenced image is registered but cannot be drawn; it is not drawn. */
  | {
      readonly kind: "unreadable_image";
      readonly href: string;
      readonly reason: string;
    }
  /** A package item that does not open; other items remain usable. */
  | {
      readonly kind: "unreadable_item";
      readonly item: string;
      readonly reason: string;
    }
  /** Non-finite samples, shown as 0. */
  | { readonly kind: "non_finite_samples"; readonly count: number }
  /** 32-bit float samples beyond half-float range, clamped to the largest half float. */
  | { readonly kind: "out_of_half_range"; readonly count: number }
  /** 32-bit float channels are shown at half-float precision. */
  | { readonly kind: "reduced_precision" };

/** Color of linear-light media and where it came from. */
export interface ExtensionLinearColor {
  /** Named primaries matching the chromaticities, if any. */
  readonly primaries: LinearPrimaries | null;
  /** Red, green, blue and white as CIE 1931 `[x, y]`. */
  readonly chromaticities: readonly (readonly [number, number])[];
  readonly origin: "file" | "format_default" | "project";
}

export interface ExtensionReport {
  readonly format: ExtensionFormat;
  readonly width: number;
  readonly height: number;
  readonly timing: ExtensionTiming;
  /** Requested play count, `0` for forever; an import hint. */
  readonly loop_count: number | null;
  /** Items in a package, such as a dotLottie's animations. */
  readonly items: readonly string[];
  /**
   * The package item described: the opened one, or for a probe the first that opens. `null` for
   * single-item files.
   */
  readonly item: string | null;
  /** For linear-light media (OpenEXR); `null` for sRGB media. */
  readonly color: ExtensionLinearColor | null;
  readonly diagnostics: readonly ExtensionDiagnostic[];
}

/** A rendered frame at its own size. */
export type ExtensionFrame = {
  readonly width: number;
  readonly height: number;
  readonly index: number;
  readonly startTicks: number;
  /** Exclusive end; absent for the last frame. */
  readonly endTicks?: number;
  /** Diagnostics specific to this frame. */
  readonly diagnostics: readonly ExtensionDiagnostic[];
} & (
  | {
      readonly precision: "rgba8";
      /** Premultiplied sRGB RGBA8, rows packed. */
      readonly pixels: Uint8Array;
    }
  | {
      readonly precision: "linear_f16";
      /**
       * Premultiplied linear-light RGBA half-float bits in BT.709 primaries, rows packed. Values
       * outside 0 to 1 are kept.
       */
      readonly pixels: Uint16Array;
    }
);

/** The wasm-bindgen glue every provider module's build writes. */
export interface ExtensionModule extends WasmBindgenGlue {
  ExtensionRegistry: new () => WasmExtensionRegistry;
  /** Added by the package build after `--experimental-reset-state-function`. */
  __wbg_reset_state_async(): Promise<void>;
}

interface WasmRenderedFrame {
  readonly width: number;
  readonly height: number;
  readonly precision: string;
  readonly pixels: Uint8Array;
  readonly halfPixels: Uint16Array;
  readonly diagnostics: string;
  readonly index: number;
  readonly startTicks: bigint;
  readonly endTicks: bigint | undefined;
  free(): void;
}

interface WasmExtensionRegistry {
  providers(): string;
  supports(provider: string): boolean;
  probe(bytes: Uint8Array): string | undefined;
  registerResource(locator: string, bytes: Uint8Array): void;
  releaseResource(locator: string): void;
  resourceBytes(): number;
  registerFont(bytes: Uint8Array): void;
  clearFonts(): void;
  openNeeds(provider: string): string;
  open(provider: string, bytes: Uint8Array | undefined): number;
  needs(handle: number, timeTicks: bigint): string;
  residentBytes(handle: number): number;
  sharedBytes(handle: number): number;
  reader(handle: number): number | undefined;
  report(handle: number): string;
  render(
    handle: number,
    timeTicks: bigint,
    fitWidth: number,
    fitHeight: number,
  ): WasmRenderedFrame;
  release(handle: number): void;
  free(): void;
}

/**
 * The provider module aborted during this call (a panic or failed allocation), or cannot be used.
 * After an abort the module restarts; {@link Extensions.ready} resolves once later calls work. The
 * source involved stays failed.
 */
export class ExtensionModuleError extends Error {
  override readonly name: string = "ExtensionModuleError";
}

/**
 * The provider module is restarting after an abort. Await {@link Extensions.ready} and call again.
 */
export class ExtensionModuleRestartingError extends ExtensionModuleError {
  override readonly name = "ExtensionModuleRestartingError";

  constructor() {
    super("the extension module is restarting: await ready() and call again");
  }
}

/**
 * The source was opened before the provider module restarted. Re-register its resources and open
 * it again.
 */
export class StaleExtensionSourceError extends Error {
  override readonly name = "StaleExtensionSourceError";

  constructor() {
    super(
      "the extension module restarted after this source was opened: open it again",
    );
  }
}

/** Reads a resource by locator, such as an image sequence frame. */
export type ResourceReader = (locator: string) => Promise<Uint8Array>;

/**
 * Whether `error` is a trap or stack overflow. Either leaves the instance unusable, with Rust state
 * mid-update and borrows never released.
 */
function isTrap(error: unknown): boolean {
  return (
    error instanceof WebAssembly.RuntimeError || error instanceof RangeError
  );
}

interface ExtensionCalls {
  call<T>(call: (wasm: WasmExtensionRegistry) => T): T;
  generation(): number;
}

/** A provider wasm module: the formats it opens and how a file's start shows it is one of them. */
interface ProviderModule {
  readonly name: ExtensionModuleName;
  readonly loader: WasmModule<ExtensionModule>;
  /** Provider source formats it opens; `null` for every format no other module lists. */
  readonly formats: readonly string[] | null;
  /** Whether a file's first bytes may be one this module reads, so probing should load it. */
  claims(head: Uint8Array): boolean;
  /** Whether its formats read registered resources and fonts (SVG, Lottie, image sequences). */
  readonly readsResources: boolean;
}

/** The provider wasm modules of the package, probed in this order. */
export type ExtensionModuleName = "core" | "pdf";

/** Where each provider module's `.wasm` file is when a bundler moved it. */
export type ExtensionWasmUrls = {
  readonly [name in ExtensionModuleName]?: string | URL;
};

const PDF_HEADER = [0x25, 0x50, 0x44, 0x46, 0x2d];

const providerModules: readonly ProviderModule[] = [
  {
    name: "core",
    loader: new WasmModule<ExtensionModule>(
      () => import("alpha2/extensions-wasm"),
    ),
    formats: null,
    claims: () => true,
    readsResources: true,
  },
  {
    name: "pdf",
    loader: new WasmModule<ExtensionModule>(
      () => import("alpha2/extensions-pdf-wasm"),
    ),
    formats: ["pdf"],
    // `%PDF-` within the first 1024 bytes, as PDF readers accept.
    claims: (head) => {
      const end = Math.min(head.length, 1024) - PDF_HEADER.length;
      for (let at = 0; at <= end; at++)
        if (PDF_HEADER.every((byte, index) => head[at + index] === byte))
          return true;
      return false;
    },
    readsResources: false,
  },
];

const CORE = providerModules[0];

function moduleFor(provider: ProviderSource): ProviderModule {
  return (
    providerModules.find((module) =>
      module.formats?.includes(provider.format),
    ) ?? CORE
  );
}

/**
 * One loaded provider module. A trap in a call restarts it asynchronously; until it is ready again
 * every call throws {@link ExtensionModuleRestartingError}.
 */
class ModuleSession {
  readonly module: ProviderModule;
  readonly #glue: ExtensionModule;
  #wasm: WasmExtensionRegistry;
  /** Restart count; sources from an earlier generation are stale. */
  #generation = 0;
  /** Settles when the current restart ends; `null` when none is running. */
  #restarting: Promise<void> | null = null;
  /** Set when a restart failed. */
  #broken: Error | null = null;
  #fonts: readonly Uint8Array[] = [];
  readonly calls: ExtensionCalls;

  constructor(module: ProviderModule, glue: ExtensionModule) {
    this.module = module;
    this.#glue = glue;
    this.#wasm = new glue.ExtensionRegistry();
    this.calls = {
      call: (call) => this.call(call),
      generation: () => this.#generation,
    };
  }

  call<T>(call: (wasm: WasmExtensionRegistry) => T): T {
    if (this.#broken !== null) throw this.#brokenError();
    if (this.#restarting !== null) throw new ExtensionModuleRestartingError();
    try {
      return call(this.#wasm);
    } catch (error) {
      if (!isTrap(error)) throw error;
      this.#restart();
      throw new ExtensionModuleError(
        `the ${this.module.name} extension module stopped (${(error as Error).message}) and is restarting`,
        { cause: error },
      );
    }
  }

  #brokenError(): ExtensionModuleError {
    const broken = this.#broken as Error;
    return new ExtensionModuleError(
      `the ${this.module.name} extension module stopped and could not be restarted: ${broken.message}`,
      { cause: broken },
    );
  }

  #restart(): void {
    this.#generation += 1;
    // Reuses the compiled module; no fetch or compile.
    this.#restarting = this.module.loader
      .restart()
      .then(() => {
        const wasm = new this.#glue.ExtensionRegistry();
        for (const font of this.#fonts) wasm.registerFont(font);
        this.#wasm = wasm;
      })
      .catch((error: unknown) => {
        this.#broken =
          error instanceof Error ? error : new Error(String(error));
      })
      .finally(() => {
        this.#restarting = null;
      });
  }

  async ready(): Promise<void> {
    await this.#restarting;
    if (this.#broken !== null) throw this.#brokenError();
  }

  setFonts(fonts: readonly Uint8Array[]): void {
    this.call((wasm) => {
      wasm.clearFonts();
      for (const font of fonts) wasm.registerFont(font);
    });
    this.#fonts = fonts;
  }
}

/**
 * The provider modules, shared by every source on the page. Load it with {@link loadExtensions},
 * which loads the core module; others load when {@link probe} or {@link prepare} needs them.
 *
 * Providers parse untrusted files, and a panic aborts that module's wasm instance. The call that
 * aborted it throws {@link ExtensionModuleError} and the module restarts asynchronously with its
 * fonts re-registered, but its sources and resources are lost. Until {@link ready} resolves, every
 * call to that module throws {@link ExtensionModuleRestartingError}. Older sources throw
 * {@link StaleExtensionSourceError} until reopened.
 */
export class Extensions {
  readonly #urls: ExtensionWasmUrls;
  readonly #sessions = new Map<ExtensionModuleName, ModuleSession>();
  readonly #loading = new Map<ExtensionModuleName, Promise<ModuleSession>>();
  #fonts: readonly Uint8Array[] = [];

  /** Use {@link loadExtensions}. */
  constructor(urls: ExtensionWasmUrls, core: ModuleSession) {
    this.#urls = urls;
    this.#sessions.set(core.module.name, core);
  }

  #session(module: ProviderModule): ModuleSession {
    const session = this.#sessions.get(module.name);
    if (session === undefined)
      throw new Error(
        `the ${module.name} extension module is not loaded: await prepare() first`,
      );
    return session;
  }

  #readers(): ModuleSession[] {
    return [...this.#sessions.values()].filter(
      (session) => session.module.readsResources,
    );
  }

  #load(module: ProviderModule): Promise<ModuleSession> {
    const loaded = this.#sessions.get(module.name);
    if (loaded !== undefined) return Promise.resolve(loaded);
    let loading = this.#loading.get(module.name);
    if (loading === undefined) {
      loading = loadSession(module, this.#urls[module.name])
        .then((session) => {
          if (module.readsResources && this.#fonts.length > 0)
            session.setFonts(this.#fonts);
          this.#sessions.set(module.name, session);
          return session;
        })
        .finally(() => this.#loading.delete(module.name));
      this.#loading.set(module.name, loading);
    }
    return loading;
  }

  /**
   * Resolves when every loaded module takes calls: at once unless one is restarting after an
   * abort.
   *
   * @throws {ExtensionModuleError} When a restart failed; every later call to that module throws.
   */
  async ready(): Promise<void> {
    await Promise.all(
      [...this.#sessions.values()].map((session) => session.ready()),
    );
  }

  /**
   * Loads the module that opens `provider`, so {@link open} can open it. Resolves at once when it
   * is loaded.
   *
   * @throws When the module cannot be fetched or instantiated.
   */
  async prepare(provider: ProviderSource): Promise<void> {
    await this.#load(moduleFor(provider));
  }

  /** Built-in providers of the loaded modules, in priority order. */
  get providers(): readonly string[] {
    const names = [...this.#sessions.values()].flatMap(
      (session) =>
        JSON.parse(session.call((wasm) => wasm.providers())) as string[],
    );
    return [...new Set(names)];
  }

  /** Whether the package can open this provider source; its module need not be loaded yet. */
  supports(provider: ProviderSource): boolean {
    const session = this.#sessions.get(moduleFor(provider).name);
    return (
      session === undefined ||
      session.call((wasm) => wasm.supports(JSON.stringify(provider)))
    );
  }

  /**
   * Probes `bytes` with the first provider that claims them, or answers `null` so the host's own
   * decoders can read them. Loads a module only when the bytes' start may be one of its formats.
   *
   * @throws When a provider claims the bytes and cannot read them, or a module cannot load.
   */
  async probe(bytes: Uint8Array): Promise<{
    readonly provider: string;
    readonly report: ExtensionReport;
  } | null> {
    for (const module of providerModules) {
      if (!module.claims(bytes)) continue;
      const session = await this.#load(module);
      const found = session.call((wasm) => wasm.probe(bytes));
      if (found !== undefined)
        return JSON.parse(found) as {
          provider: string;
          report: ExtensionReport;
        };
    }
    return null;
  }

  /**
   * Registers bytes under a locator, such as an image sequence frame or an image an SVG references.
   * They stay until released or a module restart; {@link withResources} scopes them to one call.
   */
  registerResource(locator: string, bytes: Uint8Array): void {
    for (const session of this.#readers())
      session.call((wasm) => wasm.registerResource(locator, bytes));
  }

  releaseResource(locator: string): void {
    for (const session of this.#readers())
      session.call((wasm) => wasm.releaseResource(locator));
  }

  /** Total registered resource bytes. */
  get resourceBytes(): number {
    return this.#readers().reduce(
      (total, session) => total + session.call((wasm) => wasm.resourceBytes()),
      0,
    );
  }

  /** Locators of resources opening this source reads, such as an image sequence's first frame. */
  openNeeds(provider: ProviderSource): readonly string[] {
    const module = moduleFor(provider);
    if (!module.readsResources) return [];
    return JSON.parse(
      this.#session(module).call((wasm) =>
        wasm.openNeeds(JSON.stringify(provider)),
      ),
    ) as string[];
  }

  /**
   * Runs `call` with the resources `needs` names registered only for its duration. Each is
   * fetched with `read`, registered just before `call`, and released afterwards, so an image
   * sequence enters the module one frame at a time.
   *
   * `needs` is re-evaluated after the reads, in the same task as `call`, so concurrent calls cannot
   * interfere. Locators previously registered with {@link registerResource} are also released, so
   * use one mechanism or the other.
   *
   * @example
   * const source = await extensions.withResources(
   *   read,
   *   () => extensions.openNeeds(provider),
   *   () => extensions.open(provider),
   * );
   * const frame = await extensions.withResources(
   *   read,
   *   () => source.needs(ticks),
   *   () => source.render(ticks, fit),
   * );
   */
  async withResources<T>(
    read: ResourceReader,
    needs: () => readonly string[],
    call: () => T,
  ): Promise<T> {
    const fetched = new Map<string, Uint8Array>();
    for (;;) {
      const wanted = needs();
      const missing = wanted.filter((locator) => !fetched.has(locator));
      if (missing.length === 0) {
        try {
          for (const locator of wanted)
            this.registerResource(locator, fetched.get(locator) as Uint8Array);
          return call();
        } finally {
          for (const locator of wanted) {
            // After a restart there is nothing to release, and a broken module throws.
            try {
              this.releaseResource(locator);
            } catch {
              // Nothing is left registered either way.
            }
          }
        }
      }
      for (const locator of missing) fetched.set(locator, await read(locator));
    }
  }

  /**
   * Replaces the fonts SVG text draws with, in fallback order. Sources read fonts when they open,
   * so existing sources keep their old fonts. Passing the same arrays again is a no-op.
   *
   * Use the same font stack as the native host's caption fonts to get identical glyphs.
   */
  setFonts(fonts: readonly Uint8Array[]): void {
    if (
      fonts.length === this.#fonts.length &&
      fonts.every((font, index) => font === this.#fonts[index])
    )
      return;
    for (const session of this.#readers()) session.setFonts([...fonts]);
    this.#fonts = [...fonts];
  }

  /**
   * Opens a provider source. LaTeX takes no bytes. Image sequences take none either; their frames
   * are registered resources (see {@link openNeeds} and {@link withResources}). The source's module
   * must be loaded: {@link probe} loads it for the bytes it claims, {@link prepare} for a source.
   *
   * @throws When the source cannot be opened.
   */
  open(provider: ProviderSource, bytes?: Uint8Array): ExtensionSource {
    const session = this.#session(moduleFor(provider));
    return session.call((wasm) => {
      const handle = wasm.open(JSON.stringify(provider), bytes);
      return new ExtensionSource(session.calls, handle);
    });
  }
}

/** An opened source that renders frames by source tick. */
export class ExtensionSource {
  readonly #calls: ExtensionCalls;
  readonly #generation: number;
  #handle: number | null;
  /** Set when this source's call aborted the module; the source is not run again. */
  #failed: ExtensionModuleError | null = null;
  readonly report: ExtensionReport;

  /** Use {@link Extensions.open}. */
  constructor(calls: ExtensionCalls, handle: number) {
    this.#calls = calls;
    this.#generation = calls.generation();
    this.#handle = handle;
    this.report = JSON.parse(
      calls.call((wasm) => wasm.report(handle)),
    ) as ExtensionReport;
  }

  /**
   * Renders the frame at a source tick. Vector sources are rasterized to fit `fit` preserving
   * aspect; raster sources render at their own size.
   *
   * @throws {StaleExtensionSourceError} When the module restarted since the source was opened.
   * @throws {ExtensionModuleError} When this source's render aborted the module, then and on every
   *   later render.
   */
  render(
    timeTicks: bigint,
    fit: { readonly width: number; readonly height: number },
  ): ExtensionFrame {
    const handle = this.#handle;
    if (handle === null) throw new Error("the source is released");
    if (this.#failed !== null) throw this.#failed;
    if (this.#generation !== this.#calls.generation())
      throw new StaleExtensionSourceError();
    let frame: WasmRenderedFrame;
    try {
      frame = this.#calls.call((wasm) =>
        wasm.render(handle, timeTicks, fit.width, fit.height),
      );
    } catch (error) {
      if (error instanceof ExtensionModuleError) this.#failed = error;
      throw error;
    }
    try {
      const common = {
        width: frame.width,
        height: frame.height,
        index: frame.index,
        startTicks: Number(frame.startTicks),
        ...(frame.endTicks === undefined
          ? {}
          : { endTicks: Number(frame.endTicks) }),
        diagnostics: JSON.parse(frame.diagnostics) as ExtensionDiagnostic[],
      };
      return frame.precision === "linear_f16"
        ? { ...common, precision: "linear_f16", pixels: frame.halfPixels }
        : { ...common, precision: "rgba8", pixels: frame.pixels };
    } finally {
      frame.free();
    }
  }

  /**
   * Locators of resources rendering at a source tick still needs. Empty for stale or released
   * sources.
   */
  needs(timeTicks: bigint): readonly string[] {
    const handle = this.#handle;
    if (
      handle === null ||
      this.#failed !== null ||
      this.#generation !== this.#calls.generation()
    )
      return [];
    return JSON.parse(
      this.#calls.call((wasm) => wasm.needs(handle, timeTicks)),
    ) as string[];
  }

  /**
   * Module memory the source holds between renders (decoded frames, last output, file bytes). 0 for
   * stale or released sources.
   */
  get residentBytes(): number {
    const handle = this.#handle;
    if (
      handle === null ||
      this.#failed !== null ||
      this.#generation !== this.#calls.generation()
    )
      return 0;
    return this.#calls.call((wasm) => wasm.residentBytes(handle));
  }

  /**
   * The part of {@link residentBytes} shared with readers from {@link reader}; count it once. 0 for
   * stale or released sources.
   */
  get sharedBytes(): number {
    const handle = this.#handle;
    if (
      handle === null ||
      this.#failed !== null ||
      this.#generation !== this.#calls.generation()
    )
      return 0;
    return this.#calls.call((wasm) => wasm.sharedBytes(handle));
  }

  /**
   * Creates an independently positioned reader that shares this source's decoded data, so distant
   * readers do not force each other back to the first frame. Readers outlive the source. `null`
   * when random access is already cheap, or when this source aborted the module.
   *
   * @throws {StaleExtensionSourceError} When the module restarted since the source was opened.
   */
  reader(): ExtensionSource | null {
    const handle = this.#handle;
    if (handle === null) throw new Error("the source is released");
    if (this.#generation !== this.#calls.generation())
      throw new StaleExtensionSourceError();
    if (this.#failed !== null) return null;
    const reader = this.#calls.call((wasm) => wasm.reader(handle));
    return reader === undefined
      ? null
      : new ExtensionSource(this.#calls, reader);
  }

  /**
   * Frees the source. Idempotent; later renders throw. Throws {@link ExtensionModuleError} if the
   * release itself aborts the module.
   */
  release(): void {
    const handle = this.#handle;
    if (handle === null) return;
    this.#handle = null;
    if (this.#generation === this.#calls.generation())
      this.#calls.call((wasm) => wasm.release(handle));
  }
}

async function loadSession(
  module: ProviderModule,
  wasmUrl: string | URL | undefined,
): Promise<ModuleSession> {
  return new ModuleSession(module, await module.loader.load(wasmUrl));
}

let loading: Promise<Extensions> | null = null;

/**
 * Loads the core provider module once per page; the others load when first needed. `wasmUrl`
 * locates the core module's `alpha2_extensions_bg.wasm`, or each module's file by name, when a
 * bundler moved them; it applies to the first call. A failed load is not cached.
 */
export function loadExtensions(
  wasmUrl?: string | URL | ExtensionWasmUrls,
): Promise<Extensions> {
  const urls: ExtensionWasmUrls =
    typeof wasmUrl === "string" || wasmUrl instanceof URL
      ? { core: wasmUrl }
      : (wasmUrl ?? {});
  loading ??= loadSession(CORE, urls.core)
    .then((core) => new Extensions(urls, core))
    .catch((error: unknown) => {
      loading = null;
      throw error;
    });
  return loading;
}
