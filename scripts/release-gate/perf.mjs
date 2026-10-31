#!/usr/bin/env node
// The release gate's export performance runner. Measures the release host on fixed projects in
// alternating rounds, one host process per export, and summarizes per configuration: steady
// milliseconds per frame (median, min-max), spread, export fps, CPU milliseconds per frame, start
// overhead, stage shares, and ratios between configurations that differ in one axis. Absolute
// numbers are never compared across machines or runs; a comparison with a previous release's
// summary.json compares ratios and stage shares only.
//
// usage: perf.mjs --make-source                 write the projects and the decode source
//        perf.mjs [--machine lin|win|mac] [--rounds N] [--config a,b] [--sample]
//                 [--previous <summary.json>] [--out <dir>]
//        perf.mjs --summarize <dir> [--previous <summary.json>]
// Inputs live in target/release-gate/perf/ (made by --make-source when missing). Run after
// `cargo build --release -p alpha2-host`, with the machine's release-gate environment
// (scripts/release-gate/env.sh). The export benchmark is bistable on some machines: rounds rotate
// and odd rounds run reversed, so no configuration always follows the same one.

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
  appendFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { executable } from "../lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..", "..");
export const PERF_DIRECTORY = join(root, "target", "release-gate", "perf");
export const SOURCE_PATH = join(PERF_DIRECTORY, "source-1080p.mp4");
const TICKS_PER_SECOND = 120_000;
const SPREAD_LIMIT = 1.15;
const EXPORT_TIMEOUT_SECONDS = 600;
const COMMON = [
  "--no-audio",
  "--video-codec",
  "h264",
  "--video-quality",
  "8000000",
  "--overwrite",
];
const P1_ASSETS = [
  "--asset",
  `video=${join(root, "tests", "corpus", "parity-rgb.mov")}`,
  "--asset",
  `still=${join(root, "tests", "corpus", "parity-image.png")}`,
];
const COORDINATOR = [
  "decodeWaitMs",
  "renderCacheLookupMs",
  "sourceConvertUploadMs",
  "evaluateMs",
  "frameSubmitMs",
  "readbackWaitMs",
  "readbackPackMs",
  "encodeQueueWaitMs",
  "audioDecodeMs",
  "audioMixMs",
  "muxFinishMs",
];
// Families of one-axis pairs: the axis that differs and the axes that must match.
const FAMILIES = [
  ["renderer", ["project", "pipeline", "encoder", "encclass", "decode"]],
  ["encclass", ["project", "renderer", "pipeline", "decode"]],
  ["pipeline", ["project", "renderer", "encclass", "decode"]],
  ["decode", ["project", "renderer", "pipeline", "encoder", "encclass"]],
  ["project", ["renderer", "pipeline", "encoder", "encclass", "decode"]],
];

/** The configuration order of round `round` over `count` configurations. */
export function roundOrder(count, round) {
  return Array.from({ length: count }, (_, j) =>
    round % 2 === 0 ? (j + round) % count : (count - 1 - j + round) % count,
  );
}

const median = (values) => {
  const sorted = values
    .filter((v) => typeof v === "number")
    .sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2
    ? sorted[middle]
    : (sorted[middle - 1] + sorted[middle]) / 2;
};

// ------------------------------------------------------------------------------- projects

function shiftKeyframes(node, offset) {
  if (Array.isArray(node))
    for (const value of node) shiftKeyframes(value, offset);
  else if (node && typeof node === "object") {
    if (Array.isArray(node.keyframes))
      for (const keyframe of node.keyframes)
        if (keyframe && typeof keyframe === "object" && "time" in keyframe)
          keyframe.time = Number(keyframe.time) + offset;
    for (const value of Object.values(node)) shiftKeyframes(value, offset);
  }
}

const shiftRange = (range, offset) => ({
  start: range.start + offset,
  end: range.end + offset,
});
const extentOf = (project) =>
  Math.max(
    ...project.timeline.tracks.flatMap((track) =>
      track.clips.map((clip) => clip.timeline_range.end),
    ),
  );

/**
 * P1: the complex parity project at 1920x1080, 30 fps, its timeline tiled `tiles` times. Clip
 * and transition keyframes are timeline time and move with each tile; composition keyframes are
 * composition time and stay.
 */
