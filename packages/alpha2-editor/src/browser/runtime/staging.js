// What an export frame holds in flight, computed without loading the runtime.
// `tests/cases/staging-bytes-corpus.json` holds these to the renderer's `yuv420p_staging_bytes`.

/** The canary the renderer stages after the planes, to prove the GPU wrote them. */
const CANARY_BYTES = 16;

/**
 * The bytes of the planes of a `width`x`height` I420 readback: each plane's rows padded to the copy
 * alignment (256), each plane starting on a 512-byte placement boundary, as the renderer lays them
 * out. The packed frame the encoder takes is this long.
 */
function planeBytes(width, height) {
  const row = (bytes) => Math.ceil(bytes / 256) * 256;
  const place = (offset) => Math.ceil(offset / 512) * 512;
  const chromaRows = Math.floor(height / 2);
  const chroma = row(Math.floor(width / 2)) * chromaRows;
  const u = place(row(width) * height);
  const v = place(u + chroma);
  return v + chroma;
}

/**
 * The bytes the engine's I420 readback of a `width`x`height` frame keeps staged: its planes and
 * the renderer's 16-byte canary, what a readback ticket reports as `retainedBytes`.
 */
export function estimatedStagingBytes(width, height) {
  return planeBytes(width, height) + CANARY_BYTES;
}

/**
 * The least an export of `width`x`height` frames can run with in flight: one frame's readback
 * staged and the packed copy of one frame the encoder holds.
 */
export function minimumExportWorkingSet(width, height) {
  return estimatedStagingBytes(width, height) + planeBytes(width, height);
}
