// Chrome driven over DevTools. A page reports by setting `document.title` to URI-encoded JSON
// (starting `%7B`). Every command has a deadline, so a page that never yields still times out.

import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path, { join, win32 } from "node:path";

import { track, untrack } from "./browser-cleanup.mjs";
import { BrowserStartError } from "./browser-errors.mjs";
import { recordSkip, required } from "./capabilities.mjs";

/** ALPHA2_PARITY_CHROME, else the first installed Chrome or Chromium, else null. */
export function chromeExecutable({
  env = process.env,
  platform = process.platform,
  exists = existsSync,
} = {}) {
  const configured = env.ALPHA2_PARITY_CHROME;
  if (configured) return configured;
  for (const candidate of chromeCandidates(platform, env))
    if (exists(candidate)) return candidate;
  return null;
}

function chromeCandidates(platform, env) {
  if (platform === "darwin")
    return [
      "/Applications",
      ...(env.HOME ? [`${env.HOME}/Applications`] : []),
    ].flatMap((applications) => [
      `${applications}/Google Chrome.app/Contents/MacOS/Google Chrome`,
      `${applications}/Chromium.app/Contents/MacOS/Chromium`,
    ]);
  if (platform === "win32") {
    const roots = [
      env.PROGRAMFILES ?? env.ProgramFiles,
      env["PROGRAMFILES(X86)"] ?? env["ProgramFiles(x86)"],
      env.LOCALAPPDATA,
    ].filter(Boolean);
    return [
      ...roots.map((root) =>
        win32.join(root, "Google", "Chrome", "Application", "chrome.exe"),
      ),
      ...roots.map((root) =>
        win32.join(root, "Chromium", "Application", "chrome.exe"),
      ),
    ];
  }
  return [
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
  ];
}

/** ALPHA2_EDGE, else the first installed Edge, else null. Never a fallback for Chrome. */
export function edgeExecutable({
  env = process.env,
  platform = process.platform,
  exists = existsSync,
} = {}) {
  const configured = env.ALPHA2_EDGE;
  if (configured) return configured;
  for (const candidate of edgeCandidates(platform, env))
    if (exists(candidate)) return candidate;
  return null;
}

function edgeCandidates(platform, env) {
  if (platform === "darwin")
    return ["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"];
  if (platform === "win32")
    return [
      env["PROGRAMFILES(X86)"] ?? env["ProgramFiles(x86)"],
      env.PROGRAMFILES ?? env.ProgramFiles,
    ]
      .filter(Boolean)
      .map((root) =>
        win32.join(root, "Microsoft", "Edge", "Application", "msedge.exe"),
      );
  return ["/usr/bin/microsoft-edge", "/usr/bin/microsoft-edge-stable"];
}

export const BROWSER_SETTINGS = Object.freeze(["harness", "stock"]);

/**
 * `harness` (default) adds flags for unattended pages and GPU selection; `stock` adds none.
 *
 * @returns {"harness" | "stock"}
 */
export function browserSettingsMode(env = process.env) {
  const settings = env.ALPHA2_BROWSER_SETTINGS || "harness";
  if (!BROWSER_SETTINGS.includes(settings))
    throw new Error(
      `unknown ALPHA2_BROWSER_SETTINGS ${JSON.stringify(settings)}: expected ${BROWSER_SETTINGS.join(", ")}`,
    );
  return settings;
}

/**
 * Chrome's GPU process needs DRI3 on Mesa's Vulkan drivers; without it (Xvfb) it hangs.
 *
 * @returns {{ tool: boolean, opened: boolean, dri3: boolean }}
 */
export function displayExtensions(display) {
  const result = spawnSync(
    "xdpyinfo",
    ["-display", display, "-queryExtensions"],
    { encoding: "utf8", timeout: 10_000 },
  );
  if (result.error?.code === "ENOENT")
    return { tool: false, opened: false, dri3: false };
  return {
    tool: true,
    opened: result.status === 0,
    dri3: /\bDRI3\b/.test(result.stdout ?? ""),
  };
}

const displays = new Map();
let warnedNoXdpyinfo = false;

export const CHROME_GPU_MODES = Object.freeze([
  "machine",
  "swiftshader",
  "none",
]);

/**
 * `ALPHA2_CHROME_GPU_FLAGS` (even empty) overrides `ALPHA2_CHROME_GPU`. The default is `machine`
 * on macOS and Windows and `swiftshader` on Linux, where the machine's GPU needs a headed browser
 * on a DRI3 display.
 *
 * @returns {{ mode: "machine" | "swiftshader" | "none" | "custom" | "stock", flags: string[] }}
 */
