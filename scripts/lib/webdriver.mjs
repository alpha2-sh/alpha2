// Safari and Firefox over W3C WebDriver, with the same `document.title` page contract as
// `chrome-cdp.mjs`. Each page gets its own driver and session.

import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createServer } from "node:net";
import { posix, win32 } from "node:path";

import { track, untrack } from "./browser-cleanup.mjs";
import { BrowserStartError } from "./browser-errors.mjs";
import { browserSettingsMode, stopChrome } from "./chrome-cdp.mjs";

const REQUEST_TIMEOUT_MS = 30_000;
const START_TIMEOUT_MS = 60_000;
// A first Firefox launch on a fresh CI machine can take over 60 s.
const SESSION_TIMEOUT_MS = 180_000;

const BROWSERS = {
  safari: {
    executable: "safaridriver",
    variable: "ALPHA2_SAFARIDRIVER",
    portArguments: (port) => ["-p", String(port)],
    capabilities: () => ({ browserName: "safari" }),
    sessionHint:
      "Safari automation needs `safaridriver --enable` run once, Develop > Allow Remote Automation in Safari, and an unlocked console session",
  },
  firefox: {
    executable: "geckodriver",
    variable: "ALPHA2_GECKODRIVER",
    portArguments: (port) => ["--port", String(port)],
    capabilities: ({ env, platform, exists }) => {
      const headed = env.ALPHA2_BROWSER_HEADED === "1";
      if (
        headed &&
        platform === "linux" &&
        !env.DISPLAY &&
        !env.WAYLAND_DISPLAY
      )
        throw new BrowserStartError(
          "a headed Firefox (ALPHA2_BROWSER_HEADED=1) needs DISPLAY or WAYLAND_DISPLAY",
        );
      const binary = firefoxExecutable({ env, platform, exists });
      return {
        browserName: "firefox",
        "moz:firefoxOptions": {
          ...(binary === null ? {} : { binary }),
          // Headless Firefox on macOS has no WebGPU adapter.
          args: headed ? [] : ["-headless"],
          ...(browserSettingsMode(env) === "stock"
            ? {}
            : {
                prefs: {
                  "dom.webgpu.enabled": true,
                  "gfx.webgpu.ignore-blocklist": true,
                  "media.autoplay.default": 0,
                  "media.autoplay.block-webaudio": false,
                },
              }),
        },
      };
    },
    sessionHint:
      "set ALPHA2_FIREFOX to a Firefox executable if geckodriver cannot find one",
  },
};

/** ALPHA2_FIREFOX, else the first installed Firefox, else null (geckodriver then searches). */
export function firefoxExecutable({
  env = process.env,
  platform = process.platform,
  exists = existsSync,
} = {}) {
  if (env.ALPHA2_FIREFOX) return env.ALPHA2_FIREFOX;
  return firefoxCandidates(platform, env).find((path) => exists(path)) ?? null;
}

// On Linux /usr/bin/firefox and /snap/bin/firefox are launcher scripts, which geckodriver refuses.
export function firefoxCandidates(
  platform = process.platform,
  env = process.env,
) {
  if (platform === "darwin")
    return ["/Applications/Firefox.app/Contents/MacOS/firefox"];
  if (platform === "win32")
    return [env.PROGRAMFILES ?? env.ProgramFiles]
      .filter(Boolean)
      .map((root) => win32.join(root, "Mozilla Firefox", "firefox.exe"));
  return [
    "/usr/lib/firefox/firefox",
    "/snap/firefox/current/usr/lib/firefox/firefox",
  ];
}

export const WEBDRIVER_BROWSERS = Object.freeze(Object.keys(BROWSERS));

/**
 * The driver is ALPHA2_SAFARIDRIVER or ALPHA2_GECKODRIVER, else the first on PATH.
 *
 * @returns {{ name: string, driver: string, driverArguments: (port: number) => string[],
 *   capabilities: object, sessionHint: string }}
 */
