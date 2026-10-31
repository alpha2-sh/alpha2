/**
 * Export frames arrive as I420 with rows padded to the GPU copy alignment, read through `layout`.
 * Safari ignores `layout` in the buffer constructor and reads the planes as tightly packed, which
 * shears the picture, so support is probed once and the planes are repacked where it is missing.
 */

import { recordRepackedBytes, repackedBytesOf } from "./repacked-bytes.js";

type TransferVideoFrameBufferInit = VideoFrameBufferInit & {
  readonly transfer?: ArrayBuffer[];
};

let layoutHonoured: Promise<boolean> | undefined;
let bufferTransferSupported: boolean | undefined;

/** Whether this browser's `VideoFrame` buffer constructor reads planes through `layout`. */
export function videoFrameHonoursLayout(): Promise<boolean> {
  layoutHonoured ??= measureLayoutHonoured();
  return layoutHonoured;
}

async function measureLayoutHonoured(): Promise<boolean> {
  // 2x2 I420 with luma stride 8 and 0xff padding; a browser ignoring `layout` returns padding.
  const pixels = new Uint8Array(20).fill(0xff);
  pixels.set([10, 20], 0);
  pixels.set([30, 40], 8);
  pixels[16] = 50;
  pixels[18] = 60;
  let frame: VideoFrame | undefined;
  try {
    frame = new VideoFrame(pixels, {
      format: "I420",
      codedWidth: 2,
      codedHeight: 2,
      timestamp: 0,
      layout: [
        { offset: 0, stride: 8 },
        { offset: 16, stride: 2 },
        { offset: 18, stride: 2 },
      ],
    });
    const out = new Uint8Array(frame.allocationSize());
    const planes = await frame.copyTo(out);
    const sample = (plane: number, row: number, column: number) =>
      out[planes[plane].offset + row * planes[plane].stride + column];
    return (
      sample(0, 0, 0) === 10 &&
      sample(0, 0, 1) === 20 &&
      sample(0, 1, 0) === 30 &&
      sample(0, 1, 1) === 40 &&
      sample(1, 0, 0) === 50 &&
      sample(2, 0, 0) === 60
    );
  } catch {
    // A browser that rejects the probe cannot be trusted with padded planes either.
    return false;
  } finally {
    frame?.close();
  }
}

/**
 * Builds an I420 frame over `pixels`, transferring the buffer where possible. Repacks the planes
 * where the browser ignores `layout`; {@link videoFrameRepackedBytes} reports that copy.
 */
export async function videoFrameFromI420(
  pixels: Uint8Array,
  init: VideoFrameBufferInit,
): Promise<VideoFrame> {
  // Alpha and 10-bit frames arrive with tight planes.
  if (init.format !== "I420") return ownedVideoFrame(pixels, init);
  const layout = init.layout ? [...init.layout] : undefined;
  if (
    layout === undefined ||
    isTightI420(layout, init.codedWidth, init.codedHeight) ||
    (await videoFrameHonoursLayout())
  ) {
    return ownedVideoFrame(pixels, init);
  }
  const tight = packI420(pixels, layout, init.codedWidth, init.codedHeight);
  // Read before the transfer detaches the buffer.
  const bytes = tight.byteLength;
  const frame = ownedVideoFrame(tight, { ...init, layout: undefined });
  recordRepackedBytes(frame, bytes);
  return frame;
}

/** Bytes copied to build `frame` because the browser ignores `layout`, or 0. */
export function videoFrameRepackedBytes(frame: VideoFrame): number {
  return repackedBytesOf(frame);
}

function i420Planes(width: number, height: number) {
  const chromaWidth = Math.ceil(width / 2);
  const chromaHeight = Math.ceil(height / 2);
  return [
    { width, height },
    { width: chromaWidth, height: chromaHeight },
    { width: chromaWidth, height: chromaHeight },
  ];
}

function isTightI420(
  layout: readonly PlaneLayout[],
  width: number,
  height: number,
): boolean {
  let offset = 0;
  return i420Planes(width, height).every((plane, index) => {
    const tight =
      layout[index]?.offset === offset && layout[index]?.stride === plane.width;
    offset += plane.width * plane.height;
    return tight;
  });
}

function packI420(
  pixels: Uint8Array,
  layout: readonly PlaneLayout[],
  width: number,
  height: number,
): Uint8Array {
  const planes = i420Planes(width, height);
  const tight = new Uint8Array(
    planes.reduce((bytes, plane) => bytes + plane.width * plane.height, 0),
  );
  let written = 0;
  planes.forEach((plane, index) => {
    const { offset, stride } = layout[index];
    for (let row = 0; row < plane.height; row += 1) {
      const start = offset + row * stride;
      tight.set(pixels.subarray(start, start + plane.width), written);
      written += plane.width;
    }
  });
  return tight;
}

function ownedVideoFrame(
  pixels: Uint8Array,
  init: VideoFrameBufferInit,
): VideoFrame {
  if (
    supportsBufferTransfer() &&
    pixels.buffer instanceof ArrayBuffer &&
    pixels.byteOffset === 0 &&
    pixels.byteLength === pixels.buffer.byteLength
  ) {
    const transferInit: TransferVideoFrameBufferInit = {
      ...init,
      transfer: [pixels.buffer],
    };
    return new VideoFrame(pixels, transferInit);
  }
  return new VideoFrame(pixels, init);
}

function supportsBufferTransfer(): boolean {
  if (bufferTransferSupported !== undefined) return bufferTransferSupported;
  const pixels = new Uint8Array(6);
  let frame: VideoFrame | undefined;
  try {
    const init: TransferVideoFrameBufferInit = {
      format: "I420",
      codedWidth: 2,
      codedHeight: 2,
      timestamp: 0,
      transfer: [pixels.buffer],
    };
    frame = new VideoFrame(pixels, init);
    bufferTransferSupported = pixels.buffer.byteLength === 0;
  } catch {
    bufferTransferSupported = false;
  } finally {
    frame?.close();
  }
  return bufferTransferSupported;
}