export function chromeGpuMode({
  env = process.env,
  platform = process.platform,
  extensions = displayExtensions,
} = {}) {
  if (browserSettingsMode(env) === "stock") return { mode: "stock", flags: [] };
  const configured = env.ALPHA2_CHROME_GPU_FLAGS;
  if (configured !== undefined)
    return { mode: "custom", flags: configured.split(/\s+/).filter(Boolean) };
  const mode =
    env.ALPHA2_CHROME_GPU ||
    (platform === "darwin" || platform === "win32" ? "machine" : "swiftshader");
  if (!CHROME_GPU_MODES.includes(mode))
    throw new Error(
      `unknown ALPHA2_CHROME_GPU ${JSON.stringify(mode)}: expected ${CHROME_GPU_MODES.join(", ")}`,
    );
  if (mode === "swiftshader")
    return {
      mode,
      flags: ["--enable-unsafe-swiftshader", "--use-angle=swiftshader"],
    };
  if (mode === "none")
    return { mode, flags: ["--disable-gpu", "--disable-software-rasterizer"] };
  if (platform === "darwin") return { mode, flags: ["--use-angle=metal"] };
  // Chrome's defaults: D3D11 ANGLE and Dawn's D3D12.
  if (platform === "win32") return { mode, flags: [] };
  if (
    env.ALPHA2_BROWSER_HEADED !== "1" ||
    !(env.DISPLAY || env.WAYLAND_DISPLAY)
  )
    throw new BrowserStartError(
      "gpu=machine on Linux needs a headed browser on a DRI3 display; run under scripts/run-with-gpu-display.sh or set ALPHA2_CHROME_GPU=swiftshader",
    );
  if (env.DISPLAY) {
    let display;
    if (extensions !== displayExtensions) display = extensions(env.DISPLAY);
    else {
      if (!displays.has(env.DISPLAY))
        displays.set(env.DISPLAY, extensions(env.DISPLAY));
      display = displays.get(env.DISPLAY);
    }
    if (!display.tool) {
      if (!warnedNoXdpyinfo)
        process.stderr.write(
          `warning: xdpyinfo is missing, so DISPLAY ${env.DISPLAY} was not checked for DRI3; without it Chrome's GPU process hangs on Mesa Vulkan\n`,
        );
      warnedNoXdpyinfo = true;
    } else if (!display.opened)
      throw new BrowserStartError(
        `DISPLAY ${env.DISPLAY} cannot be opened; run under scripts/run-with-gpu-display.sh`,
      );
    else if (!display.dri3)
      throw new BrowserStartError(
        `DISPLAY ${env.DISPLAY} has no DRI3 (Xvfb): Chrome's GPU process hangs on Mesa Vulkan there; use scripts/run-with-gpu-display.sh`,
      );
  }
  return {
    mode,
    flags: [
      "--use-angle=vulkan",
      "--enable-features=Vulkan",
      "--ignore-gpu-blocklist",
    ],
  };
}

/** Null when there is no Chrome, unless ALPHA2_REQUIRE names `browser`. */
export function browserForCheck({ env = process.env, platform, exists } = {}) {
  const chrome = chromeExecutable({ env, platform, exists });
  if (chrome !== null) return chrome;
  if (required("browser", env))
    throw new Error(
      "no Chrome found: set ALPHA2_PARITY_CHROME to a Chrome executable (browser is required by ALPHA2_REQUIRE)",
    );
  return null;
}

export function reportSkippedBrowser(
  what,
  suite,
  env = process.env,
  { platform, exists } = {},
) {
  const edge =
    edgeExecutable({ env, platform, exists }) === null
      ? ""
      : ", or run Edge with --browser edge";
  return recordSkip(
    {
      suite,
      capability: "browser",
      reason: `${what} did not run: no Chrome found; set ALPHA2_PARITY_CHROME to a Chrome executable${edge}`,
    },
    env,
  );
}

/**
 * @param {{ name: string, reason: string }[]} skipped
 * @returns {string}
 */
export function skippedCasesSummary(skipped) {
  if (skipped.length === 0) return "";
  return [
    `${skipped.length} case(s) skipped: this browser cannot run them; ALPHA2_REQUIRE fails the ones it names`,
    ...skipped.map(({ name, reason }) => `  ${name}: ${reason}`),
    "",
  ].join("\n");
}

