/**
 * What a browser render host does after it loses its GPU: a pure decision, owned by the page side
 * (the player, an editor's preview), shared by every host of the page.
 *
 * A browser resets its GPU process when a device is lost, and stops giving a page GPU access after
 * a few resets: after the second, Chrome withholds WebGPU from the page and WebGL2 from page
 * canvases (WebGL2 on an `OffscreenCanvas` stays), and after the third nothing GPU-backed works in
 * the browser. A loss the page's own work caused (a submission that ran into the GPU watchdog)
 * repeats on the GPU. So the ladder tries the GPU again only for a first, external loss, and moves
 * to the engine's CPU renderer, on a fresh surface, once the loss was the page's own, recurred at
 * the same frame, or the page has seen two losses. Each rung is tried once per loss.
 */

/** What a host knows about a GPU loss, read before the lost renderer is freed. */
export interface GpuLossReport {
  /**
   * The longest recent submission ran for at least half the GPU watchdog: the loss was most likely
   * this work's own, and the same work on the GPU stops it again.
   */
  readonly hung: boolean;
  /** The export frame index the longest submission drew, when it drew one. */
  readonly frameIndex: number | null;
  /** The timeline time the longest submission drew, when known. */
  readonly timeTicks: bigint | null;
  /** How long the longest recent submission ran on the GPU. */
  readonly gpuMs: number | null;
  /** One line for the user. */
  readonly message: string;
}

/** What a host draws on after a loss, and on which surface. */
export interface RecoveryStep {
  /** The GPU (WebGPU, else WebGL2, as the browser now offers) or the CPU renderer. */
  readonly renderer: "gpu" | "cpu";
  /**
   * `"same"`: the canvas the lost renderer drew on, possible only for a WebGPU-bound canvas while
   * the browser still offers a WebGPU adapter. `"fresh"`: a new canvas, since a canvas keeps its
   * first context kind for good.
   */
  readonly surface: "same" | "fresh";
  /** Why this rung, for diagnostics. */
  readonly reason: string;
}

/** The lost renderer's surface, as the host found it after the loss. */
export interface SurfaceProbe {
  /** The context kind the surface is bound to; `"none"` when no renderer held it. */
  readonly kind: "webgpu" | "webgl2" | "cpu" | "none";
  /** Whether `navigator.gpu.requestAdapter()` gives an adapter in the host's own context now. */
  readonly webgpuAdapter: boolean;
}

/** The page's count of GPU losses; see {@link pageGpuLosses}. */
export interface PageGpuLosses {
  /** Losses counted in this JavaScript realm so far. */
  count(): number;
  /** Counts one loss. */
  add(): void;
}

const LOSSES = Symbol.for("alpha2.pageGpuLosses");

/**
 * GPU losses seen in this JavaScript realm (a page, or a worker), counted on `globalThis` so that
 * two copies of this package in one page share it. {@link GpuRecoveryLadder.onLoss} counts each loss
 * it decides on; a host that sees a loss outside a ladder (an export with its own retry policy)
 * calls `add()` itself, and passes `count()` as the retry policy's `pageGpuLosses`. Losses in other
 * tabs, or in a worker's realm, are not seen here.
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

/** The order recovery moves down in after a failed rung. */
const RUNGS: readonly RecoveryStep["renderer"][] = ["gpu", "gpu", "cpu"];
const RUNG_SURFACES: readonly RecoveryStep["surface"][] = [
  "same",
  "fresh",
  "fresh",
];

/**
 * Decides recovery after each GPU loss of one host. Once it has moved to the CPU it stays there:
 * {@link GpuRecoveryLadder.renderer} says which renderer later rebuilds of the host use.
 */
export class GpuRecoveryLadder {
  readonly #maxGpuLosses: number;
  readonly #losses: PageGpuLosses;
  /** Rungs tried since the last loss, by index into `RUNGS`. */
  #tried = new Set<number>();
  #lastLossTicks: bigint | null = null;
  #renderer: "gpu" | "cpu" = "gpu";

  /**
   * @param options.maxGpuLosses Page losses after which the GPU is not tried again; 2 by default,
   *   since a browser withdraws GPU access from the page at its next reset.
   * @param options.losses The loss count the ladder adds to and reads; {@link pageGpuLosses} by
   *   default. A test, or a host that knows its losses another way, gives its own.
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

  /** The renderer recovery currently draws on: `"cpu"` once a step moved there, for good. */
  get renderer(): "gpu" | "cpu" {
    return this.#renderer;
  }

  /**
   * Counts the loss (in {@link pageGpuLosses} unless the ladder was given its own count) and decides the first step: the CPU when the loss was
   * the work's own (`hung`), when it recurred at the time of this ladder's previous loss, or when the
   * page has seen `maxGpuLosses` losses; otherwise the GPU, on the same surface when it is
   * WebGPU-bound and the host still gets a WebGPU adapter, else on a fresh one.
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
   * The next step after `step` failed with `error`: the GPU on the same surface, then the GPU on a
   * fresh one, then the CPU on a fresh one, skipping rungs already tried for this loss; after the
   * CPU, `"terminal"`.
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
    // Every rung below this one counts as passed: recovery never climbs back up.
    for (let below = 0; below < index; below += 1) this.#tried.add(below);
    const renderer = RUNGS[index];
    if (renderer === "cpu") this.#renderer = "cpu";
    return Object.freeze({ renderer, surface: RUNG_SURFACES[index], reason });
  }
}

/**
 * The {@link GpuLossReport} of a wasm renderer's `lossReport` JSON, or a plain lost-device report
 * with `fallbackMessage` when the renderer gives none.
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
