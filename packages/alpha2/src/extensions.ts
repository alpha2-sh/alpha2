// Browser extension providers (SVG, animated images, Lottie, OpenEXR, image sequences), run by the
// native host's Rust code from a wasm module loaded only when a page opens such a source.

import type {
  LinearPrimaries,
  ProviderSource,
} from "./generated/project-schema.js";

/** A format a provider reads. */
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

/** When a source's frames are shown, in source ticks. */
export type ExtensionTiming =
  | { readonly kind: "static" }
  | { readonly kind: "frames"; readonly durations: readonly number[] }
  | {
      readonly kind: "rate";
      readonly rate: { readonly num: number; readonly den: number };
      readonly frames: number;
    };

/** Something a provider did that the user should know about. */
export type ExtensionDiagnostic =
  | {
      readonly kind: "normalized_delay";
      readonly frame: number;
      readonly declared_ms: number;
    }
  | { readonly kind: "missing_font"; readonly families: string }
  /**
   * An image the document names is not registered, or declares a size larger than a provider opens;
   * it is not drawn.
   */
  | { readonly kind: "missing_image"; readonly href: string }
  | {
      readonly kind: "unsupported_feature";
      readonly feature: string;
      /** Where it first appears, as a JSON pointer. */
      readonly path: string;
      readonly count: number;
    }
  /**
   * An image the document names is registered but cannot be drawn (a format an SVG image is not
   * drawn from, or an SVG that does not parse), and why; it is not drawn.
   */
  | {
      readonly kind: "unreadable_image";
      readonly href: string;
      readonly reason: string;
    }
  /** An item of a package that does not open, and why; the package's others can still be chosen. */
  | {
      readonly kind: "unreadable_item";
      readonly item: string;
      readonly reason: string;
    }
  /** Samples that were NaN or infinite, each shown as 0. */
  | { readonly kind: "non_finite_samples"; readonly count: number }
  /**
   * 32-bit float samples beyond the half-float range, each shown as the largest half float of its
   * sign.
   */
  | { readonly kind: "out_of_half_range"; readonly count: number }
  /** 32-bit float channels are shown at half-float precision. */
  | { readonly kind: "reduced_precision" };

/** What linear-light media's RGB is: its primaries, and where that was learned. */
export interface ExtensionLinearColor {
  /** The named primaries the chromaticities are, if any. */
  readonly primaries: LinearPrimaries | null;
  /** Red, green, blue, and white, CIE 1931 `[x, y]`. */
  readonly chromaticities: readonly (readonly [number, number])[];
  /**
   * The file declares it, the format defines it when the file does not say, or the project
   * overrides it.
   */
  readonly origin: "file" | "format_default" | "project";
}

/** What probing a file finds. */
export interface ExtensionReport {
  readonly format: ExtensionFormat;
  readonly width: number;
  readonly height: number;
  readonly timing: ExtensionTiming;
  /** How many times the file asks to play, `0` for ever; a hint for import. */
  readonly loop_count: number | null;
  /** The items a package holds, such as a dotLottie's animations. */
  readonly items: readonly string[];
  /**
   * The item of a package the report describes: the one a source opened shows, or for a probe the
   * first that opens (those before it that do not are reported as `unreadable_item`); `null` for a
   * file of one item.
   */
  readonly item: string | null;
  /** For linear-light media (OpenEXR), what its RGB is; `null` for sRGB media. */
  readonly color: ExtensionLinearColor | null;
  readonly diagnostics: readonly ExtensionDiagnostic[];
}

/** One frame a source rendered, at its own size. */
export type ExtensionFrame = {
  readonly width: number;
  readonly height: number;
  /** Which of the source's frames it is. */
  readonly index: number;
  /** The first source tick it shows at. */
  readonly startTicks: number;
  /** The first source tick it no longer shows at; absent for the last frame. */
  readonly endTicks?: number;
  /** What producing this frame found, such as non-finite samples in one frame of a sequence. */
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
       * Premultiplied linear-light RGBA half floats (their bits) in BT.709 primaries, rows packed;
       * values above 1 and below 0 are kept.
       */
      readonly pixels: Uint16Array;
    }
);