export function webDriverBrowser(
  name,
  { env = process.env, platform = process.platform, exists = existsSync } = {},
) {
  const browser = BROWSERS[name];
  if (browser === undefined)
    throw new Error(
      `unknown WebDriver browser ${name}: expected one of ${WEBDRIVER_BROWSERS.join(", ")}`,
    );
  const driver =
    env[browser.variable] ||
    onPath(
      platform === "win32" ? `${browser.executable}.exe` : browser.executable,
      { env, platform, exists },
    );
  if (!driver)
    throw new Error(
      `no ${browser.executable} found for ${name}: put it on PATH or set ${browser.variable}`,
    );
  return {
    name,
    driver,
    driverArguments: browser.portArguments,
    capabilities: browser.capabilities({ env, platform, exists }),
    sessionHint: browser.sessionHint,
  };
}

function onPath(file, { env, platform, exists }) {
  const paths = platform === "win32" ? win32 : posix;
  const path = env.PATH ?? env.Path ?? "";
  for (const directory of path.split(paths.delimiter).filter(Boolean)) {
    const candidate = paths.join(directory, file);
    if (exists(candidate)) return candidate;
  }
  return null;
}

/** Safari refuses automation while the console is locked. */
export function macConsoleLocked(
  readIoreg = () =>
    spawnSync("ioreg", ["-n", "Root", "-d1"], {
      encoding: "utf8",
      timeout: 10_000,
    }).stdout ?? "",
) {
  return /"CGSSessionScreenIsLocked"\s*=\s*Yes/.test(readIoreg());
}

/**
 * Opens `url` in a new driver session and waits for the page to report; the driver is ended in
 * every case. 127.0.0.1 is opened as localhost, which these browsers treat as a secure context.
 *
 * @param {ReturnType<typeof webDriverBrowser>} browser
 * @param {string} url
 * @param {object} [options]
 * @param {number} [options.timeoutMs] Default 30 s.
 * @param {number} [options.sessionTimeoutMs] Default 180 s.
 * @param {(capabilities: object) => void} [options.onBrowser]
 * @param {string} [options.platform]
 * @param {() => string} [options.readIoreg]
 * @returns {Promise<string>} The page's title. Rejects with `BrowserStartError` when the driver
 *   or session cannot start, or Safari's console is locked.
 */
export async function runWebDriverPage(
  browser,
  url,
  {
    timeoutMs = 30_000,
    sessionTimeoutMs = SESSION_TIMEOUT_MS,
    onBrowser,
    platform = process.platform,
    readIoreg,
  } = {},
) {
  if (
    browser.name === "safari" &&
    platform === "darwin" &&
    macConsoleLocked(readIoreg)
  )
    throw new BrowserStartError(
      "the Mac's console is locked; Safari automation needs an unlocked console session",
    );
  const target = new URL(url);
  if (target.hostname === "127.0.0.1") target.hostname = "localhost";
  const port = await freePort();
  // A process group on POSIX, so ending it ends the browser too.
  const child = spawn(browser.driver, browser.driverArguments(port), {
    stdio: ["ignore", "pipe", "pipe"],
    detached: process.platform !== "win32",
    windowsHide: true,
  });
  const cleanup = track({ child, group: process.platform !== "win32" });
  // geckodriver logs to stdout, safaridriver to stderr.
  let output = "";
  for (const stream of [child.stdout, child.stderr]) {
    stream.setEncoding("utf8");
    stream.on("data", (chunk) => {
      output = (output + chunk).slice(-2_000);
    });
  }
  const driver = new Driver(`http://127.0.0.1:${port}`, child, () => output);
  let session = null;
  try {
    try {
      await driver.ready(START_TIMEOUT_MS);
      const started = await driver.newSession(browser, sessionTimeoutMs);
      session = started.id;
      onBrowser?.(started.capabilities);
    } catch (error) {
      throw new BrowserStartError(error.message, { cause: error });
    }
    const deadline = Date.now() + timeoutMs;
    const remaining = () => Math.max(1, deadline - Date.now());
    // Safari cancels a waited navigation at its page-load timeout, so navigate from script and
    // poll the title.
    await driver.command(
      "POST",
      `/session/${session}/execute/sync`,
      {
        script:
          "const url = arguments[0]; setTimeout(() => { location.href = url; }, 0); return true;",
        args: [target.href],
      },
      Math.min(REQUEST_TIMEOUT_MS, remaining()),
    );
    let title = "pending";
    while (Date.now() < deadline) {
      try {
        title =
          (await driver.command(
            "GET",
            `/session/${session}/title`,
            undefined,
            Math.min(REQUEST_TIMEOUT_MS, remaining()),
          )) ?? "";
      } catch (error) {
        // A busy main thread may answer nothing, so wait no longer than the page has left.
        if (error instanceof RequestTimeout) break;
        throw error;
      }
      if (title.startsWith("%7B") || title.startsWith("%7b")) return title;
      await new Promise((resolveTimer) => setTimeout(resolveTimer, 100));
    }
    throw new Error(
      `browser page timed out at ${title} after ${timeoutMs / 1000} s`,
    );
  } finally {
    if (session !== null)
      await driver
        .command("DELETE", `/session/${session}`, undefined, 10_000)
        .catch(() => {});
    await stopChrome(child);
    untrack(cleanup);
  }
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolvePort(port));
    });
  });
}

