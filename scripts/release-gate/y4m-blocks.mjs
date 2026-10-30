#!/usr/bin/env node
// Compares 8-bit 4:2:0 y4m captures (ALPHA2_EXPORT_FRAMES_Y4M) of one project against a reference
// capture of the same project (another adapter, or ALPHA2_RENDERER=cpu) and describes the luma
// regions that differ: stale-memory corruption shows as block-aligned regions of one value, while
// rounding differences stay within a few codes.
//
// usage: y4m-blocks.mjs REFERENCE.y4m RUN.y4m [RUN.y4m ...]
// Prints `corrupt <n>` per run; exits 1 when any run has a corrupt frame or cannot be compared.
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

/** Luma samples further than this many codes from the reference make a frame corrupt. */
export const CODE_TOLERANCE = 4;

/**
 * Parses an 8-bit 4:2:0 y4m file into its size and luma planes. Throws on a high-bit-depth or
 * non-4:2:0 header, since the comparison reads one byte per luma sample.
 */
export function parseY4m(bytes, label = "y4m") {
  const headerEnd = bytes.indexOf(0x0a);
  if (headerEnd < 0) throw new Error(`${label}: no y4m header`);
  const header = bytes.subarray(0, headerEnd).toString("latin1");
  const tokens = header.split(" ");
  if (tokens[0] !== "YUV4MPEG2") throw new Error(`${label}: not a y4m file`);
  const fields = new Map(
    tokens.slice(1).map((token) => [token[0], token.slice(1)]),
  );
  const width = Number(fields.get("W"));
  const height = Number(fields.get("H"));
  if (
    !Number.isInteger(width) ||
    !Number.isInteger(height) ||
    width <= 0 ||
    height <= 0
  )
    throw new Error(`${label}: header has no frame size`);
  const colorspace = fields.get("C") ?? "420";
  if (/p\d+$/.test(colorspace))
    throw new Error(
      `${label}: a high-bit-depth y4m (C${colorspace}); this tool reads 8-bit captures`,
    );
  if (!colorspace.startsWith("420"))
    throw new Error(`${label}: chroma C${colorspace} is not 4:2:0`);
  const lumaSize = width * height;
  const chromaSize = 2 * Math.ceil(width / 2) * Math.ceil(height / 2);
  const frames = [];
  let offset = headerEnd + 1;
  while (offset < bytes.length) {
    const lineEnd = bytes.indexOf(0x0a, offset);
    if (
      lineEnd < 0 ||
      bytes.subarray(offset, offset + 5).toString("latin1") !== "FRAME"
    )
      throw new Error(`${label}: frame ${frames.length} has no FRAME line`);
    const start = lineEnd + 1;
    if (start + lumaSize + chromaSize > bytes.length)
      throw new Error(`${label}: frame ${frames.length} is truncated`);
    frames.push(bytes.subarray(start, start + lumaSize));
    offset = start + lumaSize + chromaSize;
  }
  return { width, height, frames };
}

/** Connected regions (4-connectivity) of set samples in `mask`, as bounding boxes and members. */
export function regions(mask, width, height) {
  const seen = new Uint8Array(width * height);
  const found = [];
  for (let index = 0; index < mask.length; index++) {
    if (!mask[index] || seen[index]) continue;
    const stack = [index];
    seen[index] = 1;
    const members = [];
    let [minX, minY, maxX, maxY] = [width, height, -1, -1];
    while (stack.length > 0) {
      const sample = stack.pop();
      const x = sample % width;
      const y = (sample - x) / width;
      members.push(sample);
      minX = Math.min(minX, x);
      minY = Math.min(minY, y);
      maxX = Math.max(maxX, x);
      maxY = Math.max(maxY, y);
      for (const [dx, dy] of [
        [1, 0],
        [-1, 0],
        [0, 1],
        [0, -1],
      ]) {
        const nx = x + dx;
        const ny = y + dy;
        if (nx < 0 || ny < 0 || nx >= width || ny >= height) continue;
        const next = ny * width + nx;
        if (mask[next] && !seen[next]) {
          seen[next] = 1;
          stack.push(next);
        }
      }
    }
    found.push({
      x: minX,
      y: minY,
      width: maxX - minX + 1,
      height: maxY - minY + 1,
      members,
    });
  }
  return found;
}

