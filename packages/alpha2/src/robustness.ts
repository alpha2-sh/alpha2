/** Observable browser capability/degradation categories. */
export type BrowserCapabilityDiagnostic =
  | "webgpu-unavailable"
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

/** Stable facts used by hosts to select WebGPU, WebGL2, and media fallbacks. */
export interface BrowserCapabilities {
  /** The browser hands out a WebGPU adapter, so the engine renders through WebGPU. */
  readonly webgpu: boolean;
  readonly webgl2: boolean;
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
 * Probes the current environment.
 *
 * `webgpu` asks for an adapter, as the engine does when it picks its backend: a browser can expose
 * `navigator.gpu` and still have no adapter to give (Chrome on Linux without its flag, a
 * blocklisted GPU, acceleration turned off), and then the engine runs on WebGL2. `webgl2` is probed
 * on a canvas of its own, since a canvas's first context fixes its kind for good: a caller's canvas
 * probed for WebGL2 could no longer render WebGPU or be transferred to a worker. `wasm` validates
 * two tiny modules, one per WebAssembly feature the packaged modules need past what every WebCodecs
 * browser has (SIMD and reference types; Chrome 94 and 95 have WebCodecs and SIMD but no reference
 * types), so a browser that cannot load the engine is told so before it fetches and compiles it.
 */
export async function detectBrowserCapabilities(): Promise<BrowserCapabilities> {
  const webgpu = await detectWebgpu();
  const webgl2 = detectWebgl2();
  const webcodecs =
    globalThis.VideoDecoder !== undefined &&
    globalThis.VideoEncoder !== undefined;
  const offscreenCanvas = globalThis.OffscreenCanvas !== undefined;
  const simd = validatesWasm(WASM_SIMD_PROBE);
  const referenceTypes = validatesWasm(WASM_REFERENCE_TYPES_PROBE);
  const diagnostics: BrowserCapabilityDiagnostic[] = [];
  if (!webgpu) diagnostics.push("webgpu-unavailable");
  if (!webcodecs) diagnostics.push("webcodecs-unavailable");
  if (!offscreenCanvas) diagnostics.push("offscreen-canvas-unavailable");
  if (!simd) diagnostics.push("wasm-simd-unavailable");
  if (!referenceTypes) diagnostics.push("wasm-reference-types-unavailable");
  return {
    webgpu,
    webgl2,
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

/** Whether `navigator.gpu` gives an adapter; a refused request counts as none. */
async function detectWebgpu(): Promise<boolean> {
  const gpu = (
    globalThis.navigator as
      (Navigator & { gpu?: { requestAdapter(): Promise<unknown> } }) | undefined
  )?.gpu;
  if (gpu === undefined) return false;
  try {
    return (await gpu.requestAdapter()) != null;
  } catch {
    return false;
  }
}

/**
 * Whether a WebGL2 context can be created on a throwaway DOM canvas, or an `OffscreenCanvas` where
 * there is no DOM.
 */
function detectWebgl2(): boolean {
  const probe = (surface: HTMLCanvasElement | OffscreenCanvas): boolean => {
    try {
      return surface.getContext("webgl2") != null;
    } catch {
      return false;
    }
  };
  if (typeof document !== "undefined") {
    return probe(document.createElement("canvas"));
  }
  if (globalThis.OffscreenCanvas !== undefined) {
    return probe(new OffscreenCanvas(1, 1));
  }
  return false;
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
