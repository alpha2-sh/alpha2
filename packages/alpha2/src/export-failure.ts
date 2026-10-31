/**
 * The shape every alpha2 export failure is reported in, whichever process and backend it came from,
 * and the rules for classifying one.
 *
 * A class is named only on evidence. A refused GPU mapping carries no cause, so it stays
 * `READBACK_FAILED` unless the device itself reported running out of memory or being lost; a
 * JavaScript `RangeError` is an allocation failure only when the engine said so in its own words. A
 * classification that cannot be made is `UNKNOWN`, never a guess.
 */

/**
 * What an export failure is evidence of. `RESOURCE_LIMIT` is a refusal, not a failure: the export
 * would need more than a budget allows, and was stopped before it could exhaust it.
 * `RESOURCE_BUSY` is a refusal that ends: a device other work holds (every hardware encoder
 * session taken), which the same export can have once that work ends.
 */
export type ExportFailureCode =
  | "OUT_OF_MEMORY"
  | "ALLOCATION_FAILED"
  | "DEVICE_LOST"
  | "DEVICE_HUNG"
  | "READBACK_FAILED"
  | "GPU_TIMEOUT"
  | "VALIDATION_FAILED"
  | "INVALID_INPUT"
  | "HOST_EXITED"
  | "OUTPUT_IO_FAILED"
  | "STORAGE_QUOTA_EXCEEDED"
  | "RESOURCE_LIMIT"
  | "RESOURCE_BUSY"
  | "CANCELLED"
  | "UNKNOWN";

/**
 * The export step a failure belongs to. Steps overlap in a pipelined export, so each event names
 * its own step.
 */
export type ExportPhase =
  | "prepare"
  | "decode"
  | "upload"
  | "render"
  | "readback"
  | "encode"
  | "mux"
  | "publish"
  | "download"
  | "cleanup";

/**
 * Which process reported the failure: `node` is `@alpha2-sh/node` and `python` the `alpha2` Python
 * package, each a client of the native host.
 */
export type ExportOrigin = "browser" | "node" | "python" | "native-host";

/**
 * The memory a failure's evidence names. `js` is JavaScript `ArrayBuffer` and object memory, `wasm`
 * the engine's linear memory; neither is the GPU's.
 */
export type MemoryResource = "cpu" | "gpu" | "js" | "wasm" | "unknown";

/** A later event recorded beside a failure without replacing it. */
export interface ExportFailureNote {
  readonly code: ExportFailureCode;
  readonly phase: ExportPhase;
  readonly message: string;
}

/**
 * One classified export failure, serializable as JSON so it crosses worker and process boundaries
 * unchanged.
 */
export interface ExportFailure {
  readonly code: ExportFailureCode;
  readonly message: string;
  readonly origin: ExportOrigin;
  readonly operation: "export";
  /** `browser-webgpu`, `browser-webgl`, `native`, or `unknown`. */
  readonly backend: string;
  readonly phase: ExportPhase;
  /** The step inside the phase, such as a readback's `map-failed`. */
  readonly stage?: string | null;
  readonly frameIndex?: number | null;
  /** Timeline ticks as a decimal string, so no value loses precision. */
  readonly timeTicks?: string | null;
  readonly resource: MemoryResource;
  /** Bytes the failed allocation asked for, when known. */
  readonly requestedBytes?: number | null;
  /** The code the native host's protocol error had. */
  readonly hostCode?: string;
  /** For `RESOURCE_BUSY`: how long to wait before asking for the device again. */
  readonly retryAfterMs?: number;
  readonly jobId?: string;
  readonly attemptId?: string;
  readonly stack?: string;
  /** The last part of the native host's standard error. */
  readonly stderr?: string;
  readonly exitCode?: number | null;
  readonly signal?: string | null;
  /** Device events, memory, paths, and environment recorded up to the failure. */
  readonly diagnostics?: Readonly<Record<string, unknown>>;
  /** Failures that followed the first, such as a cleanup that also failed. */
  readonly secondary?: readonly ExportFailureNote[];
}

/** An error carrying a classified {@link ExportFailure}. */
export class ExportFailureError extends Error {
  readonly failure: ExportFailure;

  constructor(failure: ExportFailure, options?: ErrorOptions) {
    super(failure.message, options);
    this.name = "ExportFailureError";
    this.failure = failure;
  }

  /** The failure's class, for callers that switch on `error.code`. */
  get code(): ExportFailureCode {
    return this.failure.code;
  }
}

/** Where a browser export was when something went wrong. */
export interface BrowserFailureContext {
  readonly phase: ExportPhase;
  readonly backend?: string;
  readonly frameIndex?: number;
  readonly requestedBytes?: number;
  readonly diagnostics?: Readonly<Record<string, unknown>>;
}

