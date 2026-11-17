import { activeTimeout } from "alpha2/active-clock";

// Keep the source's display aspect ratio; transparent padding belongs to the clip and takes part in
// its transform and transition.
export function containRect(sourceWidth, sourceHeight, width, height) {
  const scale = Math.min(width / sourceWidth, height / sourceHeight);
  const drawWidth = sourceWidth * scale;
  const drawHeight = sourceHeight * scale;
  return [
    (width - drawWidth) / 2,
    (height - drawHeight) / 2,
    drawWidth,
    drawHeight,
  ];
}

// Select project frames from the clock instead of accumulating RAF delays.
export function previewFrameIndex(seconds, fps) {
  return Math.floor(seconds * fps + 1e-7);
}

/** Active time a media element may take to load its first frame. */
export const MEDIA_LOAD_TIMEOUT_MS = 10_000;

/**
 * Loads `video` and resolves once its first frame is available. Rejects with the element's error,
 * with a timeout after `timeoutMs` of active time (a frozen page does not count), or with
 * `signal`'s reason once it aborts. Its listeners are removed on every path.
 */
export function loadedOrFailed(
  video,
  { signal, timeoutMs = MEDIA_LOAD_TIMEOUT_MS } = {},
) {
  return new Promise((resolve, reject) => {
    const timer = new AbortController();
    const cleanup = () => {
      timer.abort();
      video.removeEventListener("loadeddata", ready);
      video.removeEventListener("error", failed);
      signal?.removeEventListener("abort", aborted);
    };
    const ready = () => {
      cleanup();
      resolve();
    };
    const failed = () => {
      cleanup();
      reject(video.error ?? new Error("video load failed"));
    };
    const aborted = () => {
      cleanup();
      reject(signal.reason);
    };
    if (signal?.aborted) return reject(signal.reason);
    void activeTimeout(timeoutMs, timer.signal).then(() => {
      cleanup();
      reject(new Error("video load timed out"));
    });
    video.addEventListener("loadeddata", ready, { once: true });
    video.addEventListener("error", failed, { once: true });
    signal?.addEventListener("abort", aborted, { once: true });
    video.load();
  });
}

/** Releases a media element's media. */
export function unloadVideo(video) {
  video.removeAttribute("src");
  video.load();
}

export async function seekVideoFrame(video, seconds, tolerance = 0.001) {
  if (
    !video.seeking &&
    Math.abs(video.currentTime - seconds) <= tolerance &&
    video.readyState >= 2
  )
    return;
  await new Promise((resolve, reject) => {
    const timeout = new AbortController();
    const cleanup = () => {
      timeout.abort();
      video.removeEventListener("seeked", done);
      video.removeEventListener("error", failed);
    };
    const done = () => {
      if (video.seeking) return;
      cleanup();
      resolve();
    };
    const failed = () => {
      cleanup();
      reject(video.error ?? new Error("video seek failed"));
    };
    // Active time, so a page frozen mid-seek does not time out.
    void activeTimeout(10_000, timeout.signal).then(() => {
      cleanup();
      reject(new Error("video seek timed out"));
    });
    video.addEventListener("seeked", done);
    video.addEventListener("error", failed);
    // currentTime changes before the new frame is available, so wait on a pending seek to the
    // target.
    if (!video.seeking || Math.abs(video.currentTime - seconds) > tolerance)
      video.currentTime = seconds;
  });
}
