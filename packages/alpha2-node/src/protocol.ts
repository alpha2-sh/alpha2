/** The version of the framed protocol spoken with `alpha2-host` over stdio. */
export const PROTOCOL_VERSION = 2 as const;

/** An attachment as a frame's header lists it; the bytes follow the header in this order. */
export interface AttachmentInfo {
  /** A media type, such as `image/png`. */
  readonly type: string;
  readonly bytes: number;
}

export function createRequest(
  operation:
    | "capabilities"
    | "export"
    | "conform"
    | "still"
    | "compose"
    | "edit"
    | "extract"
    | "extract.ack"
    | "cancel"
    | "status",
  requestId: string,
  payload: Readonly<Record<string, unknown>> = {},
): Readonly<Record<string, unknown>> {
  return {
    protocolVersion: PROTOCOL_VERSION,
    operation,
    requestId,
    ...payload,
  };
}

export interface ProtocolError {
  /** A non-empty machine-readable code, such as `INVALID_INPUT`. */
  readonly code: string;
  readonly message: string;
  /** Structured detail, such as the conform report of a `CONFORM_FAILED`. */
  readonly details?: unknown;
  /** A failed export's classified report; see `alpha2/export-failure`. */
  readonly failure?: Readonly<Record<string, unknown>>;
}

/**
 * `frames` while encoding; `finalizing` while the output is flushed and closed; `extract` while an
 * extraction pass runs.
 */
export type ProgressPhase = "frames" | "finalizing" | "extract";

/** Progress of the request `requestId`; `completed` never exceeds `total`. */
export interface ProgressMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly attachments?: readonly AttachmentInfo[];
  readonly type: "progress";
  readonly requestId: string;
  readonly phase?: ProgressPhase;
  readonly completed: number;
  /** `null` for an extraction pass over source frames, whose count is not known ahead. */
  readonly total: number | null;
  /** The extraction pass, for phase `extract`. */
  readonly pass?: number;
}

/**
 * One step of an extraction the host asks the client's extractor to take; answered by an
 * `extract.ack` with the same `seq`. `video` and `audio` steps carry one attachment.
 */
export interface ExtractStepMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly attachments?: readonly AttachmentInfo[];
  readonly type: "extract.step";
  readonly requestId: string;
  readonly seq: number;
  readonly step: "begin" | "pass" | "video" | "audio" | "end";
  readonly [field: string]: unknown;
}

/** `ready` takes requests; `draining` takes none and is about to exit. */
export type HostState = "ready" | "draining";

/** What a host reports about itself, for observation; it may gain fields. */
export interface HostStateDetails {
  readonly activity?: "idle" | "busy";
  readonly requestsServed?: number;
  readonly memory?: {
    readonly residentBytes: number | null;
    readonly footprintBytes: number | null;
    readonly baselineBytes: number | null;
    readonly capacityBytes: number | null;
  };
  readonly openHandles?: number | null;
  readonly renderer?: "gpu" | "cpu" | null;
  readonly rendererFallback?: string;
  readonly hardwareDecodeFallback?: string;
  readonly degradations?: readonly unknown[];
  readonly limits?: Readonly<Record<string, number | null>>;
  readonly [field: string]: unknown;
}

/** Sent when the host starts (`ready`, reason `started`) and whenever its state changes. */
export interface StateMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly attachments?: readonly AttachmentInfo[];
  readonly type: "state";
  readonly state: HostState;
  /** Why, such as `started`, `memory`, `idle` or `device-lost`. */
  readonly reason: string;
  readonly details: HostStateDetails;
}

/** The result of a `status` request: what a `state` message would say now. */
export interface StatusResult {
  readonly state: HostState;
  readonly reason: string;
  readonly details: HostStateDetails;
}

/** A failure of the host process itself, not tied to a request. */
export interface FatalMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly attachments?: readonly AttachmentInfo[];
  readonly type: "fatal";
  readonly error: ProtocolError;
}

/** The final answer to request `requestId`; `hostState` is the host's state once it ended. */
export type ResponseMessage =
  | {
      readonly protocolVersion: typeof PROTOCOL_VERSION;
      readonly attachments?: readonly AttachmentInfo[];
      readonly requestId: string;
      readonly ok: true;
      readonly result: unknown;
      readonly hostState: HostState;
    }
  | {
      readonly protocolVersion: typeof PROTOCOL_VERSION;
      readonly attachments?: readonly AttachmentInfo[];
      readonly requestId: string;
      readonly ok: false;
      readonly error: ProtocolError;
      readonly hostState: HostState;
    };

export type HostMessage =
  | StateMessage
  | ProgressMessage
  | ExtractStepMessage
  | FatalMessage
  | ResponseMessage;