/**
 * `domainBlocking3d` keeps Chrome's 3D API blocking, for cases about GPU reset policy.
 *
 * @param {{ settings: "harness" | "stock", gpuFlags: string[], headed: boolean, profile: string,
 *   domainBlocking3d?: boolean }} options
 * @returns {string[]}
 */
export function chromeArguments({
  settings,
  gpuFlags,
  headed,
  profile,
  domainBlocking3d = false,
}) {
  return [
    // Headless Chrome cannot import a VideoFrame into WebGPU.
    ...(headed ? [] : ["--headless=new"]),
    ...(settings === "stock"
      ? []
      : [
          "--no-sandbox",
          "--disable-gpu-sandbox",
          "--enable-unsafe-webgpu",
          // SwiftShader loses its device often on small runners, after which Chrome blocks WebGL.
          ...(domainBlocking3d
            ? []
            : ["--disable-domain-blocking-for-3d-apis"]),
          "--autoplay-policy=no-user-gesture-required",
          // GPU process exits are read from stderr; Windows Chrome logs nowhere by default.
          "--enable-logging=stderr",
          ...gpuFlags,
        ]),
    // A headed Chrome would otherwise stop at the first-run dialog or a keyring prompt.
    "--no-first-run",
    "--password-store=basic",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    "about:blank",
  ];
}

const GPU_PROCESS_EXIT =
  /GPU process exited unexpectedly|GPU process isn't usable/g;

/**
 * Pids are null when there was no GPU process and undefined when DevTools could not say. A GPU
 * process that only started during the page is not a restart.
 *
 * @typedef {{ startPid: number | null | undefined, endPid: number | null | undefined,
 *   exits: number, restarted: boolean }} GpuProcessRecord
 */

/** @returns {GpuProcessRecord} */
export function gpuProcessRecord({ startPid, endPid, exits }) {
  const changed =
    startPid !== null &&
    startPid !== undefined &&
    endPid !== undefined &&
    endPid !== startPid;
  return { startPid, endPid, exits, restarted: exits > 0 || changed };
}

/**
 * A page that lost its GPU process to the environment says nothing about the engine, so its result
 * becomes a `browser-gpu-process` skip. Null when the process did not restart.
 *
 * @param {GpuProcessRecord | undefined} record
 * @param {object} result
 */
export function gpuProcessEnvironment(record, result) {
  if (!record?.restarted) return null;
  const how =
    record.exits > 0
      ? `Chrome logged ${record.exits} GPU process exit(s)`
      : `its id changed from ${record.startPid} to ${record.endPid ?? "none"}`;
  return {
    skipped: `gpu-process-restarted: the browser's GPU process restarted during the page (${how})`,
    capability: "browser-gpu-process",
    gpuProcess: record,
    page: result.error ?? (result.ok === true ? "ok" : result),
  };
}

async function chromeProcesses(cdp) {
  try {
    const { processInfo } = await cdp.send(
      "SystemInfo.getProcessInfo",
      {},
      undefined,
      5_000,
    );
    return {
      pids: processInfo.map((process) => process.id),
      gpu:
        processInfo.find((process) => /^gpu/i.test(process.type))?.id ?? null,
    };
  } catch {
    return null;
  }
}

const STDERR_TAIL_BYTES = 4096;
const STDERR_QUOTED_LINES = 10;

function chromeSaid(stderr) {
  const lines = stderr
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .slice(-STDERR_QUOTED_LINES);
  return lines.length === 0 ? "" : `; Chrome said: ${lines.join(" | ")}`;
}

/**
 * Opens `url` in a fresh Chrome profile and waits for the page to report. Chrome is killed and its
 * profile removed in every case.
 *
 * @param {string} command
 * @param {string} url
 * @param {object} [options]
 * @param {number} [options.timeoutMs] Default 30 s.
 * @param {(cdp: Cdp, sessionId: string) => Promise<unknown>} [options.onPage] Runs after
 *   navigation starts, within the same deadline.
 * @param {object} [options.viewport]
 * @param {string} [options.initScript] Evaluated in every new document before its own scripts.
 * @param {(version: { product: string, userAgent: string }) => void} [options.onBrowser]
 * @param {boolean} [options.domainBlocking3d]
 * @param {(record: GpuProcessRecord) => void} [options.onGpuProcess] Called before the result
 *   or error.
 * @returns {Promise<string>} The page's title. Rejects with `BrowserStartError` when Chrome
 *   cannot be started or connected to.
 */