export function tiledProject(base, tiles = 8) {
  const tile = extentOf(base);
  const project = structuredClone(base);
  project.id = `perf-p1-${tiles}x`;
  project.name = `release gate perf P1 (complex parity x${tiles}, 1080p30)`;
  project.settings = {
    ...project.settings,
    width: 1920,
    height: 1080,
    frame_rate: { num: 30, den: 1 },
  };
  for (const track of project.timeline.tracks) {
    const original = base.timeline.tracks.find((t) => t.id === track.id);
    track.clips = [];
    if (original.transitions) track.transitions = [];
    for (let i = 0; i < tiles; i++) {
      const offset = i * tile;
      for (const source of original.clips) {
        const clip = structuredClone(source);
        clip.id = `${clip.id}-t${i}`;
        clip.timeline_range = shiftRange(clip.timeline_range, offset);
        for (const key of ["visual", "masks"])
          if (key in clip) shiftKeyframes(clip[key], offset);
        track.clips.push(clip);
      }
      for (const source of original.transitions ?? []) {
        const transition = structuredClone(source);
        transition.from_clip = `${transition.from_clip}-t${i}`;
        transition.to_clip = `${transition.to_clip}-t${i}`;
        transition.timeline_range = shiftRange(
          transition.timeline_range,
          offset,
        );
        if (transition.effect) shiftKeyframes(transition.effect, offset);
        track.transitions.push(transition);
      }
    }
  }
  return project;
}

/** P1g: P1 with film grain on the video and still looks, so the decode source has entropy. */
export function grainedProject(p1) {
  const project = structuredClone(p1);
  let grained = 0;
  for (const composition of project.compositions ?? []) {
    if (!["video-clip-look", "still-clip-look"].includes(composition.id))
      continue;
    const constant = (value) => ({
      mode: "constant",
      value: { type: "float", value },
    });
    composition.nodes.push({
      id: "perf-grain",
      op: {
        type: "effect",
        input: composition.output,
        effect: {
          id: "alpha2.effect.film-grain",
          version: 1,
          params: { amount: constant(0.35), size: constant(1.4) },
        },
      },
    });
    composition.output = "perf-grain";
    grained += 1;
  }
  if (grained !== 2)
    throw new Error(
      "the complex parity project no longer has video-clip-look and still-clip-look",
    );
  project.id = project.id.replace("perf-p1", "perf-p1g");
  project.name = project.name.replace("P1", "P1g (film grain)");
  return project;
}

/**
 * P2r (`overlay`): ten seconds of the decode source with the parity badge over it, so every
 * frame renders. P2t: the source alone, eligible for the GPU-resident transcode.
 */
export function decodeProject(base, overlay, seconds = 10) {
  const end = seconds * TICKS_PER_SECOND;
  const sources = [
    { id: "src", locator: "fixture://perf-source", kind: "video" },
  ];
  const tracks = [
    {
      id: "base",
      name: "Base",
      kind: "video",
      clips: [
        {
          id: "src-clip",
          source_id: "src",
          timeline_range: { start: 0, end },
          source_range: { start: 0, end },
        },
      ],
    },
  ];
  if (overlay) {
    sources.push(structuredClone(base.sources.find((s) => s.id === "badge")));
    const badge = structuredClone(
      base.timeline.tracks.find((t) => t.id === "top").clips[0],
    );
    if (badge.source_id !== "badge")
      throw new Error(
        "the complex parity project's top track no longer starts with the badge",
      );
    badge.id = "badge-over";
    badge.timeline_range = { start: 0, end };
    badge.source_range = { start: 0, end };
    badge.visual = {
      position: { mode: "constant", value: { x: 0.75, y: 0.3 } },
      anchor: { x: 0.5, y: 0.5 },
      scale: { mode: "constant", value: { x: 0.35, y: 0.35 } },
      rotation_degrees: { mode: "constant", value: 0 },
      opacity: { mode: "constant", value: 0.9 },
    };
    tracks.push({ id: "top", name: "Top", kind: "video", clips: [badge] });
  }
  return {
    schema_version: base.schema_version,
    id: overlay ? "perf-p2r" : "perf-p2t",
    name: `release gate perf ${overlay ? "P2r (decode and overlay)" : "P2t (decode only)"}`,
    settings: {
      width: 1920,
      height: 1080,
      frame_rate: { num: 30, den: 1 },
      sample_rate: base.settings.sample_rate,
    },
    sources,
    timeline: { tracks },
  };
}