/** The generated wasm module's surface. */
interface ExtensionModule {
  default(input?: { module_or_path: string | URL }): Promise<unknown>;
  ExtensionRegistry: new () => WasmExtensionRegistry;
  /**
   * Replaces the module's instance with a new one of the same compiled module: built with
   * `--experimental-reset-state-function`.
   */
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
 * The provider module stopped inside the call that throws this: a panic in a provider, or memory it
 * could not allocate, which in wasm aborts the instance. The module has been restarted, so later
 * calls work; the source the call was for, if any, stays failed.
 */
export class ExtensionModuleError extends Error {
  override readonly name = "ExtensionModuleError";
}

/**
 * The source was opened before the provider module restarted (see {@link ExtensionModuleError}),
 * and the new instance does not hold it: open it again, after registering again the resources it
 * reads.
 */
export class StaleExtensionSourceError extends Error {
  override readonly name = "StaleExtensionSourceError";

  constructor() {
    super(
      "the extension module restarted after this source was opened: open it again",
    );
  }
}

/** Reads a resource a source needs, by its locator: an image sequence's frame. */
export type ResourceReader = (locator: string) => Promise<Uint8Array>;

/**
 * Whether `error` is the instance aborting: a trap (a panic or a failed allocation), or its stack
 * running out. Either leaves it unusable: Rust's state is mid-update, and every object a call had
 * borrowed stays borrowed.
 */
function isTrap(error: unknown): boolean {
  return (
    error instanceof WebAssembly.RuntimeError || error instanceof RangeError
  );
}

/** What an {@link ExtensionSource} calls the module through. */
interface ExtensionCalls {
  call<T>(call: (wasm: WasmExtensionRegistry) => T): T;
  generation(): number;
}

/**
 * The provider module. Load it with {@link loadExtensions}; one instance serves every source a page
 * opens.
 *
 * Providers parse untrusted files. A panic in one aborts the wasm instance, so a call that aborts
 * it restarts the module instead of leaving every later call to fail: it throws {@link
 * ExtensionModuleError}, the fonts {@link setFonts} named are registered again, and the sources and
 * resources of the old instance are gone. A source opened before then throws {@link
 * StaleExtensionSourceError} until it is opened again, with its resources registered again; the one
 * whose call aborted the module stays failed.
 */
export class Extensions {
  readonly #module: ExtensionModule;
  #wasm: WasmExtensionRegistry;
  /** Counts restarts: a source of an earlier generation is stale. */
  #generation = 0;
  /** Why the module could not be restarted, if it could not. */
  #broken: Error | null = null;
  /** The fonts {@link setFonts} registered, in order. */
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
      // Reuses the compiled module, so this neither fetches nor compiles.
      this.#module.__wbg_reset_state();
      const wasm = new this.#module.ExtensionRegistry();
      for (const font of this.#fonts) wasm.registerFont(font);
      this.#wasm = wasm;
    } catch (error) {
      this.#broken = error instanceof Error ? error : new Error(String(error));
    }
  }

