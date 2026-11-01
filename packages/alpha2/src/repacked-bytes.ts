/**
 * Bytes copied to pack a `VideoFrame`'s planes tight. A frame posted to another agent arrives as a
 * new object, so whoever transfers one must re-record its count on the arriving frame.
 */
const repackedBytes = new WeakMap<VideoFrame, number>();

export function recordRepackedBytes(frame: VideoFrame, bytes: number): void {
  if (bytes > 0) repackedBytes.set(frame, bytes);
}

export function repackedBytesOf(frame: VideoFrame): number {
  return repackedBytes.get(frame) ?? 0;
}
