/** Observable browser capability/degradation categories. */
export type BrowserCapabilityDiagnostic =
  | "webgpu-unavailable"
  | "webgl2-unavailable"
  | "webcodecs-unavailable"
  | "offscreen-canvas-unavailable"
  | "wasm-simd-unavailable"
  | "wasm-reference-types-unavailable"
  | "visibility-suspended"
  | "memory-pressure"
  | "decoder-session-limit"
  | "context-lost";

/** Caller-configured browser resource limits; both must be positive integers. */
export interface BrowserResourceBudget {
  /** GPU texture memory the host allows, in bytes. */
  readonly textureBytes: number;
  /** Video decoders the host allows open at once. */
  readonly decoderSessions: number;
}

/**
 * Stable facts used by hosts to select WebGPU, WebGL2, and media fallbacks. They hold for the
 * thread that probed them: a worker can get answers the page does not (a browser that withholds
 * WebGPU from the page and WebGL2 from page canvases can still give a worker `OffscreenCanvas`
 * WebGL2), so a worker host's answer comes from `WorkerRenderBackend.detectCapabilities()`.
 */
export interface BrowserCapabilities {
  /**
   * The browser hands out a WebGPU adapter that is not its fallback (software) adapter, so the
   * engine renders through WebGPU.
   */
  readonly webgpu: boolean;
  /**
   * Whether the adapter the browser hands out is its fallback (software) one; `null` when it hands
   * out none.
   */
  readonly webgpuFallbackAdapter: boolean | null;
  /** A WebGL2 context can be made, which the engine renders through without WebGPU. */
  readonly webgl2: boolean;
  /**
   * The WebGL2 context's renderer, unmasked where the browser allows it (`WEBGL_debug_renderer_info`);
   * `null` without a context.
   */
  readonly webgl2Renderer: string | null;
  /**
   * The renderer a render host's `renderer: "auto"` draws with here: `"gpu"` (WebGPU, else WebGL2)
   * when either is available on a hardware adapter, else `"cpu"`, the engine's CPU renderer in
   * wasm. A software rasterizer (SwiftShader, llvmpipe, WARP, a WebGPU fallback adapter) does not
   * count, as the native host's `auto` does not count one.
   */
  readonly renderer: "gpu" | "cpu";
  readonly webcodecs: boolean;
  readonly offscreenCanvas: boolean;
  /**
   * The browser compiles WebAssembly with SIMD and reference types, which the engine, audio
   * worklet, and extension modules all use. When it is `false` none of them loads, and
   * `diagnostics` names each missing feature.
   */
  readonly wasm: boolean;
  readonly diagnostics: readonly BrowserCapabilityDiagnostic[];
}

/**
 * Probes the calling thread's environment (a page, or a worker: see {@link BrowserCapabilities}).
 *
 * `webgpu` asks for an adapter, as the engine does when it picks its backend: a browser can expose
 * `navigator.gpu` and still have no adapter to give (Chrome on Linux without its flag, a
 * blocklisted GPU, acceleration turned off), and then the engine runs on WebGL2; with neither (a
 * browser with no usable GPU, such as Chrome with acceleration off), it runs on the CPU, which
 * `renderer` says and `diagnostics` explains. `webgl2` is probed on an `OffscreenCanvas` of its own,
 * the kind of canvas render hosts draw on (a document canvas only where there is no
 * `OffscreenCanvas`), and never on the caller's: a canvas's first context fixes its kind for good.
 * `wasm` validates
 * two tiny modules, one per WebAssembly feature the packaged modules need past what every WebCodecs
 * browser has (SIMD and reference types; Chrome 94 and 95 have WebCodecs and SIMD but no reference
 * types), so a browser that cannot load the engine is told so before it fetches and compiles it.
 */
