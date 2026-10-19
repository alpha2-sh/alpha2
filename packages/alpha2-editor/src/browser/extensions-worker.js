// Runs extension provider calls (probe, open, thumbnail) off the page's thread: each is one
// synchronous call that can decode every frame or parse a file whole. A host bundling the
// `@alpha2-sh/editor/extensions-worker` entry passes `runtimeAssets.extensionsWorkerFactory`; each
// call gets its own worker, terminated on cancel or past `EXTENSIONS_CALL_TIMEOUT_MS`.
import { EditorError } from "../core/document.js";
import {
  exrFirstFrame,
  readProviderFile,
  svgDiagnostics,
} from "./runtime/extension-import.js";

/**
 * How long one call in an extensions worker may run before the worker is terminated and the call
 * fails as `provider_stalled`: the 40 s a native host's decode shard may spend inside one provider
 * or decoder call (10 s plus the default `ALPHA2_GPU_TIMEOUT_MS`) before the export fails as
 * stalled. The provider's budgets refuse a file whose drawing they can count long before this; the
 * limit is for work they cannot.
 */
export const EXTENSIONS_CALL_TIMEOUT_MS = 40_000;

/** Loads the provider module (`alpha2/extensions`) from `wasmUrl`, once per realm. */
async function loadModule(wasmUrl) {
  const { loadExtensions } = await import("alpha2/extensions");
  return loadExtensions(wasmUrl);
}

/** What each call reads of its file's bytes with the provider module. */
const CALLS = {
  /** What import reads of a file: `null`, or its provider, report and thumbnail. */
  read: (extensions, bytes) => readProviderFile(extensions, bytes),
  /** An SVG's diagnostics with the font stack `fonts` (bytes, in fallback order). */
  "svg-diagnostics": (extensions, bytes, { provider, fonts }) =>
    svgDiagnostics(extensions, provider, bytes, fonts),
  /** An OpenEXR sequence's first frame: its thumbnail and report. */
  "exr-first-frame": (extensions, bytes) => exrFirstFrame(extensions, bytes),
};

/** Runs `request` in this realm, checking `signal` between the steps it waits for. */
async function run(request, wasmUrl, load, signal) {
  const call = CALLS[request.call];
  if (!call) throw new Error(`unknown extensions call ${request.call}`);
  const extensions = await load(wasmUrl);
  signal?.throwIfAborted();
  const bytes = new Uint8Array(await request.file.arrayBuffer());
  signal?.throwIfAborted();
  return call(extensions, bytes, request);
}

/**
 * Answers the call posted to `scope` (a dedicated worker's global): `{ type: "call", call, file,
 * wasmUrl, ... }` is answered with `{ type: "answered", result }`, or `{ type: "failed", name,
 * message }` when the file cannot be read or the provider refuses it. `{ type: "started" }` is
 * posted first, once the worker runs.
 * @param {{ onmessage: ((event: MessageEvent) => void) | null, postMessage(message: unknown): void }} scope
 */
export function installExtensionsWorker(scope) {
  scope.onmessage = async ({ data }) => {
    if (data?.type !== "call") return;
    try {
      const result = await run(data, data.wasmUrl, loadModule);
      scope.postMessage({ type: "answered", result });
    } catch (error) {
      scope.postMessage({
        type: "failed",
        name: error?.name ?? "Error",
        // A provider's refusal is thrown as its message alone.
        message: String(error?.message ?? error),
      });
    }
  };
  scope.postMessage({ type: "started" });
}

/** `url` as an absolute URL, which a worker resolves the same as the page. */
function absolute(url) {
  if (url === undefined) return undefined;
  const base = globalThis.location?.href;
  return base ? new URL(String(url), base).href : String(url);
}

/**
 * Runs an extensions call: `request` names the call (`read`, `svg-diagnostics` or
 * `exr-first-frame`), the `file` it reads and what else the call takes. With `factory`, the call
 * runs in a worker it makes (one running {@link installExtensionsWorker}), which is terminated once
 * it answers, when `signal` aborts (rejecting with its reason), or after `timeoutMs` (rejecting as
 * `provider_stalled`). A worker that cannot start (its script does not load) leaves the call to
 * this thread, as it does without a factory; here the module call cannot be stopped, and `signal`
 * is checked between steps.
 *
 * @param {{ factory?: () => Worker, wasmUrl?: string | URL }} module
 * @param {{ call: string, file: Blob & { name?: string }, [key: string]: unknown }} request
 * @param {{ signal?: AbortSignal, timeoutMs?: number, operation?: string }} [options]
 */
export function callExtensions(
  { factory, wasmUrl },
  request,
  { signal, timeoutMs = EXTENSIONS_CALL_TIMEOUT_MS, operation = "import" } = {},
) {
  signal?.throwIfAborted();
  const here = () => run(request, wasmUrl, loadModule, signal);
  if (!factory) return here();
  let worker;
  try {
    worker = factory();
  } catch {
    return here();
  }
  return new Promise((resolve, reject) => {
    let started = false;
    let ended = false;
    const end = () => {
      ended = true;
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
      worker.terminate();
    };
    const abort = () => {
      end();
      reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    };
    const timer = setTimeout(() => {
      end();
      reject(
        new EditorError(
          "provider_stalled",
          `${request.file.name ?? "The file"} took the extension provider more than ${timeoutMs / 1000} s to read; it was stopped`,
          operation,
        ),
      );
    }, timeoutMs);
    signal?.addEventListener("abort", abort, { once: true });
    worker.onmessage = ({ data }) => {
      if (ended) return;
      if (data?.type === "started") started = true;
      else if (data?.type === "answered") {
        end();
        resolve(data.result);
      } else if (data?.type === "failed") {
        end();
        reject(
          Object.assign(new Error(data.message), {
            name: data.name ?? "Error",
          }),
        );
      }
    };
    worker.onerror = (event) => {
      if (ended) return;
      event?.preventDefault?.();
      end();
      if (started)
        reject(
          new Error(
            `the extensions worker stopped: ${event?.message ?? "an uncaught error"}`,
          ),
        );
      // The worker never ran: the call is made here instead.
      else here().then(resolve, reject);
    };
    worker.postMessage({
      type: "call",
      ...request,
      wasmUrl: absolute(wasmUrl),
    });
  });
}
