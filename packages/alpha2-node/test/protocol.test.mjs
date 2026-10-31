import assert from "node:assert/strict";
import {
  chmod,
  cp,
  mkdtemp,
  mkdir,
  readFile,
  realpath,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import test, { after } from "node:test";

import {
  Alpha2AbortError,
  Alpha2GpuInitializationError,
  Alpha2NodeClient,
  Alpha2ProjectValidationError,
  exportProject,
  getCapabilities,
  hostExecutableName,
  resolveHostBinary,
} from "../dist/index.js";
import {
  createRequest,
  describeExit,
  loaderRefused,
  parseHostMessage,
  windowsPolicyBlocked,
} from "../dist/protocol.js";
import { failureOf } from "alpha2/export-failure";
import { decideRetry, RETRY_PROFILES } from "alpha2/export-retry";

/** Directories the tests made, removed once they have all run. */
const scratchDirectories = [];

/** A new directory under the system's temporary directory, removed after the last test. */
async function scratch(prefix) {
  const directory = await mkdtemp(join(tmpdir(), prefix));
  scratchDirectories.push(directory);
  return directory;
}

after(async () => {
  // A fake host made its directory the working directory, which Windows cannot remove.
  process.chdir(tmpdir());
  await Promise.all(
    scratchDirectories.map((directory) =>
      rm(directory, { recursive: true, force: true }),
    ),
  );
});

test("shared Rust and Node protocol corpus agrees", async () => {
  const fixtures = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/host-protocol-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  for (const fixture of fixtures) {
    assert.equal(typeof fixture.request.requestId, "string", fixture.name);
    assert.equal(typeof fixture.request.operation, "string", fixture.name);
    if (fixture.requestValid) {
      const { protocolVersion, operation, requestId, ...payload } =
        fixture.request;
      assert.deepEqual(
        createRequest(operation, requestId, payload),
        fixture.request,
        fixture.name,
      );
    }
    for (const message of fixture.messages) {
      assert.deepEqual(
        parseHostMessage(JSON.stringify(message)),
        message,
        fixture.name,
      );
    }
  }
});

test("the client refuses every invalid export request of the corpus before a host starts", async () => {
  const fixtures = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/host-protocol-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  const root = await scratch("alpha2-node-corpus-");
  const starts = join(root, "starts");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, STARTED_HOST(starts)),
    concurrency: 1,
  });
  const cases = fixtures.filter(
    (fixture) =>
      !fixture.requestValid && fixture.request.operation === "export",
  );
  assert.ok(cases.length >= 8, String(cases.length));
  for (const fixture of cases) {
    // The corpus's project is a placeholder; a valid one keeps each case to the field it is about.
    const { protocolVersion, operation, requestId, project, ...fields } =
      fixture.request;
    const error = await client
      .exportProject(exportRequest("/tmp/never-written.mp4", fields))
      .then(
        () => assert.fail(`${fixture.name}: the export resolved`),
        (cause) => cause,
      );
    // The host's code for the same request is INVALID_INPUT, or INVALID_REQUEST for a field of the
    // wrong type; both are the caller's input.
    assert.ok(
      ["INVALID_INPUT", "INVALID_REQUEST"].includes(fixture.expectedErrorCode),
      fixture.name,
    );
    assert.equal(
      error.code,
      "INVALID_INPUT",
      `${fixture.name}: ${error.message}`,
    );
    assert.equal(failureOf(error).code, "INVALID_INPUT", fixture.name);
  }
  assert.deepEqual(await startsIn(starts), []);
});

test("two stills spelled as one file are refused before a host starts", async () => {
  const root = await scratch("alpha2-node-stills-");
  const starts = join(root, "starts");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, STARTED_HOST(starts)),
  });
  const { project } = exportRequest("");
  for (const [first, second] of [
    ["Frame.png", "frame.png"],
    // e-acute composed, and as e and a combining acute accent.
    ["\u00e9.png", "e\u0301.png"],
    ["a.png", "./sub/../a.png"],
  ]) {
    const error = await client
      .stills({
        project,
        assets: {},
        stills: [
          { time: 0, output: join(root, first) },
          { time: 1, output: join(root, second) },
        ],
      })
      .then(
        () => assert.fail(`${first} and ${second}: the stills resolved`),
        (cause) => cause,
      );
    assert.equal(error.code, "INVALID_INPUT", error.message);
    assert.match(error.message, /two stills write one file/);
  }
  assert.deepEqual(await startsIn(starts), []);
});

test("media, resource and font fields not shaped as the host reads them are refused before a host starts", async () => {
  const root = await scratch("alpha2-node-shapes-");
  const starts = join(root, "starts");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, STARTED_HOST(starts)),
  });
  const refusals = [
    [{ fonts: "/staging/fonts/Inter.ttf" }, /fonts/],
    [{ fonts: 5 }, /fonts/],
    [{ fonts: [""] }, /fonts/],
    [{ assets: undefined }, /assets/],
    [{ assets: ["/a.mov"] }, /assets/],
    [{ frames: { seq: "/a.png" } }, /frames for source seq/],
    [{ frames: { seq: ["/a.png", 3] } }, /frames for source seq/],
    [{ frames: [] }, /frames must map/],
    [{ proxies: { a: "/a.proxy.mov" } }, /proxies for source a/],
    [{ proxies: { a: [{ path: "" }] } }, /proxies for source a/],
    [{ resources: { label: "/badge.png" } }, /resources\.label/],
    [{ effectAssets: { lut: 7 } }, /effect asset path for lut/],
    [{ effects: {} }, /effects/],
  ];
  for (const [fields, message] of refusals) {
    const error = await client
      .exportProject(exportRequest(join(root, "never.mp4"), fields))
      .then(
        () => assert.fail(`${JSON.stringify(fields)}: the export resolved`),
        (cause) => cause,
      );
    assert.equal(error.name, "Alpha2HostError", String(error));
    assert.equal(error.code, "INVALID_INPUT", error.message);
    assert.match(error.message, message);
  }
  for (const fields of [
    { assets: [] },
    { frames: { seq: "/a.png" } },
    { proxies: { a: [{}] } },
  ])
    await assert.rejects(
      client.conform({ project: exportRequest("").project, ...fields }),
      { code: "INVALID_INPUT" },
      JSON.stringify(fields),
    );
  assert.equal(
    await client.exportProject(
      exportRequest(join(root, "gap.mp4"), {
        frames: { seq: ["/a.png", null] },
      }),
    ),
    join(root, "gap.mp4"),
  );
  assert.deepEqual(await startsIn(starts), ["unset"]);
});

test("media readiness is the host's conform check, not a guess made here", async () => {
  // No `serve` here: a host started from this directory fails rather than answering.
  process.chdir(await scratch("alpha2-node-readiness-"));
  const client = new Alpha2NodeClient({
    hostPath: process.execPath,
    concurrency: 1,
  });
  const base = exportRequest("/tmp/never-written.mp4");
  // A sourceless video reaches the (fake) host's conform check; no local media error precedes it.
  await assert.rejects(
    client.exportProject({
      ...base,
      project: {
        ...base.project,
        sources: [{ id: "clip-a", kind: "video", locator: "a.mp4" }],
      },
    }),
    (error) => error.code !== "MISSING_INPUT" && error.code !== "INVALID_INPUT",
  );
  await assert.rejects(
    client.exportProject({ ...base, assets: { "clip-a": "" } }),
    (error) => error.code === "INVALID_INPUT" && /clip-a/.test(error.message),
  );
  await assert.rejects(
    client.exportProject({ ...base, media: "draft" }),
    (error) => error.code === "INVALID_INPUT" && /media/.test(error.message),
  );
});

test("a CONFORM_FAILED error carries the host's report", async () => {
  const { parseHostMessage } = await import("../dist/protocol.js");
  const report = {
    purpose: "export_original",
    ready: false,
    sources: [
      {
        source_id: "clip-a",
        selection: { media: "unavailable" },
        issues: [{ kind: "offline", blocking: true }],
      },
    ],
  };
  const message = parseHostMessage(
    JSON.stringify({
      protocolVersion: 1,
      requestId: "r",
      ok: false,
      error: { code: "CONFORM_FAILED", message: "not ready", details: report },
    }),
  );
  assert.deepEqual(message.error.details, report);
});

test("binary resolution maps Electron ASAR paths to unpacked resources", async () => {
  const root = await scratch("alpha2-node-test-");
  const unpacked = join(root, "app.asar.unpacked", "bin");
  await mkdir(unpacked, { recursive: true });
  const executable = join(
    unpacked,
    process.platform === "win32" ? "alpha2-host.exe" : "alpha2-host",
  );
  await writeFile(executable, "fixture");
  assert.equal(
    resolveHostBinary({
      hostPath: join(root, "app.asar", "bin", executable.split(/[\\/]/).at(-1)),
    }),
    executable,
  );
});

test("the host executable is named for its platform", () => {
  assert.equal(hostExecutableName("win32"), "alpha2-host.exe");
  assert.equal(hostExecutableName("darwin"), "alpha2-host");
  assert.equal(hostExecutableName("linux"), "alpha2-host");
  assert.equal(
    hostExecutableName(),
    process.platform === "win32" ? "alpha2-host.exe" : "alpha2-host",
  );
});

test("binary resolution finds the platform's executable in Electron resources and ALPHA2_HOST_PATH, and a blank ALPHA2_HOST_PATH is unset", async () => {
  const root = await scratch("alpha2-node-resolve-");
  const name = hostExecutableName();
  const resources = join(root, "resources");
  await mkdir(join(resources, "bin"), { recursive: true });
  await writeFile(join(resources, name), "fixture");
  assert.equal(
    resolveHostBinary({ resourcesPath: resources }),
    join(resources, name),
  );
  await writeFile(join(resources, "bin", name), "fixture");
  assert.equal(
    resolveHostBinary({ resourcesPath: resources }),
    join(resources, "bin", name),
  );
  const saved = process.env.ALPHA2_HOST_PATH;
  process.env.ALPHA2_HOST_PATH = join("resources", name);
  try {
    assert.equal(
      resolveHostBinary({ cwd: root, resourcesPath: join(root, "none") }),
      join(resources, name),
    );
    process.env.ALPHA2_HOST_PATH = join("resources", "missing");
    assert.throws(() => resolveHostBinary({ cwd: root }), {
      code: "HOST_NOT_FOUND",
    });
    // Blank is unset, as Compose leaves a variable it was not given: discovery goes on.
    for (const blank of ["", " "]) {
      process.env.ALPHA2_HOST_PATH = blank;
      assert.equal(
        resolveHostBinary({ cwd: root, resourcesPath: resources }),
        join(resources, "bin", name),
        JSON.stringify(blank),
      );
    }
    for (const hostPath of ["", " "])
      assert.throws(() => resolveHostBinary({ cwd: root, hostPath }), {
        code: "INVALID_INPUT",
        message: "hostPath must not be empty",
      });
  } finally {
    if (saved === undefined) delete process.env.ALPHA2_HOST_PATH;
    else process.env.ALPHA2_HOST_PATH = saved;
  }
});