export function writeProjects(directory = PERF_DIRECTORY) {
  const base = JSON.parse(
    readFileSync(
      join(root, "tests", "cases", "host-parity-complex-project.json"),
      "utf8",
    ),
  );
  const p1 = tiledProject(base);
  const projects = {
    p1,
    p1g: grainedProject(p1),
    p2r: decodeProject(base, true),
    p2t: decodeProject(base, false),
  };
  mkdirSync(directory, { recursive: true });
  for (const [name, project] of Object.entries(projects))
    writeFileSync(
      join(directory, `${name}.json`),
      JSON.stringify(project, null, 1) + "\n",
    );
  return projects;
}

// ------------------------------------------------------------------------------- running

function hostPath() {
  return process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "release");
}

/**
 * Runs one export under bash `time` (real, user and sys of the host process; Git Bash on
 * Windows), with GNU timeout when there is one. Writes `<base>.json`, `.stderr` and `.time`.
 */
function timedExport(base, args, env) {
  const script = [
    'TIMEFORMAT="real=%3R user=%3U sys=%3S"',
    `limit=""; if timeout --version > /dev/null 2>&1; then limit="timeout ${EXPORT_TIMEOUT_SECONDS}"; fi`,
    // Windows' own timeout.exe waits for a key, so only GNU timeout is taken.
    '{ time $limit "$@" > "$A2_BASE.json" 2> "$A2_BASE.stderr"; } 2> "$A2_BASE.time"',
  ].join("\n");
  const started = Date.now() / 1000;
  const result = spawnSync(
    "bash",
    ["-c", script, "bash", hostPath(), "export", ...args],
    {
      cwd: root,
      env: { ...process.env, ...env, A2_BASE: base },
      stdio: "inherit",
    },
  );
  return { rc: result.status ?? 1, started, ended: Date.now() / 1000 };
}

function parseTime(text) {
  return Object.fromEntries(
    [...text.matchAll(/(real|user|sys)=([0-9.]+)/g)].map((m) => [
      m[1],
      Number(m[2]),
    ]),
  );
}

function parseAnswer(text) {
  const candidates = [text, ...text.split(/\r?\n/).reverse()];
  for (const candidate of candidates) {
    try {
      const value = JSON.parse(candidate);
      if (value && typeof value === "object" && "result" in value) return value;
    } catch {
      // not this one
    }
  }
  return null;
}

function parseTrace(text) {
  const [header, ...lines] = text.trim().split(/\r?\n/);
  const columns = header.split(",");
  return lines
    .map((line) =>
      Object.fromEntries(line.split(",").map((v, i) => [columns[i], v])),
    )
    .filter((row) => row.kind === "video")
    .map((row) =>
      Object.fromEntries(
        Object.entries(row).map(([k, v]) => [k, Number(v) || 0]),
      ),
    )
    .sort((a, b) => a.index - b.index);
}

/** Milliseconds per frame between the 10th and 95th percentile frames' encode completions. */
export function steadyMsPerFrame(rows) {
  if (rows.length < 3) return null;
  const finished = rows.map((row) => row.encodeFinishedMs);
  const low = Math.floor(rows.length * 0.1);
  const high = Math.max(Math.floor(rows.length * 0.95) - 1, 0);
  return high > low ? (finished[high] - finished[low]) / (high - low) : null;
}