export async function runChromePage(
  command,
  url,
  {
    timeoutMs = 30_000,
    onPage,
    viewport,
    initScript,
    onBrowser,
    onGpuProcess,
    domainBlocking3d = false,
  } = {},
) {
  const { flags: gpuFlags } = chromeGpuMode();
  const settings = browserSettingsMode();
  const profile = await mkdtemp(join(tmpdir(), "alpha2-chrome-"));
  const cleanup = track({ profile });
  let child;
  try {
    child = spawn(
      command,
      chromeArguments({
        settings,
        gpuFlags,
        headed: process.env.ALPHA2_BROWSER_HEADED === "1",
        profile,
        domainBlocking3d,
      }),
      {
        stdio: ["ignore", "ignore", "pipe"],
        // A process group on POSIX, so ending it ends the renderer and GPU processes too.
        detached: process.platform !== "win32",
      },
    );
  } catch (error) {
    // Node throws synchronously for some commands, such as a `.cmd` without a shell.
    untrack(cleanup);
    await rm(profile, { recursive: true, force: true });
    throw new BrowserStartError(`Chrome could not start: ${error.message}`);
  }
  cleanup.child = child;
  cleanup.group = process.platform !== "win32";
  // Lines may split across chunks, so each chunk is matched together with the previous tail.
  let stderr = "";
  let gpuExits = 0;
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    const tail = stderr.slice(-200);
    gpuExits +=
      (`${tail}${chunk}`.match(GPU_PROCESS_EXIT) ?? []).length -
      (tail.match(GPU_PROCESS_EXIT) ?? []).length;
    stderr = (stderr + chunk).slice(-STDERR_TAIL_BYTES);
  });
  const pids = new Set();
  const said = () => chromeSaid(stderr);
  try {
    const webSocketUrl = await new Promise((resolveUrl, reject) => {
      // First-time Chrome startup is slow on hosted runners.
      const timeout = setTimeout(
        () =>
          reject(
            new BrowserStartError(
              `Chrome DevTools endpoint timed out${said()}`,
            ),
          ),
        60_000,
      );
      let seen = "";
      const onData = (chunk) => {
        seen += chunk;
        const match = seen.match(/DevTools listening on (ws:\/\/[^\s]+)/);
        if (match) {
          clearTimeout(timeout);
          child.stderr.off("data", onData);
          resolveUrl(match[1]);
        }
      };
      child.stderr.on("data", onData);
      // A pending timer would hold the process open for 60 s.
      const fail = (message) => {
        clearTimeout(timeout);
        reject(new BrowserStartError(`${message}${said()}`));
      };
      child.once("error", (error) =>
        fail(`Chrome could not start: ${error.message}`),
      );
      // Wait briefly for stderr to close so the error can quote it; a child may hold it open.
      child.once("exit", (code) => {
        const exited = () => fail(`Chrome exited ${code}`);
        child.once("close", exited);
        setTimeout(exited, 500).unref();
      });
    });
    let cdp;
    try {
      cdp = await Cdp.open(webSocketUrl);
    } catch (error) {
      throw new BrowserStartError(
        `the DevTools connection failed: ${error?.message ?? error}${said()}`,
      );
    }
    const started = await chromeProcesses(cdp);
    for (const pid of started?.pids ?? []) pids.add(pid);
    try {
      if (onBrowser) onBrowser(await cdp.send("Browser.getVersion"));
      const { targetId } = await cdp.send("Target.createTarget", {
        url: "about:blank",
      });
      const { sessionId } = await cdp.send("Target.attachToTarget", {
        targetId,
        flatten: true,
      });
      cdp.watchSession(sessionId);
      // Inspector reports a crashed renderer.
      await cdp.send("Inspector.enable", {}, sessionId);
      await cdp.send("Runtime.enable", {}, sessionId);
      await cdp.send("Page.enable", {}, sessionId);
      if (process.env.ALPHA2_CHROME_CONSOLE === "1")
        await cdp.send("Log.enable", {}, sessionId);
      if (initScript)
        await cdp.send(
          "Page.addScriptToEvaluateOnNewDocument",
          { source: initScript },
          sessionId,
        );
      if (viewport)
        await cdp.send(
          "Emulation.setDeviceMetricsOverride",
          { ...viewport, deviceScaleFactor: 1, mobile: false },
          sessionId,
        );
      const deadline = Date.now() + timeoutMs;
      const remaining = () => Math.max(1, deadline - Date.now());
      await cdp.send("Page.navigate", { url }, sessionId);
      if (onPage)
        await cdp.within(
          onPage(cdp, sessionId),
          remaining(),
          "the page's setup",
        );
      let title = "pending";
      while (Date.now() < deadline) {
        let evaluation;
        try {
          // A busy main thread answers nothing, so wait no longer than the page has left.
          evaluation = await cdp.send(
            "Runtime.evaluate",
            { expression: "document.title", returnByValue: true },
            sessionId,
            remaining(),
          );
        } catch (error) {
          if (error instanceof CommandTimeout) break;
          // Its last title says how far a crashed or closed page got.
          throw new Error(
            `${error.message} (the page was at ${title})${said()}`,
            { cause: error },
          );
        }
        title = evaluation.result?.value ?? "";
        if (title.startsWith("%7B") || title.startsWith("%7b")) return title;
        await new Promise((resolveTimer) => setTimeout(resolveTimer, 100));
      }
      throw new Error(
        `browser page timed out at ${title} after ${timeoutMs / 1000} s${said()}`,
      );
    } finally {
      // DevTools answers nothing after a crash, leaving the end id unknown.
      const ended = await chromeProcesses(cdp);
      for (const pid of ended?.pids ?? []) pids.add(pid);
      onGpuProcess?.(
        gpuProcessRecord({
          startPid: started === null ? undefined : started.gpu,
          endPid: ended === null ? undefined : ended.gpu,
          exits: gpuExits,
        }),
      );
      cdp.close();
    }
  } finally {
    await stopChrome(child, { pids: [...pids] });
    untrack(cleanup);
    // A leftover profile never replaces the page's result or error.
    await rm(profile, {
      recursive: true,
      force: true,
      maxRetries: 10,
      retryDelay: 100,
    }).catch((error) =>
      process.stderr.write(
        `could not remove the Chrome profile ${profile}: ${error.message}\n`,
      ),
    );
  }
}