const PHASES = new Set<string>([
  "prepare",
  "decode",
  "upload",
  "render",
  "readback",
  "encode",
  "mux",
  "publish",
  "download",
  "cleanup",
]);
const CODES = new Set<string>([
  "OUT_OF_MEMORY",
  "ALLOCATION_FAILED",
  "DEVICE_LOST",
  "DEVICE_HUNG",
  "READBACK_FAILED",
  "GPU_TIMEOUT",
  "VALIDATION_FAILED",
  "INVALID_INPUT",
  "HOST_EXITED",
  "OUTPUT_IO_FAILED",
  "STORAGE_QUOTA_EXCEEDED",
  "RESOURCE_LIMIT",
  "RESOURCE_BUSY",
  "CANCELLED",
  "UNKNOWN",
]);

/** The failure `error` carries, if it carries a well-formed one. */
export function failureOf(error: unknown): ExportFailure | undefined {
  const failure = (error as { failure?: unknown } | null)?.failure;
  if (!isRecord(failure)) return undefined;
  if (
    typeof failure.code !== "string" ||
    !CODES.has(failure.code) ||
    typeof failure.phase !== "string" ||
    !PHASES.has(failure.phase)
  )
    return undefined;
  return failure as unknown as ExportFailure;
}

/**
 * Classifies an error a browser export threw at `context`.
 *
 * An error that already carries a complete failure (one with an `origin`) is returned as it is.
 * An engine readback error arrives with a partial `failure` of its own (the class the device
 * evidence supports, the stage, the frame), as does an upload the export's working set does not
 * fit (`RESOURCE_LIMIT` at stage `working-set-over-budget`, with the bytes it needs), and keeps it;
 * anything else is classified here from its type and the platform's own words.
 */
export function classifyBrowserFailure(
  error: unknown,
  context: BrowserFailureContext,
): ExportFailure {
  const existing = failureOf(error);
  if (existing !== undefined && existing.origin !== undefined) return existing;
  const partial = (error as { failure?: unknown } | null)?.failure;
  const engine = isRecord(partial) ? partial : undefined;
  const message = messageOf(error);
  const base = {
    message,
    origin: "browser" as const,
    operation: "export" as const,
    backend: context.backend ?? "unknown",
    phase:
      typeof engine?.phase === "string" && PHASES.has(engine.phase)
        ? (engine.phase as ExportPhase)
        : context.phase,
    frameIndex:
      typeof engine?.frameIndex === "number"
        ? engine.frameIndex
        : (context.frameIndex ?? null),
    ...(typeof engine?.timeTicks === "string"
      ? { timeTicks: engine.timeTicks }
      : {}),
    ...(typeof engine?.stage === "string" ? { stage: engine.stage } : {}),
    ...(typeof engine?.requestedBytes === "number"
      ? { requestedBytes: engine.requestedBytes }
      : context.requestedBytes === undefined
        ? {}
        : { requestedBytes: context.requestedBytes }),
    ...(error instanceof Error && error.stack
      ? { stack: error.stack.slice(0, 4_096) }
      : {}),
    diagnostics: {
      ...(context.diagnostics ?? {}),
      ...(isRecord(engine?.gpu) ? { gpu: engine.gpu } : {}),
    },
  };
  const [code, resource] = browserClass(error, message, base.phase, engine);
  return { ...base, code, resource };
}

/** A failure class and the memory region its evidence names. */
export interface FailureClass {
  readonly code: ExportFailureCode;
  readonly resource: MemoryResource;
}

const permanent: FailureClass = { code: "INVALID_INPUT", resource: "unknown" };
const unknown: FailureClass = { code: "UNKNOWN", resource: "unknown" };

/**
 * The class each error code is evidence of on its own: the native host protocol's codes, which
 * `alpha2-wasm` shares, as the host's own `classify_failure` reads them, the codes only
 * `@alpha2-sh/node` makes, and the browser SDK's own decode refusals. `tests/cases/failure-classes.json` holds the same table, and the host's tests and
 * these read it, so the two cannot drift.
 */
