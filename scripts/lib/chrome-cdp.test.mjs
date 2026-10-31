// Which Chrome a browser check runs in, and what it does without one.
import assert from "node:assert/strict";
import test from "node:test";

import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, posix, win32 } from "node:path";

import { BrowserStartError } from "./browser-errors.mjs";
import {
  browserForCheck,
  chromeArguments,
  chromeExecutable,
  chromeGpuMode,
  edgeExecutable,
  fromThisMachine,
  reportSkippedBrowser,
  runChromePage,
  servedFile,
  skippedCasesSummary,
  stopChrome,
} from "./chrome-cdp.mjs";

const only =
  (...paths) =>
  (candidate) =>
    paths.includes(candidate);

test("Chrome or Chromium is found where each platform installs it", () => {
  for (const path of [
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
  ])
    assert.equal(
      chromeExecutable({ env: {}, platform: "linux", exists: only(path) }),
      path,
    );
  assert.equal(
    chromeExecutable({
      env: {},
      platform: "linux",
      exists: only("/snap/bin/chromium", "/usr/bin/google-chrome"),
    }),
    "/usr/bin/google-chrome",
  );
  const windows = {
    PROGRAMFILES: "C:\\Program Files",
    "PROGRAMFILES(X86)": "C:\\Program Files (x86)",
    LOCALAPPDATA: "C:\\Users\\a\\AppData\\Local",
  };
  for (const path of [
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Users\\a\\AppData\\Local\\Google\\Chrome\\Application\\chrome.exe",
  ])
    assert.equal(
      chromeExecutable({ env: windows, platform: "win32", exists: only(path) }),
      path,
    );
  assert.equal(
    chromeExecutable({
      env: {},
      platform: "darwin",
      exists: only("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    }),
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
  );
  // An app in the user's own Applications folder counts too.
  assert.equal(
    chromeExecutable({
      env: { HOME: "/Users/a" },
      platform: "darwin",
      exists: only(
        "/Users/a/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      ),
    }),
    "/Users/a/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  );
  assert.equal(
    chromeExecutable({
      env: { ALPHA2_PARITY_CHROME: "/opt/chrome" },
      platform: "linux",
      exists: () => false,
    }),
    "/opt/chrome",
  );
});

test("Edge is found where Windows and macOS install it", () => {
  const windows = {
    PROGRAMFILES: "C:\\Program Files",
    "PROGRAMFILES(X86)": "C:\\Program Files (x86)",
  };
  for (const path of [
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
  ])
    assert.equal(
      edgeExecutable({ env: windows, platform: "win32", exists: only(path) }),
      path,
    );
  const mac = "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge";
  assert.equal(
    edgeExecutable({ env: {}, platform: "darwin", exists: only(mac) }),
    mac,
  );
  assert.equal(
    edgeExecutable({
      env: {},
      platform: "linux",
      exists: only("/usr/bin/microsoft-edge-stable"),
    }),
    "/usr/bin/microsoft-edge-stable",
  );
  assert.equal(
    edgeExecutable({ env: { ALPHA2_EDGE: "/opt/edge" }, exists: () => false }),
    "/opt/edge",
  );
  assert.equal(
    edgeExecutable({ env: windows, platform: "win32", exists: only() }),
    null,
  );
  // A run without Chrome is told about the Edge it could use instead, never switched to it.
  assert.match(
    reportSkippedBrowser("the check", "suite", windows, {
      platform: "win32",
      exists: only(
        "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
      ),
    }),
    /no Chrome found; set ALPHA2_PARITY_CHROME to a Chrome executable, or run Edge with --browser edge$/,
  );
  assert.match(
    reportSkippedBrowser("the check", "suite", windows, {
      platform: "win32",
      exists: only(),
    }),
    /a Chrome executable$/,
  );
});

test("chrome picks the machine GPU on macOS and Windows and SwiftShader on Linux unless the run names a mode", () => {
  const swiftShader = {
    mode: "swiftshader",
    flags: ["--enable-unsafe-swiftshader", "--use-angle=swiftshader"],
  };
  assert.deepEqual(chromeGpuMode({ env: {}, platform: "linux" }), swiftShader);
  assert.deepEqual(chromeGpuMode({ env: {}, platform: "darwin" }), {
    mode: "machine",
    flags: ["--use-angle=metal"],
  });
  // Chrome's own D3D11 ANGLE and Dawn's D3D12: no flag.
  assert.deepEqual(chromeGpuMode({ env: {}, platform: "win32" }), {
    mode: "machine",
    flags: [],
  });
  for (const platform of ["linux", "darwin", "win32"]) {
    assert.deepEqual(
      chromeGpuMode({ env: { ALPHA2_CHROME_GPU: "swiftshader" }, platform }),
      swiftShader,
    );
    assert.deepEqual(
      chromeGpuMode({ env: { ALPHA2_CHROME_GPU: "none" }, platform }),
      {
        mode: "none",
        flags: ["--disable-gpu", "--disable-software-rasterizer"],
      },
    );
    assert.deepEqual(
      chromeGpuMode({
        env: {
          ALPHA2_CHROME_GPU: "none",
          ALPHA2_CHROME_GPU_FLAGS:
            " --use-angle=vulkan  --enable-features=Vulkan ",
        },
        platform,
      }),
      {
        mode: "custom",
        flags: ["--use-angle=vulkan", "--enable-features=Vulkan"],
      },
    );
    // An empty value passes no GPU flag at all: Chrome's own default.
    assert.deepEqual(
      chromeGpuMode({ env: { ALPHA2_CHROME_GPU_FLAGS: "" }, platform }),
      { mode: "custom", flags: [] },
    );
    assert.deepEqual(
      chromeGpuMode({ env: { ALPHA2_BROWSER_SETTINGS: "stock" }, platform }),
      { mode: "stock", flags: [] },
    );
    assert.throws(
      () => chromeGpuMode({ env: { ALPHA2_CHROME_GPU: "hardware" }, platform }),
      /unknown ALPHA2_CHROME_GPU "hardware": expected machine, swiftshader, none/,
    );
  }
  const headed = {
    ALPHA2_CHROME_GPU: "machine",
    ALPHA2_BROWSER_HEADED: "1",
    DISPLAY: ":1",
  };
  const dri3 = () => ({ tool: true, opened: true, dri3: true });
  assert.deepEqual(
    chromeGpuMode({ env: headed, platform: "linux", extensions: dri3 }),
    {
      mode: "machine",
      flags: [
        "--use-angle=vulkan",
        "--enable-features=Vulkan",
        "--ignore-gpu-blocklist",
      ],
    },
  );
  const startError = (pattern) => (error) =>
    error instanceof BrowserStartError && pattern.test(error.message);
  // Headless, or headed without a display.
  for (const env of [
    { ALPHA2_CHROME_GPU: "machine", DISPLAY: ":1" },
    { ALPHA2_CHROME_GPU: "machine", ALPHA2_BROWSER_HEADED: "1" },
  ])
    assert.throws(
      () => chromeGpuMode({ env, platform: "linux", extensions: dri3 }),
      startError(/needs a headed browser on a DRI3 display/),
    );
  // Xvfb: Chrome's GPU process would hang on it.
  assert.throws(
    () =>
      chromeGpuMode({
        env: headed,
        platform: "linux",
        extensions: () => ({ tool: true, opened: true, dri3: false }),
      }),
    startError(/DISPLAY :1 has no DRI3 \(Xvfb\)/),
  );
  assert.throws(
    () =>
      chromeGpuMode({
        env: headed,
        platform: "linux",
        extensions: () => ({ tool: true, opened: false, dri3: false }),
      }),
    startError(/DISPLAY :1 cannot be opened/),
  );
  // Without xdpyinfo the display goes unchecked, with a warning.
  assert.equal(
    chromeGpuMode({
      env: headed,
      platform: "linux",
      extensions: () => ({ tool: false, opened: false, dri3: false }),
    }).mode,
    "machine",
  );
  assert.throws(
    () =>
      chromeGpuMode({
        env: { ALPHA2_BROWSER_SETTINGS: "user" },
        platform: "linux",
      }),
    /unknown ALPHA2_BROWSER_SETTINGS "user": expected harness, stock/,
  );
});

test("stock settings pass no harness or GPU flag", () => {
  const gpuFlags = ["--use-angle=metal"];
  const profile = "/tmp/alpha2-chrome-x";
  const stock = chromeArguments({
    settings: "stock",
    gpuFlags,
    headed: false,
    profile,
  });
  assert.deepEqual(stock, [
    "--headless=new",
    "--no-first-run",
    "--password-store=basic",
    "--remote-debugging-port=0",
    "--user-data-dir=/tmp/alpha2-chrome-x",
    "about:blank",
  ]);
  const harness = chromeArguments({
    settings: "harness",
    gpuFlags,
    headed: true,
    profile,
  });
  assert.ok(!harness.includes("--headless=new"));
  for (const flag of [
    "--no-sandbox",
    "--disable-gpu-sandbox",
    "--enable-unsafe-webgpu",
    "--disable-domain-blocking-for-3d-apis",
    "--autoplay-policy=no-user-gesture-required",
    "--use-angle=metal",
  ]) {
    assert.ok(harness.includes(flag), flag);
    assert.ok(!stock.includes(flag), flag);
  }
});

test(
  "a start error carries the tail of Chrome's stderr",
  { skip: process.platform === "win32" && "the fake Chrome is a shell script" },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "alpha2-fake-chrome-"));
    try {
      const command = join(directory, "chrome");
      await writeFile(
        command,
        '#!/bin/sh\nfor i in $(seq 1 20); do echo "line $i" >&2; done\nexit 2\n',
      );
      await chmod(command, 0o755);
      await assert.rejects(
        runChromePage(command, "about:blank", { timeoutMs: 5_000 }),
        (error) => {
          assert.ok(error instanceof BrowserStartError);
          const lines = Array.from(
            { length: 10 },
            (_, index) => `line ${index + 11}`,
          );
          assert.equal(
            error.message,
            `Chrome exited 2; Chrome said: ${lines.join(" | ")}`,
          );
          return true;
        },
      );
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test("a Chrome that exits before its endpoint fails with a start error", async () => {
  // Node stands in for Chrome: it refuses Chrome's flags and exits 9 without the DevTools line.
  await assert.rejects(
    runChromePage(process.execPath, "about:blank", { timeoutMs: 5_000 }),
    (error) =>
      error instanceof BrowserStartError &&
      /Chrome exited 9/.test(error.message),
  );
  // A command Node cannot spawn at all fails the same way.
  await assert.rejects(
    runChromePage(join(tmpdir(), "alpha2-no-such-chrome"), "about:blank", {
      timeoutMs: 5_000,
    }),
    (error) =>
      error instanceof BrowserStartError &&
      /could not start/.test(error.message),
  );
});

test("without Chrome a check skips its browser half unless the run requires it", () => {
  const none = { platform: "linux", exists: () => false };
  assert.equal(browserForCheck({ ...none, env: {} }), null);
  assert.throws(
    () => browserForCheck({ ...none, env: { ALPHA2_REQUIRE: "wasm,browser" } }),
    /no Chrome found.*browser is required by ALPHA2_REQUIRE/,
  );
  assert.equal(
    browserForCheck({
      platform: "linux",
      exists: only("/snap/bin/chromium"),
      env: { ALPHA2_REQUIRE: "browser" },
    }),
    "/snap/bin/chromium",
  );
});

test("a run's end lists each case the browser could not run, with its reason", () => {
  assert.equal(skippedCasesSummary([]), "");
  assert.equal(
    skippedCasesSummary([
      {
        name: "webgpu-video-frame",
        reason: "this browser lost WebGPU importing a VideoFrame (Destroyed)",
      },
      {
        name: "audio-decoder",
        reason: "this browser has no WebCodecs AudioDecoder",
      },
    ]),
    [
      "2 case(s) skipped: this browser cannot run them; ALPHA2_REQUIRE fails the ones it names",
      "  webgpu-video-frame: this browser lost WebGPU importing a VideoFrame (Destroyed)",
      "  audio-decoder: this browser has no WebCodecs AudioDecoder",
      "",
    ].join("\n"),
  );
});

test("a static server sends the repository's files on Windows as on POSIX, and nothing outside it", () => {
  for (const [path, root, sep] of [
    [win32, "C:\\work\\alpha2", "\\"],
    [posix, "/work/alpha2", "/"],
  ]) {
    const inside = (...parts) => [root, ...parts].join(sep);
    for (const [pathname, expected] of [
      [
        "/packages/alpha2/wasm/alpha2_wasm.js",
        inside("packages", "alpha2", "wasm", "alpha2_wasm.js"),
      ],
      [
        "/tests/cases/host-parity-complex-project.json",
        inside("tests", "cases", "host-parity-complex-project.json"),
      ],
      [
        "/node_modules/mediabunny/dist/mediabunny.mjs",
        inside("node_modules", "mediabunny", "dist", "mediabunny.mjs"),
      ],
      [
        "/tests/name%20with%20space.json",
        inside("tests", "name with space.json"),
      ],
      // A hidden name is refused, inside or not: .git and .local are not a page's to read.
      ["/..hidden", null],
      ["/.git/config", null],
      ["/.local/PLAN.md", null],
      ["/packages/.cache/file.js", null],
      ["/%2egit/config", null],
      // Out of the repository, by dots or by an encoded separator, or not a path at all.
      ["/../secret", null],
      ["/tests/../../secret", null],
      ["/%2e%2e/secret", null],
      ["/%E0%A4%A", null],
    ]) {
      assert.equal(
        servedFile(root, pathname, path),
        expected,
        `${root} ${pathname}`,
      );
    }
  }
  // Another drive is outside too, though `relative` cannot climb to it.
  assert.equal(servedFile("C:\\work\\alpha2", "/D:/secret", win32), null);
});

test("a static server answers only requests addressed to this machine", () => {
  for (const host of [
    "127.0.0.1:8080",
    "localhost:8080",
    "LOCALHOST:8080",
    "[::1]:8080",
  ])
    assert.equal(fromThisMachine(host, 8080), true, host);
  // A rebound hostname, another port, or none.
  for (const host of [
    "attacker.example:8080",
    "127.0.0.1:8081",
    "localhost",
    "127.0.0.1.attacker.example:8080",
    undefined,
  ])
    assert.equal(fromThisMachine(host, 8080), false, String(host));
});

// A page that never answers, or a renderer that dies, must end a check at its timeout. These start
// a real Chrome; a machine without one has nothing to hang.
const chrome = chromeExecutable();
const page = (script) =>
  `data:text/html,${encodeURIComponent(`<title>pending</title><script>${script}</script>`)}`;

async function settled(start, watchdogMs) {
  let opened = null;
  const outcome = await Promise.race([
    start(() => {
      opened = Date.now();
    }).then(
      (value) => ({ value }),
      (error) => ({ error: error.message }),
    ),
    new Promise((resolve) =>
      setTimeout(() => resolve({ hung: true }), watchdogMs).unref(),
    ),
  ]);
  return {
    ...outcome,
    seconds: opened === null ? null : (Date.now() - opened) / 1000,
  };
}

test(
  "a page whose main thread never yields times out",
  { skip: chrome === null && "no Chrome on this machine" },
  async () => {
    const timeoutMs = 3_000;
    const outcome = await settled(
      (onPage) =>
        runChromePage(chrome, page("setTimeout(() => { for (;;) {} }, 200);"), {
          timeoutMs,
          onPage,
        }),
      90_000,
    );
    assert.equal(outcome.hung, undefined, "the check never ended");
    assert.match(outcome.error, /browser page timed out/);
    // The timeout, then the time to stop Chrome: the bound catches a check that outlives it, not
    // a slow machine.
    assert.ok(outcome.seconds < timeoutMs / 1000 + 10, `${outcome.seconds} s`);
  },
);

test(
  "a renderer that crashes ends the page with that reason",
  { skip: chrome === null && "no Chrome on this machine" },
  async () => {
    const timeoutMs = 20_000;
    const outcome = await settled(
      (opened) =>
        runChromePage(chrome, page(""), {
          timeoutMs,
          onPage: (cdp, sessionId) => {
            opened();
            // Page.crash never answers: the renderer is gone.
            cdp.send("Page.crash", {}, sessionId).catch(() => {});
          },
        }),
      90_000,
    );
    assert.equal(outcome.hung, undefined, "the check never ended");
    // The reason, not the timeout, ended it; the bound catches only a check that hangs past both.
    assert.match(outcome.error, /crashed|closed/);
    assert.ok(outcome.seconds < timeoutMs / 1000 + 10, `${outcome.seconds} s`);
  },
);

test(
  "a page that reports returns its report",
  { skip: chrome === null && "no Chrome on this machine" },
  async () => {
    const outcome = await settled(
      (onPage) =>
        runChromePage(
          chrome,
          page(
            "document.title = encodeURIComponent(JSON.stringify({ ok: true }));",
          ),
          { timeoutMs: 10_000, onPage },
        ),
      90_000,
    );
    assert.equal(outcome.value, encodeURIComponent('{"ok":true}'));
  },
);

/** A `spawn` that records what it was asked to run and reports it done at once. */
function recordingSpawn() {
  const calls = [];
  const spawnProcess = (command, args) => {
    calls.push([command, ...args]);
    const killer = new EventEmitter();
    setImmediate(() => killer.emit("exit", 0));
    return killer;
  };
  return { calls, spawnProcess };
}

test("stopping a Chrome that already exited on Windows ends no process tree", async () => {
  // Its PID may name another process by now: `taskkill /T` would end that process and its tree.
  const child = spawn(process.execPath, ["-e", "process.exit(1)"], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  await new Promise((resolveExit) => child.once("exit", resolveExit));
  const { calls, spawnProcess } = recordingSpawn();
  await stopChrome(child, { platform: "win32", spawnProcess });
  assert.deepEqual(calls, []);
});

test("stopping a running Chrome on Windows ends its process tree", async () => {
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  const { calls, spawnProcess } = recordingSpawn();
  await stopChrome(child, { platform: "win32", spawnProcess });
  assert.deepEqual(calls, [
    ["taskkill", "/T", "/F", "/PID", String(child.pid)],
  ]);
  assert.ok(child.exitCode !== null || child.signalCode !== null);
});
