#!/usr/bin/env node

// Checks how a release host publishes a finished export where a plain rename is not enough: a file
// system without a no-replace rename, one that cannot rewrite a file at an earlier offset, a path
// longer than MAX_PATH, and files another program holds open. One case per run:
//
//   node scripts/test-publish-filesystems.mjs --host <alpha2-host> --case <name>
//
//   exfat          macOS: an exFAT disk image publishes through a placeholder; FAT32 renames.
//   rclone-writes  Linux: an rclone mount with --vfs-cache-mode writes publishes through a
//                  placeholder.
//   rclone-off     Linux: an rclone mount without a cache writes only in order, so the export is
//                  staged in the temporary directory and copied there.
//   long-path      Windows: an output path past MAX_PATH, given without the \\?\ prefix.
//   held-target    Windows: an overwritten file held open without delete sharing is waited for,
//                  and one held past the wait leaves the finished render kept beside it.
//   held-partial   Windows: a partial file held open without delete sharing is waited for.
//
// A case fails, and never skips, when its setup fails (no hdiutil, rclone or PowerShell, a mount
// that does not appear). Each asserts the publish method the host reports, which is its positive
// control: `placeholder-rename` appears only where the file system refused the no-replace rename,
// so a machine whose file system takes it fails the case loudly rather than passing it.

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { hostCommand } from "./lib/host-command.mjs";

const root = resolve(import.meta.dirname, "..");
const corpus = (name) => join(root, "tests", "corpus", name);
const CASES = {
  exfat: { platform: "darwin", run: exfat },
  "rclone-writes": { platform: "linux", run: () => rclone("writes") },
  "rclone-off": { platform: "linux", run: () => rclone("off") },
  "long-path": { platform: "win32", run: longPath },
  "held-target": { platform: "win32", run: heldTarget },
  "held-partial": { platform: "win32", run: heldPartial },
};
// How long one host run may take before it is killed.
const TIMEOUT_MS = 300_000;
// Names an export writes beside its output only while it runs.
const SIBLING = /\.a2partial\.|\.a2probe[12]\.|^\.a2del-/;

const options = parseArgs(process.argv.slice(2));
const scratch = mkdtempSync(join(tmpdir(), "alpha2-publish-"));
let failures = 0;

try {
  const chosen = CASES[options.case];
  if (process.platform !== chosen.platform) {
    throw new Error(
      `the ${options.case} case runs on ${chosen.platform}, not ${process.platform}`,
    );
  }
  await chosen.run();
} catch (error) {
  // A thrown error is a failed setup or precondition, reported as a failure.
  failures += 1;
  process.stdout.write(`FAIL ${options.case}: ${error.message}\n`);
} finally {
  rmSync(scratch, { recursive: true, force: true });
}

process.exit(failures === 0 ? 0 : 1);

function parseArgs(argv) {
  const parsed = {
    host: resolve(
      process.env.ALPHA2_HOST_BINARY ??
        join(
          root,
          "target",
          "release",
          `alpha2-host${process.platform === "win32" ? ".exe" : ""}`,
        ),
    ),
    case: null,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--host") parsed.host = resolve(argv[(index += 1)]);
    else if (flag === "--case") parsed.case = argv[(index += 1)];
    else throw new Error(`unknown argument ${flag}`);
  }
  if (!Object.hasOwn(CASES, parsed.case ?? "")) {
    throw new Error(
      `--case must be one of ${Object.keys(CASES).join(", ")}, not ${parsed.case}`,
    );
  }
  if (!existsSync(parsed.host)) {
    throw new Error(`no host at ${parsed.host}; build it or pass --host`);
  }
  return parsed;
}