export async function detectBrowserCapabilities(): Promise<BrowserCapabilities> {
  const adapter = await detectWebgpu();
  const webgpu = adapter !== null && !adapter.fallback;
  const webgl2Renderer = detectWebgl2();
  const webgl2 = webgl2Renderer !== null;
  const webcodecs =
    globalThis.VideoDecoder !== undefined &&
    globalThis.VideoEncoder !== undefined;
  const offscreenCanvas = globalThis.OffscreenCanvas !== undefined;
  const simd = validatesWasm(WASM_SIMD_PROBE);
  const referenceTypes = validatesWasm(WASM_REFERENCE_TYPES_PROBE);
  const diagnostics: BrowserCapabilityDiagnostic[] = [];
  if (!webgpu) diagnostics.push("webgpu-unavailable");
  if (!webgl2) diagnostics.push("webgl2-unavailable");
  if (!webcodecs) diagnostics.push("webcodecs-unavailable");
  if (!offscreenCanvas) diagnostics.push("offscreen-canvas-unavailable");
  if (!simd) diagnostics.push("wasm-simd-unavailable");
  if (!referenceTypes) diagnostics.push("wasm-reference-types-unavailable");
  const hardwareWebgl2 =
    webgl2Renderer !== null && !softwareRenderer(webgl2Renderer);
  return {
    webgpu,
    webgpuFallbackAdapter: adapter === null ? null : adapter.fallback,
    webgl2,
    webgl2Renderer,
    renderer: webgpu || hardwareWebgl2 ? "gpu" : "cpu",
    webcodecs,
    offscreenCanvas,
    wasm: simd && referenceTypes,
    diagnostics,
  };
}

/** `(func (result v128) i32.const 0 i8x16.splat i8x16.popcnt)`: valid only with final SIMD. */
const WASM_SIMD_PROBE = [
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8,
  0, 65, 0, 253, 15, 253, 98, 11,
];

/** `(func ref.null func drop)`: valid only with reference types. */
const WASM_REFERENCE_TYPES_PROBE = [
  0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0, 10, 7, 1, 5, 0,
  208, 112, 26, 11,
];

/** Whether `WebAssembly.validate` accepts `bytes`; no `WebAssembly` at all accepts nothing. */
function validatesWasm(bytes: readonly number[]): boolean {
  try {
    return globalThis.WebAssembly?.validate(new Uint8Array(bytes)) === true;
  } catch {
    return false;
  }
}

/**
 * Renderer names that mean a software rasterizer, compared without case: wgpu's list for WebGL
 * (`mesa offscreen`, `swiftshader`, `llvmpipe`) and WARP as ANGLE names it. The engine's native and
 * wasm adapter selection decide by the same list.
 */
const SOFTWARE_RENDERERS = [
  "mesa offscreen",
  "swiftshader",
  "llvmpipe",
  "microsoft basic render",
];

/** Whether a WebGL renderer or adapter name is a software rasterizer's. */
function softwareRenderer(name: string): boolean {
  const lower = name.toLowerCase();
  return SOFTWARE_RENDERERS.some((software) => lower.includes(software));
}

interface AdapterInfoLike {
  readonly isFallbackAdapter?: boolean;
  readonly vendor?: string;
  readonly architecture?: string;
  readonly description?: string;
}

/**
 * The adapter `navigator.gpu` gives, and whether it is a software one: flagged as the fallback
 * adapter (on its `info`, or on the adapter in older browsers), or named as a software
 * rasterizer. `null` when it gives none; a refused request counts as none.
 */
async function detectWebgpu(): Promise<{ readonly fallback: boolean } | null> {
  const gpu = (
    globalThis.navigator as
      | (Navigator & {
          gpu?: {
            requestAdapter(): Promise<
              | (AdapterInfoLike & { readonly info?: AdapterInfoLike })
              | null
              | undefined
            >;
          };
        })
      | undefined
  )?.gpu;
  if (gpu === undefined) return null;
  try {
    const adapter = await gpu.requestAdapter();
    if (adapter == null) return null;
    const info = adapter.info ?? {};
    const fallback =
      info.isFallbackAdapter === true ||
      adapter.isFallbackAdapter === true ||
      [info.vendor, info.architecture, info.description].some(
        (field) => typeof field === "string" && softwareRenderer(field),
      );
    return { fallback };
  } catch {
    return null;
  }
}

/** `WEBGL_debug_renderer_info.UNMASKED_RENDERER_WEBGL`. */
const UNMASKED_RENDERER = 0x9246;
/** `RENDERER`. */
const RENDERER = 0x1f01;