test("binary resolution does not look in a cargo target directory, and its not-found message names the native archive", async () => {
  // A source checkout with a release build above dist/: the host is never found in a build tree.
  // realpath, because the module loader resolves symlinks (macOS's /var is /private/var).
  const root = await realpath(await scratch("alpha2-node-checkout-"));
  const name = hostExecutableName();
  const packageDirectory = join(root, "packages", "node");
  await cp(
    resolve(import.meta.dirname, "..", "dist"),
    join(packageDirectory, "dist"),
    { recursive: true },
  );
  await symlink(
    resolve(import.meta.dirname, "..", "..", "..", "node_modules"),
    join(root, "node_modules"),
    "junction",
  );
  await mkdir(join(root, "target", "release"), { recursive: true });
  await writeFile(join(root, "target", "release", name), "fixture");
  const { resolveHostBinary: resolveInCheckout } = await import(
    pathToFileURL(join(packageDirectory, "dist", "index.js")).href
  );
  const saved = { path: process.env.PATH, host: process.env.ALPHA2_HOST_PATH };
  process.env.PATH = "";
  delete process.env.ALPHA2_HOST_PATH;
  try {
    const options = { cwd: root, resourcesPath: join(root, "none") };
    assert.throws(
      () => resolveInCheckout(options),
      (error) => {
        assert.equal(error.code, "HOST_NOT_FOUND");
        assert.match(error.message, /github\.com\/alpha2-sh\/alpha2\/releases/);
        assert.ok(error.message.includes(`bin/${name}`), error.message);
        return true;
      },
    );
    await mkdir(join(packageDirectory, "bin"));
    await writeFile(join(packageDirectory, "bin", name), "fixture");
    assert.equal(
      resolveInCheckout(options),
      join(packageDirectory, "bin", name),
    );
  } finally {
    process.env.PATH = saved.path;
    if (saved.host === undefined) delete process.env.ALPHA2_HOST_PATH;
    else process.env.ALPHA2_HOST_PATH = saved.host;
  }
});

test("project validation error remains a typed host error", () => {
  const error = new Alpha2ProjectValidationError([
    "project id must not be empty",
  ]);
  assert.equal(error.code, "INVALID_INPUT");
  assert.deepEqual(error.problems, ["project id must not be empty"]);
});

test("cancellation settles only after the host process exits", async () => {
  const root = await scratch("alpha2-node-lifecycle-");
  const exitMarker = join(root, "exited");
  const pidFile = join(root, "host.pid");
  const executable = await fakeHost(
    root,
    `import { writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
const exitMarker = ${JSON.stringify(exitMarker)};
writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
let keepAlive;
// POSIX only: the host takes a while to exit after SIGTERM, and the client must wait for it. Windows has no SIGTERM; the client's kill ends the process at once.
process.on("SIGTERM", () => {
  setTimeout(() => {
    writeFileSync(exitMarker, "exited");
    process.exit(0);
  }, 100);
});
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  process.stdout.write(JSON.stringify({
    protocolVersion: 1,
    type: "progress",
    requestId: request.requestId,
    completed: 0,
    total: 1,
  }) + "\\n");
  keepAlive = setInterval(() => {}, 1_000);
});
`,
  );

  const controller = new AbortController();
  const client = new Alpha2NodeClient({
    hostPath: executable,
    concurrency: 1,
  });
  await assert.rejects(
    client.exportProject(
      exportRequest(join(root, "output.mp4"), {
        signal: controller.signal,
        onProgress: () => controller.abort(),
      }),
    ),
    (error) => error instanceof Alpha2AbortError,
  );
  assertExited(Number(await readFile(pidFile, "utf8")));
  if (process.platform !== "win32")
    assert.equal(await readFile(exitMarker, "utf8"), "exited");
});

/**
 * A host that records `start <output>` when it starts and `request <output>` when it reads its
 * request, answers an export after `answerMs`, and, when `ignoreTerm`, stays up through SIGTERM.
 */
const QUEUE_HOST = (
  log,
  { answerMs = 0, ignoreTerm = false } = {},
) => `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
const record = (line) => appendFileSync(${JSON.stringify(log)}, line + "\\n");
record("start " + process.pid);
process.on("SIGTERM", () => { if (!${ignoreTerm}) process.exit(0); });
const keepAlive = setInterval(() => {}, 1_000);
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  record("request " + request.output);
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n");
  send({ type: "progress", phase: "frames", completed: 0, total: 1 });
  setTimeout(() => { clearInterval(keepAlive); send({ ok: true, result: { output: request.output } }); }, ${answerMs});
});
`;

test(
  "an export aborted while it waits for a slot never starts, and the next one still runs",
  { timeout: 10_000 },
  async () => {
    const root = await scratch("alpha2-node-queued-abort-");
    const log = join(root, "log");
    const hostPath = await fakeHost(root, QUEUE_HOST(log, { answerMs: 400 }));
    const client = new Alpha2NodeClient({ hostPath, concurrency: 1 });
    const a = client.exportProject(exportRequest("a.mp4"));
    let aSettled = false;
    a.then(
      () => (aSettled = true),
      () => (aSettled = true),
    );
    const controller = new AbortController();
    const b = client.exportProject(
      exportRequest("b.mp4", { signal: controller.signal }),
    );
    const c = client.exportProject(exportRequest("c.mp4"));
    setTimeout(() => controller.abort(), 50);
    await assert.rejects(b, (error) => error instanceof Alpha2AbortError);
    // a answers 400 ms after its host read the request, so b settled first unless it waited.
    assert.equal(aSettled, false, "the abort waited for the slot");
    assert.deepEqual(await Promise.all([a, c]), ["a.mp4", "c.mp4"]);
    const requests = (await startsIn(log)).filter((line) =>
      line.startsWith("request"),
    );
    assert.deepEqual(requests, ["request a.mp4", "request c.mp4"]);
  },
);

test(
  "a host that stays up through SIGTERM is killed, and the abort settles once it has gone",
  { timeout: 30_000 },
  async () => {
    const root = await scratch("alpha2-node-escalate-");
    const log = join(root, "log");
    const hostPath = await fakeHost(
      root,
      QUEUE_HOST(log, { answerMs: 60_000, ignoreTerm: true }),
    );
    const controller = new AbortController();
    let aborted;
    const settled = await new Alpha2NodeClient({ hostPath })
      .exportProject(
        exportRequest("out.mp4", {
          signal: controller.signal,
          onProgress: () => {
            aborted = Date.now();
            controller.abort();
          },
        }),
      )
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    const waited = Date.now() - aborted;
    assert.ok(settled instanceof Alpha2AbortError, String(settled));
    // The host ignores the cancel line for the client's 5 s cancel timeout, then SIGTERM (on POSIX)
    // for its 5 s shutdown timeout, then is killed; the upper bound catches only an abort that
    // never settles.
    assert.ok(
      waited >= 4_900 && waited < 10_000 + 10_000,
      `settled ${waited} ms after the abort`,
    );
    const pid = Number((await startsIn(log))[0].split(" ")[1]);
    assertExited(pid);
  },
);

test("an abort seen only once the host has started ends it before the request is written", async () => {
  const root = await scratch("alpha2-node-abort-at-start-");
  const log = join(root, "log");
  const hostPath = await fakeHost(root, QUEUE_HOST(log));
  // Aborted between the client's check before it starts a host and its check after; the signal
  // is read when the slot is taken, before the host starts and after.
  let reads = 0;
  const signal = {
    get aborted() {
      reads += 1;
      return reads >= 3;
    },
    addEventListener() {},
    removeEventListener() {},
  };
  await assert.rejects(
    new Alpha2NodeClient({ hostPath }).exportProject(
      exportRequest("out.mp4", { signal }),
    ),
    (error) => error instanceof Alpha2AbortError,
  );
  assert.equal(reads, 3);
  assert.deepEqual(
    (await startsIn(log)).filter((line) => line.startsWith("request")),
    [],
  );
});

test("module calls with the same host path and concurrency share one pool, whatever decode mode each asks for", async () => {
  const root = await scratch("alpha2-node-concurrency-");
  const lock = join(root, "host.lock");
  const overlap = join(root, "overlap");
  const modes = join(root, "modes");
  const executable = await fakeHost(
    root,
    `import { appendFileSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
appendFileSync(${JSON.stringify(modes)}, (process.env.ALPHA2_VIDEO_DECODE ?? "unset") + "\\n");
const lock = ${JSON.stringify(lock)};
const overlap = ${JSON.stringify(overlap)};
// The lock names its holder: a host ended without running its exit (Windows ends hosts at once, with no SIGTERM) leaves it behind, and a dead holder is no overlap.
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
let ownsLock = false;
try {
  writeFileSync(lock, String(process.pid), { flag: "wx" });
  ownsLock = true;
} catch {
  const holder = Number(readFileSync(lock, "utf8"));
  if (!holder || alive(holder)) writeFileSync(overlap, "overlap");
  else {
    writeFileSync(lock, String(process.pid));
    ownsLock = true;
  }
}
const keepAlive = setInterval(() => {}, 1_000);
const exit = () => {
  clearInterval(keepAlive);
  if (ownsLock) rmSync(lock, { force: true });
  process.exit(0);
};
process.on("SIGTERM", () => setTimeout(exit, 50));
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  setTimeout(() => {
    process.stdout.write(JSON.stringify({
      protocolVersion: 1,
      requestId: request.requestId,
      ok: true,
      result: { output: request.output },
    }) + "\\n");
  }, 100);
});
`,
  );

  const options = { hostPath: executable, concurrency: 1 };
  await Promise.all([
    exportProject(exportRequest(join(root, "first.mp4")), options),
    exportProject(exportRequest(join(root, "second.mp4")), options),
    exportProject(exportRequest(join(root, "third.mp4")), {
      ...options,
      videoDecode: "software",
    }),
    exportProject(
      exportRequest(join(root, "fourth.mp4"), { videoDecode: "auto" }),
      { ...options, videoDecode: "software" },
    ),
    getCapabilities({ ...options, videoDecode: "hardware-required" }),
  ]);
  await assert.rejects(readFile(overlap), { code: "ENOENT" });
  assert.deepEqual((await startsIn(modes)).sort(), [
    "auto",
    "hardware-required",
    "software",
    "unset",
    "unset",
  ]);
});

test("an export sends the files its SVG and Lottie sources name as resources", async () => {
  // Without them in the request, a native export draws an SVG's <image href> and a Lottie file's
  // image assets as missing.
  const root = await scratch("alpha2-node-resources-");
  const recorded = join(root, "request.json");
  const executable = await fakeHost(
    root,
    `import { writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  writeFileSync(${JSON.stringify(recorded)}, line);
  process.stdout.write(JSON.stringify({
    protocolVersion: 1,
    requestId: request.requestId,
    ok: true,
    result: { output: request.output },
  }) + "\\n");
  process.exit(0);
});
`,
  );
  const options = { hostPath: executable, concurrency: 1 };
  const resources = { label: { "art/badge.png": "/media/badge.png" } };
  await exportProject(
    exportRequest(join(root, "with.mp4"), { resources }),
    options,
  );
  assert.deepEqual(
    JSON.parse(await readFile(recorded, "utf8")).resources,
    resources,
  );
  await exportProject(exportRequest(join(root, "without.mp4")), options);
  assert.deepEqual(JSON.parse(await readFile(recorded, "utf8")).resources, {});
});

