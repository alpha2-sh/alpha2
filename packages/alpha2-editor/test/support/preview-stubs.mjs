// What a preview test needs of a browser in Node: canvases that record the frames copied to them,
// animation frames, and the bridge's renderer replaced by stub-engine.mjs. Import this before the
// bridge.
import { register } from "node:module";

register("./engine-hook.mjs", import.meta.url);

globalThis.stubEngine = { engines: [], mixers: [], start: null };

/** An OffscreenCanvas whose `bitmaprenderer` context counts the bitmaps transferred to it. */
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
globalThis.requestAnimationFrame ??= (fn) =>
  setTimeout(() => fn(performance.now()), 4);
globalThis.cancelAnimationFrame ??= (id) => clearTimeout(id);
globalThis.Worker ??= class Worker {};

/**
 * A worker running the editor's preview protocol that fails as `fail` says: `init` (its engine
 * would not start), `load` (its script would not load: an error event without a message), `seek`
 * or `replace-project` (that request is refused, with `code` and the serialized error's other
 * fields in `error`). Other requests succeed.
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
 * A worker running the editor's preview protocol that succeeds at everything: it records each
 * request (`requests`), answers a seek with `seek(request)` (default: a drawn frame with a bitmap)
 * and a load or a scale change with the render size at `scale`, as the engine rounds it.
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