/** One run's measures from its files under `base`. */
export function runMetrics(base, read = (path) => readFileSync(path, "utf8")) {
  const attempt = (suffix) => {
    try {
      return read(base + suffix);
    } catch {
      return "";
    }
  };
  const answer = parseAnswer(attempt(".json"));
  const timing = parseTime(attempt(".time"));
  if (!answer?.ok) {
    const stderr = attempt(".stderr").trim().split(/\r?\n/);
    return {
      ok: false,
      error: (stderr.at(-1) || "no result").slice(0, 400),
      ...timing,
    };
  }
  const result = answer.result;
  const performance = result.performance ?? {};
  const frames = Number(result.frames) || 0;
  const renderMs = Number(result.renderMs) || 0;
  const trace = attempt(".trace.csv");
  const coordinator = Object.fromEntries(
    COORDINATOR.map((key) => [key, Number(performance[key]) || 0]),
  );
  return {
    ok: true,
    frames,
    renderMs,
    exportFps: renderMs > 0 ? frames / (renderMs / 1000) : null,
    steadyMsPerFrame: trace ? steadyMsPerFrame(parseTrace(trace)) : null,
    cpuMsPerFrame:
      frames && "user" in timing
        ? ((timing.user + timing.sys) * 1000) / frames
        : null,
    startOverheadMs:
      "real" in timing && renderMs > 0 ? timing.real * 1000 - renderMs : null,
    shares: Object.fromEntries(
      Object.entries(coordinator).map(([key, value]) => [
        key,
        renderMs ? value / renderMs : null,
      ]),
    ),
    identity: {
      renderer: result.renderer ?? null,
      adapter: result.adapter ?? null,
      pipeline: result.videoPipeline ?? performance.videoPipeline ?? null,
      encoder: result.videoEncoder ?? null,
      encoderHardware: result.videoEncoderHardware ?? null,
      hardwareDecoded: Number(performance.hardwareDecodedFrames) || 0,
      softwareDecoded: Number(performance.softwareDecodedFrames) || 0,
      frames,
    },
  };
}

/** Why a configuration's runs are not a valid measurement; none when they are. */
export function validity(axes, runs) {
  const good = runs.filter((run) => run.ok);
  if (good.length === 0) return ["no successful run"];
  const problems = [];
  for (const key of Object.keys(good[0].identity)) {
    const values = [
      ...new Set(good.map((run) => JSON.stringify(run.identity[key]))),
    ];
    if (values.length > 1)
      problems.push(`${key} differs between rounds: ${values.join(", ")}`);
  }
  const run = good[0].identity;
  if (axes.renderer && run.renderer !== axes.renderer)
    problems.push(`renderer is ${run.renderer}, configured ${axes.renderer}`);
  const pipeline = { resident: "gpu-resident", readback: "readback" }[
    axes.pipeline
  ];
  if (pipeline && run.pipeline !== pipeline)
    problems.push(`videoPipeline is ${run.pipeline}, configured ${pipeline}`);
  if (axes.encoder && run.encoder !== axes.encoder)
    problems.push(`videoEncoder is ${run.encoder}, configured ${axes.encoder}`);
  if (
    axes.encclass &&
    run.encoderHardware !== null &&
    run.encoderHardware !== (axes.encclass === "hw")
  )
    problems.push(
      `videoEncoderHardware is ${run.encoderHardware}, configured ${axes.encclass}`,
    );
  if (
    axes.decode === "hw" &&
    (run.hardwareDecoded === 0 || run.softwareDecoded > 0)
  )
    problems.push(
      `hardware decode configured, decoded hw=${run.hardwareDecoded} sw=${run.softwareDecoded}`,
    );
  if (axes.decode === "sw" && run.hardwareDecoded > 0)
    problems.push(
      `software decode configured, ${run.hardwareDecoded} frames decoded in hardware`,
    );
  return problems;
}

/** A configuration's summary over its measured (not warm-up) runs. */
export function aggregate(configuration, runs) {
  const good = runs.filter((run) => run.ok);
  const steady = good.map((run) => run.steadyMsPerFrame).filter((v) => v > 0);
  const spread =
    steady.length > 1 ? Math.max(...steady) / Math.min(...steady) : null;
  const shareKeys = good.length ? Object.keys(good[0].shares) : [];
  return {
    config: configuration.name,
    axes: configuration.axes,
    runs: runs.length,
    ok: good.length,
    errors: runs.filter((run) => !run.ok).map((run) => run.error),
    steadyMsPerFrame: {
      median: median(steady),
      min: steady.length ? Math.min(...steady) : null,
      max: steady.length ? Math.max(...steady) : null,
    },
    spread,
    addRounds: spread !== null && spread > SPREAD_LIMIT,
    exportFps: median(good.map((run) => run.exportFps)),
    cpuMsPerFrame: median(good.map((run) => run.cpuMsPerFrame)),
    startOverheadMs: median(good.map((run) => run.startOverheadMs)),
    shares: Object.fromEntries(
      shareKeys.map((key) => [key, median(good.map((run) => run.shares[key]))]),
    ),
    identity: good[0]?.identity ?? {},
    validity: validity(configuration.axes, runs),
  };
}