test("a failed export keeps the host's failure report and its stderr", async () => {
  // The host answers once its stderr is in the pipe: on macOS a Node child writes a pipe
  // asynchronously, and the client's SIGTERM would otherwise lose the buffered tail.
  const root = await scratch("alpha2-node-failure-");
  const executable = await fakeHost(
    root,
    `import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n");
  process.stderr.write("x".repeat(40_000) + "\\nalpha2-host uncaptured GPU out-of-memory error: staging\\n", () => {
    send({ type: "progress", phase: "frames", completed: 3, total: 9 });
    send({ ok: false, error: {
      code: "ALPHA2_ERROR",
      message: "GPU texture readback failed: map-failed",
      failure: { code: "OUT_OF_MEMORY", origin: "native-host", operation: "export", backend: "native", phase: "readback", stage: "map-failed", frameIndex: 4, resource: "gpu", diagnostics: { videoPipeline: "readback" } },
    } });
  });
});
`,
  );
  const phases = [];
  const client = new Alpha2NodeClient({ hostPath: executable });
  await assert.rejects(
    client.exportProject(
      exportRequest(join(root, "output.mp4"), {
        onProgress: (progress) => phases.push(progress.phase),
      }),
    ),
    (error) => {
      assert.equal(error.code, "ALPHA2_ERROR");
      assert.equal(error.failure.code, "OUT_OF_MEMORY");
      assert.equal(error.failure.phase, "readback");
      assert.equal(error.failure.frameIndex, 4);
      assert.equal(error.failure.diagnostics.videoPipeline, "readback");
      assert.match(error.failure.stderr, /out-of-memory error: staging/);
      assert.ok(Buffer.byteLength(error.stderr) <= 16_384);
      return true;
    },
  );
  assert.deepEqual(phases, ["frames"]);
});

test("the stderr an error keeps is at most 16 KiB of UTF-8, from a whole character", async () => {
  const root = await scratch("alpha2-node-stderr-bytes-");
  const line = "오류: /미디어/서울/촬영본.mov 를 읽을 수 없습니다 😀\n";
  const hostPath = await fakeHost(
    root,
    `import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  process.stderr.write(${JSON.stringify(line)}.repeat(2000) + "the last line\\n", () => {
    process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: false, error: { code: "ALPHA2_ERROR", message: "failed" } }) + "\\n");
  });
});
`,
  );
  const error = await new Alpha2NodeClient({ hostPath })
    .exportProject(exportRequest(join(root, "out.mp4")))
    .then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
  for (const stderr of [error.stderr, error.failure.stderr]) {
    const bytes = Buffer.byteLength(stderr);
    assert.ok(bytes <= 16_384 && bytes > 16_300, `${bytes} bytes`);
    assert.ok(!stderr.includes("\uFFFD"), "a character was cut");
    assert.ok(stderr.endsWith("the last line\n"));
  }
});

test(
  "stderr the host wrote before answering is kept when it is read after the answer",
  {
    skip:
      process.platform === "win32" &&
      "the fixture writes its late stderr while it stays up through SIGTERM, and Windows ends a process at once on any kill",
  },
  async () => {
    // On macOS the answer on stdout is often read before the last chunk of stderr: here the host
    // writes that chunk only after answering, and takes a while to exit.
    const root = await scratch("alpha2-node-stderr-late-");
    const hostPath = await fakeHost(
      root,
      `import { createInterface } from "node:readline";
process.on("SIGTERM", () => {});
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  process.stderr.write("x".repeat(40_000) + "\\n");
  process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: false, error: { code: "ALPHA2_ERROR", message: "GPU texture readback failed: map-failed" } }) + "\\n", () => {
    setTimeout(() => process.stderr.write("alpha2-host uncaptured GPU out-of-memory error: staging\\n", () => process.exit(1)), 100);
  });
});
`,
    );
    const error = await new Alpha2NodeClient({ hostPath })
      .exportProject(exportRequest(join(root, "out.mp4")))
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    assert.equal(error.code, "ALPHA2_ERROR");
    for (const stderr of [error.stderr, error.failure.stderr])
      assert.ok(
        stderr.endsWith("out-of-memory error: staging\n"),
        `the tail ends ${JSON.stringify(stderr.slice(-80))}`,
      );
  },
);

test("a line on the host's stdout that is not a message is kept as its diagnostic output", async () => {
  // A Vulkan validation layer enabled from outside prints its messages to stdout, the host's
  // protocol stream, and can do so before the host answers.
  const root = await scratch("alpha2-node-stdout-noise-");
  const banner =
    "Validation Error: [ VUID-StandaloneSpirv-None-10684 ] vkCreateShaderModule";
  const host = (answer) => `import { createInterface } from "node:readline";
process.stdout.write(${JSON.stringify(banner)} + "\\n\\n");
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  process.stdout.write(" " + ${JSON.stringify(banner)} + "\\n" + ${JSON.stringify(answer)} .replace("REQUEST", request.requestId) + "\\n");
});
`;
  const ready = await new Alpha2NodeClient({
    hostPath: await fakeHost(
      join(root, "ready"),
      host(
        '{"protocolVersion":1,"requestId":"REQUEST","ok":true,"result":{"status":"ready"}}',
      ),
    ),
  }).getCapabilities();
  assert.equal(ready.status, "ready");
  const failed = await new Alpha2NodeClient({
    hostPath: await fakeHost(
      join(root, "failed"),
      host(
        '{"protocolVersion":1,"requestId":"REQUEST","ok":false,"error":{"code":"ALPHA2_ERROR","message":"failed"}}',
      ),
    ),
  })
    .exportProject(exportRequest(join(root, "out.mp4")))
    .then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
  assert.equal(failed.code, "ALPHA2_ERROR");
  assert.ok(failed.stderr.includes(banner), failed.stderr);
  const broken = new Alpha2NodeClient({
    hostPath: await fakeHost(join(root, "broken"), host("{not json")),
  });
  await assert.rejects(broken.getCapabilities(), {
    code: "INVALID_RESPONSE",
  });
});

test("a host that dies mid-export is HOST_EXITED, with no cause claimed", async () => {
  const root = await scratch("alpha2-node-exited-");
  const executable = await fakeHost(
    root,
    `import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  process.stdout.write(JSON.stringify({ protocolVersion: 1, type: "progress", requestId: request.requestId, phase: "finalizing", completed: 9, total: 9 }) + "\\n", () => {
    process.stderr.write("last words\\n", () => process.kill(process.pid, "SIGKILL"));
  });
});
`,
  );
  const client = new Alpha2NodeClient({ hostPath: executable });
  await assert.rejects(
    client.exportProject(exportRequest(join(root, "output.mp4"))),
    (error) => {
      assert.equal(error.code, "HOST_EXITED");
      // Windows reports a killed process by its exit code, with no signal.
      if (process.platform === "win32") {
        assert.equal(error.signal, null);
        assert.notEqual(error.exitCode, 0);
      } else assert.equal(error.signal, "SIGKILL");
      assert.equal(error.failure.code, "HOST_EXITED");
      assert.equal(error.failure.resource, "unknown");
      assert.equal(error.failure.phase, "mux");
      assert.match(error.failure.stderr, /last words/);
      return true;
    },
  );
});

test("a host the loader refuses to start is HOST_LOAD_FAILED, which no retry mends", async () => {
  // glibc's ld.so writes this and exits 127 when a library the host links (FFmpeg's) is missing or
  // of another major, before main and before the host can answer.
  const root = await scratch("alpha2-node-loader-");
  const words =
    "/opt/alpha2/bin/alpha2-host: error while loading shared libraries: libavcodec.so.62: cannot open shared object file: No such file or directory";
  const executable = await fakeHost(
    root,
    `process.stderr.write(${JSON.stringify(words + "\n")}, () => process.exit(127));\n`,
  );
  const client = new Alpha2NodeClient({ hostPath: executable });
  for (const call of [
    () => client.exportProject(exportRequest(join(root, "output.mp4"))),
    () => client.getCapabilities(),
  ]) {
    const error = await call().then(
      () => assert.fail("the request resolved"),
      (cause) => cause,
    );
    assert.equal(error.code, "HOST_LOAD_FAILED");
    assert.match(error.message, /libavcodec\.so\.62/);
    assert.equal(error.exitCode, 127);
    assert.equal(failureOf(error)?.code, "INVALID_INPUT");
    assert.equal(failureOf(error)?.phase, "prepare");
    assert.equal(retried(error), false);
  }
});

test("the loader's refusal is told apart from a host that started and died", () => {
  const refused = [
    // glibc: a missing library, and one of another version.
    [
      127,
      null,
      "alpha2-host: error while loading shared libraries: libswscale.so.9: cannot open shared object file: No such file or directory\n",
    ],
    [
      127,
      null,
      "./alpha2-host: /usr/lib/x86_64-linux-gnu/libavcodec.so.62: version `LIBAVCODEC_62' not found (required by ./alpha2-host)\n",
    ],
    [
      127,
      null,
      "./alpha2-host: symbol lookup error: ./alpha2-host: undefined symbol: av_frame_side_data_get_c\n",
    ],
    // musl.
    [
      127,
      null,
      "Error loading shared library libavcodec.so.62: No such file or directory (needed by /usr/bin/alpha2-host)\n",
    ],
    // macOS dyld aborts the process.
    [
      null,
      "SIGABRT",
      "dyld[4242]: Library not loaded: @rpath/libavcodec.62.dylib\n  Referenced from: <UUID> /opt/alpha2/bin/alpha2-host\n  Reason: tried: '/opt/alpha2/lib/libavcodec.62.dylib' (no such file)\n",
    ],
    [
      null,
      "SIGABRT",
      "dyld[4242]: Symbol not found: _av_frame_side_data_get_c\n",
    ],
    // Windows ends it with STATUS_DLL_NOT_FOUND or STATUS_ENTRYPOINT_NOT_FOUND, and writes nothing;
    // Node reports the status unsigned.
    [0xc0000135, null, ""],
    [0xc0000139, null, ""],
  ];
  // The signal is what the platform reports beside the words; the words decide.
  for (const [code, , stderr] of refused)
    assert.equal(loaderRefused(code, stderr), true, stderr || code);
  const started = [
    [null, "SIGKILL", "last words\n"],
    [1, null, "alpha2-host: stream did not contain valid UTF-8\n"],
    [127, null, ""],
    [
      134,
      null,
      "thread 'main' panicked at 'error while loading the project'\n",
    ],
    [0xc0000005, null, ""],
  ];
  for (const [code, , stderr] of started)
    assert.equal(loaderRefused(code, stderr), false, stderr || code);
});

test("a windows crash status is named", () => {
  assert.equal(
    describeExit(0xc0000409, null),
    "exit 3221226505 (0xC0000409 STATUS_STACK_BUFFER_OVERRUN)",
  );
  assert.equal(
    describeExit(0xc0000005, null),
    "exit 3221225477 (0xC0000005 STATUS_ACCESS_VIOLATION)",
  );
  assert.equal(describeExit(1, null), "exit 1");
  assert.equal(describeExit(null, "SIGKILL"), "exit SIGKILL");
  assert.equal(describeExit(null, null), "exit unknown");
});

/**
 * A host that fails with `DECODE_STALLED` unless it decodes in software, and records each decode
 * mode it ran with.
 * `resource` is what its stall reports: `gpu` for a shard that had opened a hardware session.
 */
