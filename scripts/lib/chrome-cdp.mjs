// Headless Chrome driven over the DevTools protocol, for checks that need a real browser GPU stack.
// A page reports by setting `document.title` to URI-encoded JSON (it starts with `%7B`).
// Every command has a deadline, so a page whose main thread never yields still ends at its timeout.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path, { join, win32 } from "node:path";

import { BrowserStartError } from "./browser-errors.mjs";
import { recordSkip, required } from "./capabilities.mjs";

/**
 * The Chrome to run: ALPHA2_PARITY_CHROME, else the first usual install of Chrome or Chromium on
 * this platform that exists, else null. The options stand in for the process's own, for tests.
 */
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

/** Where Chrome and Chromium install on a platform, Chrome first. */
function chromeCandidates(platform, env) {
  if (platform === "darwin")
    return [
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ];
  if (platform === "win32") {
    // Machine-wide installs, then the per-user one Chrome makes without administrator rights.
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

/** The GPU modes `ALPHA2_CHROME_GPU` takes. */
export const CHROME_GPU_MODES = Object.freeze([
  "machine",
  "swiftshader",
  "none",
]);

/**
 * The GPU stack a Chrome launch uses, by name, and the flags that pick it.
 *
 * - `stock` when `ALPHA2_BROWSER_SETTINGS=stock`: no GPU flag at all.
 * - `custom` when `ALPHA2_CHROME_GPU_FLAGS` is set (even empty): those flags, split on whitespace.
 * - otherwise `ALPHA2_CHROME_GPU` (`machine`, `swiftshader` or `none`), by default `machine` on
 *   macOS and Windows and `swiftshader` on Linux, where headless Chrome lands on SwiftShader anyway
 *   and the machine's GPU needs a headed browser on a DRI3 display.
 *
 * Throws on an unknown mode, and a `BrowserStartError` for `machine` on Linux without a headed
 * browser (`ALPHA2_CHROME_HEADED=1`).
 *
 * @returns {{ mode: "machine" | "swiftshader" | "none" | "custom" | "stock", flags: string[] }}
 */
export function chromeGpuMode({
  env = process.env,
  platform = process.platform,
} = {}) {
  if (env.ALPHA2_BROWSER_SETTINGS === "stock")
    return { mode: "stock", flags: [] };
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
  // No GPU process at all: the browser's `auto` renderer falls back to the CPU.
  if (mode === "none")
    return { mode, flags: ["--disable-gpu", "--disable-software-rasterizer"] };
  if (platform === "darwin") return { mode, flags: ["--use-angle=metal"] };
  // Chrome's own D3D11 ANGLE and Dawn's D3D12.
  if (platform === "win32") return { mode, flags: [] };
  if (env.ALPHA2_CHROME_HEADED !== "1")
    throw new BrowserStartError(
      "gpu=machine on Linux needs a headed browser on a DRI3 display; run under scripts/run-with-gpu-display.sh or set ALPHA2_CHROME_GPU=swiftshader",
    );
  return {
    mode,
    flags: [
      "--use-angle=vulkan",
      "--enable-features=Vulkan",
      "--ignore-gpu-blocklist",
    ],
  };
}

/**
 * The Chrome a browser check runs in, or null when the machine has none (the check then skips its
 * browser half). A missing Chrome throws when ALPHA2_REQUIRE names `browser`.
 */
export function browserForCheck({ env = process.env, platform, exists } = {}) {
  const chrome = chromeExecutable({ env, platform, exists });
  if (chrome !== null) return chrome;
  if (required("browser", env))
    throw new Error(
      "no Chrome found: set ALPHA2_PARITY_CHROME to a Chrome executable (browser is required by ALPHA2_REQUIRE)",
    );
  return null;
}

/** Records that a check's browser half did not run (the `browser` capability), and why. */
export function reportSkippedBrowser(what, suite, env = process.env) {
  recordSkip(
    {
      suite,
      capability: "browser",
      reason: `${what} did not run: no Chrome found; set ALPHA2_PARITY_CHROME to a Chrome executable`,
    },
    env,
  );
}

/**
 * The closing summary of the cases a browser could not run, one line per case; empty when none.
 *
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
 * Starts a fresh Chrome with a temporary profile, opens `url`, and waits for the page to report.
 * Rejects when the page does not report in time, its renderer crashes, or Chrome or the DevTools
 * connection goes away. Chrome is killed and its profile removed in every case.
 *
 * @param {string} command The Chrome executable.
 * @param {string} url The page to open.
 * @param {object} [options]
 * @param {number} [options.timeoutMs] How long the page has to report, 30 s by default.
 * @param {(cdp: Cdp, sessionId: string) => Promise<unknown>} [options.onPage] Runs after
 *   navigation starts, within the same deadline.
 * @param {object} [options.viewport] `Emulation.setDeviceMetricsOverride` width and height.
 * @param {string} [options.initScript] Evaluated in every new document before its own scripts.
 * @param {(version: { product: string, userAgent: string }) => void} [options.onBrowser] Called
 *   with `Browser.getVersion` once the DevTools connection is open.
 * @returns {Promise<string>} The URI-encoded JSON title the page set. Rejects with a
 *   `BrowserStartError` when Chrome cannot be started or connected to.
 */
export async function runChromePage(
  command,
  url,
  { timeoutMs = 30_000, onPage, viewport, initScript, onBrowser } = {},
) {
  const { flags: gpuFlags } = chromeGpuMode();
  const profile = await mkdtemp(join(tmpdir(), "alpha2-chrome-"));
  let child;
  try {
    child = spawn(
      command,
      [
        // ALPHA2_CHROME_HEADED=1 runs a windowed Chrome, for a virtual display (xvfb-run): headless
        // Chrome cannot import a VideoFrame into WebGPU.
        ...(process.env.ALPHA2_CHROME_HEADED === "1" ? [] : ["--headless=new"]),
        "--no-sandbox",
        // A windowed Chrome otherwise stops at its first-run dialog, or on a desktop keyring prompt,
        // before it opens the DevTools endpoint.
        "--no-first-run",
        "--password-store=basic",
        "--disable-gpu-sandbox",
        "--enable-unsafe-webgpu",
        // SwiftShader on a small runner loses its device often, and Chrome then refuses WebGL to the
        // page, which would fail the recovery cases on Chrome's policy instead of on the engine.
        "--disable-domain-blocking-for-3d-apis",
        // An AudioContext would otherwise wait for a user gesture.
        "--autoplay-policy=no-user-gesture-required",
        ...gpuFlags,
        "--remote-debugging-port=0",
        `--user-data-dir=${profile}`,
        "about:blank",
      ],
      { stdio: ["ignore", "ignore", "pipe"] },
    );
  } catch (error) {
    // Node refuses some commands at once (a `.cmd` without a shell on Windows) instead of
    // reporting them on the child.
    await rm(profile, { recursive: true, force: true });
    throw new BrowserStartError(`Chrome could not start: ${error.message}`);
  }
  try {
    const webSocketUrl = await new Promise((resolveUrl, reject) => {
      // Allow for slow first-time Chrome startup on hosted runners.
      const timeout = setTimeout(
        () =>
          reject(new BrowserStartError("Chrome DevTools endpoint timed out")),
        60_000,
      );
      child.stderr.setEncoding("utf8");
      child.stderr.on("data", (chunk) => {
        const match = chunk.match(/DevTools listening on (ws:\/\/[^\s]+)/);
        if (match) {
          clearTimeout(timeout);
          resolveUrl(match[1]);
        }
      });
      child.once("error", (error) =>
        reject(
          new BrowserStartError(`Chrome could not start: ${error.message}`),
        ),
      );
      child.once("exit", (code) =>
        reject(new BrowserStartError(`Chrome exited ${code}`)),
      );
    });
    let cdp;
    try {
      cdp = await Cdp.open(webSocketUrl);
    } catch (error) {
      throw new BrowserStartError(
        `the DevTools connection failed: ${error?.message ?? error}`,
      );
    }
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
          // A page busy on its main thread answers nothing: wait no longer than the page has left.
          evaluation = await cdp.send(
            "Runtime.evaluate",
            { expression: "document.title", returnByValue: true },
            sessionId,
            remaining(),
          );
        } catch (error) {
          if (error instanceof CommandTimeout) break;
          throw error;
        }
        title = evaluation.result?.value ?? "";
        if (title.startsWith("%7B") || title.startsWith("%7b")) return title;
        await new Promise((resolveTimer) => setTimeout(resolveTimer, 100));
      }
      throw new Error(
        `browser page timed out at ${title} after ${timeoutMs / 1000} s`,
      );
    } finally {
      cdp.close();
    }
  } finally {
    await stopChrome(child);
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

/**
 * Ends Chrome and every process it started, and waits up to 2 s for it to exit. On Windows the
 * whole tree is ended by `taskkill /T`: a plain kill leaves the renderer and GPU processes holding
 * the profile and stderr pipe.
 */
export async function stopChrome(
  child,
  { platform = process.platform, spawnProcess = spawn } = {},
) {
  const running = child.exitCode === null && child.signalCode === null;
  const exited = running
    ? new Promise((resolveExit) => child.once("exit", resolveExit))
    : Promise.resolve();
  // Windows may reuse the pid after the process exits, so a tree is ended only while Chrome runs.
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
  child.kill();
  await Promise.race([
    exited,
    new Promise((resolveTimer) => setTimeout(resolveTimer, 2_000)),
  ]);
  child.stderr?.destroy();
}

/** A DevTools command that got no answer in time. */
class CommandTimeout extends Error {}

/** How long a command may wait for its answer unless it says otherwise. */
const COMMAND_TIMEOUT_MS = 30_000;

class Cdp {
  #nextId = 0;
  #pending = new Map();
  #sessionId = null;
  /** Why the connection or the page is gone, once it is. */
  #failure = null;

  constructor(socket) {
    this.socket = socket;
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      // ALPHA2_CHROME_CONSOLE=1 echoes the page's console, which is where a browser explains a lost GPU.
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

  /** The page session whose crash or detach fails every command. */
  watchSession(sessionId) {
    this.#sessionId = sessionId;
  }

  /** Fails every command waiting now and every one sent later with `reason`. */
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

  /** `promise`, failed after `timeoutMs` or when the connection or the page is gone. */
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

/**
 * Whether a request whose Host header is `host` was addressed to this machine's loopback at `port`.
 * Refusing other hosts blocks DNS rebinding from reading the repository.
 */
export function fromThisMachine(host, port) {
  return [`127.0.0.1:${port}`, `localhost:${port}`, `[::1]:${port}`].includes(
    String(host ?? "").toLowerCase(),
  );
}

/**
 * The file under `root` a static server sends for the URL path `pathname`, or null for a path
 * outside `root`, a hidden one (any part starting with a dot) or an escape that does not decode.
 * Containment uses `pathModule.relative` so Windows roots work; `pathModule` is injectable for tests.
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
