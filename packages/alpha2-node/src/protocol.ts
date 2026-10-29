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

export type HostMessage = ProgressMessage | FatalMessage | ResponseMessage;

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