async function stallingHost(root, { resource = "gpu" } = {}) {
  const runs = join(root, "runs");
  const executable = await fakeHost(
    root,
    `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const mode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  appendFileSync(${JSON.stringify(runs)}, mode + "\\n");
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n");
  if (mode === "software") send({ ok: true, result: { frames: 9 } });
  else send({ ok: false, error: {
    code: "DECODE_STALLED",
    message: "decode shard 0 has been inside one decoder operation for 40001 ms",
    failure: { code: "GPU_TIMEOUT", origin: "native-host", operation: "export", backend: "native", phase: "decode", resource: ${JSON.stringify(resource)} },
  } });
});
`,
  );
  return {
    executable,
    runs: async () =>
      (await readFile(runs, "utf8")).split("\n").filter(Boolean),
  };
}

/** Runs `body` with `ALPHA2_VIDEO_DECODE` unset, as a caller that never chose a mode has it. */
async function withoutDecodeEnv(body) {
  const saved = process.env.ALPHA2_VIDEO_DECODE;
  delete process.env.ALPHA2_VIDEO_DECODE;
  try {
    return await body();
  } finally {
    if (saved !== undefined) process.env.ALPHA2_VIDEO_DECODE = saved;
  }
}

test("an auto export whose hardware decoder stalls runs again in software", async () => {
  const root = await scratch("alpha2-node-stall-");
  const host = await stallingHost(root);
  let result;
  const client = new Alpha2NodeClient({
    hostPath: host.executable,
    videoDecode: "auto",
  });
  const output = join(root, "output.mp4");
  assert.equal(
    await client.exportProject(
      exportRequest(output, { onResult: (value) => (result = value) }),
    ),
    output,
  );
  assert.deepEqual(await host.runs(), ["auto", "software"]);
  assert.deepEqual(result, { frames: 9 });
});

test("a software run after a stall that fails too keeps the stall", async () => {
  const root = await scratch("alpha2-node-stall-then-fail-");
  const hostPath = await fakeHost(
    root,
    `import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n");
  if (process.env.ALPHA2_VIDEO_DECODE === "software") {
    process.stderr.write("second host\\n", () => send({ ok: false, error: { code: "RESOURCE_LIMIT", message: "decode queue budget too small", failure: { code: "RESOURCE_LIMIT", origin: "native-host", operation: "export", backend: "native", phase: "decode", resource: "cpu", diagnostics: { videoPipeline: "readback" } } } }));
  } else {
    process.stderr.write("first host: decoder stuck\\n", () => send({ ok: false, error: { code: "DECODE_STALLED", message: "decode shard 0 has been inside one decoder operation for 40001 ms", failure: { code: "GPU_TIMEOUT", origin: "native-host", operation: "export", backend: "native", phase: "decode", resource: "gpu" } } }));
  }
});
`,
  );
  const client = new Alpha2NodeClient({ hostPath, videoDecode: "auto" });
  const error = await client
    .exportProject(exportRequest(join(root, "out.mp4")))
    .then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
  assert.equal(error.code, "RESOURCE_LIMIT");
  assert.equal(error.failure.code, "RESOURCE_LIMIT");
  assert.match(error.stderr, /second host/);
  assert.equal(error.failure.diagnostics.videoPipeline, "readback");
  assert.equal(error.cause?.code, "DECODE_STALLED");
  assert.equal(
    error.failure.diagnostics.decodeStall.hostCode,
    "DECODE_STALLED",
  );
  assert.equal(error.failure.diagnostics.decodeStall.code, "GPU_TIMEOUT");
  assert.equal(error.failure.diagnostics.decodeStall.phase, "decode");
  assert.match(error.failure.diagnostics.decodeStall.message, /40001 ms/);
  assert.match(error.failure.diagnostics.decodeStall.stderr, /decoder stuck/);
  assert.equal(retried(error), false);
});

test("an export with no decode mode runs again in software when its hardware decoder stalls", async () => {
  // The host picks the default per platform (`auto` on Linux), so the client cannot tell from the
  // mode it sent.
  const root = await scratch("alpha2-node-stall-default-");
  const host = await stallingHost(root);
  const client = new Alpha2NodeClient({ hostPath: host.executable });
  const output = join(root, "output.mp4");
  await withoutDecodeEnv(async () =>
    assert.equal(await client.exportProject(exportRequest(output)), output),
  );
  assert.deepEqual(await host.runs(), ["unset", "software"]);
});

test("a stall with only software decode sessions is reported, not retried", async () => {
  const root = await scratch("alpha2-node-stall-software-");
  const host = await stallingHost(root, { resource: "unknown" });
  const client = new Alpha2NodeClient({
    hostPath: host.executable,
    videoDecode: "auto",
  });
  await assert.rejects(
    client.exportProject(exportRequest(join(root, "output.mp4"))),
    (error) => error.code === "DECODE_STALLED",
  );
  assert.deepEqual(await host.runs(), ["auto"]);
});

test("a stall where hardware decode is required is reported, not retried", async () => {
  const root = await scratch("alpha2-node-stall-required-");
  const host = await stallingHost(root);
  const client = new Alpha2NodeClient({
    hostPath: host.executable,
    videoDecode: "hardware-required",
  });
  await assert.rejects(
    client.exportProject(exportRequest(join(root, "output.mp4"))),
    (error) => {
      assert.equal(error.code, "DECODE_STALLED");
      assert.equal(error.failure.code, "GPU_TIMEOUT");
      assert.equal(error.failure.phase, "decode");
      return true;
    },
  );
  assert.deepEqual(await host.runs(), ["hardware-required"]);
});

test("an encoder stall is reported, not run again in software", async () => {
  // Software decoding does not move the encoder off the GPU: the export fails with what the host
  // found, and GPU_TIMEOUT is transient for the caller's retry policy.
  const root = await scratch("alpha2-node-encode-stall-");
  const runs = join(root, "runs");
  const executable = await fakeHost(
    root,
    `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  appendFileSync(${JSON.stringify(runs)}, (process.env.ALPHA2_VIDEO_DECODE ?? "unset") + "\\n");
  process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: false, error: {
    code: "ENCODE_STALLED",
    message: "the encode worker has been inside one encoder call for 40001 ms, past the 40000 ms limit, holding a hardware encode session",
    failure: { code: "GPU_TIMEOUT", origin: "native-host", operation: "export", backend: "native", phase: "encode", resource: "gpu" },
  } }) + "\\n");
  process.exit(1);
});
`,
  );
  const client = new Alpha2NodeClient({
    hostPath: executable,
    videoDecode: "auto",
  });
  await assert.rejects(
    client.exportProject(exportRequest(join(root, "output.mp4"))),
    (error) => {
      assert.equal(error.code, "ENCODE_STALLED");
      assert.equal(error.failure.code, "GPU_TIMEOUT");
      assert.equal(error.failure.resource, "gpu");
      return true;
    },
  );
  assert.deepEqual((await readFile(runs, "utf8")).split("\n").filter(Boolean), [
    "auto",
  ]);
});

test("a render kept after a failed publish is named on the error", async () => {
  const root = await scratch("alpha2-node-publish-kept-");
  const kept = join(root, "output.a2kept.mp4");
  const executable = await fakeHost(
    root,
    `import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: false, error: {
    code: "OUTPUT_PUBLISH_FAILED",
    message: "cannot publish the output: denied; the finished render is kept at " + ${JSON.stringify(kept)},
    details: { keptPath: ${JSON.stringify(kept)} },
    failure: { code: "OUTPUT_IO_FAILED", origin: "native-host", operation: "export", backend: "native", phase: "mux", resource: "unknown" },
  } }) + "\\n");
  process.exit(1);
});
`,
  );
  const client = new Alpha2NodeClient({ hostPath: executable });
  await assert.rejects(
    client.exportProject(exportRequest(join(root, "output.mp4"))),
    (error) => {
      assert.equal(error.code, "OUTPUT_PUBLISH_FAILED");
      assert.equal(error.keptPath, kept);
      assert.equal(error.failure.code, "OUTPUT_IO_FAILED");
      return true;
    },
  );
});

/** A host that records each start in `starts` and answers any request with success. */
const STARTED_HOST = (starts) => `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
appendFileSync(${JSON.stringify(starts)}, (process.env.ALPHA2_VIDEO_DECODE ?? "unset") + "\\n");
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const result = request.operation === "capabilities" ? { status: "ready", capabilities: {}, effectCatalog: {} } : { output: request.output };
  process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: true, result }) + "\\n", () => process.exit(0));
});
`;

/** The decode modes the hosts recorded in `starts` were started with, in order. */
async function startsIn(starts) {
  return (await readFile(starts, "utf8").catch(() => ""))
    .split("\n")
    .filter(Boolean);
}

test("a decode mode that is not one is refused before a host starts", async () => {
  const root = await scratch("alpha2-node-decode-mode-");
  const starts = join(root, "starts");
  const hostPath = await fakeHost(root, STARTED_HOST(starts));
  const refused = (error) => {
    assert.equal(error.code, "INVALID_INPUT");
    assert.match(
      error.message,
      /videoDecode must be software, auto, or hardware-required/,
    );
    assert.equal(failureOf(error)?.code, "INVALID_INPUT");
    return true;
  };
  assert.throws(
    () => new Alpha2NodeClient({ hostPath, videoDecode: "hardware" }),
    refused,
  );
  await assert.rejects(
    exportProject(exportRequest(join(root, "module.mp4")), {
      hostPath,
      videoDecode: "gpu",
    }),
    refused,
  );
  await assert.rejects(
    getCapabilities({ hostPath, videoDecode: "gpu" }),
    refused,
  );
  assert.deepEqual(await startsIn(starts), []);
});

/**
 * A host that records each request it reads in `log` as `<operation> <decode mode>`, answers
 * `capabilities` at once (failing the first one when `failFirst`) and `export` after `exportMs`.
 */
const PROBED_HOST = (
  log,
  { exportMs = 0, failFirst = false } = {},
) => `import { appendFileSync, existsSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
process.on("SIGTERM", () => process.exit(0));
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const mode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  appendFileSync(${JSON.stringify(log)}, request.operation + " " + mode + "\\n");
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n", () => process.exit(0));
  if (request.operation === "export") {
    setTimeout(() => send({ ok: true, result: { output: request.output } }), ${exportMs});
    return;
  }
  const failed = ${JSON.stringify(log)} + ".failed";
  if (${failFirst} && !existsSync(failed)) {
    writeFileSync(failed, "");
    send({ ok: false, error: { code: "ALPHA2_ERROR", message: "probe failed" } });
  } else send({ ok: true, result: { status: "ready", capabilities: { decode: { mode } }, effectCatalog: {} } });
});
`;

test("a capability probe a caller withdraws while an export holds the slot never starts", async () => {
  const root = await scratch("alpha2-node-probe-abort-");
  const log = join(root, "requests");
  const hostPath = await fakeHost(root, PROBED_HOST(log, { exportMs: 600 }));
  const client = new Alpha2NodeClient({ hostPath, concurrency: 1 });
  const exporting = client.exportProject(exportRequest(join(root, "out.mp4")));
  let exported = false;
  exporting.then(
    () => (exported = true),
    () => (exported = true),
  );
  const controller = new AbortController();
  const probe = client.getCapabilities({ signal: controller.signal });
  setTimeout(() => controller.abort(), 50);
  await assert.rejects(probe, (error) => error instanceof Alpha2AbortError);
  // The export answers 600 ms after its host read the request.
  assert.equal(exported, false, "the withdrawn call waited for the export");
  await exporting;
  // Slots are taken in the order they were asked for: once a later export has run, a probe still
  // queued behind the first export would have started before it.
  await client.exportProject(exportRequest(join(root, "after.mp4")));
  assert.deepEqual(await startsIn(log), ["export unset", "export unset"]);
});

