// Runs extension provider calls (probe, open, thumbnail) off the page's thread, since each is one
// synchronous call that may decode every frame or parse a whole file. Hosts bundling the
// `@alpha2-sh/editor/extensions-worker` entry pass `runtimeAssets.extensionsWorkerFactory`; each
// call gets its own worker, terminated on cancel or past `EXTENSIONS_CALL_TIMEOUT_MS`.
import { EditorError } from "../core/document.js";
import {
  exrFirstFrame,
  readProviderFile,
  svgDiagnostics,
} from "./runtime/extension-import.js";

/**
 * How long one extensions worker call may run before the worker is terminated and the call fails as
 * `provider_stalled`. Matches the 40 s a native decode shard may spend in one provider or decoder
 * call (10 s plus the default `ALPHA2_GPU_TIMEOUT_MS`). Provider budgets refuse countable work long
 * before this; the limit covers the rest.
 */
export const EXTENSIONS_CALL_TIMEOUT_MS = 40_000;

/** Loads the provider module (`alpha2/extensions`) from `wasmUrl`, once per realm. */
async function loadModule(wasmUrl) {
  const { loadExtensions } = await import("alpha2/extensions");
  return loadExtensions(wasmUrl);
}

const CALLS = {
  read: (extensions, bytes) => readProviderFile(extensions, bytes),
  /** An SVG's diagnostics with the font stack `fonts` (bytes, in fallback order). */
  "svg-diagnostics": (extensions, bytes, { provider, fonts }) =>
    svgDiagnostics(extensions, provider, bytes, fonts),
  /** An OpenEXR sequence's first frame: its thumbnail and report. */
  "exr-first-frame": (extensions, bytes) => exrFirstFrame(extensions, bytes),
};

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
 * Answers calls posted to `scope` (a dedicated worker's global): `{ type: "call", call, file,
 * wasmUrl, ... }` gets `{ type: "answered", result }`, or `{ type: "failed", name, message }`
 * when the file cannot be read or the provider refuses it. `{ type: "started" }` is posted first.
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

/** Makes `url`, or each URL in a map of them, absolute, so a worker resolves it like the page. */
function absolute(url) {
  if (url === undefined) return undefined;
  if (typeof url === "object" && !(url instanceof URL))
    return Object.fromEntries(
      Object.entries(url).map(([name, each]) => [name, absolute(each)]),
    );
  const base = globalThis.location?.href;
  return base ? new URL(String(url), base).href : String(url);
}

/**
 * Runs an extensions call (`read`, `svg-diagnostics` or `exr-first-frame`) on `request.file`.
 * With `factory`, the call runs in a new worker running {@link installExtensionsWorker}, terminated
 * once it answers, when `signal` aborts (rejecting with its reason), or after `timeoutMs`
 * (rejecting as `provider_stalled`). Without a factory, or if the worker script fails to load, the
 * call runs on this thread, where it cannot be stopped and `signal` is checked between steps.
 *
 * @param {{ factory?: () => Worker, wasmUrl?: string | URL | import("alpha2/extensions").ExtensionWasmUrls }} module
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
      // The worker never ran, so make the call here.
      else here().then(resolve, reject);
    };
    worker.postMessage({
      type: "call",
      ...request,
      wasmUrl: absolute(wasmUrl),
    });
  });
}
