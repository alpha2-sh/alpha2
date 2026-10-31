// Browser extension providers (SVG, animated images, Lottie, OpenEXR, image sequences), running the
// native host's Rust code in a lazily loaded wasm module.

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
  | "exr_sequence";

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

interface ExtensionModule {
  default(input?: { module_or_path: string | URL }): Promise<unknown>;
  ExtensionRegistry: new () => WasmExtensionRegistry;
  /** Requires building with `--experimental-reset-state-function`. */
  __wbg_reset_state(): void;
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
 * The provider module aborted during this call (a panic or failed allocation). The module has been
 * restarted so later calls work, but the source involved stays failed.
 */
export class ExtensionModuleError extends Error {
  override readonly name = "ExtensionModuleError";
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

/**
 * The provider module, shared by every source on the page. Load it with {@link loadExtensions}.
 *
 * Providers parse untrusted files, and a panic aborts the wasm instance. The call that aborted it
 * throws {@link ExtensionModuleError} and the module restarts with its fonts re-registered, but
 * sources and resources are lost. Older sources throw {@link StaleExtensionSourceError} until
 * reopened.
 */
export class Extensions {
  readonly #module: ExtensionModule;
  #wasm: WasmExtensionRegistry;
  /** Restart count; sources from an earlier generation are stale. */
  #generation = 0;
  /** Set when a restart failed. */
  #broken: Error | null = null;
  #fonts: readonly Uint8Array[] = [];
  readonly #calls: ExtensionCalls;

  /** Use {@link loadExtensions}. */
  constructor(module: ExtensionModule) {
    this.#module = module;
    this.#wasm = new module.ExtensionRegistry();
    this.#calls = {
      call: (call) => this.#call(call),
      generation: () => this.#generation,
    };
  }

  #call<T>(call: (wasm: WasmExtensionRegistry) => T): T {
    if (this.#broken !== null)
      throw new ExtensionModuleError(
        `the extension module stopped and could not be restarted: ${this.#broken.message}`,
        { cause: this.#broken },
      );
    try {
      return call(this.#wasm);
    } catch (error) {
      if (!isTrap(error)) throw error;
      this.#restart();
      throw new ExtensionModuleError(
        `the extension module stopped (${(error as Error).message}) and was restarted`,
        { cause: error },
      );
    }
  }

  #restart(): void {
    this.#generation += 1;
    try {
      // Reuses the compiled module; no fetch or compile.
      this.#module.__wbg_reset_state();
      const wasm = new this.#module.ExtensionRegistry();
      for (const font of this.#fonts) wasm.registerFont(font);
      this.#wasm = wasm;
    } catch (error) {
      this.#broken = error instanceof Error ? error : new Error(String(error));
    }
  }

  /** Built-in providers, in priority order. */
  get providers(): readonly string[] {
    return JSON.parse(this.#call((wasm) => wasm.providers())) as string[];
  }

  /** Whether the module can open this provider source. */
  supports(provider: ProviderSource): boolean {
    return this.#call((wasm) => wasm.supports(JSON.stringify(provider)));
  }

  /**
   * Probes `bytes` with the first provider that claims them, or returns `null` so the host's own
   * decoders can read them.
   *
   * @throws When a provider claims the bytes and cannot read them.
   */
  probe(
    bytes: Uint8Array,
  ): { readonly provider: string; readonly report: ExtensionReport } | null {
    const found = this.#call((wasm) => wasm.probe(bytes));
    return found === undefined
      ? null
      : (JSON.parse(found) as {
          provider: string;
          report: ExtensionReport;
        });
  }

  /**
   * Registers bytes under a locator, such as an image sequence frame or an image an SVG references.
   * They stay until released or a module restart; {@link withResources} scopes them to one call.
   */
  registerResource(locator: string, bytes: Uint8Array): void {
    this.#call((wasm) => wasm.registerResource(locator, bytes));
  }

  releaseResource(locator: string): void {
    this.#call((wasm) => wasm.releaseResource(locator));
  }

  /** Total registered resource bytes. */
  get resourceBytes(): number {
    return this.#call((wasm) => wasm.resourceBytes());
  }

  /** Locators of resources opening this source reads, such as an image sequence's first frame. */
  openNeeds(provider: ProviderSource): readonly string[] {
    return JSON.parse(
      this.#call((wasm) => wasm.openNeeds(JSON.stringify(provider))),
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
    this.#call((wasm) => {
      wasm.clearFonts();
      for (const font of fonts) wasm.registerFont(font);
    });
    this.#fonts = [...fonts];
  }

  /**
   * Opens a provider source. Image sequences take no bytes; their frames are registered resources
   * (see {@link openNeeds} and {@link withResources}).
   *
   * @throws When the source cannot be opened.
   */
  open(provider: ProviderSource, bytes?: Uint8Array): ExtensionSource {
    return this.#call((wasm) => {
      const handle = wasm.open(JSON.stringify(provider), bytes);
      return new ExtensionSource(this.#calls, handle);
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

let loading: Promise<Extensions> | null = null;

/**
 * Loads the provider module once per page. `wasmUrl` locates `alpha2_extensions_bg.wasm` when a
 * bundler moved it. A failed load is not cached.
 */
export function loadExtensions(wasmUrl?: string | URL): Promise<Extensions> {
  loading ??= (async () => {
    const module =
      (await import("alpha2/extensions-wasm")) as unknown as ExtensionModule;
    await module.default(
      wasmUrl === undefined ? undefined : { module_or_path: wasmUrl },
    );
    return new Extensions(module);
  })().catch((error: unknown) => {
    loading = null;
    throw error;
  });
  return loading;
}