class RequestTimeout extends Error {}

class Driver {
  /** Why the driver is gone, once it is. */
  #failure = null;

  constructor(base, child, output) {
    this.base = base;
    this.output = output;
    child.once("error", (error) => {
      this.#failure ??= new Error(
        `the WebDriver driver could not start: ${error.message}`,
      );
    });
    child.once("exit", (code, signal) => {
      this.#failure ??= new Error(
        `the WebDriver driver exited (${signal ?? code})${this.#said()}`,
      );
    });
  }

  #said() {
    const said = this.output().trim();
    return said ? `: ${said.split("\n").slice(-3).join(" | ")}` : "";
  }

  async ready(timeoutMs) {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (this.#failure) throw this.#failure;
      try {
        await this.command("GET", "/status", undefined, 1_000);
        return;
      } catch (error) {
        if (this.#failure) throw this.#failure;
        if (!(error instanceof RequestTimeout) && !isRefused(error))
          throw error;
      }
      await new Promise((resolveTimer) => setTimeout(resolveTimer, 50));
    }
    throw new Error(
      `the WebDriver driver did not answer within ${timeoutMs / 1000} s${this.#said()}`,
    );
  }

  async newSession(browser, timeoutMs) {
    try {
      const value = await this.command(
        "POST",
        "/session",
        { capabilities: { alwaysMatch: browser.capabilities } },
        timeoutMs,
      );
      if (typeof value?.sessionId !== "string")
        throw new Error(`no session id in ${JSON.stringify(value)}`);
      return { id: value.sessionId, capabilities: value.capabilities ?? {} };
    } catch (error) {
      throw new Error(
        `${browser.name} did not start a WebDriver session (${error.message}${this.#said()}); ${browser.sessionHint}`,
        { cause: error },
      );
    }
  }

  async command(method, path, body, timeoutMs = REQUEST_TIMEOUT_MS) {
    if (this.#failure) throw this.#failure;
    let response;
    try {
      response = await fetch(this.base + path, {
        method,
        headers: { "content-type": "application/json; charset=utf-8" },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (error) {
      if (error?.name === "TimeoutError")
        throw new RequestTimeout(
          `${method} ${path} got no answer within ${timeoutMs / 1000} s`,
        );
      if (this.#failure) throw this.#failure;
      throw error;
    }
    const text = await response.text();
    let json;
    try {
      json = JSON.parse(text);
    } catch {
      throw new Error(
        `${method} ${path} answered ${response.status} without JSON: ${text.slice(0, 200)}`,
      );
    }
    const value = json?.value;
    if (!response.ok || (value && typeof value.error === "string"))
      throw new Error(
        `${method} ${path}: ${value?.error ?? response.status}: ${value?.message ?? text.slice(0, 200)}`,
      );
    return value;
  }
}

function isRefused(error) {
  const code = error?.cause?.code ?? error?.code;
  return code === "ECONNREFUSED" || code === "ECONNRESET";
}
