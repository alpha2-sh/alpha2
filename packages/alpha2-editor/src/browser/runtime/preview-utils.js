// Keep the source's display aspect ratio; transparent padding belongs to the clip and participates
// in its transform and transition.
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

export async function seekVideoFrame(video, seconds, tolerance = 0.001) {
  if (
    !video.seeking &&
    Math.abs(video.currentTime - seconds) <= tolerance &&
    video.readyState >= 2
  )
    return;
  await new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timeout);
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
    const timeout = setTimeout(() => {
      cleanup();
      reject(new Error("video seek timed out"));
    }, 10_000);
    video.addEventListener("seeked", done);
    video.addEventListener("error", failed);
    // currentTime changes before the new frame is available; wait on a pending seek to this target.
    if (!video.seeking || Math.abs(video.currentTime - seconds) > tolerance)
      video.currentTime = seconds;
  });
}
