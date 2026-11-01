// GPU export harness checks, testable without a GPU. Each returns null or the failure reason.

import { createHash } from "node:crypto";
import { inflateRawSync } from "node:zlib";

// Encoders measured 43.8-49.7 dB on this fixture and the defects it catches 10.8-29.5 dB.
export const PSNR_Y_FLOOR_DB = 38;

// Measured probe overhead was under 1.1 s; 5 s absorbs a loaded runner and stays well under the
// 30 s probe bound.
export const STARTUP_SLACK_MS = 5_000;

// Parsed as the host's `configured_gpu_wait_timeout` does.
export function gpuWaitTimeoutMs(value) {
  const parsed = /^\d+$/.test(value ?? "") ? Number(value) : 0;
  return parsed > 0 ? parsed : 30_000;
}

/** As `EncoderInputLayout::Nv12.frame_bytes` counts them. */
export function nv12FrameBytes(width, height) {
  const luma = width * height;
  return luma + Math.floor(luma / 2);
}

/** One copy per plane per frame; null for a handoff that copies nothing (`direct-target`). */
export function encoderInputCopyLedger(inputHandoff, frames, width, height) {
  if (inputHandoff !== "gpu-copy") return null;
  return {
    operations: 2 * frames,
    bytes: frames * nv12FrameBytes(width, height),
  };
}

export function copyLedgerProblem(performance, ledger) {
  if (ledger === null) return null;
  const operations = performance?.encoderInputCopyOperations;
  const bytes = performance?.encoderInputCopyBytes;
  if (operations === ledger.operations && bytes === ledger.bytes) return null;
  return (
    `the encoder-input copies were ${JSON.stringify(operations)} operations of ` +
    `${JSON.stringify(bytes)} B, expected ${ledger.operations} of ${ledger.bytes} B`
  );
}

export function psnr(actual, reference) {
  if (actual.length !== reference.length || actual.length === 0) {
    throw new Error(
      `a PSNR over ${actual.length} samples against ${reference.length}`,
    );
  }
  let squared = 0;
  for (let index = 0; index < actual.length; index += 1) {
    const difference = actual[index] - reference[index];
    squared += difference * difference;
  }
  if (squared === 0) return Infinity;
  return 10 * Math.log10((255 * 255 * actual.length) / squared);
}

export function psnrFloorProblems(samples, floor = PSNR_Y_FLOOR_DB) {
  if (samples.length === 0) return ["no frame was sampled"];
  return samples
    .filter((sample) => !(sample.psnr >= floor))
    .map(
      (sample) =>
        `frame ${sample.frame} is ${Number(sample.psnr).toFixed(2)} dB PSNR-Y, below the ${floor} dB floor`,
    );
}

// `render_cache.rs` layout: magic, version, width, height, YUV encoding (8 bytes), planes' length
// (u64), their SHA-256, then the deflated Y, U and V planes.
const CACHE_MAGIC = "A2RC";
const CACHE_FRAME_VERSION = 3;
const CACHE_HEADER_BYTES = 64;

export function renderCacheLuma(bytes) {
  if (bytes.length < CACHE_HEADER_BYTES) {
    throw new Error("a render-cache frame shorter than its header");
  }
  if (bytes.toString("latin1", 0, 4) !== CACHE_MAGIC) {
    throw new Error("a render-cache frame without its magic");
  }
  const version = bytes.readUInt32LE(4);
  if (version !== CACHE_FRAME_VERSION) {
    throw new Error(
      `a render-cache frame of version ${version}; this harness reads version ${CACHE_FRAME_VERSION}`,
    );
  }
  const width = bytes.readUInt32LE(8);
  const height = bytes.readUInt32LE(12);
  const length = Number(bytes.readBigUInt64LE(24));
  const luma = width * height;
  const expected = luma + 2 * Math.ceil(width / 2) * Math.ceil(height / 2);
  const planes = inflateRawSync(bytes.subarray(CACHE_HEADER_BYTES));
  if (length !== expected || planes.length !== expected) {
    throw new Error(
      `a ${width}x${height} render-cache frame of ${planes.length} B (header ${length} B), expected ${expected} B`,
    );
  }
  const digest = createHash("sha256").update(planes).digest();
  if (!digest.equals(bytes.subarray(32, CACHE_HEADER_BYTES))) {
    throw new Error("a render-cache frame whose planes fail its checksum");
  }
  return { width, height, y: planes.subarray(0, luma) };
}

export function unavailableCapabilityProblem(gpu) {
  if (gpu?.available !== false) {
    return (
      `gpuOutputPath.available is ${JSON.stringify(gpu?.available)}, expected false: this host ` +
      "has a GPU-resident path, so run with ALPHA2_REQUIRE=gpu-resident here, or set ALPHA2_GPU_ENCODE=0"
    );
  }
  if (typeof gpu.reason !== "string" || gpu.reason.trim() === "") {
    return `gpuOutputPath.reason is ${JSON.stringify(gpu.reason)}, expected the reason the path is unavailable`;
  }
  return null;
}

export function readbackFallbackProblem(exported, frames) {
  if (exported.status !== 0) {
    return `the auto export exited ${exported.status}, expected 0`;
  }
  const result = exported.result ?? {};
  if (result.videoPipeline !== "readback") {
    return `videoPipeline is ${JSON.stringify(result.videoPipeline)}, expected "readback"`;
  }
  if (result.performance?.videoPipeline !== "readback") {
    return `performance.videoPipeline is ${JSON.stringify(result.performance?.videoPipeline)}, expected "readback"`;
  }
  if (
    typeof result.gpuOutputPathUnavailable !== "string" ||
    result.gpuOutputPathUnavailable.trim() === ""
  ) {
    return `gpuOutputPathUnavailable is ${JSON.stringify(result.gpuOutputPathUnavailable)}, expected the reason the GPU path was not taken`;
  }
  if (result.frames !== frames) {
    return `the auto export wrote ${JSON.stringify(result.frames)} frames, expected ${frames}`;
  }
  if (!exported.outputExists) return "the auto export left no output";
  return null;
}

export function gpuRequiredRefusalProblem(answer, leftBehind) {
  if (answer?.ok !== false) {
    return `a gpu-required export answered ${JSON.stringify(answer)?.slice(0, 500)}, expected a refusal`;
  }
  if (answer.error?.code !== "GPU_OUTPUT_UNAVAILABLE") {
    return (
      `a gpu-required export failed with ${JSON.stringify(answer.error?.code)}, expected ` +
      `"GPU_OUTPUT_UNAVAILABLE": ${answer.error?.message ?? ""}`
    );
  }
  if (
    !/GPU-resident output is required but unavailable: \S/.test(
      answer.error.message ?? "",
    )
  ) {
    return `the refusal did not say why: ${JSON.stringify(answer.error.message)}`;
  }
  if (leftBehind.length > 0) {
    return `the refused export left ${leftBehind.join(", ")} behind`;
  }
  return null;
}

export function startupProblem({
  probedMs,
  baselineMs,
  timeoutMs,
  slackMs = STARTUP_SLACK_MS,
}) {
  const bound = baselineMs + timeoutMs + slackMs;
  if (probedMs <= bound) return null;
  return (
    `the host took ${Math.round(probedMs)} ms to answer with the GPU encode probe and ` +
    `${Math.round(baselineMs)} ms without it, over the ${timeoutMs} ms probe bound plus ${slackMs} ms`
  );
}
