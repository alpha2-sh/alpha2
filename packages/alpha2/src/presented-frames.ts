// Presented VideoFrame uploads. Where WebGPU refuses a VideoFrame (Firefox), the engine draws each
// frame on a 2D canvas on the calling thread; a bitmap from `createImageBitmap` skips that draw.

import type { FrameUploadReceipt } from "./effects.js";

/** The engine calls {@link uploadPresentedFrame} makes. */
export interface PresentedFrameEngine {
  /** `"bitmap"` where the engine copies frames through a canvas after WebGPU refused one. */
  readonly videoFrameCopy?: string;
  uploadPresentedVideoFrame(
    sourceId: string,
    mediaTimeTicks: bigint,
    frame: VideoFrame,
    width: number,
    height: number,
    rotation: number,
    mirror: boolean,
  ): FrameUploadReceipt;
  /** `uploadPresentedVideoFrame` for a bitmap made with `premultiplyAlpha: "none"`. */
  uploadPresentedImageBitmap?(
    sourceId: string,
    mediaTimeTicks: bigint,
    bitmap: ImageBitmap,
    width: number,
    height: number,
    rotation: number,
    mirror: boolean,
  ): FrameUploadReceipt;
}

/** Whether {@link uploadPresentedFrame} sends `engine` bitmaps instead of frames. */
export function sendsBitmaps(engine: PresentedFrameEngine): boolean {
  return (
    engine.videoFrameCopy === "bitmap" &&
    typeof engine.uploadPresentedImageBitmap === "function" &&
    typeof createImageBitmap === "function"
  );
}

/**
 * Uploads `frame`, as a bitmap where {@link sendsBitmaps}, and closes it either way. A frame that
 * cannot be made into a bitmap is sent as it is.
 */
export async function uploadPresentedFrame(
  engine: PresentedFrameEngine,
  sourceId: string,
  mediaTimeTicks: bigint,
  frame: VideoFrame,
  width: number,
  height: number,
  rotation: number,
  mirror: boolean,
): Promise<FrameUploadReceipt> {
  const send = () =>
    engine.uploadPresentedVideoFrame(
      sourceId,
      mediaTimeTicks,
      frame,
      width,
      height,
      rotation,
      mirror,
    );
  if (!sendsBitmaps(engine) || engine.uploadPresentedImageBitmap === undefined)
    return send();
  let bitmap: ImageBitmap;
  try {
    // The display size, as the engine's canvas draws it.
    bitmap = await createImageBitmap(frame, {
      premultiplyAlpha: "none",
      resizeWidth: frame.displayWidth,
      resizeHeight: frame.displayHeight,
    });
  } catch {
    return send();
  }
  frame.close();
  return engine.uploadPresentedImageBitmap(
    sourceId,
    mediaTimeTicks,
    bitmap,
    width,
    height,
    rotation,
    mirror,
  );
}