  /** The providers the module was built with, in priority order. */
  get providers(): readonly string[] {
    return JSON.parse(this.#call((wasm) => wasm.providers())) as string[];
  }

  /** Whether the module opens a project's provider source. */
  supports(provider: ProviderSource): boolean {
    return this.#call((wasm) => wasm.supports(JSON.stringify(provider)));
  }

  /**
   * What the first provider that claims `bytes` finds, or `null` when none does: then the bytes are
   * the host's own decoders' to read.
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
   * Registers bytes under a locator: an image sequence's frame, or an image an SVG names. They stay
   * until released; {@link withResources} registers them only for the call that reads them. A
   * restart forgets them (see {@link Extensions}).
   */
  registerResource(locator: string, bytes: Uint8Array): void {
    this.#call((wasm) => wasm.registerResource(locator, bytes));
  }

  /** Forgets the bytes registered under a locator. */
  releaseResource(locator: string): void {
    this.#call((wasm) => wasm.releaseResource(locator));
  }

  /** The bytes registered under locators, all together. */
  get resourceBytes(): number {
    return this.#call((wasm) => wasm.resourceBytes());
  }

  /**
   * The resources opening a project's provider source reads, by locator: an image sequence's first
   * frame.
   */
  openNeeds(provider: ProviderSource): readonly string[] {
    return JSON.parse(
      this.#call((wasm) => wasm.openNeeds(JSON.stringify(provider))),
    ) as string[];
  }

  /**
   * Runs `call` with the resources `needs` names registered, and only for it: each is read with
   * `read`, registered just before `call` runs, and released when it returns or throws. So an image
   * sequence's frames enter the module one render at a time, never the whole sequence, and nothing
   * stays registered after.
   *
   * `needs` is asked again once the reads finish, in the same task as `call`: what another call
   * decoded or let go meanwhile is accounted for, and no other call can release what this one
   * registered. A locator registered before, by {@link registerResource}, is released after the
   * call too: a page registers a resource one way or the other.
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
            // A call that stopped the module took its resources with it: the new instance has none
            // to release, and one that could not restart answers nothing.
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
   * The fonts an SVG's text draws with, in fallback order: they replace those set before. A source
   * reads them when it opens, so one opened before keeps the fonts it opened with; set them and
   * open in the same task to open with them. Setting the same fonts again (the same arrays, in the
   * same order) copies nothing into the module. They are kept, to be registered again if the module
   * restarts.
   *
   * The native host draws an SVG's text with its caption fonts; a page that shows the same project
   * should give the same stack so both draw the same glyphs.
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
   * Opens a project's provider source with its own bytes (none for an image sequence, whose frames
   * are registered resources: see {@link openNeeds} and {@link withResources}).
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

/** An opened source: asked for frames by source tick. */
export class ExtensionSource {
  readonly #calls: ExtensionCalls;
  readonly #generation: number;
  #handle: number | null;
  /** Why this source's own call stopped the module: it is not run again. */
  #failed: ExtensionModuleError | null = null;
  /** What the source holds, as read when it opened. */
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
   * The frame shown at a source tick. A vector source is rasterized to fit `fit` keeping its shape;
   * a raster source answers at its own size.
   *
   * @throws {StaleExtensionSourceError} When the module restarted since the source was opened.
   * @throws {ExtensionModuleError} When this source's render stopped the module, then and on every
   * later render.
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
   * The resources rendering at a source tick reads, by locator: those the source does not already
   * hold, such as an image sequence's frame it has not decoded. Empty for a source that reads none,
   * or a stale or released one.
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
   * The bytes the source holds in the module between renders: its decoded frames and images, its
   * last rendered frame, its file's bytes when it keeps them. A host that bounds what its open
   * sources hold counts this and releases a source to free it. 0 for a stale or released source.
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
   * Of {@link residentBytes}, what the source shares with the readers {@link reader} made from it:
   * the same bytes for each of them, which a host counts once. 0 for a stale or released source.
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
   * Another reader of this source, positioned on its own: it shares what the source decoded when it
   * opened (an animated image's file and the frames it keeps) and decodes on its own, so readers
   * far apart do not send each other back to the first frame, and the file is decoded once for all
   * of them. Readers stay valid after the source they were made from is released. `null` for a
   * source that draws any time as cheaply (one source serves every reader), or one whose render
   * stopped the module.
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
   * Frees the source in the module. Idempotent; later renders throw. Like every module call, it
   * throws {@link ExtensionModuleError} if the release itself stops the module.
   */
  release(): void {
    const handle = this.#handle;
    if (handle === null) return;
    this.#handle = null;
    // A source of an earlier instance has nothing left to release.
    if (this.#generation === this.#calls.generation())
      this.#calls.call((wasm) => wasm.release(handle));
  }
}

let loading: Promise<Extensions> | null = null;

/**
 * Loads the provider module once per page. `wasmUrl` is where a bundler put
 * `alpha2_extensions_bg.wasm`; without it the module is found next to its glue. A load that fails
 * (the fetch, the instantiation) is not kept: the next call tries again.
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