test("one probe answers every capability call for a decode mode, and a failed one is not kept", async () => {
  const root = await scratch("alpha2-node-probe-shared-");
  const log = join(root, "requests");
  const hostPath = await fakeHost(root, PROBED_HOST(log, { failFirst: true }));
  const client = new Alpha2NodeClient({ hostPath, videoDecode: "auto" });
  await assert.rejects(client.getCapabilities(), { code: "ALPHA2_ERROR" });
  const withdrawn = new AbortController();
  const [first, second, third] = await Promise.allSettled([
    client.getCapabilities(),
    client.getCapabilities({ signal: withdrawn.signal }),
    client.getCapabilities(),
    Promise.resolve().then(() => withdrawn.abort()),
  ]);
  assert.equal(first.status, "fulfilled");
  assert.ok(second.reason instanceof Alpha2AbortError);
  assert.equal(third.value, first.value);
  assert.equal(await client.getCapabilities(), first.value);
  assert.equal(first.value.capabilities.decode.mode, "auto");
  const software = await client.getCapabilities({ videoDecode: "software" });
  assert.equal(software.capabilities.decode.mode, "software");
  assert.equal(
    await client.getCapabilities({ videoDecode: "software" }),
    software,
  );
  assert.deepEqual(await startsIn(log), [
    "capabilities auto",
    "capabilities auto",
    "capabilities software",
  ]);
});

/**
 * A host that records each request it reads in `log` as `<operation> <decode mode> <renderer>`,
 * from its environment, and answers it at once: a capability report naming its renderer, or an
 * export result.
 */
const RENDERER_HOST = (log) => `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const decode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  const renderer = process.env.ALPHA2_RENDERER ?? "unset";
  appendFileSync(${JSON.stringify(log)}, request.operation + " " + decode + " " + renderer + "\\n");
  const result = request.operation === "capabilities"
    ? { status: "ready", renderer, rendererReason: null, capabilities: {}, effectCatalog: {} }
    : { output: request.output };
  process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: true, result }) + "\\n", () => process.exit(0));
});
`;

/** Runs `body` with `ALPHA2_RENDERER` set to `value`, or unset for `undefined`. */
async function withRendererEnv(value, body) {
  const saved = process.env.ALPHA2_RENDERER;
  if (value === undefined) delete process.env.ALPHA2_RENDERER;
  else process.env.ALPHA2_RENDERER = value;
  try {
    return await body();
  } finally {
    if (saved === undefined) delete process.env.ALPHA2_RENDERER;
    else process.env.ALPHA2_RENDERER = saved;
  }
}

test("a renderer that is not one is refused before a host starts", async () => {
  const root = await scratch("alpha2-node-renderer-mode-");
  const starts = join(root, "starts");
  const hostPath = await fakeHost(root, STARTED_HOST(starts));
  const refused = (error) => {
    assert.equal(error.code, "INVALID_INPUT");
    assert.match(error.message, /renderer must be auto, gpu, or cpu/);
    assert.equal(failureOf(error)?.code, "INVALID_INPUT");
    return true;
  };
  assert.throws(
    () => new Alpha2NodeClient({ hostPath, renderer: "software" }),
    refused,
  );
  const client = new Alpha2NodeClient({ hostPath });
  await assert.rejects(
    client.exportProject(
      exportRequest(join(root, "client.mp4"), { renderer: "vulkan" }),
    ),
    refused,
  );
  await assert.rejects(client.getCapabilities({ renderer: "GPU" }), refused);
  await assert.rejects(
    exportProject(exportRequest(join(root, "module.mp4")), {
      hostPath,
      renderer: "metal",
    }),
    refused,
  );
  await assert.rejects(getCapabilities({ hostPath, renderer: "" }), refused);
  assert.deepEqual(await startsIn(starts), []);
});

test("the renderer reaches the host in ALPHA2_RENDERER, per request over the client's", async () => {
  const root = await scratch("alpha2-node-renderer-env-");
  const log = join(root, "requests");
  const hostPath = await fakeHost(root, RENDERER_HOST(log));
  await withoutDecodeEnv(() =>
    withRendererEnv(undefined, async () => {
      await new Alpha2NodeClient({ hostPath }).exportProject(
        exportRequest(join(root, "unset.mp4")),
      );
      const client = new Alpha2NodeClient({
        hostPath,
        renderer: "cpu",
        videoDecode: "software",
      });
      await client.exportProject(exportRequest(join(root, "client.mp4")));
      await client.exportProject(
        exportRequest(join(root, "request.mp4"), { renderer: "gpu" }),
      );
      await client.conform({ project: exportRequest("x.mp4").project });
      await exportProject(exportRequest(join(root, "module.mp4")), {
        hostPath,
        renderer: "auto",
      });
      await exportProject(
        exportRequest(join(root, "module-request.mp4"), { renderer: "cpu" }),
        { hostPath, renderer: "gpu" },
      );
    }),
  );
  assert.deepEqual(await startsIn(log), [
    "export unset unset",
    "export software cpu",
    "export software gpu",
    "conform software cpu",
    "export unset auto",
    "export unset cpu",
  ]);
});

test("one probe answers every capability call for a renderer, and the environment's is its own", async () => {
  const root = await scratch("alpha2-node-renderer-probe-");
  const log = join(root, "requests");
  const hostPath = await fakeHost(root, RENDERER_HOST(log));
  const client = new Alpha2NodeClient({ hostPath, renderer: "cpu" });
  await withoutDecodeEnv(() =>
    withRendererEnv(undefined, async () => {
      const cpu = await client.getCapabilities();
      assert.equal(cpu.renderer, "cpu");
      assert.equal(await client.getCapabilities(), cpu);
      assert.equal(await client.getCapabilities({ renderer: "cpu" }), cpu);
      const gpu = await client.getCapabilities({ renderer: "gpu" });
      assert.equal(gpu.renderer, "gpu");
      assert.equal(await client.getCapabilities({ renderer: "gpu" }), gpu);
      // The decode mode and the renderer each start their own probe.
      const software = await client.getCapabilities({
        videoDecode: "software",
      });
      assert.notEqual(software, cpu);
      assert.equal(software.renderer, "cpu");
      // Unset, the renderer is the one this process's environment names when the probe starts.
      const unset = new Alpha2NodeClient({ hostPath });
      const host = await unset.getCapabilities();
      assert.equal(host.renderer, "unset");
      assert.equal(await unset.getCapabilities(), host);
      const environment = await withRendererEnv("cpu", () =>
        unset.getCapabilities(),
      );
      assert.equal(environment.renderer, "cpu");
      assert.notEqual(environment, host);
    }),
  );
  assert.deepEqual(await startsIn(log), [
    "capabilities unset cpu",
    "capabilities unset gpu",
    "capabilities software cpu",
    "capabilities unset unset",
    "capabilities unset cpu",
  ]);
});

test("module calls that differ only in renderer share one pool", async () => {
  // A probe per renderer, each in the one slot.
  const root = await scratch("alpha2-node-renderer-pool-");
  const log = join(root, "requests");
  const hostPath = await fakeHost(root, RENDERER_HOST(log));
  const options = { hostPath, concurrency: 1 };
  await withoutDecodeEnv(() =>
    withRendererEnv(undefined, async () => {
      const [cpu, gpu] = await Promise.all([
        getCapabilities({ ...options, renderer: "cpu" }),
        getCapabilities({ ...options, renderer: "gpu" }),
      ]);
      assert.equal(cpu.renderer, "cpu");
      assert.equal(gpu.renderer, "gpu");
      assert.equal(await getCapabilities({ ...options, renderer: "cpu" }), cpu);
    }),
  );
  assert.deepEqual((await startsIn(log)).sort(), [
    "capabilities unset cpu",
    "capabilities unset gpu",
  ]);
});

test("an export that runs again in software after a decode stall keeps its renderer", async () => {
  const root = await scratch("alpha2-node-renderer-stall-");
  const runs = join(root, "runs");
  const hostPath = await fakeHost(
    root,
    `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  const mode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  appendFileSync(${JSON.stringify(runs)}, mode + " " + (process.env.ALPHA2_RENDERER ?? "unset") + "\\n");
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n");
  if (mode === "software") send({ ok: true, result: { frames: 9 } });
  else send({ ok: false, error: {
    code: "DECODE_STALLED",
    message: "decode shard 0 has been inside one decoder operation for 40001 ms",
    failure: { code: "GPU_TIMEOUT", origin: "native-host", operation: "export", backend: "native", phase: "decode", resource: "gpu" },
  } });
});
`,
  );
  const client = new Alpha2NodeClient({
    hostPath,
    videoDecode: "auto",
    renderer: "cpu",
  });
  await client.exportProject(exportRequest(join(root, "out.mp4")));
  assert.deepEqual(await startsIn(runs), ["auto cpu", "software cpu"]);
});

test("an answer to a line the host could not read is the request's answer", async () => {
  // The host answers a line it cannot read an id from with requestId "unknown" and reads on; the
  // client ended its stdin, so the host then exits 0.
  const root = await scratch("alpha2-node-unread-");
  const hostPath = await fakeHost(
    root,
    ANSWERING_HOST({
      requestId: "unknown",
      ok: false,
      error: {
        code: "INVALID_REQUEST",
        message: "the request line is 40000000 bytes; the maximum is 134217728",
      },
    }),
  );
  const error = await new Alpha2NodeClient({ hostPath })
    .exportProject(exportRequest(join(root, "out.mp4")))
    .then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
  assert.equal(error.code, "INVALID_REQUEST", error.message);
  assert.match(error.message, /maximum is 134217728/);
  assert.equal(failureOf(error).code, "INVALID_INPUT");
  assert.equal(retried(error), false);
});

test("a request line longer than the host reads is refused before a host starts", async () => {
  const root = await scratch("alpha2-node-long-line-");
  const starts = join(root, "starts");
  const hostPath = await fakeHost(root, STARTED_HOST(starts));
  const assets = Object.fromEntries(
    Array.from({ length: 140_000 }, (_, index) => [
      `asset-${index}`,
      `/media/${"x".repeat(1_000)}-${index}.mov`,
    ]),
  );
  await assert.rejects(
    new Alpha2NodeClient({ hostPath }).exportProject(
      exportRequest(join(root, "out.mp4"), { assets }),
    ),
    (error) =>
      error.code === "INVALID_INPUT" &&
      /the host reads at most 134217728/.test(error.message),
  );
  assert.deepEqual(await startsIn(starts), []);
});