const CODE_CLASSES: Readonly<Record<string, FailureClass>> = Object.freeze({
  // The request would meet the same refusal again.
  UNSUPPORTED_PROTOCOL: permanent,
  INVALID_REQUEST: permanent,
  INVALID_INPUT: permanent,
  CONFORM_FAILED: permanent,
  MISSING_INPUT: permanent,
  INCOMPLETE_COMPOSITION: permanent,
  EFFECT_ERROR: permanent,
  HARDWARE_DECODE_UNAVAILABLE: permanent,
  COPY_INELIGIBLE: permanent,
  GPU_OUTPUT_UNAVAILABLE: permanent,
  OUTPUT_IN_USE: permanent,
  GPU_DEVICE_LOST: { code: "DEVICE_LOST", resource: "gpu" },
  // The export's own submission ran long enough to stop the GPU.
  GPU_DEVICE_HUNG: { code: "DEVICE_HUNG", resource: "gpu" },
  GPU_TIMEOUT: { code: "GPU_TIMEOUT", resource: "gpu" },
  // A stuck decoder or encoder; the host names the region when it knows the session was a hardware
  // one.
  DECODE_STALLED: { code: "GPU_TIMEOUT", resource: "unknown" },
  ENCODE_STALLED: { code: "GPU_TIMEOUT", resource: "unknown" },
  IO_ERROR: { code: "OUTPUT_IO_FAILED", resource: "unknown" },
  // The output rendered in full and is kept beside its path; moving it into place failed.
  OUTPUT_PUBLISH_FAILED: { code: "OUTPUT_IO_FAILED", resource: "unknown" },
  CANCELLED: { code: "CANCELLED", resource: "unknown" },
  RESOURCE_LIMIT: { code: "RESOURCE_LIMIT", resource: "unknown" },
  GPU_INIT_FAILED: unknown,
  ENCODE_ERROR: unknown,
  // Every session of the hardware encoder is held by other exports; it frees up when they end.
  ENCODER_BUSY: { code: "RESOURCE_BUSY", resource: "gpu" },
  INTERNAL: unknown,
  ALPHA2_ERROR: unknown,
  // @alpha2-sh/node's own: a host it cannot run is setup that a retry does not change.
  HOST_NOT_FOUND: permanent,
  HOST_NOT_EXECUTABLE: permanent,
  // The loader refused the host (a missing or mismatched FFmpeg): the machine's setup again.
  HOST_LOAD_FAILED: permanent,
  HOST_START_FAILED: unknown,
  // No first line in time: a loaded machine may start the host late, so a retry may work.
  HOST_START_TIMEOUT: unknown,
  // macOS would hold the host on an approval prompt, and Windows refused it by policy: setup.
  HOST_QUARANTINED: permanent,
  HOST_BLOCKED: permanent,
  HOST_WRITE_FAILED: unknown,
  HOST_EXITED: { code: "HOST_EXITED", resource: "unknown" },
  // The kernel's out-of-memory killer ended the host (its cgroup's `oom_kill` count rose): a retry
  // with a smaller `memory.scale` asks for less.
  HOST_OOM_KILLED: { code: "OUT_OF_MEMORY", resource: "cpu" },
  INVALID_RESPONSE: unknown,
  PROGRESS_CALLBACK_FAILED: unknown,
  ABORTED: { code: "CANCELLED", resource: "unknown" },
  // The browser SDK's own: a track this browser cannot decode, or decodes only to frames the
  // engine cannot read, fails the same way in every attempt.
  DECODE_UNAVAILABLE: permanent,
  PLANAR_DECODE_UNAVAILABLE: permanent,
  NON_PLANAR_FRAME: permanent,
  FRAME_SUPPLY_FAILED: unknown,
  SOFTWARE_DECODE_REQUIRED: unknown,
});

/**
 * The class `code`, a protocol or `@alpha2-sh/node` error code, is evidence of on its own; `UNKNOWN`
 * for any other.
 */
export function failureClassOfCode(code: unknown): FailureClass {
  return (
    (typeof code === "string" && Object.hasOwn(CODE_CLASSES, code)
      ? CODE_CLASSES[code]
      : undefined) ?? unknown
  );
}

/** What is known where a coded error with no failure of its own is reported. */
export interface CodeFailureContext {
  readonly message: string;
  readonly origin: ExportOrigin;
  readonly phase: ExportPhase;
  readonly backend?: string;
  readonly frameIndex?: number | null;
  readonly stderr?: string;
  readonly exitCode?: number | null;
  readonly signal?: string | null;
  readonly diagnostics?: Readonly<Record<string, unknown>>;
}

/**
 * The failure an error with protocol or `@alpha2-sh/node` code `code` and no failure of its own
 * reports: classified by the code (see {@link failureClassOfCode}), which it keeps as `hostCode`;
 * `UNKNOWN` without one. A process boundary that receives such an error (`@alpha2-sh/node`, a service
 * relaying it) classifies it with this, never by its own default.
 */
export function failureForCode(
  code: string | undefined,
  context: CodeFailureContext,
): ExportFailure {
  const { code: failureCode, resource } = failureClassOfCode(code);
  return {
    code: failureCode,
    message: context.message,
    origin: context.origin,
    operation: "export",
    backend: context.backend ?? "unknown",
    phase: context.phase,
    resource,
    ...(code === undefined ? {} : { hostCode: code }),
    ...(context.frameIndex === undefined
      ? {}
      : { frameIndex: context.frameIndex }),
    ...(context.stderr === undefined ? {} : { stderr: context.stderr }),
    ...(context.exitCode === undefined ? {} : { exitCode: context.exitCode }),
    ...(context.signal === undefined ? {} : { signal: context.signal }),
    ...(context.diagnostics === undefined
      ? {}
      : { diagnostics: context.diagnostics }),
  };
}

