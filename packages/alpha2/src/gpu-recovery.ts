/**
 * Recovery decisions for a browser render host after a GPU loss.
 *
 * Browsers withdraw GPU access from a page after repeated GPU process resets: after the second,
 * Chrome withholds WebGPU and page-canvas WebGL2 (`OffscreenCanvas` WebGL2 stays), and after the
 * third nothing GPU-backed works. A loss caused by the page's own work recurs on the GPU. So the
 * GPU is retried only for a first external loss; otherwise recovery moves to the CPU renderer on a
 * fresh surface. Each rung is tried once per loss.
 */

/** GPU loss details, read before the lost renderer is freed. */
export interface GpuLossReport {
  /**
   * The longest recent submission ran for at least half the GPU watchdog, so the loss was most
   * likely caused by this work and would recur.
   */
  readonly hung: boolean;
  /** Export frame index of the longest submission, when known. */
  readonly frameIndex: number | null;
  /** Timeline time of the longest submission, when known. */
  readonly timeTicks: bigint | null;
  /** GPU time of the longest recent submission. */
  readonly gpuMs: number | null;
  /** One-line message for the user. */
  readonly message: string;
}

/** Renderer and surface to use after a loss. */
export interface RecoveryStep {
  /** `gpu` uses WebGPU, else WebGL2, as the browser now offers. */
  readonly renderer: "gpu" | "cpu";
  /**
   * `"same"` reuses the lost renderer's canvas, possible only for a WebGPU canvas while an adapter
   * is still offered. `"fresh"` needs a new canvas, since a canvas keeps its first context kind.
   */
  readonly surface: "same" | "fresh";
  /** Diagnostic reason. */
  readonly reason: string;
}

/** State of the lost renderer's surface after the loss. */
export interface SurfaceProbe {
  /** Context kind the surface is bound to; `"none"` when no renderer held it. */
  readonly kind: "webgpu" | "webgl2" | "cpu" | "none";
  /** Whether `navigator.gpu.requestAdapter()` now returns an adapter in the host's context. */
  readonly webgpuAdapter: boolean;
}

/** See {@link pageGpuLosses}. */
export interface PageGpuLosses {
  /** Losses counted in this JavaScript realm so far. */
  count(): number;
  add(): void;
}

const LOSSES = Symbol.for("alpha2.pageGpuLosses");

/**
 * GPU losses in this JavaScript realm, stored on `globalThis` so duplicate copies of the package
 * share it. {@link GpuRecoveryLadder.onLoss} counts its own losses; code that handles a loss
 * outside a ladder should call `add()` and pass `count()` as the retry policy's `pageGpuLosses`.
 * Losses in other tabs or realms are not counted.
 */
export const pageGpuLosses: PageGpuLosses = Object.freeze({
  count(): number {
    const holder = globalThis as { [LOSSES]?: number };
    return holder[LOSSES] ?? 0;
  },
  add(): void {
    const holder = globalThis as { [LOSSES]?: number };
    holder[LOSSES] = (holder[LOSSES] ?? 0) + 1;
  },
});

/** Recovery rungs in order; recovery never moves back up. */
const RUNGS: readonly RecoveryStep["renderer"][] = ["gpu", "gpu", "cpu"];
const RUNG_SURFACES: readonly RecoveryStep["surface"][] = [
  "same",
  "fresh",
  "fresh",
];

/**
 * Decides recovery after each GPU loss of one host. Once it moves to the CPU it stays there, as
 * reported by {@link GpuRecoveryLadder.renderer}.
 */
export class GpuRecoveryLadder {
  readonly #maxGpuLosses: number;
  readonly #losses: PageGpuLosses;
  /** Indices into `RUNGS` tried since the last loss. */
  #tried = new Set<number>();
  #lastLossTicks: bigint | null = null;
  #renderer: "gpu" | "cpu" = "gpu";

  /**
   * @param options.maxGpuLosses Page losses after which the GPU is not tried again. Default 2,
   *   since the browser withdraws GPU access at the next reset.
   * @param options.losses Loss counter to use. Default {@link pageGpuLosses}.
   * @throws RangeError When `maxGpuLosses` is not a positive integer.
   */
  constructor(
    options: {
      readonly maxGpuLosses?: number;
      readonly losses?: PageGpuLosses;
    } = {},
  ) {
    const max = options.maxGpuLosses ?? 2;
    if (!Number.isSafeInteger(max) || max < 1)
      throw new RangeError("maxGpuLosses must be a positive integer");
    this.#maxGpuLosses = max;
    this.#losses = options.losses ?? pageGpuLosses;
  }

