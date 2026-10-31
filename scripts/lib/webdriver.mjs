// Safari and Firefox driven over W3C WebDriver, for checks Chrome's DevTools runner cannot reach.
// Same page contract as `chrome-cdp.mjs`: the page reports through `document.title`. Each page
// gets its own driver and session, and every request has a deadline.

import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createServer } from "node:net";
import { posix, win32 } from "node:path";

import { track, untrack } from "./browser-cleanup.mjs";
import { BrowserStartError } from "./browser-errors.mjs";
import { browserSettingsMode, stopChrome } from "./chrome-cdp.mjs";

/** How long one WebDriver request may wait for its answer unless the page has less time left. */
const REQUEST_TIMEOUT_MS = 30_000;
/** How long a driver has to start answering. */
const START_TIMEOUT_MS = 60_000;
/**
 * How long a session has to start its browser. A first Firefox launch on a fresh CI machine has
 * taken over 60 s.
 */
const SESSION_TIMEOUT_MS = 180_000;

/**
 * What each WebDriver browser runs: the driver's executable name, the variable that names it
 * instead, how the driver is told its port, and the capabilities a session asks for.
 */
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
                  // An AudioContext would otherwise wait for a user gesture.
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

/**
 * The Firefox a session runs: ALPHA2_FIREFOX, else the first usual install on this platform that
 * exists, else null (geckodriver then looks on its own).
 */
export function firefoxExecutable({
  env = process.env,
  platform = process.platform,
  exists = existsSync,
} = {}) {
  if (env.ALPHA2_FIREFOX) return env.ALPHA2_FIREFOX;
  return firefoxCandidates(platform, env).find((path) => exists(path)) ?? null;
}

/**
 * Where Firefox installs on a platform. On Linux these are the executables themselves:
 * /usr/bin/firefox and /snap/bin/firefox are launcher scripts, which geckodriver refuses as a
 * binary ("not a Firefox executable").
 */
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

/** The browsers `webDriverBrowser` knows. */
export const WEBDRIVER_BROWSERS = Object.freeze(Object.keys(BROWSERS));

/**
 * The browser `name` ("safari" or "firefox") with this machine's driver for it: ALPHA2_SAFARIDRIVER
 * or ALPHA2_GECKODRIVER, else the first one on PATH. Throws when there is none.
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

/** The first `file` in a PATH directory, or null. */
function onPath(file, { env, platform, exists }) {
  const paths = platform === "win32" ? win32 : posix;
  const path = env.PATH ?? env.Path ?? "";
  for (const directory of path.split(paths.delimiter).filter(Boolean)) {
    const candidate = paths.join(directory, file);
    if (exists(candidate)) return candidate;
  }
  return null;
}

/**
 * Whether the Mac's console is locked, from `ioreg -n Root -d1` (`readIoreg` returns its output).
 * Safari refuses automation while it is.
 */
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
 * Starts the browser's driver, opens a session, navigates to `url`, and waits for the page to
 * report. Rejects when the page does not report in time, the session cannot start, or the driver
 * goes away; the driver is ended in every case.
 *
 * A URL on 127.0.0.1 is opened as localhost, the origin Safari and Firefox runs of these pages use
 * for a secure context; the check servers answer both.
 *
 * @param {ReturnType<typeof webDriverBrowser>} browser
 * @param {string} url The page to open.
 * @param {object} [options]
 * @param {number} [options.timeoutMs] How long the page has to report, 30 s by default.
 * @param {number} [options.sessionTimeoutMs] How long the session has to start its browser, 180 s
 *   by default.
 * @param {(capabilities: object) => void} [options.onBrowser] Called with the session's
 *   capabilities (`browserName`, `browserVersion`) once the session started.
 * @param {string} [options.platform] Stands in for `process.platform` in tests.
 * @param {() => string} [options.readIoreg] Stands in for `ioreg -n Root -d1` in tests.
 * @returns {Promise<string>} The URI-encoded JSON title the page set. Rejects with a
 *   `BrowserStartError` when the driver or the session cannot start, and before the driver starts
 *   when Safari is asked for on a Mac whose console is locked.
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
  // Its own process group on POSIX, so ending it ends the browser it started too.
  const child = spawn(browser.driver, browser.driverArguments(port), {
    stdio: ["ignore", "pipe", "pipe"],
    detached: process.platform !== "win32",
    windowsHide: true,
  });
  const cleanup = track({ child, group: process.platform !== "win32" });
  // geckodriver logs to stdout, safaridriver to stderr: keep the tail of both.
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
    // Safari cancels a waited navigation when its page-load timeout runs out, and these pages may load
    // late: navigate from script, which returns at once, and poll the title.
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
        // A page busy on its main thread may answer nothing: wait no longer than it has left.
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
    // Ends the driver's process group, and with it the browser it started.
    await stopChrome(child);
    untrack(cleanup);
  }
}

/** A port nothing on this machine listens on now. */
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

/** A WebDriver request that got no answer in time. */
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

  /** Waits until the driver answers `/status`. */
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

  /** Opens a session of `browser` and returns its id and capabilities. */
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

  /** Sends one request and returns its `value`, or throws the error the driver answered. */
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

/** Whether a fetch failed because nothing listens yet. */
function isRefused(error) {
  const code = error?.cause?.code ?? error?.code;
  return code === "ECONNREFUSED" || code === "ECONNRESET";
}