test("a project over the engine's text budget is refused before a host starts", async () => {
  const client = new Alpha2NodeClient({
    hostPath: process.execPath,
    concurrency: 1,
  });
  const request = exportRequest("/tmp/never.mp4");
  // A valid object whose text is over 64 MiB: only the text shows it.
  request.project.name = "x".repeat(64 * 1024 * 1024);
  for (const run of [
    () => client.exportProject(request),
    () => client.conform({ project: request.project }),
  ])
    await assert.rejects(
      run(),
      (error) =>
        error instanceof Alpha2ProjectValidationError &&
        /over the 67108864 byte limit/.test(error.problems[0]),
    );
});

test("a memory budget is checked before a host starts, and sent as given", async () => {
  const client = new Alpha2NodeClient({
    hostPath: process.execPath,
    concurrency: 1,
  });
  await assert.rejects(
    client.exportProject(
      exportRequest("/tmp/never.mp4", { memory: { inFlightBytes: 0 } }),
    ),
    (error) =>
      error.code === "INVALID_INPUT" &&
      /memory\.inFlightBytes/.test(error.message),
  );
  await assert.rejects(
    client.exportProject(
      exportRequest("/tmp/never.mp4", { memory: { gpuBytes: 1 } }),
    ),
    (error) => error.code === "INVALID_INPUT" && /gpuBytes/.test(error.message),
  );
  await assert.rejects(
    client.exportProject(
      exportRequest("/tmp/never.mp4", { memory: { providerSourceBytes: 0 } }),
    ),
    (error) =>
      error.code === "INVALID_INPUT" &&
      /memory\.providerSourceBytes/.test(error.message),
  );
});

test("progress phases other than frames and finalizing are refused", () => {
  assert.throws(() =>
    parseHostMessage(
      JSON.stringify({
        protocolVersion: 1,
        type: "progress",
        requestId: "r",
        phase: "done",
        completed: 1,
        total: 1,
      }),
    ),
  );
});

/** A host that fails to start: it reports `fatal` and exits without reading its request. */
const FAILING_HOST = `process.stdout.write(JSON.stringify({
  protocolVersion: 1,
  type: "fatal",
  error: { code: "GPU_INIT_FAILED", message: "no adapter", fatal: true },
}) + "\\n", () => process.exit(70));
`;

/**
 * A request far larger than a pipe buffer, so part of it is still unwritten when the host exits.
 */
const largeRequest = (output, overrides = {}) =>
  exportRequest(output, {
    assets: Object.fromEntries(
      Array.from({ length: 120_000 }, (_, i) => [
        `asset-${i}`,
        `/media/${"x".repeat(48)}-${i}.mov`,
      ]),
    ),
    ...overrides,
  });

test("a host that exits before reading a large request reports why, without killing this process", async () => {
  const root = await scratch("alpha2-node-epipe-");
  const executable = await fakeHost(root, FAILING_HOST);
  const client = new Alpha2NodeClient({
    hostPath: executable,
    concurrency: 1,
  });
  await assert.rejects(
    client.exportProject(largeRequest(join(root, "output.mp4"))),
    (error) => error.code === "GPU_INIT_FAILED",
  );
});

test("aborting while a large request is being written settles as an abort", async () => {
  const root = await scratch("alpha2-node-abort-start-");
  // Starts slowly and never reads: the request stays half written until the abort ends the host.
  const executable = await fakeHost(
    root,
    `setInterval(() => {}, 1_000);
`,
  );
  const controller = new AbortController();
  const client = new Alpha2NodeClient({
    hostPath: executable,
    concurrency: 1,
  });
  setTimeout(() => controller.abort(), 200);
  await assert.rejects(
    client.exportProject(
      largeRequest(join(root, "output.mp4"), { signal: controller.signal }),
    ),
    (error) => error instanceof Alpha2AbortError,
  );
});

function exportRequest(output, overrides = {}) {
  return {
    project: {
      schema_version: 1,
      id: "project",
      name: "Project",
      settings: {
        width: 16,
        height: 16,
        frame_rate: { num: 30, den: 1 },
        sample_rate: 48_000,
      },
      sources: [],
      timeline: { tracks: [] },
    },
    assets: {},
    startTicks: 0,
    endTicks: 1,
    frameRate: { num: 30, den: 1 },
    output,
    ...overrides,
  };
}

/**
 * Writes a fake `alpha2-host` into `directory`, makes `directory` this process's working directory,
 * and returns the `hostPath` that runs it.
 *
 * The client starts `<hostPath> serve`. With `hostPath` this Node, that runs the file `serve` in
 * the working directory, which loads `source` (an ES module) from `host.mjs` beside it. Unlike a
 * script with a shebang, this starts the same way on Windows, macOS, and Linux. node --test runs
 * each test file in a process of its own and this file's tests one at a time, so the directory
 * change reaches no other test.
 */
async function fakeHost(directory, source) {
  await mkdir(directory, { recursive: true });
  await writeFile(join(directory, "host.mjs"), source);
  // `serve` has no extension: it is CommonJS whatever package.json is above the directory.
  await writeFile(
    join(directory, "package.json"),
    JSON.stringify({ type: "commonjs" }),
  );
  await writeFile(
    join(directory, "serve"),
    'import(require("node:url").pathToFileURL(require("node:path").join(__dirname, "host.mjs")).href);\n',
  );
  process.chdir(directory);
  return process.execPath;
}

/** Asserts that process `pid` has exited. */
function assertExited(pid) {
  assert.ok(Number.isSafeInteger(pid) && pid > 0, `no pid: ${pid}`);
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
}

/**
 * A host that records its pid, answers its request with `progress` messages then success, and exits
 * when its stdin ends or it is told to.
 */
const RECORDING_HOST = (pidFile) => `import { writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
const keepAlive = setInterval(() => {}, 1_000);
process.on("SIGTERM", () => process.exit(0));
const lines = createInterface({ input: process.stdin });
lines.once("line", (line) => {
  const request = JSON.parse(line);
  const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ...message }) + "\\n");
  send({ type: "progress", phase: "frames", completed: 1, total: 2 });
  setTimeout(() => {
    send({ type: "progress", phase: "frames", completed: 2, total: 2 });
    send({ ok: true, result: { output: request.output, frames: 2 } });
  }, 50);
});
lines.once("close", () => { clearInterval(keepAlive); });
`;

/**
 * Runs `body` (the text of an async function given `{ client, exportRequest, output }`) in a Node
 * process of its own, where an uncaught exception or an unhandled rejection ends that process
 * instead of this test runner.
 * What `body` returns is reported along with whether the host the client spawned is still alive
 * afterwards. The process must then exit by itself: a host left running holds its pipes, and they
 * keep the event loop up.
 */
async function runEmbedded(root, body, { fdLimit } = {}) {
  const pidFile = join(root, "host.pid");
  const executable = await fakeHost(root, RECORDING_HOST(pidFile));
  const script = join(root, "embedded.mjs");
  await writeFile(
    script,
    `import { existsSync, readFileSync } from "node:fs";
import { Alpha2NodeClient } from ${JSON.stringify(new URL("../dist/index.js", import.meta.url).href)};
const exportRequest = ${exportRequest.toString()};
const client = new Alpha2NodeClient({ hostPath: ${JSON.stringify(executable)}, concurrency: 1 });
const output = ${JSON.stringify(join(root, "output.mp4"))};
const outcome = await (${body})({ client, exportRequest, output });
await new Promise((resolve) => setTimeout(resolve, 300));
let hostAlive = false;
if (existsSync(${JSON.stringify(pidFile)})) {
  const pid = Number(readFileSync(${JSON.stringify(pidFile)}, "utf8"));
  try { process.kill(pid, 0); hostAlive = true; process.kill(pid, "SIGKILL"); } catch {}
}
process.stdout.write(JSON.stringify({ ...outcome, hostStarted: existsSync(${JSON.stringify(pidFile)}), hostAlive }) + "\\n");
`,
  );
  const { spawn } = await import("node:child_process");
  const command =
    fdLimit === undefined
      ? [process.execPath, [script]]
      : [
          "/bin/sh",
          [
            "-c",
            `ulimit -n ${fdLimit} && exec "$0" "$1"`,
            process.execPath,
            script,
          ],
        ];
  const child = spawn(command[0], command[1], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => (stdout += chunk));
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const timer = setTimeout(() => child.kill("SIGKILL"), 15_000);
  const [code, signal] = await new Promise((resolve) =>
    child.on("close", (...args) => resolve(args)),
  );
  clearTimeout(timer);
  assert.equal(
    code,
    0,
    `the embedding process ended with ${signal ?? code}: ${stderr}${stdout}`,
  );
  return JSON.parse(stdout.trim().split("\n").at(-1));
}

/** What a rejection looked like, in a form `runEmbedded` can report. */
const DESCRIBE_REJECTION = `(error) => ({ name: error?.constructor?.name, code: error?.code, message: String(error?.message), cause: error?.cause?.message })`;

test(
  "a host that cannot be spawned for want of descriptors is a HOST_START_FAILED rejection",
  {
    skip:
      process.platform === "win32" &&
      "the descriptor limit is set with POSIX ulimit, and /dev/null is opened to use it up",
  },
  async () => {
    const root = await scratch("alpha2-node-emfile-");
    // Every descriptor the process may open is taken before the client spawns: the spawn fails with
    // EMFILE, before Node sets up the host's stdio.
    const outcome = await runEmbedded(
      root,
      `async ({ client, exportRequest, output }) => {
        const { openSync, closeSync } = await import("node:fs");
        const held = [];
        try { for (;;) held.push(openSync("/dev/null", "r")); } catch (error) { if (error.code !== "EMFILE") throw error; }
        let result;
        try { await client.getCapabilities(); result = { resolved: true }; }
        catch (error) { result = (${DESCRIBE_REJECTION})(error); }
        // An export with a signal fails the same way, and aborting it afterwards finds nothing left to end.
        const controller = new AbortController();
        const exported = await client.exportProject(exportRequest(output, { signal: controller.signal })).then(() => "resolved", (error) => error.code);
        controller.abort();
        for (const fd of held) closeSync(fd);
        // The slot is free and nothing is left listening: the next request runs.
        const capabilities = await client.getCapabilities().then(() => "resolved", (error) => error.code);
        return { ...result, held: held.length, exported, next: capabilities };
      }`,
      { fdLimit: 64 },
    );
    assert.ok(outcome.held > 0, JSON.stringify(outcome));
    assert.equal(outcome.name, "Alpha2HostError", JSON.stringify(outcome));
    assert.equal(outcome.code, "HOST_START_FAILED");
    assert.match(outcome.message, /EMFILE/);
    assert.equal(outcome.exported, "HOST_START_FAILED");
    assert.equal(outcome.next, "resolved");
  },
);

test(
  "a spawn that throws is a HOST_START_FAILED rejection",
  {
    skip:
      process.platform !== "linux" &&
      "an environment string over 128 KiB fails a spawn with E2BIG only on Linux (MAX_ARG_STRLEN)",
  },
  async () => {
    // Node throws from spawn() itself for errors other than EACCES, EAGAIN, EMFILE, ENFILE and
    // ENOENT; this is one, as a Windows batch file's EINVAL is.
    const root = await scratch("alpha2-node-spawn-throws-");
    const starts = join(root, "starts");
    const hostPath = await fakeHost(root, STARTED_HOST(starts));
    const saved = process.env.ALPHA2_NODE_TEST_HUGE;
    process.env.ALPHA2_NODE_TEST_HUGE = "x".repeat(256 * 1024);
    try {
      const error = await new Alpha2NodeClient({ hostPath })
        .getCapabilities()
        .then(
          () => assert.fail("the probe resolved"),
          (cause) => cause,
        );
      assert.equal(error.name, "Alpha2HostError");
      assert.equal(error.code, "HOST_START_FAILED");
      assert.match(error.message, /E2BIG/);
    } finally {
      if (saved === undefined) delete process.env.ALPHA2_NODE_TEST_HUGE;
      else process.env.ALPHA2_NODE_TEST_HUGE = saved;
    }
    assert.deepEqual(await startsIn(starts), []);
  },
);

