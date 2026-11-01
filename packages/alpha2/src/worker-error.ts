// Structured clone keeps little more than an `Error`'s message, so errors cross a worker boundary
// as plain data with the fields failure classification reads.

/** An error as plain data that survives `postMessage`. */
export interface SerializedAlpha2Error {
  /** The error's own `code`, else a wasm trap code or the worker's default. */
  readonly code: string;
  readonly message: string;
  /** Whether the worker's binding is unusable and the worker must be replaced. */
  readonly fatal: boolean;
  /** Present when the name is not `Error`. */
  readonly name?: string;
  readonly failure?: Readonly<Record<string, unknown>>;
}

/** A worker's error rebuilt on the host side, keeping its code, fatality, name and `failure`. */
export class Alpha2WasmError extends Error {
  readonly code: string;
  readonly fatal: boolean;
  readonly failure?: Readonly<Record<string, unknown>>;

  constructor(failure: SerializedAlpha2Error) {
    super(failure.message);
    // Keep the original name for classification by name (quota, range).
    this.name = failure.name ?? "Alpha2WasmError";
    this.code = failure.code;
    this.fatal = failure.fatal;
    if (failure.failure !== undefined) this.failure = failure.failure;
  }
}

/**
 * Normalizes a thrown value for the other side of a worker boundary.
 *
 * Pass `trapped` once the worker has seen a wasm trap: borrows are still held, so every later error
 * is fatal. `uncoded` is the code for an error without one.
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
    // Fatal, or the host would retry a call that can only trap again.
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

// A non-cloneable failure is dropped so it cannot make the reply itself throw.
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