/**
 * The renderer of a WebGL2 context made on a throwaway `OffscreenCanvas` (a document canvas where
 * there is none), unmasked where the browser allows it; `null` when no context can be made. The
 * probe's context is given back at once, since browsers cap live WebGL contexts.
 */
function detectWebgl2(): string | null {
  let surface: HTMLCanvasElement | OffscreenCanvas;
  if (globalThis.OffscreenCanvas !== undefined)
    surface = new OffscreenCanvas(1, 1);
  else if (typeof document !== "undefined")
    surface = document.createElement("canvas");
  else return null;
  let context: WebGL2RenderingContext | null;
  try {
    context = surface.getContext("webgl2") as WebGL2RenderingContext | null;
  } catch {
    return null;
  }
  if (context == null) return null;
  let renderer = "";
  try {
    const unmasked = context.getExtension("WEBGL_debug_renderer_info");
    const name: unknown = context.getParameter(
      unmasked === null ? RENDERER : UNMASKED_RENDERER,
    );
    if (typeof name === "string") renderer = name;
    context.getExtension("WEBGL_lose_context")?.loseContext();
  } catch {
    // A context that answers no renderer still exists; its name is unknown.
  }
  return renderer;
}

/**
 * Tracks browser lifecycle events against a {@link BrowserResourceBudget} and
 * reports each degradation through `notify`. It owns no player or renderer:
 * the host acts on what it reports.
 *
 * @throws RangeError From the constructor when either budget is not a positive
 *   safe integer.
 */
export class BrowserRobustnessCoordinator {
  readonly #budget: BrowserResourceBudget;
  readonly #notify: (diagnostic: BrowserCapabilityDiagnostic) => void;
  #decoders = 0;
  #visible = true;

  constructor(
    budget: BrowserResourceBudget,
    notify: (diagnostic: BrowserCapabilityDiagnostic) => void,
  ) {
    if (
      !Number.isSafeInteger(budget.textureBytes) ||
      budget.textureBytes <= 0 ||
      !Number.isSafeInteger(budget.decoderSessions) ||
      budget.decoderSessions <= 0
    ) {
      throw new RangeError("browser resource budget must be positive integers");
    }
    this.#budget = budget;
    this.#notify = notify;
  }

  /**
   * Sizes `canvas`'s backing store to its CSS size times `scale` (the device
   * pixel ratio), rounded, at least 1x1.
   *
   * @throws RangeError When the scaled size is not a safe integer.
   */
  resize(
    canvas: HTMLCanvasElement | OffscreenCanvas,
    cssWidth: number,
    cssHeight: number,
    scale = 1,
  ): void {
    const width = Math.max(1, Math.round(cssWidth * scale));
    const height = Math.max(1, Math.round(cssHeight * scale));
    if (!Number.isSafeInteger(width) || !Number.isSafeInteger(height)) {
      throw new RangeError("canvas size is unsupported");
    }
    canvas.width = width;
    canvas.height = height;
  }

  /** Records page visibility; hiding a visible page reports `visibility-suspended`. */
  setVisible(visible: boolean): void {
    if (this.#visible && !visible) this.#notify("visibility-suspended");
    this.#visible = visible;
  }

  /**
   * Reports `memory-pressure` and returns the texture budget to fall back to:
   * half the configured one, at least one byte.
   */
  memoryPressure(): number {
    this.#notify("memory-pressure");
    return Math.max(1, Math.floor(this.#budget.textureBytes / 2));
  }

  /** Reports `context-lost`. */
  contextLost(): void {
    this.#notify("context-lost");
  }

  /**
   * Takes one decoder session. Returns `false`, and reports
   * `decoder-session-limit`, when all of the budget's sessions are taken.
   */
  acquireDecoder(): boolean {
    if (this.#decoders >= this.#budget.decoderSessions) {
      this.#notify("decoder-session-limit");
      return false;
    }
    this.#decoders += 1;
    return true;
  }

  /** Returns a session taken by {@link acquireDecoder}; extra calls are ignored. */
  releaseDecoder(): void {
    this.#decoders = Math.max(0, this.#decoders - 1);
  }
}
