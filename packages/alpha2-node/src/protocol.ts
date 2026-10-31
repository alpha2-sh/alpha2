/** The version of the line-delimited JSON protocol spoken with `alpha2-host` over stdio. */
export const PROTOCOL_VERSION = 1 as const;

/**
 * A request object for the host: `payload`'s fields beside the protocol version, operation and id.
 */
export function createRequest(
  operation: "capabilities" | "export" | "conform" | "still",
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

/** An error as the host reports it. */
export interface ProtocolError {
  /** A non-empty machine-readable code, such as `INVALID_INPUT`. */
  readonly code: string;
  readonly message: string;
  /** Structured detail, such as the conform report of a `CONFORM_FAILED`. */
  readonly details?: unknown;
  /**
   * A failed export's report: its class (`code`), the step and frame it failed at, and the host's
   * diagnostics. See `alpha2/export-failure`.
   */
  readonly failure?: Readonly<Record<string, unknown>>;
}

/**
 * `frames` while frames are encoded; `finalizing` once every one is and the output is being flushed
 * and closed.
 */
export type ProgressPhase = "frames" | "finalizing";

/** Progress of the request `requestId`; `completed` never exceeds `total`. */
export interface ProgressMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly type: "progress";
  readonly requestId: string;
  readonly phase?: ProgressPhase;
  readonly completed: number;
  readonly total: number;
}

/**
 * The first line `serve` writes, before it reads any input: the process is running. A host that
 * writes nothing for long is held before it starts (a security scan, an approval prompt).
 */
export interface ReadyMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly type: "ready";
}

/** A failure of the host process itself, not tied to a request. */
export interface FatalMessage {
  readonly protocolVersion: typeof PROTOCOL_VERSION;
  readonly type: "fatal";
  readonly error: ProtocolError;
}

/** The final answer to request `requestId`. */
export type ResponseMessage =
  | {
      readonly protocolVersion: typeof PROTOCOL_VERSION;
      readonly requestId: string;
      readonly ok: true;
      readonly result: unknown;
    }
  | {
      readonly protocolVersion: typeof PROTOCOL_VERSION;
      readonly requestId: string;
      readonly ok: false;
      readonly error: ProtocolError;
    };

export type HostMessage =
  ReadyMessage | ProgressMessage | FatalMessage | ResponseMessage;

/**
 * Parses and checks one line the host wrote. Throws when the line is not JSON, speaks another
 * protocol version, or is not a well-formed message.
 */
export function parseHostMessage(line: string): HostMessage {
  const value: unknown = JSON.parse(line);
  if (!record(value)) throw new Error("host message must be an object");
  if (value.protocolVersion !== PROTOCOL_VERSION) {
    throw new Error(
      `unsupported host protocol ${String(value.protocolVersion)}; expected ${PROTOCOL_VERSION}`,
    );
  }
  if (value.type === "progress") {
    if (
      typeof value.requestId !== "string" ||
      !nonNegativeInteger(value.completed) ||
      !nonNegativeInteger(value.total) ||
      value.completed > value.total ||
      (value.phase !== undefined &&
        value.phase !== "frames" &&
        value.phase !== "finalizing")
    ) {
      throw new Error("invalid progress message");
    }
    return value as unknown as ProgressMessage;
  }
  if (value.type === "ready") return value as unknown as ReadyMessage;
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
  return value as unknown as ResponseMessage;
}

/**
 * Windows ends a process whose DLLs it cannot load with one of these statuses, before `main` and
 * without a word on stderr: `STATUS_DLL_NOT_FOUND`, `STATUS_ORDINAL_NOT_FOUND`,
 * `STATUS_ENTRYPOINT_NOT_FOUND` and `STATUS_INVALID_IMAGE_FORMAT`. Node reports them unsigned.
 */
const WINDOWS_LOADER_STATUSES = new Set([
  0xc0000135, 0xc0000138, 0xc0000139, 0xc000007b,
]);

/**
 * The statuses Windows ends a process with when a policy refuses its executable or a DLL it loads:
 * `STATUS_INVALID_IMAGE_HASH` (Code Integrity: an unsigned or tampered image) and
 * `STATUS_ACCESS_DISABLED_BY_POLICY_DEFAULT`, `_PATH`, `_PUBLISHER` and `_OTHER` (Software
 * Restriction Policies, AppLocker).
 */
const WINDOWS_POLICY_STATUSES = new Set([
  0xc0000428, 0xc0000361, 0xc0000362, 0xc0000363, 0xc0000364,
]);

/** The facility of the system-integrity statuses (`STATUS_SYSTEM_INTEGRITY_*`, 0xC0E9xxxx). */
const WINDOWS_SYSTEM_INTEGRITY_FACILITY = 0xc0e90000;

/**
 * Whether a host that ended without answering was refused by a Windows policy before it ran: Smart
 * App Control, an App Control (WDAC) or AppLocker policy, or reputation checks on unsigned code.
 * The same files on the same machine are refused the same way however often they are started.
 */
export function windowsPolicyBlocked(exitCode: number | null): boolean {
  if (exitCode === null) return false;
  const status = exitCode < 0 ? exitCode + 2 ** 32 : exitCode;
  return (
    WINDOWS_POLICY_STATUSES.has(status) ||
    (status & 0xffff0000) >>> 0 === WINDOWS_SYSTEM_INTEGRITY_FACILITY
  );
}

/**
 * The statuses Windows ends a crashed process with, by name, so a crash reads as one rather than
 * as a ten-digit exit code.
 */
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
 * A process's end for a message: `exit SIGKILL`, `exit 1`, or `exit 3221226505 (0xC0000409
 * STATUS_STACK_BUFFER_OVERRUN)` for a Windows crash status.
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

/**
 * What the dynamic loaders write when they refuse a program, before it runs a line of its own:
 * glibc's ld.so (a missing library, one without a version the program names, a symbol it lacks),
 * musl's, and macOS's dyld.
 */
const LOADER_WORDS = [
  /: error while loading shared libraries: /,
  /: version `[^']+' not found \(required by /,
  /: symbol lookup error: /,
  /^Error loading shared library /m,
  /^dyld(?:\[\d+\])?: (?:Library not loaded|Symbol not found)/m,
];

/**
 * Whether a host that ended without answering was refused by the operating system's loader: a
 * library it links (FFmpeg's above all) is missing or of another version. That is the machine's
 * setup, so the same host fails the same way however often it is started, unlike a host that ran
 * and died, which says nothing of a cause.
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