test(
  "a batch file is refused as a host before it would start",
  {
    skip:
      process.platform !== "win32" &&
      "only Windows refuses to start a batch file without a shell",
  },
  async () => {
    const root = await scratch("alpha2-node-batch-");
    for (const name of ["alpha2-host.cmd", "alpha2-host.BAT"]) {
      const launcher = join(root, name);
      await writeFile(launcher, "@echo off\r\n");
      const error = await new Alpha2NodeClient({ hostPath: launcher })
        .exportProject(exportRequest(join(root, "never.mp4")))
        .then(
          () => assert.fail("the export resolved"),
          (cause) => cause,
        );
      assert.equal(error.code, "HOST_NOT_EXECUTABLE", name);
      assert.match(error.message, /not a batch file/);
      assert.equal(retried(error), false);
    }
  },
);

test("a request that cannot be written as JSON is refused before a host starts", async () => {
  const root = await scratch("alpha2-node-bigint-");
  const outcome = await runEmbedded(
    root,
    `async ({ client, exportRequest, output }) => {
        try {
          await client.exportProject(exportRequest(output, { proxies: { a: [{ path: "/x.mp4", madeFrom: { byte_length: 10n } }] } }));
          return { resolved: true };
        } catch (error) { return (${DESCRIBE_REJECTION})(error); }
      }`,
  );
  assert.equal(outcome.name, "Alpha2HostError", JSON.stringify(outcome));
  assert.equal(outcome.code, "INVALID_INPUT");
  assert.match(outcome.message, /JSON/);
  assert.match(outcome.cause, /BigInt/);
  assert.equal(outcome.hostStarted, false);
  assert.equal(outcome.hostAlive, false);
});

test("an async onProgress that rejects fails the export as PROGRESS_CALLBACK_FAILED and ends its host", async () => {
  const root = await scratch("alpha2-node-async-progress-");
  const outcome = await runEmbedded(
    root,
    `async ({ client, exportRequest, output }) => {
        try {
          await client.exportProject(exportRequest(output, { onProgress: async () => { throw new Error("progress store down"); } }));
          return { resolved: true };
        } catch (error) { return (${DESCRIBE_REJECTION})(error); }
      }`,
  );
  assert.equal(outcome.name, "Alpha2HostError", JSON.stringify(outcome));
  assert.equal(outcome.code, "PROGRESS_CALLBACK_FAILED");
  assert.equal(outcome.cause, "progress store down");
  assert.equal(outcome.hostStarted, true);
  assert.equal(outcome.hostAlive, false);
});

test("an async onProgress still running when the host answers is waited for, and its rejection fails the export", async () => {
  const root = await scratch("alpha2-node-late-progress-");
  const outcome = await runEmbedded(
    root,
    `async ({ client, exportRequest, output }) => {
        const seen = [];
        try {
          await client.exportProject(exportRequest(output, {
            onProgress: async ({ completed }) => {
              seen.push(completed);
              if (completed === 2) {
                await new Promise((resolve) => setTimeout(resolve, 200));
                throw new Error("last write lost");
              }
            },
          }));
          return { resolved: true, seen };
        } catch (error) { return { ...(${DESCRIBE_REJECTION})(error), seen }; }
      }`,
  );
  assert.equal(
    outcome.code,
    "PROGRESS_CALLBACK_FAILED",
    JSON.stringify(outcome),
  );
  assert.equal(outcome.cause, "last write lost");
  assert.deepEqual(outcome.seen, [1, 2]);
});

test("an async onResult that rejects rejects the export with its error", async () => {
  const root = await scratch("alpha2-node-async-result-");
  const outcome = await runEmbedded(
    root,
    `async ({ client, exportRequest, output }) => {
        const progress = [];
        try {
          await client.exportProject(exportRequest(output, {
            onProgress: async ({ completed }) => { progress.push(completed); },
            onResult: async (result) => { await null; throw new Error("metrics store down: " + result.frames); },
          }));
          return { resolved: true };
        } catch (error) { return { ...(${DESCRIBE_REJECTION})(error), progress }; }
      }`,
  );
  assert.equal(outcome.name, "Error", JSON.stringify(outcome));
  assert.equal(outcome.message, "metrics store down: 2");
  assert.deepEqual(outcome.progress, [1, 2]);
  assert.equal(outcome.hostAlive, false);
});

/** Whether a reference retry policy would run `error`'s export again. */
function retried(error) {
  const failure = failureOf(error);
  assert.ok(failure, `${error.code} carries no failure`);
  return decideRetry([
    {
      failure,
      profile: RETRY_PROFILES[0],
      renderer: "gpu",
      videoEncoder: "default",
    },
  ]).retry;
}

/** A host that answers its request, or starts, with `line` and exits. */
const ANSWERING_HOST = (
  line,
) => `import { createInterface } from "node:readline";
const answer = ${JSON.stringify(line)};
if (answer.type === "fatal") {
  process.stdout.write(JSON.stringify({ protocolVersion: 1, ...answer }) + "\\n", () => process.exit(70));
} else {
  createInterface({ input: process.stdin }).once("line", (request) => {
    const { requestId } = JSON.parse(request);
    process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId, ...answer }) + "\\n", () => process.exit(0));
  });
}
`;

test("a host that cannot make its GPU device is an Alpha2GpuInitializationError, whichever way it says so", async () => {
  const root = await scratch("alpha2-node-gpu-init-");
  const cases = [
    {
      name: "fatal",
      host: `process.stderr.write("alpha2-host: no adapter\\n", () => process.stdout.write(JSON.stringify({ protocolVersion: 1, type: "fatal", error: { code: "GPU_INIT_FAILED", message: "could not acquire a GPU adapter" } }) + "\\n", () => process.exit(70)));
`,
      message: /could not acquire a GPU adapter/,
    },
    {
      name: "exit 70",
      host: `process.stderr.write("alpha2-host: no adapter\\n", () => process.exit(70));
`,
      message: /no adapter/,
      exitCode: 70,
    },
  ];
  for (const { name, host, message, exitCode } of cases) {
    const client = new Alpha2NodeClient({
      hostPath: await fakeHost(join(root, name.replace(" ", "-")), host),
    });
    for (const request of [
      () => client.getCapabilities(),
      () => client.exportProject(exportRequest(join(root, "out.mp4"))),
    ]) {
      const error = await request().then(
        () => assert.fail("the request resolved"),
        (cause) => cause,
      );
      assert.ok(error instanceof Alpha2GpuInitializationError, name);
      assert.equal(error.code, "GPU_INIT_FAILED", name);
      assert.match(error.message, message, name);
      assert.match(error.stderr, /alpha2-host: no adapter/, name);
      if (exitCode !== undefined) assert.equal(error.exitCode, exitCode, name);
      assert.equal(failureOf(error).hostCode, "GPU_INIT_FAILED", name);
    }
  }
  const decode = new Alpha2NodeClient({
    hostPath: await fakeHost(
      join(root, "decode"),
      ANSWERING_HOST({
        type: "fatal",
        error: {
          code: "HARDWARE_DECODE_UNAVAILABLE",
          message: "a Vulkan adapter without a video-decode queue",
        },
      }),
    ),
  });
  const error = await decode.getCapabilities().then(
    () => assert.fail("the probe resolved"),
    (cause) => cause,
  );
  assert.equal(error.code, "HARDWARE_DECODE_UNAVAILABLE");
  assert.ok(!(error instanceof Alpha2GpuInitializationError));
  assert.equal(failureOf(error).code, "INVALID_INPUT");
});

test("every refusal the client makes itself carries a classified failure", async () => {
  const client = new Alpha2NodeClient({
    hostPath: process.execPath,
    concurrency: 1,
  });
  const refusals = [
    exportRequest("/tmp/never-written.mp4", { startTicks: 0, endTicks: 0 }),
    exportRequest("/tmp/never-written.mp4", {
      videoQuality: { mode: "bogus" },
    }),
    exportRequest("/tmp/never-written.mp4", { loudness: { target: "loud" } }),
    exportRequest("/tmp/never-written.mp4", { project: { id: "" } }),
  ];
  for (const request of refusals) {
    const error = await client.exportProject(request).then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
    assert.equal(error.code, "INVALID_INPUT", error.message);
    const failure = failureOf(error);
    assert.equal(failure?.code, "INVALID_INPUT", error.message);
    assert.equal(failure.origin, "node");
    assert.equal(failure.operation, "export");
    assert.equal(failure.phase, "prepare");
    assert.equal(failure.hostCode, "INVALID_INPUT");
    assert.equal(failure.message, error.message);
    assert.equal(retried(error), false, error.message);
  }
});

test("a host error without a failure, and a host that cannot run, are classified by their code", async () => {
  const root = await scratch("alpha2-node-failure-");
  const cases = [
    [
      { ok: false, error: { code: "INVALID_REQUEST", message: "bad line" } },
      "INVALID_REQUEST",
      "INVALID_INPUT",
      false,
    ],
    [
      { ok: false, error: { code: "UNSUPPORTED_PROTOCOL", message: "v9" } },
      "UNSUPPORTED_PROTOCOL",
      "INVALID_INPUT",
      false,
    ],
    [
      {
        type: "fatal",
        error: {
          code: "HARDWARE_DECODE_UNAVAILABLE",
          message: "no decode device",
        },
      },
      "HARDWARE_DECODE_UNAVAILABLE",
      "INVALID_INPUT",
      false,
    ],
    [
      { type: "fatal", error: { code: "GPU_INIT_FAILED", message: "none" } },
      "GPU_INIT_FAILED",
      "UNKNOWN",
      true,
    ],
  ];
  for (const [index, [answer, code, failureCode, retry]] of cases.entries()) {
    const executable = await fakeHost(
      join(root, `host-${index}`),
      ANSWERING_HOST(answer),
    );
    const client = new Alpha2NodeClient({ hostPath: executable });
    const error = await client
      .exportProject(exportRequest(join(root, `out-${index}.mp4`)))
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    assert.equal(error.code, code);
    const failure = failureOf(error);
    assert.equal(failure?.code, failureCode, code);
    assert.equal(failure.hostCode, code);
    assert.equal(failure.phase, "prepare");
    assert.equal(retried(error), retry, code);
  }
});

test(
  "a file that cannot be executed is refused before it would start",
  {
    skip:
      process.platform === "win32" &&
      "Windows has no execute permission: the client checks that the file exists",
  },
  async () => {
    const root = await scratch("alpha2-node-not-executable-");
    const plain = join(root, "not-executable");
    await writeFile(plain, "#!/bin/sh\n");
    await chmod(plain, 0o644);
    const error = await new Alpha2NodeClient({ hostPath: plain })
      .exportProject(exportRequest(join(root, "never.mp4")))
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    assert.equal(error.code, "HOST_NOT_EXECUTABLE");
    assert.equal(failureOf(error)?.code, "INVALID_INPUT");
    assert.equal(retried(error), false);
  },
);

