/**
 * The export failure shape shared by every process and backend, and the rules for classifying one.
 *
 * A class is assigned only on evidence; otherwise the failure is `UNKNOWN`. For example, a refused
 * GPU mapping stays `READBACK_FAILED` unless the device reported memory exhaustion or loss.
 */

/**
 * Failure class. `RESOURCE_LIMIT` means the export was stopped before exceeding a budget.
 * `RESOURCE_BUSY` means a device is held by other work (for example all hardware encoder
 * sessions) and may free up later.
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

/** Export step a failure belongs to. Steps overlap in a pipelined export. */
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

/** Reporting process. `node` is `@alpha2-sh/node` and `python` the `alpha2` Python package. */
export type ExportOrigin = "browser" | "node" | "python" | "native-host";

/** Memory named by the evidence. `js` is the JavaScript heap; `wasm` the engine's linear memory. */
export type MemoryResource = "cpu" | "gpu" | "js" | "wasm" | "unknown";

/** A later event recorded alongside a failure without replacing it. */
export interface ExportFailureNote {
  readonly code: ExportFailureCode;
  readonly phase: ExportPhase;
  readonly message: string;
}

/** A classified export failure. JSON-serializable so it crosses worker and process boundaries. */
export interface ExportFailure {
  readonly code: ExportFailureCode;
  readonly message: string;
  readonly origin: ExportOrigin;
  readonly operation: "export";
  /** `browser-webgpu`, `browser-webgl`, `native`, or `unknown`. */
  readonly backend: string;
  readonly phase: ExportPhase;
  /** Step within the phase, such as a readback's `map-failed`. */
  readonly stage?: string | null;
  readonly frameIndex?: number | null;
  /** Timeline ticks as a decimal string to keep full precision. */
  readonly timeTicks?: string | null;
  readonly resource: MemoryResource;
  /** Bytes the failed allocation asked for, when known. */
  readonly requestedBytes?: number | null;
  /** Original native host protocol error code. */
  readonly hostCode?: string;
  /** For `RESOURCE_BUSY`, how long to wait before retrying. */
  readonly retryAfterMs?: number;
  readonly jobId?: string;
  readonly attemptId?: string;
  readonly stack?: string;
  /** Tail of the native host's standard error. */
  readonly stderr?: string;
  readonly exitCode?: number | null;
  readonly signal?: string | null;
  /** Device events, memory, paths, and environment recorded up to the failure. */
  readonly diagnostics?: Readonly<Record<string, unknown>>;
  /** Later failures, such as a failed cleanup. */
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

  get code(): ExportFailureCode {
    return this.failure.code;
  }
}

/** Browser export state at the time of a failure. */
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

/** The well-formed failure `error` carries, if any. */
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
 * A complete failure (one with an `origin`) is returned as is. A partial engine `failure` (such as
 * a readback error or `RESOURCE_LIMIT` at `working-set-over-budget`) keeps its fields. Anything
 * else is classified from its type and message.
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

export interface FailureClass {
  readonly code: ExportFailureCode;
  readonly resource: MemoryResource;
}

const permanent: FailureClass = { code: "INVALID_INPUT", resource: "unknown" };
const unknown: FailureClass = { code: "UNKNOWN", resource: "unknown" };

/**
 * Failure class of each error code: native host protocol codes (shared by `alpha2-wasm`), codes
 * only `@alpha2-sh/node` produces, and browser decode refusals. Must match the host's
 * `classify_failure`; `tests/cases/failure-classes.json` checks both.
 */
