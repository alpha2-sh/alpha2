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
  readonly textureBytes: number;
  /** Video decoders allowed open at once. */
  readonly decoderSessions: number;
}

/**
 * Facts hosts use to select WebGPU, WebGL2 and media fallbacks. They hold only for the probing
 * thread: a worker can get different answers than the page, so worker hosts should use
 * `WorkerRenderBackend.detectCapabilities()`.
 */
export interface BrowserCapabilities {
  /** A non-fallback (hardware) WebGPU adapter is available. */
  readonly webgpu: boolean;
  /** Whether the WebGPU adapter is the software fallback; `null` when there is none. */
  readonly webgpuFallbackAdapter: boolean | null;
  readonly webgl2: boolean;
  /** The WebGL2 renderer name, unmasked where allowed; `null` without a context. */
  readonly webgl2Renderer: string | null;
  /**
   * What `renderer: "auto"` would draw with here. Software rasterizers (SwiftShader, llvmpipe,
   * WARP, a WebGPU fallback adapter) count as `"cpu"`.
   */
  readonly renderer: "gpu" | "cpu";
  readonly webcodecs: boolean;
  readonly offscreenCanvas: boolean;
  /**
   * WebAssembly SIMD and reference types are supported. Without them neither the engine, the audio
   * worklet, nor extension modules load; `diagnostics` names the missing feature.
   */
  readonly wasm: boolean;
  readonly diagnostics: readonly BrowserCapabilityDiagnostic[];
}

/**
 * Probes the calling thread's environment (see {@link BrowserCapabilities}).
 *
 * `webgpu` requests an adapter, since `navigator.gpu` can exist without one. `webgl2` is probed on
 * a throwaway canvas because a canvas's first context fixes its kind. `wasm` validates tiny probe
 * modules, so it is cheap to check before fetching the engine.
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

// `(func (result v128) i32.const 0 i8x16.splat i8x16.popcnt)`, valid only with final SIMD.
const WASM_SIMD_PROBE = [
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8,
  0, 65, 0, 253, 15, 253, 98, 11,
];

// `(func ref.null func drop)`, valid only with reference types.
const WASM_REFERENCE_TYPES_PROBE = [
  0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0, 10, 7, 1, 5, 0,
  208, 112, 26, 11,
];

function validatesWasm(bytes: readonly number[]): boolean {
  try {
    return globalThis.WebAssembly?.validate(new Uint8Array(bytes)) === true;
  } catch {
    return false;
  }
}

// Case-insensitive; must match the engine's native and wasm adapter selection list.
const SOFTWARE_RENDERERS = [
  "mesa offscreen",
  "swiftshader",
  "llvmpipe",
  "microsoft basic render",
];

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

// Older browsers flag `isFallbackAdapter` on the adapter rather than its `info`.
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

const UNMASKED_RENDERER = 0x9246;
const RENDERER = 0x1f01;

// The probe context is lost at once because browsers cap live WebGL contexts.
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
    // The context exists even if its name is unknown.
  }
  return renderer;
}

/**
 * Tracks browser lifecycle events against a {@link BrowserResourceBudget} and reports each
 * degradation through `notify`. The host acts on what it reports.
 *
 * @throws RangeError From the constructor when either budget is not a positive safe integer.
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
   * Sizes `canvas`'s backing store to its CSS size times `scale` (device pixel ratio), min 1x1.
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

  /** Reports `memory-pressure` and returns the reduced texture budget (half, at least 1 byte). */
  memoryPressure(): number {
    this.#notify("memory-pressure");
    return Math.max(1, Math.floor(this.#budget.textureBytes / 2));
  }

  contextLost(): void {
    this.#notify("context-lost");
  }

  /** Takes one decoder session, or reports `decoder-session-limit` and returns `false`. */
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
