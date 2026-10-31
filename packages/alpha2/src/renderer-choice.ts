import type {
  WasmCanvasRenderer,
  WasmEffectRegistryBinding,
} from "./effects.js";

/**
 * The renderer a browser render host is asked for. `"auto"` draws on a hardware GPU, through WebGPU
 * or else WebGL2, and on the engine's CPU renderer in wasm when the browser offers neither or only
 * a software adapter (SwiftShader, llvmpipe, WARP, a WebGPU fallback adapter), as the native host's
 * `auto` does; `"gpu"` draws on whatever GPU adapter the browser gives, software ones included, and
 * never falls back; `"cpu"` draws on the CPU and opens no GPU context.
 */
export type RendererMode = "auto" | "gpu" | "cpu";

/** The renderer a render host draws with, as the native host reports it. */
export interface RendererChoice {
  /** The renderer that draws the frames. */
  readonly renderer: "gpu" | "cpu";
  /**
   * Why it draws on the CPU: `"auto"` found no hardware GPU (the reason names a refused software
   * adapter), `"cpu"` was asked for, or a GPU loss moved it there. `null` when it draws on a GPU.
   */
  readonly rendererReason: string | null;
  /** The graphics API: `"webgpu"`, `"webgl2"`, or `"cpu"` for the CPU renderer. */
  readonly backend: "webgpu" | "webgl2" | "cpu";
  /** The GPU adapter as the browser names it; `null` on the CPU or when the browser names none. */
  readonly adapter: string | null;
}

/** A surface a renderer presents to: a worker's `OffscreenCanvas` or a page's canvas element. */
export type RendererSurface = OffscreenCanvas | HTMLCanvasElement;

/**
 * Makes the renderer `mode` asks for on `surface` and says which it is.
 *
 * `"auto"` asks the binding to refuse software adapters and falls back to the CPU renderer, on the
 * same surface, when the GPU renderer cannot be made for want of a hardware GPU
 * (`GPU_INIT_FAILED`, or a canvas that takes no GPU surface: `INVALID_INPUT`). The binding opens a
 * WebGPU device before it binds the canvas and probes WebGL2 on a canvas of its own, so a refused or
 * failed GPU leaves the surface free for the CPU renderer's 2D context.
 *
 * Rejects when the binding lacks the constructor `mode` needs (`createForCanvas` or
 * `createForHtmlCanvas` for the surface's kind, `createCpu` for `"cpu"`), with the GPU's error when
 * `"gpu"` cannot make one or `"auto"` fails for another reason (a lost WebGL context), and with the
 * GPU's error too when the CPU renderer cannot take the surface either.
 */
export async function createRenderer(
  binding: WasmEffectRegistryBinding,
  surface: RendererSurface,
  mode: RendererMode,
): Promise<{ wasm: WasmCanvasRenderer; choice: RendererChoice }> {
  if (mode !== "auto" && mode !== "gpu" && mode !== "cpu")
    throw new RangeError('renderer must be "auto", "gpu" or "cpu"');
  const cpu = (rendererReason: string) => {
    if (binding.createCpu === undefined)
      throw new Error("wasm binding does not expose createCpu");
    return {
      wasm: binding.createCpu(surface),
      choice: {
        renderer: "cpu",
        rendererReason,
        backend: "cpu",
        adapter: null,
      } as const,
    };
  };
  if (mode === "cpu")
    return cpu('the CPU renderer was asked for (renderer "cpu")');
  try {
    const wasm = await createGpuRenderer(
      binding,
      surface,
      mode === "auto" ? "refuse" : "allow",
    );
    return { wasm, choice: gpuChoice(wasm) };
  } catch (error) {
    const code = (error as { code?: unknown } | null)?.code;
    const missing = error instanceof MissingConstructorError;
    if (
      mode !== "auto" ||
      binding.createCpu === undefined ||
      (!missing && code !== "GPU_INIT_FAILED" && code !== "INVALID_INPUT")
    )
      throw error;
    if (missing) return cpu("the wasm binding has no GPU renderer");
    const message = error instanceof Error ? error.message : String(error);
    try {
      return cpu(
        message.startsWith("the browser offers only the software adapter")
          ? `${message}, which renderer "auto" does not draw on`
          : `the browser offers no WebGPU adapter and no WebGL2 context (${message})`,
      );
    } catch {
      // The GPU's failure is the one to report: it says why there is no renderer at all.
      throw error;
    }
  }
}

/** The binding has no GPU constructor for the surface's kind. */
class MissingConstructorError extends Error {}

/**
 * Makes a GPU renderer on `surface` with the binding's constructor for its kind, refusing software
 * adapters when `softwareAdapter` says so. Rejects as the constructor does, or when the binding has
 * no constructor for the surface's kind.
 */
export async function createGpuRenderer(
  binding: WasmEffectRegistryBinding,
  surface: RendererSurface,
  softwareAdapter: "allow" | "refuse",
): Promise<WasmCanvasRenderer> {
  const html =
    typeof HTMLCanvasElement !== "undefined" &&
    surface instanceof HTMLCanvasElement;
  if (html) {
    if (binding.createForHtmlCanvas === undefined)
      throw new MissingConstructorError(
        "wasm binding does not expose createForHtmlCanvas",
      );
    return binding.createForHtmlCanvas(surface, { softwareAdapter });
  }
  if (binding.createForCanvas === undefined)
    throw new MissingConstructorError(
      "wasm binding does not expose createForCanvas",
    );
  return binding.createForCanvas(surface as OffscreenCanvas, {
    softwareAdapter,
  });
}

/** The choice a GPU renderer `wasm` reports; a binding that does not say is WebGPU. */
export function gpuChoice(wasm: WasmCanvasRenderer): RendererChoice {
  return {
    renderer: "gpu",
    rendererReason: null,
    backend: wasm.backend ?? "webgpu",
    adapter: wasm.adapter ? wasm.adapter : null,
  };
}