test("every code the client makes is classified in the shared table", async () => {
  const table = JSON.parse(
    await readFile(
      new URL("../../../tests/cases/failure-classes.json", import.meta.url),
      "utf8",
    ),
  );
  const source = await readFile(
    new URL("../src/index.ts", import.meta.url),
    "utf8",
  );
  const made = new Set(
    [
      ...source.matchAll(/new Alpha2HostError\(\s*"([A-Z_]+)"/g),
      ...source.matchAll(/super\(\s*"([A-Z_]+)"/g),
    ].map((match) => match[1]),
  );
  assert.ok(made.size >= 8, [...made].join());
  for (const code of made)
    assert.ok(
      code in table.node || code in table.protocol,
      `${code} is not in tests/cases/failure-classes.json`,
    );
});

test("parseHostMessage accepts the ready line", () => {
  const ready = { protocolVersion: 1, type: "ready" };
  assert.deepEqual(parseHostMessage(JSON.stringify(ready)), ready);
});

test(
  "a host that never writes is HOST_START_TIMEOUT and is killed",
  { timeout: 15_000 },
  async () => {
    const root = await scratch("alpha2-node-start-timeout-");
    const pidFile = join(root, "host.pid");
    const hostPath = await fakeHost(
      root,
      `import { writeFileSync } from "node:fs";
writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
setTimeout(() => {}, 60_000);
`,
    );
    const started = Date.now();
    const error = await new Alpha2NodeClient({
      hostPath,
      hostStartTimeoutMs: 300,
    })
      .exportProject(exportRequest(join(root, "out.mp4")))
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    assert.ok(Date.now() - started < 3_000, `${Date.now() - started} ms`);
    assert.equal(error.code, "HOST_START_TIMEOUT", error.message);
    assert.match(error.message, /did not start within 300 ms/);
    assert.equal(failureOf(error)?.code, "UNKNOWN");
    assertExited(Number(await readFile(pidFile, "utf8")));
  },
);

test("a slow host that writes ready in time is not timed out", async () => {
  const root = await scratch("alpha2-node-start-ready-");
  const hostPath = await fakeHost(
    root,
    `import { createInterface } from "node:readline";
const keepAlive = setInterval(() => {}, 1_000);
setTimeout(() => process.stdout.write(JSON.stringify({ protocolVersion: 1, type: "ready" }) + "\\n"), 200);
createInterface({ input: process.stdin }).once("line", (line) => {
  const request = JSON.parse(line);
  setTimeout(() => {
    clearInterval(keepAlive);
    process.stdout.write(JSON.stringify({ protocolVersion: 1, requestId: request.requestId, ok: true, result: { output: request.output } }) + "\\n");
  }, 1_500);
});
`,
  );
  assert.equal(
    await new Alpha2NodeClient({
      hostPath,
      hostStartTimeoutMs: 500,
    }).exportProject(exportRequest("slow.mp4")),
    "slow.mp4",
  );
});

test("a host that answers without a ready line still works", async () => {
  const root = await scratch("alpha2-node-no-ready-");
  const hostPath = await fakeHost(root, STARTED_HOST(join(root, "starts")));
  assert.equal(
    await new Alpha2NodeClient({
      hostPath,
      hostStartTimeoutMs: 5_000,
    }).exportProject(exportRequest("plain.mp4")),
    "plain.mp4",
  );
});

test("a start timeout that is not above zero is refused, and Infinity waits forever", async () => {
  const root = await scratch("alpha2-node-start-option-");
  const hostPath = await fakeHost(root, STARTED_HOST(join(root, "starts")));
  for (const hostStartTimeoutMs of [0, -1, Number.NaN, "5000", null]) {
    assert.throws(
      () => new Alpha2NodeClient({ hostPath, hostStartTimeoutMs }),
      (error) => {
        assert.equal(error.code, "INVALID_INPUT", String(hostStartTimeoutMs));
        assert.match(error.message, /hostStartTimeoutMs/);
        return true;
      },
    );
  }
  assert.equal(
    await new Alpha2NodeClient({
      hostPath,
      hostStartTimeoutMs: Number.POSITIVE_INFINITY,
    }).exportProject(exportRequest("forever.mp4")),
    "forever.mp4",
  );
});

/**
 * A host that logs every line it reads to `log`, answers a cancel line for its request as `serve`
 * does, then the request `CANCELLED`, and exits when its input ends or on SIGTERM;
 * `answerCancel: false` makes it ignore the cancel.
 */
const CANCELLABLE_HOST = (
  log,
  { answerCancel = true } = {},
) => `import { appendFileSync } from "node:fs";
import { createInterface } from "node:readline";
const record = (line) => appendFileSync(${JSON.stringify(log)}, line + "\\n");
record("start " + process.pid);
const keepAlive = setInterval(() => {}, 1_000);
const send = (message) => process.stdout.write(JSON.stringify({ protocolVersion: 1, ...message }) + "\\n");
send({ type: "ready" });
let requestId;
const lines = createInterface({ input: process.stdin });
lines.on("line", (line) => {
  const message = JSON.parse(line);
  record(message.operation + " " + (message.target ?? ""));
  if (message.operation === "export") {
    requestId = message.requestId;
    send({ type: "progress", requestId, phase: "frames", completed: 0, total: 100 });
  } else if (message.operation === "cancel" && ${answerCancel}) {
    send({ requestId: message.requestId, ok: true, result: { target: message.target, state: message.target === requestId ? "running" : "pending" } });
    setTimeout(() => {
      record("partial removed");
      send({ requestId, ok: false, error: { code: "CANCELLED", message: "native export was cancelled: the client cancelled request " + requestId } });
    }, 100);
  }
});
lines.on("close", () => clearInterval(keepAlive));
`;

test(
  "an abort writes a cancel line and settles after the host answers and exits",
  { timeout: 10_000 },
  async () => {
    const root = await scratch("alpha2-node-cancel-line-");
    const log = join(root, "log");
    const hostPath = await fakeHost(root, CANCELLABLE_HOST(log));
    const controller = new AbortController();
    let aborted;
    const error = await new Alpha2NodeClient({ hostPath })
      .exportProject(
        exportRequest("out.mp4", {
          signal: controller.signal,
          onProgress: () => {
            aborted = Date.now();
            controller.abort();
          },
        }),
      )
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    assert.ok(error instanceof Alpha2AbortError, String(error));
    assert.ok(Date.now() - aborted < 3_000, `${Date.now() - aborted} ms`);
    const lines = await startsIn(log);
    assert.equal(lines[1], "export ");
    assert.match(lines[2], /^cancel node-\d+-\d+-\d+$/);
    // The host answered the request (having removed its partial file) before it was ended.
    assert.equal(lines[3], "partial removed");
    assertExited(Number(lines[0].split(" ")[1]));
  },
);

test(
  "a host that does not answer a cancel within the timeout is ended",
  { timeout: 30_000 },
  async () => {
    const root = await scratch("alpha2-node-cancel-timeout-");
    const log = join(root, "log");
    const hostPath = await fakeHost(
      root,
      CANCELLABLE_HOST(log, { answerCancel: false }),
    );
    const controller = new AbortController();
    let aborted;
    const error = await new Alpha2NodeClient({ hostPath })
      .exportProject(
        exportRequest("out.mp4", {
          signal: controller.signal,
          onProgress: () => {
            aborted = Date.now();
            controller.abort();
          },
        }),
      )
      .then(
        () => assert.fail("the export resolved"),
        (cause) => cause,
      );
    const waited = Date.now() - aborted;
    assert.ok(error instanceof Alpha2AbortError, String(error));
    assert.ok(waited >= 4_900 && waited < 15_000, `${waited} ms`);
    const lines = await startsIn(log);
    assert.match(lines[2], /^cancel node-/);
    assertExited(Number(lines[0].split(" ")[1]));
  },
);

test("a host Windows blocks by policy is HOST_BLOCKED, which no retry mends", async () => {
  for (const status of [
    0xc0000428, 0xc0000361, 0xc0000362, 0xc0000363, 0xc0000364, 0xc0e90002,
    0xc0e90007, 0xc0e9000a, 0xc0e9000b,
  ]) {
    assert.equal(windowsPolicyBlocked(status), true, status.toString(16));
    // Node reports a Windows status unsigned, and a negative exit code is the same status.
    assert.equal(windowsPolicyBlocked(status - 2 ** 32), true);
  }
  for (const status of [0xc0000135, 0xc0000005, 0, 1, 127, null])
    assert.equal(windowsPolicyBlocked(status), false, String(status));
  if (process.platform !== "win32") return;
  const root = await scratch("alpha2-node-blocked-");
  // STATUS_INVALID_IMAGE_HASH, as Code Integrity ends a process whose DLL it refused.
  const hostPath = await fakeHost(root, "process.exit(-1073740760);\n");
  const error = await new Alpha2NodeClient({ hostPath })
    .exportProject(exportRequest(join(root, "out.mp4")))
    .then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
  assert.equal(error.code, "HOST_BLOCKED", error.message);
  assert.match(error.message, /status 0xC0000428/);
  assert.match(error.message, /Smart App Control/);
  assert.equal(failureOf(error)?.code, "INVALID_INPUT");
  assert.equal(retried(error), false);
});

test(
  "a musl-style spawn ENOENT for an existing file is HOST_LOAD_FAILED",
  {
    skip:
      process.platform !== "linux" &&
      "Linux only: the loader's missing interpreter is how a glibc build fails on musl",
  },
  async () => {
    const root = await scratch("alpha2-node-musl-");
    const hostPath = join(root, "alpha2-host");
    // execve fails ENOENT for a missing interpreter exactly as for a missing ELF loader.
    await writeFile(hostPath, "#!/nonexistent/interp\n");
    await chmod(hostPath, 0o755);
    const error = await new Alpha2NodeClient({ hostPath })
      .getCapabilities()
      .then(
        () => assert.fail("the call resolved"),
        (cause) => cause,
      );
    assert.equal(error.code, "HOST_LOAD_FAILED", error.message);
    assert.match(
      error.message,
      /program interpreter \/nonexistent\/interp is missing/,
    );
    assert.match(error.message, /musl/);
    assert.equal(failureOf(error)?.code, "INVALID_INPUT");
    assert.equal(retried(error), false);
  },
);

test("the glibc refusal names the floor", async () => {
  const root = await scratch("alpha2-node-glibc-");
  const words =
    "/opt/alpha2/bin/alpha2-host: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.38' not found (required by /opt/alpha2/bin/alpha2-host)";
  const hostPath = await fakeHost(
    root,
    `process.stderr.write(${JSON.stringify(words + "\n")}, () => process.exit(1));\n`,
  );
  const error = await new Alpha2NodeClient({ hostPath }).getCapabilities().then(
    () => assert.fail("the call resolved"),
    (cause) => cause,
  );
  assert.equal(error.code, "HOST_LOAD_FAILED", error.message);
  assert.match(
    error.message,
    /GLIBC_2\.38.*; this alpha2-host needs glibc 2\.\d+ or later \(this system has /s,
  );
});