function countInto(map, key, by = 1) {
  map.set(key, (map.get(key) ?? 0) + by);
}

function mostCommon(map, limit) {
  return [...map.entries()].sort((a, b) => b[1] - a[1]).slice(0, limit);
}

/** Compares one run against the reference; both as returned by `parseY4m`. */
export function compareRun(reference, run) {
  if (
    run.width !== reference.width ||
    run.height !== reference.height ||
    run.frames.length !== reference.frames.length
  ) {
    return {
      mismatch: `size or frame count differs from the reference (${run.width}x${run.height}x${run.frames.length})`,
    };
  }
  const { width, height } = reference;
  const corrupt = [];
  const sizes = new Map();
  const origins = new Map();
  const values = new Map();
  run.frames.forEach((frame, number) => {
    const expected = reference.frames[number];
    const mask = new Uint8Array(frame.length);
    let samples = 0;
    let maxDiff = 0;
    for (let index = 0; index < frame.length; index++) {
      const diff = Math.abs(frame[index] - expected[index]);
      maxDiff = Math.max(maxDiff, diff);
      if (diff > CODE_TOLERANCE) {
        mask[index] = 1;
        samples++;
      }
    }
    if (samples === 0) return;
    const found = regions(mask, width, height);
    corrupt.push({ frame: number, maxDiff, samples, regions: found.length });
    for (const region of found) {
      countInto(sizes, `${region.width}x${region.height}`);
      countInto(origins, `${region.x % 8},${region.y % 8}`);
      for (const member of region.members) countInto(values, frame[member]);
    }
  });
  const bad = new Set(corrupt.map((entry) => entry.frame));
  const clean = run.frames
    .map((_, number) => number)
    .filter((number) => !bad.has(number));
  return {
    frames: run.frames.length,
    corrupt,
    firstClean: clean.slice(0, 6),
    sizes: mostCommon(sizes, 8),
    origins: mostCommon(origins, 6),
    values: mostCommon(values, 6),
  };
}

function report(path, result) {
  if (result.mismatch) return [`${path}: ${result.mismatch}`];
  const first = result.corrupt
    .slice(0, 6)
    .map(
      (entry) =>
        `(${entry.frame}, ${entry.maxDiff}, ${entry.samples}, ${entry.regions})`,
    )
    .join(" ");
  return [
    `${path}: frames ${result.frames}, corrupt ${result.corrupt.length}, first clean frames [${result.firstClean.join(", ")}]`,
    `  first corrupt (frame, max code diff, samples > ${CODE_TOLERANCE}, regions): ${first || "none"}`,
    `  region sizes (w x h): ${result.sizes.map(([key, n]) => `${key} x${n}`).join(", ") || "none"}`,
    `  region origins (x mod 8, y mod 8): ${result.origins.map(([key, n]) => `(${key}) x${n}`).join(", ") || "none"}`,
    `  luma values inside regions: ${result.values.map(([key, n]) => `${key} x${n}`).join(", ") || "none"}`,
  ];
}

/** Runs the comparison over file paths; returns the process exit code. */
export function main(args, log = console.log) {
  if (args.length < 2) {
    log("usage: y4m-blocks.mjs REFERENCE.y4m RUN.y4m [RUN.y4m ...]");
    return 2;
  }
  const [referencePath, ...runPaths] = args;
  const reference = parseY4m(readFileSync(referencePath), referencePath);
  let failed = false;
  for (const path of runPaths) {
    const result = compareRun(reference, parseY4m(readFileSync(path), path));
    for (const line of report(path, result)) log(line);
    if (result.mismatch || result.corrupt.length > 0) failed = true;
  }
  return failed ? 1 : 0;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    process.exitCode = main(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    process.exitCode = 2;
  }
}