/** Steady and CPU ratios (b over a) of every pair of configurations differing in one axis. */
export function pairs(entries) {
  const found = [];
  for (const [vary, same] of FAMILIES)
    for (const a of entries)
      for (const b of entries) {
        if (a === b || a.axes.default || b.axes.default) continue;
        if (
          !(vary in a.axes) ||
          !(vary in b.axes) ||
          a.axes[vary] >= b.axes[vary]
        )
          continue;
        if (same.some((key) => a.axes[key] !== b.axes[key])) continue;
        const ratio = (x, y) => (x && y ? y / x : null);
        found.push({
          axis: vary,
          a: a.config,
          b: b.config,
          steadyRatio: ratio(
            a.steadyMsPerFrame.median,
            b.steadyMsPerFrame.median,
          ),
          cpuRatio: ratio(a.cpuMsPerFrame, b.cpuMsPerFrame),
          valid: a.validity.length === 0 && b.validity.length === 0,
        });
      }
  return found;
}

/** This run's pair ratios and stage shares beside a previous release's: no absolute numbers. */
export function compareWith(previous, current) {
  const before = new Map(
    previous.pairs.map((pair) => [`${pair.a}|${pair.b}`, pair]),
  );
  const shares = new Map(
    previous.configurations.map((entry) => [entry.config, entry.shares]),
  );
  return {
    pairs: current.pairs
      .filter((pair) => before.has(`${pair.a}|${pair.b}`))
      .map((pair) => ({
        a: pair.a,
        b: pair.b,
        steadyRatio: pair.steadyRatio,
        previousSteadyRatio: before.get(`${pair.a}|${pair.b}`).steadyRatio,
      })),
    shares: current.configurations
      .filter((entry) => shares.has(entry.config))
      .map((entry) => ({
        config: entry.config,
        shares: entry.shares,
        previousShares: shares.get(entry.config),
      })),
  };
}

function summarize(directory, configurations, previousPath) {
  const runs = readFileSync(join(directory, "runs.tsv"), "utf8")
    .trim()
    .split("\n")
    .slice(1)
    .map((line) => line.split("\t"))
    .filter(([round]) => round !== "warm");
  const entries = configurations.map((configuration) =>
    aggregate(
      configuration,
      runs
        .filter(([, , name]) => name === configuration.name)
        .map(([round]) =>
          runMetrics(
            join(directory, "runs", `${configuration.name}-r${round}`),
          ),
        ),
    ),
  );
  const summary = { configurations: entries, pairs: pairs(entries) };
  if (previousPath)
    summary.comparison = compareWith(
      JSON.parse(readFileSync(previousPath, "utf8")),
      summary,
    );
  writeFileSync(
    join(directory, "summary.json"),
    JSON.stringify(summary, null, 2) + "\n",
  );
  const fmt = (value, digits = 2) =>
    typeof value === "number" ? value.toFixed(digits) : "-";
  const lines = [];
  for (const entry of entries) {
    const steady = entry.steadyMsPerFrame;
    lines.push(
      `${entry.validity.length ? "INVALID" : "ok"} ${entry.config}: steady ${fmt(steady.median)} ms/frame (${fmt(steady.min)}-${fmt(steady.max)}), spread ${fmt(entry.spread)}${entry.addRounds ? " (add rounds)" : ""}, ${fmt(entry.exportFps, 1)} fps, cpu ${fmt(entry.cpuMsPerFrame)} ms/frame, start ${fmt(entry.startOverheadMs, 0)} ms, ${entry.ok}/${entry.runs} ok`,
    );
    for (const problem of entry.validity) lines.push(`  ${problem}`);
    for (const error of entry.errors) lines.push(`  error: ${error}`);
  }
  for (const pair of summary.pairs)
    lines.push(
      `pair ${pair.axis}: ${pair.b} / ${pair.a} steady x${fmt(pair.steadyRatio)} cpu x${fmt(pair.cpuRatio)}${pair.valid ? "" : " (invalid)"}`,
    );
  for (const pair of summary.comparison?.pairs ?? [])
    lines.push(
      `previous ${pair.b} / ${pair.a}: x${fmt(pair.previousSteadyRatio)} -> x${fmt(pair.steadyRatio)}`,
    );
  writeFileSync(join(directory, "summary.txt"), lines.join("\n") + "\n");
  for (const line of lines) console.log(line);
  return entries.every((entry) => entry.validity.length === 0);
}

