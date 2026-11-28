// Runs a plugin's extraction off the page's thread. Hosts bundling the
// `@alpha2-sh/editor/extract-worker` entry pass `runtimeAssets.extractWorkerFactory`; each run gets
// its own worker, terminated when it settles or is cancelled.
import { runExtractor } from "alpha2/extract";
import { restorePlugin } from "alpha2/plugins";

import { reviveRangeFile } from "./runtime/range-file.js";
import { loadEngineWasm } from "./runtime/wasm.js";

/** An error as it crosses to the page: its name, message, code, details and decode facts. */
function wireError(error) {
  return {
    name: error?.name ?? "Error",
    message: String(error?.message ?? error),
    ...(typeof error?.code === "string" ? { code: error.code } : {}),
    ...(error?.details === undefined ? {} : { details: error.details }),
    ...(error?.facts === undefined ? {} : { facts: error.facts }),
  };
}

/** The error the worker reported, with the code, details and facts callers read. */
function pageError(wired) {
  const error = new Error(wired.message);
  error.name = wired.name;
  if (wired.code !== undefined) error.code = wired.code;
  if (wired.details !== undefined) error.details = wired.details;
  if (wired.facts !== undefined) error.facts = wired.facts;
  return error;
}

/**
 * Answers one `{ type: "run" }` posted to `scope` (a dedicated worker's global): runs the plugin
 * with `runExtractor`, posting `{ type: "progress" }` as it goes and `{ type: "frame", id, time,
 * width, height }` for each timeline frame, which the page answers with `{ type: "frame", id,
 * rgba }` or `{ id, error }`; then `{ type: "done", result }` or `{ type: "failed", error }`.
 * @param {{ onmessage: ((event: MessageEvent) => void) | null, postMessage(message: unknown, transfer?: Transferable[]): void }} scope
 */
export function installExtractWorker(scope) {
  const frames = new Map();
  let nextFrame = 0;
  scope.onmessage = async ({ data }) => {
    if (data?.type === "frame") {
      const pending = frames.get(data.id);
      frames.delete(data.id);
      if (data.error === undefined) pending?.resolve(data.rgba);
      else pending?.reject(pageError(data.error));
      return;
    }
    if (data?.type !== "run") return;
    try {
      const binding = await loadEngineWasm(data.wasmUrl);
      const plugin = restorePlugin(data.plugin);
      const timelineFrame = data.timeline
        ? (time, width, height) =>
            new Promise((resolve, reject) => {
              const id = nextFrame++;
              frames.set(id, { resolve, reject });
              scope.postMessage({ type: "frame", id, time, width, height });
            }).then((rgba) => ({ width, height, rgba }))
        : undefined;
      const result = await runExtractor(
        {
          binding,
          audioBinding: binding,
          project: data.project,
          request: data.request,
          ...(data.media
            ? {
                media: Object.fromEntries(
                  Object.entries(data.media).map(([id, file]) => [
                    id,
                    reviveRangeFile(file),
                  ]),
                ),
              }
            : {}),
          ...(timelineFrame ? { timelineFrame } : {}),
          ...(data.madeFrom ? { madeFrom: data.madeFrom } : {}),
          onProgress: (progress) =>
            scope.postMessage({ type: "progress", progress }),
        },
        plugin.extractor(data.request.params),
      );
      scope.postMessage({ type: "done", result });
    } catch (error) {
      scope.postMessage({ type: "failed", error: wireError(error) });
    }
  };
}

/**
 * Runs `plugin` (`loadPlugin`'s) in a worker from `factory` (running {@link installExtractWorker})
 * over `run`, as `runExtractor` would on the page. `timelineFrame` draws the frames a timeline
 * input asks for, on the page. Aborting `signal` terminates the worker at once, a plugin call in
 * flight included, and rejects with its reason. Resolves with `null` when the worker does not
 * start, so the caller can run on the page instead.
 * @param {() => Worker} factory
 * @param {{
 *   wasmUrl?: string,
 *   plugin: import("alpha2/plugins").LoadedPlugin,
 *   project: import("alpha2/project").Project,
 *   request: import("alpha2/extractor").ExtractRun,
 *   media?: Readonly<Record<string, Blob | import("./types.js").RangeFile>>,
 *   timelineFrame?: (time: bigint, width: number, height: number) => Promise<{ rgba: Uint8Array }>,
 *   madeFrom?: unknown,
 *   onProgress?: (progress: import("alpha2/extractor").ExtractProgress) => void,
 *   signal: AbortSignal,
 * }} options
 * @returns {Promise<import("alpha2/extractor").ExtractResult | null>}
 */
export async function extractInWorker(factory, options) {
  const { transferablePlugin } = await import("alpha2/plugins");
  const { signal } = options;
  signal.throwIfAborted();
  let worker;
  try {
    worker = factory();
  } catch {
    return null;
  }
  return new Promise((resolve, reject) => {
    let started = false;
    const end = () => {
      signal.removeEventListener("abort", abort);
      worker.terminate();
    };
    const abort = () => {
      end();
      reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    };
    signal.addEventListener("abort", abort, { once: true });
    worker.onmessage = async ({ data }) => {
      started = true;
      if (data?.type === "progress") options.onProgress?.(data.progress);
      else if (data?.type === "frame") {
        try {
          const { rgba } = await options.timelineFrame(
            data.time,
            data.width,
            data.height,
          );
          worker.postMessage({ type: "frame", id: data.id, rgba }, [
            rgba.buffer,
          ]);
        } catch (error) {
          worker.postMessage({
            type: "frame",
            id: data.id,
            error: wireError(error),
          });
        }
      } else if (data?.type === "done") {
        end();
        resolve(data.result);
      } else if (data?.type === "failed") {
        end();
        reject(pageError(data.error));
      }
    };
    worker.onerror = (event) => {
      event?.preventDefault?.();
      end();
      if (started)
        reject(new Error(`the extraction worker failed: ${event?.message}`));
      else resolve(null);
    };
    worker.postMessage({
      type: "run",
      ...(options.wasmUrl === undefined ? {} : { wasmUrl: options.wasmUrl }),
      plugin: transferablePlugin(options.plugin),
      project: options.project,
      request: options.request,
      timeline: options.timelineFrame !== undefined,
      ...(options.media ? { media: options.media } : {}),
      ...(options.madeFrom ? { madeFrom: options.madeFrom } : {}),
    });
  });
}
