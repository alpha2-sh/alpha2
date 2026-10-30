// Which Chrome a browser check runs in, and what it does without one.
import assert from "node:assert/strict";
import test from "node:test";

import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { tmpdir } from "node:os";
import { join, posix, win32 } from "node:path";

import { BrowserStartError } from "./browser-errors.mjs";
import {
  browserForCheck,
  chromeExecutable,
  chromeGpuMode,
  fromThisMachine,
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
  assert.equal(
    chromeExecutable({
      env: { ALPHA2_PARITY_CHROME: "/opt/chrome" },
      platform: "linux",
      exists: () => false,
    }),
    "/opt/chrome",
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
  assert.deepEqual(
    chromeGpuMode({
      env: { ALPHA2_CHROME_GPU: "machine", ALPHA2_CHROME_HEADED: "1" },
      platform: "linux",
    }),
    {
      mode: "machine",
      flags: [
        "--use-angle=vulkan",
        "--enable-features=Vulkan",
        "--ignore-gpu-blocklist",
      ],
    },
  );
  assert.throws(
    () =>
      chromeGpuMode({
        env: { ALPHA2_CHROME_GPU: "machine" },
        platform: "linux",
      }),
    (error) =>
      error instanceof BrowserStartError && /DRI3 display/.test(error.message),
  );
});

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
  assert.equal(browserForCheck({ ...none, argv: [], env: {} }), null);
  assert.throws(
    () => browserForCheck({ ...none, argv: ["--require-browser"], env: {} }),
    /no Chrome found.*a browser is required/,
  );
  assert.throws(
    () =>
      browserForCheck({
        ...none,
        argv: [],
        env: { ALPHA2_REQUIRE_BROWSER: "1" },
      }),
    /a browser is required/,
  );
  assert.equal(
    browserForCheck({
      platform: "linux",
      exists: only("/snap/bin/chromium"),
      argv: ["--require-browser"],
      env: {},
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
        reason:
          "this browser cannot import a VideoFrame into WebGPU (Destroyed)",
      },
      {
        name: "audio-decoder",
        reason: "this browser has no WebCodecs AudioDecoder",
      },
    ]),
    [
      "2 case(s) skipped: this browser cannot run them; --no-skips fails them",
      "  webgpu-video-frame: this browser cannot import a VideoFrame into WebGPU (Destroyed)",
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
    const outcome = await settled(
      (onPage) =>
        runChromePage(chrome, page("setTimeout(() => { for (;;) {} }, 200);"), {
          timeoutMs: 3_000,
          onPage,
        }),
      90_000,
    );
    assert.equal(outcome.hung, undefined, "the check never ended");
    assert.match(outcome.error, /browser page timed out/);
    // The 3 s timeout, then up to 2 s to stop Chrome.
    assert.ok(outcome.seconds < 10, `${outcome.seconds} s`);
  },
);

test(
  "a renderer that crashes ends the page with that reason",
  { skip: chrome === null && "no Chrome on this machine" },
  async () => {
    const outcome = await settled(
      (opened) =>
        runChromePage(chrome, page(""), {
          timeoutMs: 20_000,
          onPage: (cdp, sessionId) => {
            opened();
            // Page.crash never answers: the renderer is gone.
            cdp.send("Page.crash", {}, sessionId).catch(() => {});
          },
        }),
      90_000,
    );
    assert.equal(outcome.hung, undefined, "the check never ended");
    assert.match(outcome.error, /crashed|closed/);
    assert.ok(outcome.seconds < 15, `${outcome.seconds} s`);
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
