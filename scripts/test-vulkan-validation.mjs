#!/usr/bin/env node

// Runs hardware-decoded, GPU-resident exports (one per process, and several in one `serve`
// process) and `alpha2 probe-hwdecode` behind the Khronos validation layer with synchronization
// validation; fails on any hazard or unknown validation message.
//
// Needs a 1.4 layer (1.3.x crashes FFmpeg's video sessions): point VK_LAYER_PATH at its
// `explicit_layer.d` and add its `lib` to LD_LIBRARY_PATH. Without one this fails rather than
// skips. ALPHA2_VULKAN_DEVICE picks the probe's device; logs go to target/vulkan-validation (or
// `ALPHA2_VALIDATION_LOGS`), one file per export since the layer truncates the file it opens.

import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import {
  checkValidationLog,
  DECODE_KNOWN,
  findValidationLayer,
  KNOWN,
} from "./lib/vulkan-validation-log.mjs";

const root = resolve(import.meta.dirname, "..");
const host =
  process.env.ALPHA2_HOST_BINARY ?? join(root, "target/release/alpha2-host");
const native =
  process.env.ALPHA2_NATIVE_BINARY ?? join(root, "target/release/alpha2");
// One export's limit; behind the layer an export runs many times slower than without it.
const TIMEOUT_MS = Number(process.env.ALPHA2_GPU_EXPORT_TIMEOUT_MS ?? 600_000);

const CASES = [
  {
    name: "h264-hardware",
    fixture: "h264-long-gop.mp4",
    decode: "hardware-required",
  },
  // H.264 High 10 exercises first-frame hardware failure, software fallback and decoder teardown
  // before device destruction.
  { name: "h264-high10-fallback", fixture: "h264-high10.mp4", decode: "auto" },
  // A clip at the output's own size and color: decoder surfaces are copied straight into the
  // encoder's input, with no graph.
  {
    name: "h264-yuv-transcode",
    fixture: "h264-bt709-transcode.mp4",
    decode: "hardware-required",
    size: [256, 144],
    // Whole frames: a range ending mid-frame shows the background in its last one.
    ticks: 60_000,
    transcode: "yuv-repack",
  },
  {
    name: "hevc-main10-hardware",
    fixture: "hevc-main10-padded.mp4",
    decode: "hardware-required",
  },
];

// One `serve` process running several exports on its one device: leftovers add up and the layer
// reports them at vkDestroyDevice.
const SERVES = [
  { name: "serve-h264-hardware", fixture: "h264-long-gop.mp4", exports: 3 },
];

// The decode probe encodes nothing, so the encoder's known messages fail it.
// hevc-main10-padded.mp4 decodes in hardware on the 4090 and RADV: P010 views run here.
const PROBES = [
  { name: "probe-h264", fixture: "h264-long-gop.mp4", frames: 8 },
  { name: "probe-hevc-main10", fixture: "hevc-main10-padded.mp4", frames: 8 },
];

const layer = findValidationLayer(process.env.VK_LAYER_PATH);
if (layer.problem !== undefined) {
  process.stdout.write(`FAIL ${layer.problem}\n`);
  process.exit(1);
}
if (!existsSync(host)) {
  process.stdout.write(
    `FAIL no host at ${host}; run cargo build --release -p alpha2-host\n`,
  );
  process.exit(1);
}
if (!existsSync(native)) {
  process.stdout.write(
    `FAIL no alpha2 CLI at ${native}; run cargo build --release -p alpha2-native --features hwdecode\n`,
  );
  process.exit(1);
}

const scratch = mkdtempSync(join(tmpdir(), "alpha2-vulkan-validation-"));
const logs =
  process.env.ALPHA2_VALIDATION_LOGS ?? join(root, "target/vulkan-validation");