// ------------------------------------------------------------------------------- sampling

function startSampler(directory) {
  const stop = join(directory, ".stop-sampler");
  if (process.platform === "win32") {
    const child = spawn(
      "powershell",
      [
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        join(import.meta.dirname, "perf-sampler.ps1"),
        "-Out",
        join(directory, "samples-windows.csv"),
        "-Stop",
        stop,
      ],
      { stdio: "ignore" },
    );
    return () => {
      writeFileSync(stop, "");
      return new Promise((done) => child.on("exit", done));
    };
  }
  const script =
    process.platform === "darwin"
      ? `while [ ! -f "$1" ]; do date +%s; ioreg -r -d 1 -w 0 -c IOAccelerator | grep -o '"[A-Za-z ]* Utilization %"=[0-9]*' | tr '\\n' ' '; echo; sleep 1; done`
      : `while [ ! -f "$1" ]; do date +%s.%N; head -1 /proc/stat; nvidia-smi --query-gpu=index,utilization.gpu,utilization.encoder,utilization.decoder,clocks.sm,power.draw --format=csv,noheader,nounits 2> /dev/null; sleep 0.25; done`;
  const out = join(directory, `samples-${process.platform}.txt`);
  const child = spawn(
    "bash",
    ["-c", `${script} > "${out}" 2>&1`, "bash", stop],
    {
      stdio: "ignore",
    },
  );
  return () => {
    writeFileSync(stop, "");
    return new Promise((done) => child.on("exit", done));
  };
}

// ------------------------------------------------------------------------------- commands

function readMachines() {
  return JSON.parse(
    readFileSync(join(import.meta.dirname, "machines.json"), "utf8"),
  );
}

export function defaultMachine(platform = process.platform) {
  return (
    process.env.ALPHA2_GATE_MACHINE ??
    { linux: "lin", darwin: "mac", win32: "win" }[platform]
  );
}