const STOP_WAIT_MS = 10_000;

export async function processAlive(pid, platform = process.platform) {
  if (platform !== "win32") {
    try {
      process.kill(pid, 0);
      return true;
    } catch (error) {
      return error.code === "EPERM";
    }
  }
  const output = await new Promise((resolveList) => {
    const list = spawn(
      "tasklist",
      ["/FI", `PID eq ${pid}`, "/FO", "CSV", "/NH"],
      { stdio: ["ignore", "pipe", "ignore"], windowsHide: true },
    );
    let text = "";
    list.stdout.setEncoding("utf8");
    list.stdout.on("data", (chunk) => (text += chunk));
    list.once("close", () => resolveList(text));
    list.once("error", () => resolveList(""));
  });
  return output.includes(`"${pid}"`);
}

/**
 * Ends Chrome's whole process tree (a plain kill on Windows leaves children holding the profile and
 * stderr), then waits up to 10 s for every process in `pids`, since a lingering GPU process holds
 * the GPU the next launch opens.
 */
export async function stopChrome(
  child,
  {
    platform = process.platform,
    spawnProcess = spawn,
    pids = [],
    alive = (pid) => processAlive(pid, platform),
    waitMs = STOP_WAIT_MS,
  } = {},
) {
  const started = Date.now();
  const running = child.exitCode === null && child.signalCode === null;
  const exited = running
    ? new Promise((resolveExit) => child.once("exit", resolveExit))
    : Promise.resolve();
  // Windows may reuse the pid, so the tree is ended only while Chrome runs.
  if (platform === "win32" && running && child.pid !== undefined) {
    await new Promise((resolveKill) => {
      const killer = spawnProcess(
        "taskkill",
        ["/T", "/F", "/PID", String(child.pid)],
        { stdio: "ignore", windowsHide: true },
      );
      killer.once("exit", resolveKill);
      killer.once("error", resolveKill);
    });
  }
  const signalGroup = (signal) => {
    if (platform === "win32" || !running || child.pid === undefined)
      return false;
    try {
      process.kill(-child.pid, signal);
      return true;
    } catch {
      // Not a group leader, or the group is gone.
      return false;
    }
  };
  const grouped = signalGroup("SIGTERM");
  if (!grouped) child.kill();
  let timer;
  const stopped = await Promise.race([
    exited.then(() => true),
    new Promise((resolveTimer) => {
      timer = setTimeout(() => resolveTimer(false), 2_000);
    }),
  ]);
  clearTimeout(timer);
  if (!stopped && grouped) signalGroup("SIGKILL");
  child.stderr?.destroy();
  let left = pids.filter((pid) => pid !== child.pid || running);
  while (left.length > 0) {
    left = (
      await Promise.all(
        left.map(async (pid) => ((await alive(pid)) ? pid : null)),
      )
    ).filter((pid) => pid !== null);
    if (left.length === 0 || Date.now() - started >= waitMs) break;
    await new Promise((resolveTimer) => setTimeout(resolveTimer, 100));
  }
  if (left.length > 0)
    process.stderr.write(
      `Chrome's process(es) ${left.join(", ")} still ran ${waitMs / 1000} s after it was stopped\n`,
    );
}

