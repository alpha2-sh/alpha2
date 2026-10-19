// Hashes an imported file off the page's thread (about 5 ms of CPU per megabyte in JavaScript).
// A host bundling the `@alpha2-sh/editor/hash-worker` entry passes
// `runtimeAssets.hashWorkerFactory`; each file gets its own worker, terminated on cancel.
import { sha256Blob } from "alpha2/assets";

/**
 * Answers hash requests posted to `scope` (a dedicated worker's global): `{ type: "hash", file }`
 * is answered with `{ type: "progress", bytes }` as it is read and `{ type: "hashed", sha256 }`, or
 * `{ type: "failed", message, name }` when the file cannot be read.
 * @param {{ onmessage: ((event: MessageEvent) => void) | null, postMessage(message: unknown): void }} scope
 */
export function installHashWorker(scope) {
  scope.onmessage = async ({ data }) => {
    if (data?.type !== "hash") return;
    try {
      const sha256 = await sha256Blob(data.file, {
        onProgress: (bytes) => scope.postMessage({ type: "progress", bytes }),
      });
      scope.postMessage({ type: "hashed", sha256 });
    } catch (error) {
      scope.postMessage({
        type: "failed",
        name: error?.name ?? "Error",
        message: String(error?.message ?? error),
      });
    }
  };
}

/**
 * The SHA-256 of `file`, as `sha256Blob` gives it, hashed in a worker `factory` makes (one running
 * {@link installHashWorker}), which is terminated once it answers or `signal` aborts. A worker that
 * cannot start (its script does not load) leaves the hash to this thread's `sha256Blob`, which lets
 * other tasks run every few milliseconds.
 * @param {() => Worker} factory
 * @param {Blob} file
 * @param {{ signal?: AbortSignal, onProgress?: (bytes: number) => void }} [options]
 * @returns {Promise<string>}
 */
export function hashInWorker(factory, file, { signal, onProgress } = {}) {
  signal?.throwIfAborted();
  let worker;
  try {
    worker = factory();
  } catch {
    return sha256Blob(file, { signal, onProgress });
  }
  return new Promise((resolve, reject) => {
    let answered = false;
    const end = () => {
      answered = true;
      signal?.removeEventListener("abort", abort);
      worker.terminate();
    };
    const abort = () => {
      end();
      reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    };
    signal?.addEventListener("abort", abort, { once: true });
    worker.onmessage = ({ data }) => {
      if (answered) return;
      if (data?.type === "progress") onProgress?.(data.bytes);
      else if (data?.type === "hashed") {
        end();
        resolve(data.sha256);
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
      if (answered) return;
      event?.preventDefault?.();
      end();
      // The worker never ran: the hash is made here instead.
      sha256Blob(file, { signal, onProgress }).then(resolve, reject);
    };
    worker.postMessage({ type: "hash", file });
  });
}
