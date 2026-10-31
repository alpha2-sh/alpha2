// Browser stand-ins for preview tests in Node; import this before the bridge.
import { register } from "node:module";

register("./engine-hook.mjs", import.meta.url);

globalThis.stubEngine = { engines: [], mixers: [], start: null };

/** Resets this realm's `pageGpuLosses` so a test's recovery ladder starts as a fresh page's. */
export function resetPageGpuLosses() {
  delete globalThis[Symbol.for("alpha2.pageGpuLosses")];
}

export class FakeCanvas {
  constructor(width = 16, height = 16) {
    this.width = width;
    this.height = height;
    this.drawn = 0;
    this.transferred = 0;
  }
  getContext(kind) {
    if (kind === "bitmaprenderer")
      return {
        transferFromImageBitmap: () => {
          this.transferred += 1;
        },
      };
    return null;
  }
  transferToImageBitmap() {
    return { close() {} };
  }
}
globalThis.OffscreenCanvas ??= FakeCanvas;
globalThis.createImageBitmap ??= async () => ({ close() {} });
globalThis.requestAnimationFrame ??= (fn) =>
  setTimeout(() => fn(performance.now()), 4);
globalThis.cancelAnimationFrame ??= (id) => clearTimeout(id);
globalThis.Worker ??= class Worker {};

/**
 * A preview worker that fails per `fail`: `init` (engine would not start), `load` (script would
 * not load), or `seek`/`replace-project` (refused with `code` and `error`'s fields).
 */
export function failingWorker(fail, code = "worker_error", error = {}) {
  const worker = {
    messages: [],
    terminated: false,
    onmessage: null,
    onerror: null,
    postMessage(message) {
      worker.messages.push(message.type);
      queueMicrotask(() => {
        if (fail === "load") {
          worker.onerror?.({ type: "error" });
          return;
        }
        const refused = message.type === fail;
        worker.onmessage?.({
          data: {
            sessionId: message.sessionId,
            requestId: message.requestId,
            ok: !refused,
            ...(refused
              ? {
                  error: {
                    code,
                    message: `injected ${fail} failure`,
                    ...error,
                  },
                }
              : {
                  value:
                    message.type === "seek"
                      ? { rendered: true, bitmap: { close() {} } }
                      : {},
                }),
          },
        });
      });
    },
    terminate() {
      worker.terminated = true;
    },
  };
  return worker;
}

/**
 * A preview worker that succeeds at everything, recording `requests`. Seeks answer with
 * `seek(request)` (default a drawn frame with a bitmap); loads and scale changes with the render
 * size.
 */
export function workingWorker({ seek } = {}) {
  let project = null,
    scale = 0.5;
  const renderSize = () =>
    project && [
      Math.round(project.settings.width * scale),
      Math.round(project.settings.height * scale),
    ];
  const worker = {
    requests: [],
    terminated: false,
    onmessage: null,
    onerror: null,
    postMessage(message) {
      worker.requests.push(message);
      if (message.type === "replace-project") project = message.project;
      if (message.type === "set-preview-scale") scale = message.scale;
      const value =
        message.type === "seek"
          ? (seek?.(message) ?? { rendered: true, bitmap: { close() {} } })
          : message.type === "replace-project" ||
              message.type === "set-preview-scale"
            ? { renderSize: renderSize() }
            : undefined;
      queueMicrotask(() =>
        worker.onmessage?.({
          data: {
            sessionId: message.sessionId,
            requestId: message.requestId,
            ok: true,
            value,
          },
        }),
      );
    },
    terminate() {
      worker.terminated = true;
    },
  };
  return worker;
}