  /** Current renderer; `"cpu"` is permanent once reached. */
  get renderer(): "gpu" | "cpu" {
    return this.#renderer;
  }

  /**
   * Counts the loss and decides the first step. Moves to the CPU when the loss was `hung`, recurred
   * at the previous loss's time, or the page reached `maxGpuLosses`. Otherwise retries the GPU, on
   * the same surface when it is WebGPU-bound and an adapter is still offered.
   */
  onLoss(report: GpuLossReport, surface: SurfaceProbe): RecoveryStep {
    this.#losses.add();
    this.#tried = new Set();
    const recurred =
      report.timeTicks !== null && report.timeTicks === this.#lastLossTicks;
    this.#lastLossTicks = report.timeTicks;
    if (this.#renderer === "cpu")
      return this.#take(2, "recovery already moved to the CPU renderer");
    if (report.hung)
      return this.#take(
        2,
        `the GPU was lost while this work ran ${report.gpuMs ?? "?"} ms in one submission; the same work on the GPU would stop it again`,
      );
    if (recurred)
      return this.#take(2, "the GPU was lost again at the same frame");
    if (this.#losses.count() >= this.#maxGpuLosses)
      return this.#take(
        2,
        `the page has lost the GPU ${this.#losses.count()} times; the browser withdraws GPU access after another reset`,
      );
    if (surface.kind === "webgpu" && surface.webgpuAdapter)
      return this.#take(
        0,
        "a first GPU loss: the WebGPU canvas takes a new device",
      );
    return this.#take(
      1,
      surface.kind === "webgpu"
        ? "a first GPU loss, and the browser gives no WebGPU adapter now: a fresh canvas takes WebGL2"
        : "a first GPU loss: a WebGL2 canvas keeps its lost context, so a fresh canvas takes the GPU",
    );
  }

  /**
   * Next step after `step` failed: GPU on the same surface, GPU on a fresh one, then CPU on a fresh
   * one, skipping rungs already tried for this loss. `"terminal"` after the CPU.
   */
  onRecoveryFailed(
    step: RecoveryStep,
    error: unknown,
  ): RecoveryStep | "terminal" {
    const failed = RUNGS.findIndex(
      (renderer, index) =>
        renderer === step.renderer && RUNG_SURFACES[index] === step.surface,
    );
    const message = error instanceof Error ? error.message : String(error);
    for (let next = failed + 1; next < RUNGS.length; next += 1) {
      if (this.#tried.has(next)) continue;
      return this.#take(
        next,
        `the ${step.renderer} renderer on the ${step.surface} surface failed (${message})`,
      );
    }
    return "terminal";
  }

  #take(index: number, reason: string): RecoveryStep {
    this.#tried.add(index);
    for (let below = 0; below < index; below += 1) this.#tried.add(below);
    const renderer = RUNGS[index];
    if (renderer === "cpu") this.#renderer = "cpu";
    return Object.freeze({ renderer, surface: RUNG_SURFACES[index], reason });
  }
}

/**
 * Parses a wasm renderer's `lossReport` JSON, or returns a plain lost-device report with
 * `fallbackMessage` when there is none.
 */
export function lossReportFromJson(
  json: string | undefined,
  fallbackMessage = "the GPU device was lost",
): GpuLossReport {
  if (json === undefined)
    return {
      hung: false,
      frameIndex: null,
      timeTicks: null,
      gpuMs: null,
      message: fallbackMessage,
    };
  const value = JSON.parse(json) as {
    hung?: boolean;
    gpuMs?: number | null;
    watchdogMs?: number;
    timeTicks?: string | null;
    frameIndex?: number | null;
  };
  const hung = value.hung === true;
  const gpuMs = typeof value.gpuMs === "number" ? value.gpuMs : null;
  return {
    hung,
    frameIndex: typeof value.frameIndex === "number" ? value.frameIndex : null,
    timeTicks:
      typeof value.timeTicks === "string" ? BigInt(value.timeTicks) : null,
    gpuMs,
    message: hung
      ? `the GPU device hung: one submission ran ${gpuMs} ms against a ${value.watchdogMs} ms watchdog`
      : fallbackMessage,
  };
}
