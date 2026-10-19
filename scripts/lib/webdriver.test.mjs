// The WebDriver runner for Safari and Firefox, and the browser choice of a page check. A scripted
// Node driver stands in for safaridriver and geckodriver; a real Firefox runs where installed.
import assert from "node:assert/strict";
import test from "node:test";

import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { browserForRun, PAGE_BROWSERS } from "./browser-page.mjs";
import { runWebDriverPage, webDriverBrowser } from "./webdriver.mjs";

const only =
  (...paths) =>
  (candidate) =>
    paths.includes(candidate);

test("a WebDriver browser runs the driver named for it, else the one on PATH, else none", () => {
  const env = { PATH: "/opt/a:/usr/local/bin:/usr/bin" };
  const safari = webDriverBrowser("safari", {
    env,
    platform: "darwin",
    exists: only("/usr/bin/safaridriver"),
  });
  assert.equal(safari.driver, "/usr/bin/safaridriver");
  assert.deepEqual(safari.driverArguments(4444), ["-p", "4444"]);
  assert.deepEqual(safari.capabilities, { browserName: "safari" });
  const firefox = webDriverBrowser("firefox", {
    env: { ...env, ALPHA2_FIREFOX: "/opt/firefox/firefox" },
    platform: "linux",
    exists: only("/usr/local/bin/geckodriver", "/usr/bin/geckodriver"),
  });
  assert.equal(firefox.driver, "/usr/local/bin/geckodriver");
  assert.deepEqual(firefox.driverArguments(4444), ["--port", "4444"]);
  const options = firefox.capabilities["moz:firefoxOptions"];
  assert.equal(options.binary, "/opt/firefox/firefox");
  assert.deepEqual(options.args, ["-headless"]);
  assert.equal(options.prefs["dom.webgpu.enabled"], true);
  assert.equal(options.prefs["media.autoplay.default"], 0);
  // The variable wins over PATH; Windows looks for the .exe on its own separator.
  assert.equal(
    webDriverBrowser("firefox", {
      env: { ...env, ALPHA2_GECKODRIVER: "/elsewhere/geckodriver" },
      platform: "linux",
      exists: only("/usr/bin/geckodriver"),
    }).driver,
    "/elsewhere/geckodriver",
  );
  assert.equal(
    webDriverBrowser("firefox", {
      env: { Path: "C:\\tools;C:\\bin" },
      platform: "win32",
      exists: only("C:\\bin\\geckodriver.exe"),
    }).driver,
    "C:\\bin\\geckodriver.exe",
  );
  assert.throws(
    () =>
      webDriverBrowser("safari", { env, platform: "darwin", exists: only() }),
    /no safaridriver found for safari: put it on PATH or set ALPHA2_SAFARIDRIVER/,
  );
  assert.throws(
    () => webDriverBrowser("edge", { env, platform: "linux", exists: only() }),
    /unknown WebDriver browser edge/,
  );
});

test("a page check opens Chrome unless the run names Safari or Firefox", () => {
  assert.deepEqual(PAGE_BROWSERS, ["chrome", "safari", "firefox"]);
  const machine = {
    env: { PATH: "/usr/bin" },
    platform: "linux",
    exists: only("/usr/bin/google-chrome", "/usr/bin/geckodriver"),
  };
  assert.deepEqual(browserForRun({ argv: [], ...machine }), {
    kind: "chrome",
    name: "chrome",
    command: "/usr/bin/google-chrome",
  });
  for (const argv of [["--browser", "firefox"], ["--browser=firefox"]]) {
    const browser = browserForRun({ argv, ...machine });
    assert.equal(browser.kind, "webdriver");
    assert.equal(browser.name, "firefox");
    assert.equal(browser.webdriver.driver, "/usr/bin/geckodriver");
  }
  assert.equal(
    browserForRun({
      argv: [],
      ...machine,
      env: { ...machine.env, ALPHA2_BROWSER: "firefox" },
    }).name,
    "firefox",
  );
  assert.equal(
    browserForRun({
      argv: ["--browser", "chrome"],
      ...machine,
      env: { ...machine.env, ALPHA2_BROWSER: "firefox" },
    }).name,
    "chrome",
  );
  // A browser asked for by name must run; a missing Chrome skips unless the run requires it.
  assert.throws(
    () => browserForRun({ argv: ["--browser", "safari"], ...machine }),
    /no safaridriver found/,
  );
  const withoutChrome = { ...machine, exists: only() };
  assert.equal(browserForRun({ argv: [], ...withoutChrome }), null);
  assert.throws(
    () => browserForRun({ argv: [], ...withoutChrome, required: true }),
    /no Chrome found.*a browser is required/,
  );
  assert.throws(
    () => browserForRun({ argv: ["--browser", "opera"], ...machine }),
    /unknown browser "opera": --browser takes chrome, safari, firefox/,
  );
});