mkdirSync(logs, { recursive: true });
let failures = 0;
let adapter = process.env.ALPHA2_VULKAN_DEVICE;
try {
  for (const testCase of CASES) {
    const report = run(testCase);
    if (report === null) failures += 1;
    else adapter ??= report.adapter;
  }
  for (const testCase of SERVES) failures += serve(testCase) ? 0 : 1;
  for (const testCase of PROBES) failures += probe(testCase, adapter) ? 0 : 1;
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
process.exit(failures === 0 ? 0 : 1);

function layerEnvironment(log) {
  return {
    ...process.env,
    VK_INSTANCE_LAYERS: "VK_LAYER_KHRONOS_validation",
    VK_KHRONOS_VALIDATION_VALIDATE_SYNC: "true",
    VK_KHRONOS_VALIDATION_DEBUG_ACTION: "VK_DBG_LAYER_ACTION_LOG_MSG",
    VK_KHRONOS_VALIDATION_LOG_FILENAME: log,
    VK_KHRONOS_VALIDATION_DUPLICATE_MESSAGE_LIMIT: "1000",
  };
}

// The log's problems against `known`, with a summary of the known messages it holds.
function judge(log, known) {
  const { problems, counts } = checkValidationLog(
    existsSync(log) ? readFileSync(log, "utf8") : null,
    known,
  );
  const summary = [...counts]
    .sort((a, b) => b[1] - a[1])
    .map(([id, count]) => `${count} ${id}`)
    .join(", ");
  return { problems, summary: summary || "none" };
}

// Runs the decode probe on `fixture` behind the layer, on the device named `adapter` (FFmpeg picks
// its first device otherwise, which may have no video decode).
function probe({ name, fixture, frames }, adapter) {
  const log = join(logs, `${name}.log`);
  rmSync(log, { force: true });
  const result = spawnSync(
    native,
    ["probe-hwdecode", join(root, "tests/corpus", fixture), String(frames)],
    {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      timeout: TIMEOUT_MS,
      killSignal: "SIGKILL",
      env: {
        ...layerEnvironment(log),
        ...(adapter === undefined ? {} : { ALPHA2_VULKAN_DEVICE: adapter }),
      },
    },
  );
  if (result.error?.code === "ETIMEDOUT") {
    process.stdout.write(
      `FAIL ${name}: the probe did not finish within ${TIMEOUT_MS / 1000} s and was killed\n`,
    );
    return false;
  }
  if (result.status !== 0) {
    process.stdout.write(
      `FAIL ${name}: probe-hwdecode exited ${result.status ?? result.signal}\n${result.stderr.slice(-2000)}\n`,
    );
    return false;
  }
  const submissions = /interop_submissions: (\d+)/.exec(result.stdout)?.[1];
  if (!(Number(submissions) > 0)) {
    process.stdout.write(
      `FAIL ${name}: the probe submitted nothing\n${result.stdout.slice(-2000)}\n`,
    );
    return false;
  }
  const { problems, summary } = judge(log, DECODE_KNOWN);
  if (problems.length !== 0) {
    process.stdout.write(`FAIL ${name}: ${problems.join("; ")} (log ${log})\n`);
    return false;
  }
  process.stdout.write(
    `ok ${name} (${submissions} submissions on ${adapter ?? "FFmpeg's first device"}; known: ${summary})\n`,
  );
  return true;
}

function exportProject(size, ticks) {
  const project = JSON.parse(
    readFileSync(
      join(root, "tests/cases/host-video-export-project.json"),
      "utf8",
    ),
  );
  [project.settings.width, project.settings.height] = size;
  for (const track of project.timeline.tracks)
    for (const clip of track.clips) {
      clip.timeline_range.end = ticks;
      clip.source_range.end = ticks;
    }
  return project;
}

// Sends `exports` hardware-decoded GPU-resident exports to one `serve` process behind the layer.
function serve({ name, fixture, exports }) {
  const ticks = 30_000;
  const project = exportProject([640, 360], ticks);
  const requests = Array.from({ length: exports }, (_, index) =>
    JSON.stringify({
      protocolVersion: 1,
      operation: "export",
      requestId: `${name}-${index}`,
      project,
      assets: { "video-source": join(root, "tests/corpus", fixture) },
      startTicks: 0,
      endTicks: ticks,
      frameRate: {
        num: project.settings.frame_rate?.num ?? 30,
        den: project.settings.frame_rate?.den ?? 1,
      },
      includeAudio: false,
      videoCodec: "h264",
      output: join(scratch, `${name}-${index}.mp4`),
    }),
  );
  const log = join(logs, `${name}.log`);
  rmSync(log, { force: true });
  const result = spawnSync(host, ["serve"], {
    input: `${requests.join("\n")}\n`,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    timeout: TIMEOUT_MS * exports,
    killSignal: "SIGKILL",
    env: { ...layerEnvironment(log), ALPHA2_VIDEO_DECODE: "hardware-required" },
  });
  if (result.error?.code === "ETIMEDOUT") {
    process.stdout.write(
      `FAIL ${name}: serve did not finish within ${(TIMEOUT_MS * exports) / 1000} s and was killed\n${(result.stderr ?? "").slice(-2000)}\n`,
    );
    return false;
  }
  if (result.status !== 0) {
    process.stdout.write(
      `FAIL ${name}: serve exited ${result.status ?? result.signal}\n${result.stderr.slice(-2000)}\n`,
    );
    return false;
  }
  const answers = result.stdout
    .split("\n")
    .filter((text) => text.startsWith("{"))
    .map((text) => JSON.parse(text))
    .filter((message) => "ok" in message);
  const resident = answers.filter(
    (answer) =>
      answer.ok === true && answer.result?.videoPipeline === "gpu-resident",
  );
  if (answers.length !== exports || resident.length !== exports) {
    process.stdout.write(
      `FAIL ${name}: ${resident.length} of ${exports} exports took the GPU-resident path (${answers.length} answers)\n${result.stdout.slice(-2000)}\n`,
    );
    return false;
  }
  const { problems, summary } = judge(log, KNOWN);
  if (problems.length !== 0) {
    process.stdout.write(`FAIL ${name}: ${problems.join("; ")} (log ${log})\n`);
    return false;
  }
  process.stdout.write(
    `ok ${name} (${exports} exports in one process; known: ${summary})\n`,
  );
  return true;
}

// Runs one export behind the layer, and returns its result, or null when it fails.
function run({
  name,
  fixture,
  decode,
  size = [640, 360],
  ticks = 30_000,
  transcode = null,
}) {
  // 64x48 is below the hardware encoders' minimum extent; a quarter second fills the pipeline.
  const project = exportProject(size, ticks);
  const projectPath = join(scratch, `${name}.json`);
  writeFileSync(projectPath, JSON.stringify(project));
  const log = join(logs, `${name}.log`);
  rmSync(log, { force: true });
  const result = spawnSync(
    host,
    [
      "export",
      "--project",
      projectPath,
      "--asset",
      `video-source=${join(root, "tests/corpus", fixture)}`,
      "--out",
      join(scratch, `${name}.mp4`),
      "--no-audio",
      "--video-codec",
      "h264",
      "--video-decode",
      decode,
    ],
    {
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      // A hung export is killed and reported rather than holding the run until the job's limit.
      timeout: TIMEOUT_MS,
      killSignal: "SIGKILL",
      env: layerEnvironment(log),
    },
  );
  if (result.error?.code === "ETIMEDOUT") {
    process.stdout.write(
      `FAIL ${name}: the export did not finish within ${TIMEOUT_MS / 1000} s and was killed\n${(result.stderr ?? "").slice(-2000)}\n`,
    );
    return null;
  }
  if (result.status !== 0) {
    process.stdout.write(
      `FAIL ${name}: export exited ${result.status ?? result.signal}\n${result.stderr.slice(-2000)}\n`,
    );
    return null;
  }
  // The layer prints a line of its own to stdout before the host's JSON.
  const line = result.stdout.split("\n").find((text) => text.startsWith("{"));
  const report = JSON.parse(line ?? "{}").result ?? {};
  if (report.videoPipeline !== "gpu-resident") {
    process.stdout.write(
      `FAIL ${name}: the export took ${report.videoPipeline}, not the GPU-resident path\n`,
    );
    return null;
  }
  if (report.videoTranscode !== transcode) {
    process.stdout.write(
      `FAIL ${name}: videoTranscode is ${report.videoTranscode}, not ${transcode} (${report.yuvTranscodeIneligible})\n`,
    );
    return null;
  }
  const { problems, summary } = judge(log, KNOWN);
  if (problems.length !== 0) {
    process.stdout.write(`FAIL ${name}: ${problems.join("; ")} (log ${log})\n`);
    return null;
  }
  process.stdout.write(
    `ok ${name} (${report.performance?.hardwareDecodedFrames ?? 0} hardware frames; known: ${summary})\n`,
  );
  return report;
}