function browserClass(
  error: unknown,
  message: string,
  phase: ExportPhase,
  engine: Record<string, unknown> | undefined,
  depth = 0,
): [ExportFailureCode, MemoryResource] {
  if (typeof engine?.code === "string" && CODES.has(engine.code))
    return [
      engine.code as ExportFailureCode,
      engine.resource === "gpu" ? "gpu" : "unknown",
    ];
  const name = (error as { name?: unknown } | null)?.name;
  const code = (error as { code?: unknown } | null)?.code;
  if (
    name === "AbortError" ||
    code === "cancelled" ||
    (name === "ExportAbortedError" && code === "cancelled")
  )
    return ["CANCELLED", "unknown"];
  if (code === "device-lost") return ["DEVICE_LOST", "gpu"];
  if (code === "device-hung") return ["DEVICE_HUNG", "gpu"];
  // An error that knows its own class names it.
  const own = (error as { failureCode?: unknown } | null)?.failureCode;
  if (typeof own === "string" && CODES.has(own))
    return [own as ExportFailureCode, "unknown"];
  const coded = failureClassOfCode(code);
  if (coded.code !== "UNKNOWN") return [coded.code, coded.resource];
  // An error that wraps a classified one (a frame's failure around the decode refusal that caused
  // it) is that one: its cause is read, one level down and no further.
  const cause = (error as { cause?: unknown } | null)?.cause;
  if (depth === 0 && cause !== undefined && cause !== null) {
    const partial = (cause as { failure?: unknown }).failure;
    const inner = browserClass(
      cause,
      messageOf(cause),
      phase,
      isRecord(partial) ? partial : undefined,
      1,
    );
    if (inner[0] !== "UNKNOWN") return inner;
  }
  // SpiderMonkey reports running out of memory by throwing this string, not an error.
  if (error === "out of memory") return ["ALLOCATION_FAILED", "js"];
  // A worker relays the name, not the type: `RangeError` by name is the same evidence.
  if (error instanceof RangeError || name === "RangeError") {
    // V8's and JavaScriptCore's words for a backing store they could not allocate, as opposed to
    // a length they refused.
    if (/array buffer allocation failed/i.test(message))
      return ["ALLOCATION_FAILED", "js"];
    if (message === "Out of memory" || message.startsWith("Out of memory: "))
      return ["ALLOCATION_FAILED", "js"];
    if (/webassembly\.memory|wasm memory|memory\.grow/i.test(message))
      return ["ALLOCATION_FAILED", "wasm"];
  }
  if (name === "QuotaExceededError")
    // A codec reclaimed under memory pressure reports the same name as a full disk; only where the
    // export was tells them apart.
    return phase === "publish" || phase === "mux"
      ? ["STORAGE_QUOTA_EXCEEDED", "unknown"]
      : ["ALLOCATION_FAILED", "unknown"];
  if (
    (phase === "publish" || phase === "mux") &&
    (name === "NotAllowedError" ||
      name === "NotReadableError" ||
      name === "InvalidStateError" ||
      name === "NoModificationAllowedError")
  )
    return ["OUTPUT_IO_FAILED", "unknown"];
  return ["UNKNOWN", "unknown"];
}

/**
 * Records `later` beside `failure` without replacing it: the first failure is the cause. Returns a
 * new failure that keeps the latest 8 secondary notes; `failure` is not modified.
 */
export function withSecondaryFailure(
  failure: ExportFailure,
  later: ExportFailureNote,
): ExportFailure {
  return {
    ...failure,
    secondary: [...(failure.secondary ?? []), later].slice(-8),
  };
}

/**
 * Records that cleaning up after `error` failed too, without replacing it: an export runner calls
 * this when its sink's `abort` throws, then rethrows `error` itself.
 *
 * An {@link ExportFailureError} gains a `cleanup` note in its `failure` (see
 * {@link withSecondaryFailure}), classified from `cleanup`; the error stays the same object, so a
 * caller comparing it with an abort signal's reason still finds it. Any other error has no
 * classified failure to note it in, and the cleanup failure is dropped: the first failure is the
 * cause.
 */
export function recordCleanupFailure(error: unknown, cleanup: unknown): void {
  if (!(error instanceof ExportFailureError)) return;
  const note: ExportFailureNote = {
    code: classifyBrowserFailure(cleanup, { phase: "cleanup" }).code,
    phase: "cleanup",
    message: messageOf(cleanup),
  };
  (error as { failure: ExportFailure }).failure = withSecondaryFailure(
    error.failure,
    note,
  );
}

function messageOf(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
