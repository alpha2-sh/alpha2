// What an export frame holds in flight, computed without loading the runtime. Checked against the
// renderer's `yuv420p_staging_bytes` by `tests/cases/staging-bytes-corpus.json`.

/** The canary the renderer stages after the planes, to prove the GPU wrote them. */
const CANARY_BYTES = 16;

/**
 * Plane bytes of a `width`x`height` I420 readback as the renderer lays them out: rows padded to
 * 256-byte copy alignment, each plane on a 512-byte boundary. The packed encoder frame is this
 * long.
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
 * Staged bytes of the engine's I420 readback (planes plus canary), as a ticket's `retainedBytes`.
 */
export function estimatedStagingBytes(width, height) {
  return planeBytes(width, height) + CANARY_BYTES;
}

/** The minimum in-flight bytes of an export: one staged readback plus one packed encoder frame. */
export function minimumExportWorkingSet(width, height) {
  return estimatedStagingBytes(width, height) + planeBytes(width, height);
}