class CommandTimeout extends Error {}

const COMMAND_TIMEOUT_MS = 30_000;

class Cdp {
  #nextId = 0;
  #pending = new Map();
  #sessionId = null;
  #failure = null;

  constructor(socket) {
    this.socket = socket;
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      // The console is where a browser explains a lost GPU.
      if (process.env.ALPHA2_CHROME_CONSOLE === "1") {
        if (message.method === "Runtime.consoleAPICalled")
          process.stderr.write(
            `console.${message.params.type}: ${message.params.args.map((arg) => arg.value ?? arg.description).join(" ")}\n`,
          );
        if (message.method === "Log.entryAdded")
          process.stderr.write(`log: ${message.params.entry.text}\n`);
      }
      if (
        message.method === "Inspector.targetCrashed" &&
        message.sessionId === this.#sessionId
      )
        this.#fail("the browser page crashed");
      if (
        message.method === "Target.detachedFromTarget" &&
        message.params?.sessionId === this.#sessionId
      )
        this.#fail("the browser page was closed");
      const pending = this.#pending.get(message.id);
      if (!pending) return;
      this.#pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
    });
    socket.addEventListener("close", () =>
      this.#fail("Chrome closed the DevTools connection"),
    );
    socket.addEventListener("error", () =>
      this.#fail("the DevTools connection failed"),
    );
  }

  static async open(url) {
    const socket = new WebSocket(url);
    await new Promise((resolveOpen, reject) => {
      socket.addEventListener("open", resolveOpen, { once: true });
      socket.addEventListener("error", reject, { once: true });
    });
    return new Cdp(socket);
  }

  watchSession(sessionId) {
    this.#sessionId = sessionId;
  }

  #fail(reason) {
    this.#failure ??= new Error(reason);
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(this.#failure);
    }
    this.#pending.clear();
  }

  send(method, params = {}, sessionId, timeoutMs = COMMAND_TIMEOUT_MS) {
    if (this.#failure) return Promise.reject(this.#failure);
    const id = ++this.#nextId;
    return new Promise((resolveCommand, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(
          new CommandTimeout(
            `${method} got no answer within ${timeoutMs / 1000} s`,
          ),
        );
      }, timeoutMs);
      this.#pending.set(id, { resolve: resolveCommand, reject, timer });
      this.socket.send(
        JSON.stringify({
          id,
          method,
          params,
          ...(sessionId ? { sessionId } : {}),
        }),
      );
    });
  }

  within(promise, timeoutMs, what) {
    let timer;
    let poll;
    return Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(
          () =>
            reject(
              new Error(`${what} did not finish within ${timeoutMs / 1000} s`),
            ),
          timeoutMs,
        );
        poll = setInterval(() => {
          if (this.#failure) reject(this.#failure);
        }, 100);
      }),
    ]).finally(() => {
      clearTimeout(timer);
      clearInterval(poll);
    });
  }

  close() {
    this.#fail("the DevTools connection was closed");
    this.socket.close();
  }
}

/** Refusing other Host headers blocks DNS rebinding from reading the repository. */
export function fromThisMachine(host, port) {
  return [`127.0.0.1:${port}`, `localhost:${port}`, `[::1]:${port}`].includes(
    String(host ?? "").toLowerCase(),
  );
}

/**
 * Null for a path outside `root`, a hidden one, or an undecodable escape. `pathModule.relative`
 * handles Windows roots.
 */
export function servedFile(root, pathname, pathModule = path) {
  let decoded;
  try {
    decoded = decodeURIComponent(pathname);
  } catch {
    return null;
  }
  if (decoded.split(/[\\/]/).some((part) => part.startsWith("."))) return null;
  const file = pathModule.resolve(root, `.${decoded}`);
  const inside = pathModule.relative(root, file);
  if (
    inside === ".." ||
    inside.startsWith(`..${pathModule.sep}`) ||
    pathModule.isAbsolute(inside)
  ) {
    return null;
  }
  return file;
}
