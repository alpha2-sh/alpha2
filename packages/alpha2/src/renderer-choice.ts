import type {
  WasmCanvasRenderer,
  WasmEffectRegistryBinding,
} from "./effects.js";

/**
 * Renderer selection for a browser render host.
 *
 * - `"auto"` uses a hardware GPU (WebGPU, else WebGL2) and falls back to the wasm CPU renderer when
 *   only a software adapter or none is available.
 * - `"gpu"` uses any GPU adapter, software included, and never falls back.
 * - `"cpu"` uses the CPU renderer and opens no GPU context.
 */
export type RendererMode = "auto" | "gpu" | "cpu";

/** The renderer a render host draws with, in the same shape the native host reports. */
export interface RendererChoice {
  readonly renderer: "gpu" | "cpu";
  /** Why it draws on the CPU (no hardware GPU, asked for, or GPU loss); `null` on a GPU. */
  readonly rendererReason: string | null;
  readonly backend: "webgpu" | "webgl2" | "cpu";
  /** The browser's adapter name; `null` on the CPU or when the browser names none. */
  readonly adapter: string | null;
}

export type RendererSurface = OffscreenCanvas | HTMLCanvasElement;

/**
 * Creates the renderer `mode` asks for on `surface` and reports which one it is.
 *
 * `"auto"` falls back to the CPU renderer on the same surface when the GPU renderer fails with
 * `GPU_INIT_FAILED` or `INVALID_INPUT`. A failed GPU attempt leaves the surface free for the CPU
 * renderer's 2D context.
 *
 * Rejects when the binding lacks the constructor `mode` needs, or with the GPU's error when `"gpu"`
 * fails, `"auto"` fails for another reason, or the CPU fallback also fails.
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
      // Report the GPU's failure, which explains why there is no renderer at all.
      throw error;
    }
  }
}

class MissingConstructorError extends Error {}

/**
 * Creates a GPU renderer on `surface`. Rejects as the constructor does, or when the binding has no
 * constructor for the surface's kind.
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

/** The choice a GPU renderer reports; a binding that names no backend is WebGPU. */
export function gpuChoice(wasm: WasmCanvasRenderer): RendererChoice {
  return {
    renderer: "gpu",
    rendererReason: null,
    backend: wasm.backend ?? "webgpu",
    adapter: wasm.adapter ? wasm.adapter : null,
  };
}