const CODE_CLASSES: Readonly<Record<string, FailureClass>> = Object.freeze({
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
  GPU_DEVICE_HUNG: { code: "DEVICE_HUNG", resource: "gpu" },
  GPU_TIMEOUT: { code: "GPU_TIMEOUT", resource: "gpu" },
  DECODE_STALLED: { code: "GPU_TIMEOUT", resource: "unknown" },
  ENCODE_STALLED: { code: "GPU_TIMEOUT", resource: "unknown" },
  IO_ERROR: { code: "OUTPUT_IO_FAILED", resource: "unknown" },
  OUTPUT_PUBLISH_FAILED: { code: "OUTPUT_IO_FAILED", resource: "unknown" },
  CANCELLED: { code: "CANCELLED", resource: "unknown" },
  RESOURCE_LIMIT: { code: "RESOURCE_LIMIT", resource: "unknown" },
  // Never started: the host was retiring, and another host runs it.
  HOST_RETIRING: { code: "HOST_EXITED", resource: "unknown" },
  // An extractor's own output or failure repeats on a retry; a slow client may not.
  INVALID_EMIT: permanent,
  EXTRACTOR_FAILED: permanent,
  EXTRACTOR_TIMEOUT: unknown,
  // A plugin that is refused, does not load or traps does so again; a server may answer later.
  PLUGIN_FETCH_FAILED: unknown,
  PLUGIN_HASH_MISMATCH: permanent,
  PLUGIN_ABI_UNSUPPORTED: permanent,
  PLUGIN_NO_ARTIFACT: permanent,
  PLUGIN_NATIVE_REFUSED: permanent,
  PLUGIN_DESCRIPTOR_MISMATCH: permanent,
  PLUGIN_UNSUPPORTED_FEATURE: permanent,
  PLUGIN_LOAD_FAILED: permanent,
  PLUGIN_TRAPPED: permanent,
  PLUGIN_LIMIT: { code: "RESOURCE_LIMIT", resource: "unknown" },
  GPU_INIT_FAILED: unknown,
  ENCODE_ERROR: unknown,
  ENCODER_BUSY: { code: "RESOURCE_BUSY", resource: "gpu" },
  INTERNAL: unknown,
  ALPHA2_ERROR: unknown,
  // `@alpha2-sh/node` setup errors that a retry does not fix.
  HOST_NOT_FOUND: permanent,
  HOST_NOT_EXECUTABLE: permanent,
  HOST_LOAD_FAILED: permanent,
  HOST_START_FAILED: unknown,
  // A loaded machine may start the host late, so a retry may work.
  HOST_START_TIMEOUT: unknown,
  HOST_QUARANTINED: permanent,
  HOST_BLOCKED: permanent,
  HOST_WRITE_FAILED: unknown,
  HOST_EXITED: { code: "HOST_EXITED", resource: "unknown" },
  // The cgroup's `oom_kill` count rose; a retry with a smaller `memory.scale` may fit.
  HOST_OOM_KILLED: { code: "OUT_OF_MEMORY", resource: "cpu" },
  INVALID_RESPONSE: unknown,
  PROGRESS_CALLBACK_FAILED: unknown,
  ABORTED: { code: "CANCELLED", resource: "unknown" },
  // Browser decode refusals repeat on every attempt.
  DECODE_UNAVAILABLE: permanent,
  PLANAR_DECODE_UNAVAILABLE: permanent,
  NON_PLANAR_FRAME: permanent,
  FRAME_SUPPLY_FAILED: unknown,
  SOFTWARE_DECODE_REQUIRED: unknown,
});

/** Failure class of a protocol or `@alpha2-sh/node` error code; `UNKNOWN` for any other. */
export function failureClassOfCode(code: unknown): FailureClass {
  return (
    (typeof code === "string" && Object.hasOwn(CODE_CLASSES, code)
      ? CODE_CLASSES[code]
      : undefined) ?? unknown
  );
}

/** Context for {@link failureForCode}. */
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
 * Builds a failure for an error that has only a protocol or `@alpha2-sh/node` code, classified by
 * {@link failureClassOfCode} and keeping the code as `hostCode`. Process boundaries relaying such
 * errors should classify with this rather than their own defaults.
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
  const own = (error as { failureCode?: unknown } | null)?.failureCode;
  if (typeof own === "string" && CODES.has(own))
    return [own as ExportFailureCode, "unknown"];
  const coded = failureClassOfCode(code);
  if (coded.code !== "UNKNOWN") return [coded.code, coded.resource];
  // A wrapper takes its cause's class, one level deep.
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
  // SpiderMonkey throws this string, not an error, when out of memory.
  if (error === "out of memory") return ["ALLOCATION_FAILED", "js"];
  // A worker relays the name, not the type.
  if (error instanceof RangeError || name === "RangeError") {
    // V8 and JavaScriptCore messages for a failed backing-store allocation, as opposed to an
    // invalid length.
    if (/array buffer allocation failed/i.test(message))
      return ["ALLOCATION_FAILED", "js"];
    if (message === "Out of memory" || message.startsWith("Out of memory: "))
      return ["ALLOCATION_FAILED", "js"];
    if (/webassembly\.memory|wasm memory|memory\.grow/i.test(message))
      return ["ALLOCATION_FAILED", "wasm"];
  }
  if (name === "QuotaExceededError")
    // A codec reclaimed under memory pressure reports the same name as a full disk; only the
    // phase tells them apart.
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
 * Returns a copy of `failure` with `later` appended to its secondary notes (latest 8 kept). The
 * first failure remains the cause.
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
 * Records a cleanup failure on `error` without replacing it. An {@link ExportFailureError} gains a
 * `cleanup` note in place, so identity comparisons with an abort reason still hold. Other errors
 * are left unchanged and the cleanup failure is dropped.
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
