/**
 * Bytes script copied to pack a `VideoFrame`'s planes tight, kept per frame object. A frame posted
 * to another agent arrives there as another object, so whoever transfers one carries its count
 * across and records it on the frame that arrives.
 */
const repackedBytes = new WeakMap<VideoFrame, number>();

/** Records that building `frame` copied `bytes`; 0 records nothing. */
export function recordRepackedBytes(frame: VideoFrame, bytes: number): void {
  if (bytes > 0) repackedBytes.set(frame, bytes);
}

/** Bytes recorded for `frame`; 0 for a frame nothing was recorded for. */
export function repackedBytesOf(frame: VideoFrame): number {
  return repackedBytes.get(frame) ?? 0;
}