function makeSource(machine) {
  writeProjects();
  const settings = readMachines()[machine].perf;
  const base = join(PERF_DIRECTORY, "source");
  let rc = 1;
  // The machine's default GPU path; when its GPU-resident encoder refuses, the readback one.
  for (const policy of ["auto", "cpu-readback"]) {
    ({ rc } = timedExport(
      base,
      [
        "--project",
        join(PERF_DIRECTORY, "p1g.json"),
        ...P1_ASSETS,
        "--end-ticks",
        String(10 * TICKS_PER_SECOND),
        "--no-audio",
        "--video-codec",
        "h264",
        "--video-quality",
        "12000000",
        "--renderer",
        "gpu",
        "--video-decode",
        "software",
        "--input-policy",
        policy,
        "--out",
        SOURCE_PATH,
        "--overwrite",
      ],
      settings.sourceEnv ?? {},
    ));
    if (rc === 0) break;
  }
  if (rc !== 0) {
    console.log(`FAIL the decode source could not be made; see ${base}.stderr`);
    return 1;
  }
  const bytes = readFileSync(SOURCE_PATH);
  const answer = parseAnswer(readFileSync(`${base}.json`, "utf8"));
  const record = {
    encoder: answer.result.videoEncoder,
    bytes: bytes.length,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
  writeFileSync(
    join(PERF_DIRECTORY, "source.json"),
    JSON.stringify(record, null, 2) + "\n",
  );
  console.log(`ok decode source ${SOURCE_PATH}: ${JSON.stringify(record)}`);
  return 0;
}

function configurationArgs(configuration) {
  const projects = {
    p1: [join(PERF_DIRECTORY, "p1.json"), P1_ASSETS],
    p2r: [join(PERF_DIRECTORY, "p2r.json"), ["--asset", `src=${SOURCE_PATH}`]],
    p2t: [join(PERF_DIRECTORY, "p2t.json"), ["--asset", `src=${SOURCE_PATH}`]],
  };
  const [project, assets] = projects[configuration.axes.project];
  return ["--project", project, ...assets, ...COMMON, ...configuration.args];
}

async function measure(options) {
  const settings = readMachines()[options.machine].perf;
  let configurations = settings.configurations;
  if (options.config)
    configurations = configurations.filter((c) =>
      options.config.includes(c.name),
    );
  if (configurations.length === 0) {
    console.log("FAIL no configurations");
    return 1;
  }
  const needsSource = configurations.some((c) => c.axes.project !== "p1");
  if (
    !existsSync(join(PERF_DIRECTORY, "p1.json")) ||
    (needsSource && !existsSync(SOURCE_PATH))
  )
    if (makeSource(options.machine) !== 0) return 1;
  const directory = options.out;
  mkdirSync(join(directory, "runs"), { recursive: true });
  writeFileSync(
    join(directory, "configurations.json"),
    JSON.stringify(configurations, null, 2) + "\n",
  );
  const tsv = join(directory, "runs.tsv");
  writeFileSync(tsv, "round\torder\tconfig\tstart\tend\trc\n");
  const stopSampler = options.sample ? startSampler(directory) : null;
  const run = (index, round, order) => {
    const configuration = configurations[index];
    const base = join(directory, "runs", `${configuration.name}-r${round}`);
    const out = join(directory, "runs", `${configuration.name}.mp4`);
    const { rc, started, ended } = timedExport(
      base,
      [...configurationArgs(configuration), "--out", out],
      { ...configuration.env, ALPHA2_EXPORT_TRACE: `${base}.trace.csv` },
    );
    appendFileSync(
      tsv,
      `${round}\t${order}\t${configuration.name}\t${started}\t${ended}\t${rc}\n`,
    );
    console.log(`${configuration.name} r${round} rc=${rc}`);
  };
  // One warm-up export of each (driver shader caches, first-open costs), not summarized.
  configurations.forEach((_, index) => run(index, "warm", index));
  const rounds = options.rounds ?? settings.rounds;
  for (let round = 0; round < rounds; round++)
    roundOrder(configurations.length, round).forEach((index, order) =>
      run(index, round, order),
    );
  if (stopSampler) await stopSampler();
  return summarize(directory, configurations, options.previous) ? 0 : 1;
}

function parseOptions(argv) {
  const options = { machine: defaultMachine(), sample: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new Error(`${arg} needs a value`);
      return argv[++i];
    };
    if (arg === "--make-source") options.makeSource = true;
    else if (arg === "--sample") options.sample = true;
    else if (arg === "--machine") options.machine = value();
    else if (arg === "--rounds") options.rounds = Number(value());
    else if (arg === "--config") options.config = value().split(",");
    else if (arg === "--previous") options.previous = resolve(value());
    else if (arg === "--out") options.out = resolve(value());
    else if (arg === "--summarize") options.summarize = resolve(value());
    else throw new Error(`unknown argument ${arg}`);
  }
  options.out ??= join(
    root,
    "target",
    "release-gate",
    `perf-${options.machine}-${new Date().toISOString().replace(/[:.]/g, "-")}`,
  );
  return options;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const options = parseOptions(process.argv.slice(2));
  if (!readMachines()[options.machine]) {
    console.log(`FAIL unknown machine ${options.machine}`);
    process.exit(2);
  }
  if (options.makeSource) process.exitCode = makeSource(options.machine);
  else if (options.summarize) {
    const configurations = JSON.parse(
      readFileSync(join(options.summarize, "configurations.json"), "utf8"),
    );
    process.exitCode = summarize(
      options.summarize,
      configurations,
      options.previous,
    )
      ? 0
      : 1;
  } else process.exitCode = await measure(options);
}