/** Throws when the header is not JSON, has another protocol version, or is malformed. */
export function parseHostMessage(header: string): HostMessage {
  const value: unknown = JSON.parse(header);
  if (!record(value)) throw new Error("host message must be an object");
  if (value.protocolVersion !== PROTOCOL_VERSION) {
    throw new Error(
      `unsupported host protocol ${String(value.protocolVersion)}; expected ${PROTOCOL_VERSION}`,
    );
  }
  if (
    value.attachments !== undefined &&
    !(Array.isArray(value.attachments) && value.attachments.every(attachment))
  )
    throw new Error("invalid attachment list");
  if (value.type === "progress") {
    const extract = value.phase === "extract";
    if (
      typeof value.requestId !== "string" ||
      !nonNegativeInteger(value.completed) ||
      !(nonNegativeInteger(value.total) || (extract && value.total === null)) ||
      (typeof value.total === "number" && value.completed > value.total) ||
      (value.phase !== undefined &&
        value.phase !== "frames" &&
        value.phase !== "finalizing" &&
        !extract)
    ) {
      throw new Error("invalid progress message");
    }
    return value as unknown as ProgressMessage;
  }
  if (value.type === "extract.step") {
    if (
      typeof value.requestId !== "string" ||
      !nonNegativeInteger(value.seq) ||
      !["begin", "pass", "video", "audio", "end"].includes(String(value.step))
    )
      throw new Error("invalid extract step");
    return value as unknown as ExtractStepMessage;
  }
  if (value.type === "state") {
    if (
      !hostState(value.state) ||
      typeof value.reason !== "string" ||
      !record(value.details)
    )
      throw new Error("invalid state message");
    return value as unknown as StateMessage;
  }
  if (value.type === "fatal") {
    if (!protocolError(value.error)) throw new Error("invalid fatal message");
    return value as unknown as FatalMessage;
  }
  if (typeof value.requestId !== "string" || typeof value.ok !== "boolean") {
    throw new Error("invalid response message");
  }
  if (!value.ok && !protocolError(value.error)) {
    throw new Error("invalid error response");
  }
  if (value.ok && !("result" in value)) {
    throw new Error("successful response is missing result");
  }
  if (!hostState(value.hostState)) {
    throw new Error("invalid response hostState");
  }
  return value as unknown as ResponseMessage;
}

/**
 * Windows DLL load failures, which exit before `main` with nothing on stderr:
 * `STATUS_DLL_NOT_FOUND`, `STATUS_ORDINAL_NOT_FOUND`, `STATUS_ENTRYPOINT_NOT_FOUND`,
 * `STATUS_INVALID_IMAGE_FORMAT`.
 */
const WINDOWS_LOADER_STATUSES = new Set([
  0xc0000135, 0xc0000138, 0xc0000139, 0xc000007b,
]);

/**
 * Windows policy refusals: `STATUS_INVALID_IMAGE_HASH` (Code Integrity) and
 * `STATUS_ACCESS_DISABLED_BY_POLICY_DEFAULT`, `_PATH`, `_PUBLISHER`, `_OTHER` (SRP, AppLocker).
 */
const WINDOWS_POLICY_STATUSES = new Set([
  0xc0000428, 0xc0000361, 0xc0000362, 0xc0000363, 0xc0000364,
]);

/** Facility of `STATUS_SYSTEM_INTEGRITY_*` (0xC0E9xxxx). */
const WINDOWS_SYSTEM_INTEGRITY_FACILITY = 0xc0e90000;

/**
 * Whether a Windows policy (Smart App Control, WDAC, AppLocker, reputation checks) refused the host
 * before it ran.
 */
export function windowsPolicyBlocked(exitCode: number | null): boolean {
  if (exitCode === null) return false;
  const status = exitCode < 0 ? exitCode + 2 ** 32 : exitCode;
  return (
    WINDOWS_POLICY_STATUSES.has(status) ||
    (status & 0xffff0000) >>> 0 === WINDOWS_SYSTEM_INTEGRITY_FACILITY
  );
}

/** Names for common Windows crash statuses. */
const WINDOWS_CRASH_STATUSES = new Map([
  [0xc0000005, "STATUS_ACCESS_VIOLATION"],
  [0xc0000409, "STATUS_STACK_BUFFER_OVERRUN"],
  [0xc00000fd, "STATUS_STACK_OVERFLOW"],
  [0xc0000374, "STATUS_HEAP_CORRUPTION"],
  [0xc000001d, "STATUS_ILLEGAL_INSTRUCTION"],
  [0xc0000094, "STATUS_INTEGER_DIVIDE_BY_ZERO"],
  [0xc000013a, "STATUS_CONTROL_C_EXIT"],
]);

/**
 * `exit SIGKILL`, `exit 1`, or for a Windows crash status
 * `exit 3221226505 (0xC0000409 STATUS_STACK_BUFFER_OVERRUN)`.
 */
export function describeExit(
  exitCode: number | null,
  signal: string | null,
): string {
  if (signal !== null) return `exit ${signal}`;
  if (exitCode === null) return "exit unknown";
  const status = exitCode < 0 ? exitCode + 2 ** 32 : exitCode;
  const name = WINDOWS_CRASH_STATUSES.get(status);
  if (name === undefined) return `exit ${exitCode}`;
  return `exit ${exitCode} (0x${status.toString(16).toUpperCase().padStart(8, "0")} ${name})`;
}

/** Refusal messages from glibc ld.so, musl and macOS dyld. */
const LOADER_WORDS = [
  /: error while loading shared libraries: /,
  /: version `[^']+' not found \(required by /,
  /: symbol lookup error: /,
  /^Error loading shared library /m,
  /^dyld(?:\[\d+\])?: (?:Library not loaded|Symbol not found)/m,
];

/**
 * Whether the OS loader refused the host because a linked library (usually FFmpeg) is missing or
 * the wrong version. Unlike a crash, this fails the same way on every start.
 */
export function loaderRefused(
  exitCode: number | null,
  stderr: string,
): boolean {
  if (
    exitCode !== null &&
    WINDOWS_LOADER_STATUSES.has(exitCode < 0 ? exitCode + 2 ** 32 : exitCode)
  )
    return true;
  return LOADER_WORDS.some((words) => words.test(stderr));
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function attachment(value: unknown): value is AttachmentInfo {
  return (
    record(value) &&
    typeof value.type === "string" &&
    value.type.length > 0 &&
    nonNegativeInteger(value.bytes)
  );
}

function hostState(value: unknown): value is HostState {
  return value === "ready" || value === "draining";
}

function protocolError(value: unknown): value is ProtocolError {
  return (
    record(value) &&
    typeof value.code === "string" &&
    value.code.length > 0 &&
    typeof value.message === "string" &&
    (value.failure === undefined || record(value.failure))
  );
}

function nonNegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