function check(name, body) {
  try {
    body();
    process.stdout.write(`ok ${name}\n`);
  } catch (error) {
    failures += 1;
    process.stdout.write(`FAIL ${name}\n  ${error.message}\n`);
  }
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

/** Runs `command` with `args`, failing with its output when it does not exit 0. */
function run(command, args, spawnOptions = {}) {
  const result = spawnSync(command, args, {
    encoding: "utf8",
    ...spawnOptions,
  });
  if (result.error) {
    throw new Error(`${command} could not run: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(
      `${command} ${args.join(" ")} exited ${result.status}: ${result.stderr}${result.stdout}`,
    );
  }
  return result.stdout;
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

/** An export of `project` with `assets` to `output`, `endTicks` long at `rate` frames a second. */
function exportRequest({
  project,
  assets,
  output,
  endTicks,
  rate,
  overwrite = false,
}) {
  return {
    protocolVersion: 1,
    operation: "export",
    requestId: "publish",
    project,
    assets,
    startTicks: 0,
    endTicks,
    frameRate: { num: rate, den: 1 },
    includeAudio: false,
    videoCodec: "mpeg4-part2",
    output,
    overwrite,
  };
}

/** A project of one still image, `frames` frames long at 30 frames a second. */
function imageExport(
  output,
  { overwrite = false, frames = 1, size = [64, 48] } = {},
) {
  const end = frames * 4_000;
  return exportRequest({
    project: {
      schema_version: 1,
      id: "publish-image",
      name: "Publish image",
      settings: {
        width: size[0],
        height: size[1],
        frame_rate: { num: 30, den: 1 },
        sample_rate: 48_000,
      },
      sources: [{ id: "still", locator: "fixture://still", kind: "image" }],
      timeline: {
        tracks: [
          {
            id: "v",
            name: "V",
            kind: "video",
            clips: [
              {
                id: "clip",
                source_id: "still",
                timeline_range: { start: 0, end },
                source_range: { start: 0, end },
              },
            ],
          },
        ],
      },
    },
    assets: { still: corpus("parity-image.png") },
    output,
    endTicks: end,
    rate: 30,
    overwrite,
  });
}

/** Sends `request` to a serving host; its answer, or `null` when there is none, and its stderr. */
function serve(request) {
  const result = spawnSync(...hostCommand(options.host, ["serve"]), {
    encoding: "utf8",
    input: `${JSON.stringify(request)}\n`,
    timeout: TIMEOUT_MS,
    killSignal: "SIGKILL",
  });
  if (result.error) {
    throw new Error(`the host could not run: ${result.error.message}`);
  }
  return {
    answer: answerOf(result.stdout ?? "", request),
    stderr: result.stderr ?? "",
  };
}

/**
 * Sends `request` to a serving host without waiting: `onProgress` sees each progress line as it
 * arrives. Resolves with the answer (or `null`) and the host's stderr.
 */
function serveWatching(request, onProgress) {
  return new Promise((resolvePromise, reject) => {
    const [command, args] = hostCommand(options.host, ["serve"]);
    const child = spawn(command, args, { stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    let pending = "";
    const timer = setTimeout(() => child.kill("SIGKILL"), TIMEOUT_MS);
    child.stdout.setEncoding("utf8").on("data", (chunk) => {
      stdout += chunk;
      pending += chunk;
      let newline = pending.indexOf("\n");
      while (newline >= 0) {
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        const message = parsed(line);
        if (message?.type === "progress") onProgress(message);
        newline = pending.indexOf("\n");
      }
    });
    child.stderr.setEncoding("utf8").on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", () => {
      clearTimeout(timer);
      resolvePromise({ answer: answerOf(stdout, request), stderr });
    });
    child.stdin.end(`${JSON.stringify(request)}\n`);
  });
}

function parsed(line) {
  try {
    return JSON.parse(line);
  } catch {
    return null;
  }
}

function answerOf(stdout, request) {
  for (const line of stdout.split("\n")) {
    const message = parsed(line);
    if (message?.requestId === request.requestId && "ok" in message)
      return message;
  }
  return null;
}

/** The result of an answer that must have succeeded. */
function succeeded({ answer, stderr }, what) {
  assert(
    answer?.ok === true,
    `${what} failed: ${JSON.stringify(answer?.error ?? null)} ${stderr.slice(-1500)}`,
  );
  return answer.result;
}

/** The error of an answer that must have failed. */
function failed({ answer, stderr }, what) {
  assert(
    answer?.ok === false,
    `${what} did not fail: ${JSON.stringify(answer)} ${stderr.slice(-1500)}`,
  );
  return answer.error;
}

function expectPublish(result, method, what) {
  assert(
    result.publish?.method === method,
    `${what} published by ${JSON.stringify(result.publish)}, not ${method}`,
  );
}

/**
 * Decodes every frame of the video at `path` by exporting it again, `seconds` long at `rate`
 * frames a second, into the scratch directory: a file that is not whole does not export.
 */
function expectPlayable(path, { seconds, rate, width, height }) {
  const end = Math.round(seconds * 120_000);
  const output = join(
    scratch,
    `decoded-${createHash("sha1").update(path).digest("hex")}.mp4`,
  );
  rmSync(output, { force: true });
  const result = succeeded(
    serve(
      exportRequest({
        project: {
          schema_version: 1,
          id: "publish-decode",
          name: "Publish decode",
          settings: {
            width,
            height,
            frame_rate: { num: rate, den: 1 },
            sample_rate: 48_000,
          },
          sources: [{ id: "video", locator: "fixture://video", kind: "video" }],
          timeline: {
            tracks: [
              {
                id: "v",
                name: "V",
                kind: "video",
                clips: [
                  {
                    id: "clip",
                    source_id: "video",
                    timeline_range: { start: 0, end },
                    source_range: { start: 0, end },
                  },
                ],
              },
            ],
          },
        },
        assets: { video: path },
        output,
        endTicks: end,
        rate,
      }),
    ),
    `decoding ${path}`,
  );
  const frames = Math.round(seconds * rate);
  assert(
    result.frames === frames,
    `${path} decoded ${result.frames} frames of ${frames}`,
  );
  rmSync(output, { force: true });
}

/** What an export left in `directory` besides its outputs. */
function leftovers(directory) {
  return readdirSync(directory).filter((name) => SIBLING.test(name));
}

// exfat (macOS)

async function exfat() {
  const image = join(scratch, "a2x.dmg");
  run("hdiutil", [
    "create",
    "-size",
    "200m",
    "-fs",
    "ExFAT",
    "-volname",
    "A2X",
    image,
  ]);
  // Attached where macOS puts it: a mount point under ~/Documents failed to attach.
  const volume = attach(image);
  try {
    const output = join(volume, "out.mp4");
    check("exfat: a new output publishes through a placeholder", () => {
      const result = succeeded(serve(imageExport(output)), "the export");
      expectPublish(result, "placeholder-rename", "the export");
      expectPlayable(output, {
        seconds: 1 / 30,
        rate: 30,
        width: 64,
        height: 48,
      });
    });
    check("exfat: an existing output is refused before rendering", () => {
      const before = sha256(output);
      const error = failed(serve(imageExport(output)), "the second export");
      assert(
        error.code === "IO_ERROR" &&
          /refusing to overwrite/.test(error.message),
        JSON.stringify(error),
      );
      assert(
        sha256(output) === before,
        "the refused export changed the output",
      );
    });
    check("exfat: an overwrite replaces the output", () => {
      const result = succeeded(
        serve(imageExport(output, { overwrite: true })),
        "the overwrite",
      );
      expectPublish(result, "rename-replace", "the overwrite");
    });
    check("exfat: nothing is left beside the output", () => {
      const left = leftovers(volume);
      assert(left.length === 0, `left ${left.join(", ")}`);
    });
  } finally {
    detach(volume);
  }

  const fat = join(scratch, "a2f.dmg");
  run("hdiutil", [
    "create",
    "-size",
    "200m",
    "-fs",
    "MS-DOS",
    "-volname",
    "A2F",
    fat,
  ]);
  const fatVolume = attach(fat);
  try {
    check("exfat: FAT32 takes the no-replace rename", () => {
      const result = succeeded(
        serve(imageExport(join(fatVolume, "out.mp4"))),
        "the export",
      );
      expectPublish(result, "rename-no-replace", "the FAT32 export");
    });
  } finally {
    detach(fatVolume);
  }
}

/** Attaches the disk image at `image` and returns where its volume is mounted. */
function attach(image) {
  const output = run("hdiutil", ["attach", "-nobrowse", image]);
  const volume = output
    .split("\n")
    .map((line) => line.split("\t").at(-1)?.trim())
    .find((field) => field?.startsWith("/Volumes/"));
  assert(volume, `hdiutil attach printed no volume: ${output}`);
  return volume;
}

function detach(volume) {
  spawnSync("hdiutil", ["detach", volume, "-force"], { encoding: "utf8" });
}

// rclone-writes, rclone-off (Linux)

async function rclone(cacheMode) {
  const name = `rclone-${cacheMode}`;
  const fusermount = ["fusermount", "fusermount3"].find(
    (command) => spawnSync(command, ["-V"]).error === undefined,
  );
  assert(fusermount, "neither fusermount nor fusermount3 is installed");
  assert(
    spawnSync("rclone", ["version"]).error === undefined,
    "rclone is not installed",
  );
  const back = join(scratch, "back");
  const mount = join(scratch, "mount");
  const cache = join(scratch, "cache");
  const config = join(scratch, "rclone.conf");
  for (const directory of [back, mount, cache]) mkdirSync(directory);
  writeFileSync(config, "");
  run("rclone", [
    "--config",
    config,
    "--cache-dir",
    cache,
    "mount",
    `:local:${back}`,
    mount,
    "--daemon",
    ...(cacheMode === "writes" ? ["--vfs-cache-mode", "writes"] : []),
  ]);
  try {
    await waitForMount(mount);
    const assets = {
      video: join(mount, "parity-rgb.mov"),
      still: join(mount, "parity-image.png"),
    };
    copyFileSync(corpus("parity-rgb.mov"), assets.video);
    copyFileSync(corpus("parity-image.png"), assets.still);
    const project = JSON.parse(
      readFileSync(
        join(root, "tests", "cases", "host-parity-complex-project.json"),
        "utf8",
      ),
    );
    const output = join(mount, "out.mp4");
    check(`${name}: the export publishes`, () => {
      const result = succeeded(
        serve(
          exportRequest({
            project,
            assets,
            output,
            endTicks: 180_000,
            rate: 24,
          }),
        ),
        "the export",
      );
      // A native rename here means this rclone build takes RENAME_NOREPLACE, and the case no
      // longer covers the placeholder.
      expectPublish(result, "placeholder-rename", "the export");
      if (cacheMode === "writes") {
        assert(
          !result.publish.stagedLocally,
          "a cached mount was staged locally",
        );
      } else {
        assert(
          result.publish.stagedLocally,
          "a mount without a cache was not staged locally",
        );
      }
      expectPlayable(output, { seconds: 1.5, rate: 24, width: 96, height: 64 });
    });
    // rclone writes cached files back to the remote 5 s after they close.
    await sleep(7_000);
    check(`${name}: nothing is left beside the output`, () => {
      const names = readdirSync(mount).sort();
      const expected = ["out.mp4", "parity-image.png", "parity-rgb.mov"];
      assert(
        JSON.stringify(names) === JSON.stringify(expected),
        `the mount holds ${names.join(", ")}`,
      );
    });
  } finally {
    spawnSync(fusermount, ["-u", mount], { encoding: "utf8" });
  }
}

async function waitForMount(mount) {
  for (let tries = 0; tries < 100; tries += 1) {
    const mounts = readFileSync("/proc/self/mountinfo", "utf8");
    if (mounts.split("\n").some((line) => line.split(" ")[4] === mount)) return;
    await sleep(100);
  }
  throw new Error(`the rclone mount at ${mount} did not appear`);
}

// long-path, held-target, held-partial (Windows)

async function longPath() {
  const top = join(tmpdir(), `a2long-${process.pid}`);
  const directory = join(top, "a".repeat(120), "b".repeat(120));
  mkdirSync(directory, { recursive: true });
  try {
    const output = join(directory, "out.mp4");
    assert(!output.startsWith("\\\\?\\") && output.length > 260, output);
    check("long-path: a new output past MAX_PATH publishes", () => {
      const result = succeeded(serve(imageExport(output)), "the export");
      expectPublish(result, "rename-no-replace", "the export");
      assert(existsSync(output), "no output");
    });
    check("long-path: an overwrite past MAX_PATH replaces the output", () => {
      const result = succeeded(
        serve(imageExport(output, { overwrite: true })),
        "the overwrite",
      );
      expectPublish(result, "rename-replace", "the overwrite");
    });
  } finally {
    rmSync(top, { recursive: true, force: true });
  }
}

/**
 * Opens `path` in a PowerShell process the way a program that does not share deletion does (read
 * access, sharing reads and writes but not deletion) and resolves once it is open. `release()`
 * closes it and resolves when the process has ended.
 */
async function holdWithoutDeleteSharing(path) {
  const script =
    "$s = [System.IO.File]::Open($env:A2_HOLD, 'Open', 'Read', 'ReadWrite'); " +
    "[Console]::Out.WriteLine('held'); [Console]::Out.Flush(); " +
    "[void][Console]::In.ReadLine(); $s.Close()";
  const child = spawn(
    "powershell",
    ["-NoProfile", "-NonInteractive", "-Command", script],
    { env: { ...process.env, A2_HOLD: path }, stdio: ["pipe", "pipe", "pipe"] },
  );
  let stderr = "";
  child.stderr.setEncoding("utf8").on("data", (chunk) => {
    stderr += chunk;
  });
  const ended = new Promise((resolvePromise) =>
    child.on("close", resolvePromise),
  );
  await new Promise((resolvePromise, reject) => {
    child.on("error", (error) =>
      reject(new Error(`PowerShell could not run: ${error.message}`)),
    );
    child.stdout.setEncoding("utf8").on("data", (chunk) => {
      if (chunk.includes("held")) resolvePromise();
    });
    ended.then(() =>
      reject(new Error(`PowerShell did not hold ${path}: ${stderr}`)),
    );
  });
  let released = null;
  return {
    release() {
      released ??= (() => {
        child.stdin.end("\n");
        return ended;
      })();
      return released;
    },
  };
}

async function heldTarget() {
  const output = join(scratch, "out.mp4");
  succeeded(serve(imageExport(output, { frames: 30 })), "the first export");

  const briefly = await holdWithoutDeleteSharing(output);
  let releaseTimer = null;
  const waited = await serveWatching(
    imageExport(output, { overwrite: true, frames: 30 }),
    (progress) => {
      // Held through the publish: released 3 s after the frames are all encoded.
      if (progress.phase === "finalizing" && releaseTimer === null) {
        releaseTimer = setTimeout(() => briefly.release(), 3_000);
      }
    },
  );
  clearTimeout(releaseTimer);
  await briefly.release();
  check("held-target: an overwrite waits for a holder that lets go", () => {
    const result = succeeded(waited, "the overwrite");
    expectPublish(result, "rename-replace", "the overwrite");
    assert(result.publish.retries >= 1, JSON.stringify(result.publish));
    assert(
      /is open in another program/.test(waited.stderr),
      `the claim did not say the output is held: ${waited.stderr.slice(-1000)}`,
    );
  });

  const before = sha256(output);
  const long = await holdWithoutDeleteSharing(output);
  // Held past the 10 s the publish waits, until the export has answered or 30 s have passed.
  const longTimer = setTimeout(() => long.release(), 30_000);
  const refused = await serveWatching(
    imageExport(output, { overwrite: true, frames: 30 }),
    () => {},
  );
  clearTimeout(longTimer);
  await long.release();
  check(
    "held-target: a holder past the wait leaves the render kept beside the output",
    () => {
      const error = failed(refused, "the overwrite");
      assert(error.code === "OUTPUT_PUBLISH_FAILED", JSON.stringify(error));
      const kept = error.details?.keptPath;
      assert(
        typeof kept === "string" && existsSync(kept),
        `no kept file: ${JSON.stringify(error)}`,
      );
      assert(basename(kept) === "out.a2kept.mp4", kept);
      expectPlayable(kept, { seconds: 1, rate: 30, width: 64, height: 48 });
      assert(sha256(output) === before, "the held output changed");
      const left = leftovers(scratch);
      assert(left.length === 0, `left ${left.join(", ")}`);
    },
  );
}

async function heldPartial() {
  const output = join(scratch, "out.mp4");
  const partial = join(scratch, "out.a2partial.mp4");
  // Long enough that the holder opens the partial while the frames still encode.
  const request = imageExport(output, { frames: 600, size: [1280, 720] });
  let holder = null;
  let holding = null;
  let holdError = null;
  let releaseTimer = null;
  const exported = serveWatching(request, (progress) => {
    if (progress.phase === "finalizing" && releaseTimer === null) {
      releaseTimer = setTimeout(() => holder?.release(), 3_000);
    }
  });
  let finished = false;
  exported.then(() => {
    finished = true;
  });
  while (!finished && holding === null) {
    if (existsSync(partial)) {
      holding = holdWithoutDeleteSharing(partial).then(
        (held) => {
          holder = held;
        },
        (error) => {
          holdError = error;
        },
      );
    } else {
      await sleep(5);
    }
  }
  const answered = await exported;
  clearTimeout(releaseTimer);
  await holding;
  await holder?.release();
  check(
    "held-partial: a held partial is published once its holder lets go",
    () => {
      assert(holdError === null, holdError?.message);
      assert(
        holder !== null,
        "the export finished before its partial file could be held",
      );
      const result = succeeded(answered, "the export");
      expectPublish(result, "rename-no-replace", "the export");
      assert(result.publish.retries >= 1, JSON.stringify(result.publish));
      assert(
        existsSync(output) && !existsSync(partial),
        "the partial was not published",
      );
    },
  );
}