/**
 * A scripted WebDriver driver, run as `node <script> <port> <log> <mode>`, logging each request as
 * JSON lines. Modes: `report`, `never` (title stays "pending"), `refuse` (no session), `exit`
 * (dies before answering), `hang` (title requests never answered).
 */
const fakeDriver = `
import { appendFileSync } from "node:fs";
import { createServer } from "node:http";
const [port, log, mode] = process.argv.slice(2);
const note = (entry) => appendFileSync(log, JSON.stringify(entry) + "\\n");
note({ pid: process.pid });
if (mode === "exit") {
  process.stderr.write("could not bind the port\\n");
  process.exit(3);
}
let title = "";
const reply = (response, status, value) => {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify({ value }));
};
createServer((request, response) => {
  let body = "";
  request.on("data", (chunk) => (body += chunk));
  request.on("end", () => {
    const parsed = body ? JSON.parse(body) : undefined;
    note({ method: request.method, path: request.url, body: parsed });
    if (request.url === "/status") return reply(response, 200, { ready: true });
    if (request.method === "POST" && request.url === "/session") {
      if (mode === "refuse")
        return reply(response, 500, { error: "session not created", message: "remote automation is off" });
      return reply(response, 200, { sessionId: "s1", capabilities: {} });
    }
    if (request.url.endsWith("/execute/sync")) {
      if (mode === "report") title = encodeURIComponent(JSON.stringify({ ok: true, url: parsed.args[0] }));
      else title = "pending";
      return reply(response, 200, true);
    }
    if (request.url.endsWith("/title")) {
      if (mode === "hang") return;
      return reply(response, 200, title);
    }
    if (request.method === "DELETE") return reply(response, 200, null);
    reply(response, 404, { error: "unknown command", message: request.url });
  });
}).listen(Number(port), "127.0.0.1");
`;

async function withFakeDriver(mode, run) {
  const dir = await mkdtemp(join(tmpdir(), "alpha2-webdriver-"));
  try {
    const script = join(dir, "driver.mjs");
    const log = join(dir, "log.jsonl");
    await writeFile(script, fakeDriver);
    await writeFile(log, "");
    const browser = {
      name: "fake",
      driver: process.execPath,
      driverArguments: (port) => [script, String(port), log, mode],
      capabilities: { browserName: "fake" },
      sessionHint: "enable the fake",
    };
    const started = Date.now();
    const outcome = await run(browser).then(
      (value) => ({ value }),
      (error) => ({ error: error.message }),
    );
    outcome.seconds = (Date.now() - started) / 1000;
    const entries = (await readFile(log, "utf8"))
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line));
    const pid = entries[0]?.pid;
    return {
      ...outcome,
      requests: entries.slice(1),
      driverRunning: pid !== undefined && isRunning(pid),
    };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

