/**
 * An error as it crosses a worker boundary (`postMessage`), and the error the other side rebuilds
 * from it.
 *
 * Structured clone keeps an `Error`'s message and little else, and a `DOMException`'s name only in
 * some engines, so the worker sends what the failure's classification reads: the code, whether the
 * binding can still be used, the error's name (`QuotaExceededError`, `RangeError`, ...), and the
 * `failure` it carries, whole (an `ExportFailureError`'s) or partial (an engine readback's).
 */

/** An error as plain data that survives `postMessage`. */
export interface SerializedAlpha2Error {
  /** The error's own `code`; otherwise a wasm trap's code or the worker's default. */
  readonly code: string;
  readonly message: string;
  /** Whether the worker's binding is unusable and the worker must be replaced. */
  readonly fatal: boolean;
  /** The error's own name, when it had one other than `Error`. */
  readonly name?: string;
  /** The `failure` the error carried, as plain data. */
  readonly failure?: Readonly<Record<string, unknown>>;
}

/**
 * A worker's error rebuilt on the host side, with the code, fatality, name,
 * and `failure` it had in the worker.
 */
export class Alpha2WasmError extends Error {
  readonly code: string;
  readonly fatal: boolean;
  /** The failure the error carried in the worker, if any. */
  readonly failure?: Readonly<Record<string, unknown>>;

  constructor(failure: SerializedAlpha2Error) {
    super(failure.message);
    // The original name, so what classifies by name (a quota, a range) reads it as it was.
    this.name = failure.name ?? "Alpha2WasmError";
    this.code = failure.code;
    this.fatal = failure.fatal;
    if (failure.failure !== undefined) this.failure = failure.failure;
  }
}

/**
 * Normalizes a thrown value for the other side of a worker boundary.
 *
 * `trapped` reports whether this worker has already seen a wasm trap. After one, destructors never
 * ran and the borrow an exported method held is still held, so nothing the worker reports
 * afterwards is retryable. `uncoded` is the code of an error that names none.
 */
export function serializeError(
  error: unknown,
  trapped = false,
  uncoded = "RENDER_WORKER_ERROR",
): SerializedAlpha2Error {
  const message = error instanceof Error ? error.message : String(error);
  const name = nameOf(error);
  const trap = wasmTrapCode(error);
  if (trap !== undefined) {
    // The raw message is "unreachable": classify it as fatal here, or the host would retry a call
    // that can only trap again.
    return {
      code: trap,
      message: `wasm ${message}; the module is no longer usable and the worker must be replaced`,
      fatal: true,
      ...(name === undefined ? {} : { name }),
    };
  }
  const value = error as {
    code?: unknown;
    fatal?: unknown;
    failure?: unknown;
  } | null;
  const failure = plainFailure(value?.failure);
  const carried = {
    ...(name === undefined ? {} : { name }),
    ...(failure === undefined ? {} : { failure }),
  };
  if (trapped) {
    return {
      code: typeof value?.code === "string" ? value.code : "WASM_TRAPPED",
      message: `${message} (after an earlier wasm trap; the worker must be replaced)`,
      fatal: true,
      ...carried,
    };
  }
  return {
    code: typeof value?.code === "string" ? value.code : uncoded,
    message,
    fatal: typeof value?.fatal === "boolean" ? value.fatal : false,
    ...carried,
  };
}

/** Classifies the unrecoverable `WebAssembly` failures, or `undefined`. */
export function wasmTrapCode(error: unknown): string | undefined {
  if (typeof WebAssembly === "undefined") return undefined;
  if (error instanceof WebAssembly.RuntimeError) return "WASM_TRAP";
  if (error instanceof WebAssembly.CompileError) return "WASM_COMPILE_ERROR";
  if (error instanceof WebAssembly.LinkError) return "WASM_LINK_ERROR";
  return undefined;
}

function nameOf(error: unknown): string | undefined {
  const name = (error as { name?: unknown } | null)?.name;
  return typeof name === "string" && name !== "" && name !== "Error"
    ? name
    : undefined;
}

/**
 * `failure` as data `postMessage` can carry. A failure is plain data by contract; one that is not
 * (a function, a host object) is dropped rather than left to make the reply itself throw and never
 * arrive.
 */
function plainFailure(
  failure: unknown,
): Readonly<Record<string, unknown>> | undefined {
  if (typeof failure !== "object" || failure === null || Array.isArray(failure))
    return undefined;
  try {
    return structuredClone(failure) as Record<string, unknown>;
  } catch {
    return undefined;
  }
}