function isRunning(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

test("a page that reports returns its report, opened as localhost, and the session ends", async () => {
  const outcome = await withFakeDriver("report", (browser) =>
    runWebDriverPage(browser, "http://127.0.0.1:8123/webgpu-smoke?x=1", {
      timeoutMs: 10_000,
    }),
  );
  assert.equal(outcome.error, undefined);
  assert.deepEqual(JSON.parse(decodeURIComponent(outcome.value)), {
    ok: true,
    url: "http://localhost:8123/webgpu-smoke?x=1",
  });
  const asked = outcome.requests.map(({ method, path }) => `${method} ${path}`);
  assert.ok(asked.includes("POST /session"));
  assert.deepEqual(
    outcome.requests.find(({ path }) => path === "/session").body,
    { capabilities: { alwaysMatch: { browserName: "fake" } } },
  );
  const navigation = outcome.requests.find(({ path }) =>
    path.endsWith("/execute/sync"),
  );
  assert.match(navigation.body.script, /setTimeout\(.*location\.href = url/);
  assert.equal(asked.at(-1), "DELETE /session/s1");
  assert.equal(outcome.driverRunning, false, "the driver outlived the page");
});

test("a page that never reports times out, and the session ends", async () => {
  const outcome = await withFakeDriver("never", (browser) =>
    runWebDriverPage(browser, "http://127.0.0.1:8123/case", {
      timeoutMs: 1_500,
    }),
  );
  assert.match(outcome.error, /browser page timed out at pending after 1.5 s/);
  assert.ok(outcome.seconds < 10, `${outcome.seconds} s`);
  assert.equal(
    outcome.requests.map(({ method, path }) => `${method} ${path}`).at(-1),
    "DELETE /session/s1",
  );
  assert.equal(outcome.driverRunning, false);
});

test("a page whose title requests go unanswered ends at its timeout", async () => {
  const outcome = await withFakeDriver("hang", (browser) =>
    runWebDriverPage(browser, "http://127.0.0.1:8123/case", {
      timeoutMs: 1_500,
    }),
  );
  assert.match(outcome.error, /browser page timed out/);
  assert.ok(outcome.seconds < 15, `${outcome.seconds} s`);
  assert.equal(outcome.driverRunning, false);
});

test("a session the browser refuses fails with its reason and what to do", async () => {
  const outcome = await withFakeDriver("refuse", (browser) =>
    runWebDriverPage(browser, "http://127.0.0.1:8123/case"),
  );
  assert.match(
    outcome.error,
    /fake did not start a WebDriver session \(POST \/session: session not created: remote automation is off\); enable the fake/,
  );
  assert.equal(outcome.driverRunning, false);
});

test("a driver that dies fails the page with what it said", async () => {
  const outcome = await withFakeDriver("exit", (browser) =>
    runWebDriverPage(browser, "http://127.0.0.1:8123/case"),
  );
  assert.match(
    outcome.error,
    /the WebDriver driver exited \(3\): could not bind the port/,
  );
  assert.ok(outcome.seconds < 10, `${outcome.seconds} s`);
});

// A real Firefox through geckodriver, where both are installed.
let firefox = null;
try {
  firefox = webDriverBrowser("firefox");
} catch {
  // No geckodriver.
}
const hasFirefox =
  firefox !== null &&
  (process.env.ALPHA2_FIREFOX
    ? existsSync(process.env.ALPHA2_FIREFOX)
    : ["/usr/bin/firefox", "/snap/bin/firefox"].some((path) =>
        existsSync(path),
      ));

test(
  "a Firefox page reports through geckodriver",
  { skip: !hasFirefox && "no geckodriver and Firefox on this machine" },
  async () => {
    const server = createServer((request, response) => {
      response.setHeader("content-type", "text/html; charset=utf-8");
      response.end(
        `<title>pending</title><script>document.title = encodeURIComponent(JSON.stringify({ ok: true, host: location.host, secure: isSecureContext }));</script>`,
      );
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    try {
      const { port } = server.address();
      const title = await runWebDriverPage(
        firefox,
        `http://127.0.0.1:${port}/page`,
        { timeoutMs: 60_000 },
      );
      assert.deepEqual(JSON.parse(decodeURIComponent(title)), {
        ok: true,
        host: `localhost:${port}`,
        secure: true,
      });
    } finally {
      server.close();
    }
  },
);
