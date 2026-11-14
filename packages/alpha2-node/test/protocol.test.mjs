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
import test, { after, afterEach } from "node:test";

import {
  Alpha2AbortError,
  Alpha2GpuInitializationError,
  Alpha2NodeClient as KeptHostsClient,
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

const scratchDirectories = [];

/** The module fake hosts import for their protocol I/O. */
const IO = JSON.stringify(new URL("./fake-host-io.mjs", import.meta.url).href);

const clients = new Set();

/** Ends the hosts each test's clients keep, before the test's directory is removed. */
class Alpha2NodeClient extends KeptHostsClient {
  constructor(options) {
    super(options);
    clients.add(this);
  }
}

afterEach(async () => {
  const open = [...clients];
  clients.clear();
  await Promise.all(open.map((client) => client.close()));
});

async function scratch(prefix) {
  const directory = await mkdtemp(join(tmpdir(), prefix));
  scratchDirectories.push(directory);
  return directory;
}

after(async () => {
  // Windows cannot remove a working directory.
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
    const { protocolVersion, operation, requestId, project, ...fields } =
      fixture.request;
    const error = await client
      .exportProject(exportRequest("/tmp/never-written.mp4", fields))
      .then(
        () => assert.fail(`${fixture.name}: the export resolved`),
        (cause) => cause,
      );
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
  process.chdir(await scratch("alpha2-node-readiness-"));
  const client = new Alpha2NodeClient({
    hostPath: process.execPath,
    concurrency: 1,
  });
  const base = exportRequest("/tmp/never-written.mp4");
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
      protocolVersion: 2,
      requestId: "r",
      ok: false,
      error: { code: "CONFORM_FAILED", message: "not ready", details: report },
      hostState: "ready",
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
  // realpath: the module loader resolves symlinks (macOS /var is /private/var).
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
import { progress, serve } from ${IO};
const exitMarker = ${JSON.stringify(exitMarker)};
writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
// POSIX only: the host takes a while to exit after SIGTERM, and the client must wait for it. Windows has no SIGTERM; the client's kill ends the process at once.
process.on("SIGTERM", () => {
  setTimeout(() => {
    writeFileSync(exitMarker, "exited");
    process.exit(0);
  }, 100);
});
setInterval(() => {}, 1_000);
// Ignores the cancel and the end of its input.
serve((request) => progress(request, { completed: 0, total: 1 }), { onEnd: () => {} });
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
 * Logs `start` and `request` lines and answers after `answerMs`. With `ignoreTerm` it survives
 * SIGTERM and the end of its input.
 */
const QUEUE_HOST = (
  log,
  { answerMs = 0, ignoreTerm = false } = {},
) => `import { appendFileSync } from "node:fs";
import { basename } from "node:path";
import { progress, respond, serve } from ${IO};
const record = (line) => appendFileSync(${JSON.stringify(log)}, line + "\\n");
record("start " + process.pid);
process.on("SIGTERM", () => { if (!${ignoreTerm}) process.exit(0); });
if (${ignoreTerm}) setInterval(() => {}, 1_000);
serve((request) => {
  record("request " + basename(request.output));
  progress(request, { phase: "frames", completed: 0, total: 1 });
  setTimeout(() => respond(request, { ok: true, result: { output: request.output } }), ${answerMs});
}, ${ignoreTerm} ? { onEnd: () => {} } : {});
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
    // a answers 400 ms after its request, so b settles first unless it waited.
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
    // Cancel timeout plus shutdown timeout; the bound only catches an abort that never settles.
    assert.ok(
      waited >= 4_900 && waited < 10_000 + 10_000,
      `settled ${waited} ms after the abort`,
    );
    const pid = Number((await startsIn(log))[0].split(" ")[1]);
    assertExited(pid);
  },
);

test("an abort seen only once the host is ready is refused before the request is written, and the host is kept", async () => {
  const root = await scratch("alpha2-node-abort-at-start-");
  const log = join(root, "log");
  const hostPath = await fakeHost(root, QUEUE_HOST(log));
  let reads = 0;
  const signal = {
    get aborted() {
      reads += 1;
      return reads >= 3;
    },
    addEventListener() {},
    removeEventListener() {},
  };
  const client = new Alpha2NodeClient({ hostPath });
  await assert.rejects(
    client.exportProject(exportRequest("out.mp4", { signal })),
    (error) => error instanceof Alpha2AbortError,
  );
  assert.equal(reads, 3);
  assert.deepEqual(
    (await startsIn(log)).filter((line) => line.startsWith("request")),
    [],
  );
  assert.equal(
    await client.exportProject(exportRequest("next.mp4")),
    "next.mp4",
  );
  const lines = await startsIn(log);
  assert.equal(lines.length, 2, lines.join());
  assert.match(lines[0], /^start /);
  assert.equal(lines[1], "request next.mp4");
});

test("module calls with the same host path and concurrency share one pool, whatever decode mode each asks for", async () => {
  const root = await scratch("alpha2-node-concurrency-");
  const lock = join(root, "host.lock");
  const overlap = join(root, "overlap");
  const modes = join(root, "modes");
  const executable = await fakeHost(
    root,
    `import { appendFileSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { respond, serve } from ${IO};
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
const exit = () => {
  if (ownsLock) rmSync(lock, { force: true });
  process.exit(0);
};
process.on("SIGTERM", () => setTimeout(exit, 50));
// Each host drains after one request, so every call starts a host of its own.
serve((request) => {
  const result = request.operation === "capabilities" ? { status: "ready", capabilities: {}, effectCatalog: {} } : { output: request.output };
  setTimeout(() => respond(request, { ok: true, result, hostState: "draining" }), 100);
}, { onEnd: exit });
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
  const root = await scratch("alpha2-node-resources-");
  const recorded = join(root, "request.json");
  const executable = await fakeHost(
    root,
    `import { writeFileSync } from "node:fs";
import { respond, serve } from ${IO};
// Module calls share one client across tests, so this host takes no later test's request.
serve((request) => {
  writeFileSync(${JSON.stringify(recorded)}, JSON.stringify(request));
  respond(request, { ok: true, result: { output: request.output }, hostState: "draining" });
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
  // The fake host answers only after stderr is flushed, since macOS pipe writes are async.
  const root = await scratch("alpha2-node-failure-");
  const executable = await fakeHost(
    root,
    `import { progress, respond, serve } from ${IO};
serve((request) => {
  process.stderr.write("x".repeat(40_000) + "\\nalpha2-host uncaptured GPU out-of-memory error: staging\\n", () => {
    progress(request, { phase: "frames", completed: 3, total: 9 });
    respond(request, { ok: false, error: {
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
    `import { respond, serve } from ${IO};
serve((request) => {
  process.stderr.write(${JSON.stringify(line)}.repeat(2000) + "the last line\\n", () => {
    respond(request, { ok: false, error: { code: "ALPHA2_ERROR", message: "failed" } });
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

test("stderr the host wrote before answering is kept when it is read after the answer", async () => {
  // On macOS stdout is often read before the last stderr chunk; this host writes it last.
  const root = await scratch("alpha2-node-stderr-late-");
  const hostPath = await fakeHost(
    root,
    `import { respond, serve } from ${IO};
serve((request) => {
  process.stderr.write("x".repeat(40_000) + "\\n");
  respond(request, { ok: false, error: { code: "ALPHA2_ERROR", message: "GPU texture readback failed: map-failed" } }, () => {
    process.stderr.write("alpha2-host uncaptured GPU out-of-memory error: staging\\n");
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
});

test("output before the magic is kept as diagnostics, and a host that never writes the magic is INVALID_RESPONSE", async () => {
  // A library may print to stdout before the host takes it over.
  const root = await scratch("alpha2-node-preamble-");
  const banner =
    "Validation Error: [ VUID-StandaloneSpirv-None-10684 ] vkCreateShaderModule";
  const ready = await new Alpha2NodeClient({
    hostPath: await fakeHost(
      join(root, "ready"),
      `import { respond, serve } from ${IO};
process.stdout.write(${JSON.stringify(banner)} + "\\n\\x89A2HOS");
serve((request) => respond(request, { ok: true, result: { status: "ready" } }));
`,
    ),
  }).getCapabilities();
  assert.equal(ready.status, "ready");
  const exited = await new Alpha2NodeClient({
    hostPath: await fakeHost(
      join(root, "exited"),
      `process.stdout.write(${JSON.stringify(banner)} + "\\n", () => process.exit(1));
`,
    ),
  })
    .exportProject(exportRequest(join(root, "out.mp4")))
    .then(
      () => assert.fail("the export resolved"),
      (cause) => cause,
    );
  assert.equal(exited.code, "HOST_EXITED");
  assert.ok(exited.stderr.includes(banner), exited.stderr);
  const flooded = new Alpha2NodeClient({
    hostPath: await fakeHost(
      join(root, "flooded"),
      `process.stdout.write("x".repeat(1024 * 1024 + 16));
setInterval(() => {}, 1_000);
`,
    ),
  });
  await assert.rejects(flooded.getCapabilities(), (error) => {
    assert.equal(error.code, "INVALID_RESPONSE");
    assert.match(error.message, /without the protocol's magic/);
    return true;
  });
});

test("a frame the host writes in pieces is read whole", async () => {
  const root = await scratch("alpha2-node-split-frames-");
  const hostPath = await fakeHost(
    root,
    `import { MAGIC, frame, requests } from ${IO};
const ready = frame({ protocolVersion: 2, type: "state", state: "ready", reason: "started", details: {} });
const bytes = Buffer.concat([MAGIC, ready]);
const trickle = (data, done) => {
  if (data.length === 0) return done();
  process.stdout.write(data.subarray(0, 3), () => setTimeout(() => trickle(data.subarray(3), done), 1));
};
trickle(bytes, () => {
  requests((request) => {
    const answer = frame({ protocolVersion: 2, requestId: request.requestId, ok: true, result: { status: "ready", note: "\u00e9".repeat(5000) }, hostState: "ready" });
    // One write ends mid-header and the next carries the rest.
    process.stdout.write(answer.subarray(0, 11), () => setTimeout(() => process.stdout.write(answer.subarray(11)), 20));
  });
});
`,
  );
  const report = await new Alpha2NodeClient({ hostPath }).getCapabilities();
  assert.equal(report.note, "\u00e9".repeat(5000));
});

test("a header whose attachment list does not match the frame is INVALID_RESPONSE", async () => {
  const root = await scratch("alpha2-node-bad-attachments-");
  const cases = [
    [
      `[{ type: "image/png", bytes: 3 }]`,
      4,
      /lists 3 attachment bytes and the frame has 4/,
    ],
    [`[{ type: "", bytes: 4 }]`, 4, /invalid attachment list/],
    [`undefined`, 4, /lists 0 attachment bytes and the frame has 4/],
  ];
  for (const [index, [listed, bytes, message]] of cases.entries()) {
    const hostPath = await fakeHost(
      join(root, String(index)),
      `import { frame, sendBytes, serve } from ${IO};
serve((request) => {
  const header = { protocolVersion: 2, requestId: request.requestId, ok: true, result: {}, hostState: "ready", attachments: ${listed} };
  sendBytes(frame(header, [{ data: Buffer.alloc(${bytes}) }]));
});
`,
    );
    await assert.rejects(
      new Alpha2NodeClient({ hostPath }).getCapabilities(),
      (error) => {
        assert.equal(error.code, "INVALID_RESPONSE", listed);
        assert.match(error.message, message);
        return true;
      },
    );
  }
});

test("a still without an output comes back as the PNG bytes of its attachment", async () => {
  const root = await scratch("alpha2-node-still-attachment-");
  const recorded = join(root, "request.json");
  const hostPath = await fakeHost(
    root,
    `import { writeFileSync } from "node:fs";
import { respond, serve } from ${IO};
serve((request) => {
  writeFileSync(${JSON.stringify(recorded)}, JSON.stringify(request));
  const png = (time) => Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47]), Buffer.from(String(time))]);
  const attachments = [];
  const stills = request.stills.map(({ time, output }) => {
    if (output !== undefined) return { output, frameTime: time, width: 16, height: 16, publish: { method: "rename" } };
    attachments.push({ type: "image/png", data: png(time) });
    return { output: null, frameTime: time, width: 16, height: 16, attachment: attachments.length - 1 };
  });
  respond(request, { ok: true, result: { stills, renderer: "cpu" } }, undefined, attachments);
});
`,
  );
  const client = new Alpha2NodeClient({ hostPath });
  const { project } = exportRequest("");
  const written = join(root, "written.png");
  const result = await client.stills({
    project,
    assets: {},
    stills: [{ time: 4000 }, { time: 0, output: written }, { time: 8000 }],
  });
  assert.deepEqual(JSON.parse(await readFile(recorded, "utf8")).stills, [
    { time: 4000 },
    { time: 0, output: written },
    { time: 8000 },
  ]);
  const [first, file, last] = result.stills;
  assert.equal(first.output, null);
  assert.ok(first.png instanceof Uint8Array);
  assert.equal(Buffer.from(first.png).toString("latin1"), "\x89PNG4000");
  assert.equal("attachment" in first, false);
  assert.equal(file.output, written);
  assert.equal(file.png, undefined);
  assert.equal(Buffer.from(last.png).toString("latin1"), "\x89PNG8000");
  const one = await client.still({ project, assets: {}, time: 0 });
  assert.equal(Buffer.from(one.stills[0].png).toString("latin1"), "\x89PNG0");
});

test("a host that dies mid-export is HOST_EXITED, with no cause claimed", async () => {
  const root = await scratch("alpha2-node-exited-");
  const executable = await fakeHost(
    root,
    `import { progress, serve } from ${IO};
serve((request) => {
  progress(request, { phase: "finalizing", completed: 9, total: 9 }, () => {
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

test(
  "a host the kernel's out-of-memory killer ended is HOST_OOM_KILLED, class OUT_OF_MEMORY",
  { skip: process.platform === "win32" && "Windows has no SIGKILL" },
  async () => {
    const root = await scratch("alpha2-node-oom-");
    const events = join(root, "memory.events");
    await writeFile(
      events,
      "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\noom_group_kill 0\n",
    );
    const executable = await fakeHost(
      root,
      `import { readFileSync, writeFileSync } from "node:fs";
import { progress, serve } from ${IO};
serve((request) => {
  progress(request, { phase: "frames", completed: 3, total: 9 }, () => {
    if (process.env.RAISE_OOM_KILL === "1") {
      const path = process.env.ALPHA2_TEST_MEMORY_EVENTS;
      writeFileSync(path, readFileSync(path, "utf8").replace("oom_kill 1", "oom_kill 2"));
    }
    process.kill(process.pid, "SIGKILL");
  });
});
`,
    );
    const client = new Alpha2NodeClient({ hostPath: executable });
    process.env.ALPHA2_TEST_MEMORY_EVENTS = events;
    try {
      process.env.RAISE_OOM_KILL = "1";
      await assert.rejects(
        client.exportProject(exportRequest(join(root, "output.mp4"))),
        (error) => {
          assert.equal(error.code, "HOST_OOM_KILLED");
          assert.equal(error.signal, "SIGKILL");
          assert.equal(error.failure.code, "OUT_OF_MEMORY");
          assert.equal(error.failure.resource, "cpu");
          assert.equal(error.failure.origin, "node");
          return true;
        },
      );
      process.env.RAISE_OOM_KILL = "0";
      await assert.rejects(
        client.exportProject(exportRequest(join(root, "again.mp4"))),
        { code: "HOST_EXITED" },
      );
    } finally {
      delete process.env.ALPHA2_TEST_MEMORY_EVENTS;
      delete process.env.RAISE_OOM_KILL;
    }
  },
);

test("a host the loader refuses to start is HOST_LOAD_FAILED, which no retry mends", async () => {
  // glibc's ld.so exits 127 with this when a linked library is missing or of another major.
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
    // Windows exits with STATUS_DLL_NOT_FOUND or STATUS_ENTRYPOINT_NOT_FOUND, reported unsigned.
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

/** Fails with `DECODE_STALLED` (reporting `resource`) unless decoding in software. */
async function stallingHost(root, { resource = "gpu" } = {}) {
  const runs = join(root, "runs");
  const executable = await fakeHost(
    root,
    `import { appendFileSync } from "node:fs";
import { respond, serve } from ${IO};
serve((request) => {
  const mode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  appendFileSync(${JSON.stringify(runs)}, mode + "\\n");
  if (mode === "software") respond(request, { ok: true, result: { frames: 9 } });
  else respond(request, { ok: false, error: {
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
    `import { respond, serve } from ${IO};
serve((request) => {
  const send = (message) => respond(request, message);
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
  // The host's default is per platform (`auto` on Linux).
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
  // The software retry does not move the encoder off the GPU, so the GPU failure stands.
  const root = await scratch("alpha2-node-encode-stall-");
  const runs = join(root, "runs");
  const executable = await fakeHost(
    root,
    `import { appendFileSync } from "node:fs";
import { respond, serve } from ${IO};
serve((request) => {
  appendFileSync(${JSON.stringify(runs)}, (process.env.ALPHA2_VIDEO_DECODE ?? "unset") + "\\n");
  respond(request, { ok: false, hostState: "draining", error: {
    code: "ENCODE_STALLED",
    message: "the encode worker has been inside one encoder call for 40001 ms, past the 40000 ms limit, holding a hardware encode session",
    failure: { code: "GPU_TIMEOUT", origin: "native-host", operation: "export", backend: "native", phase: "encode", resource: "gpu" },
  } });
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
    `import { respond, serve } from ${IO};
serve((request) => {
  respond(request, { ok: false, error: {
    code: "OUTPUT_PUBLISH_FAILED",
    message: "cannot publish the output: denied; the finished render is kept at " + ${JSON.stringify(kept)},
    details: { keptPath: ${JSON.stringify(kept)} },
    failure: { code: "OUTPUT_IO_FAILED", origin: "native-host", operation: "export", backend: "native", phase: "mux", resource: "unknown" },
  } });
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

const STARTED_HOST = (starts) => `import { appendFileSync } from "node:fs";
import { respond, serve } from ${IO};
appendFileSync(${JSON.stringify(starts)}, (process.env.ALPHA2_VIDEO_DECODE ?? "unset") + "\\n");
serve((request) => {
  const result = request.operation === "capabilities" ? { status: "ready", capabilities: {}, effectCatalog: {} } : { output: request.output };
  respond(request, { ok: true, result });
});
`;

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

/** Logs `<operation> <decode mode>`; fails the first capabilities probe if `failFirst`. */
const PROBED_HOST = (
  log,
  { exportMs = 0, failFirst = false } = {},
) => `import { appendFileSync, existsSync, writeFileSync } from "node:fs";
import { respond, serve } from ${IO};
serve((request) => {
  const mode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  appendFileSync(${JSON.stringify(log)}, request.operation + " " + mode + "\\n");
  const send = (message) => respond(request, message);
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
  assert.equal(exported, false, "the withdrawn call waited for the export");
  await exporting;
  // Slots are granted in request order.
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
 * Logs `<operation> <decode mode> <renderer>` from its environment and answers at once. It drains
 * after one request: module calls share one client across tests.
 */
const RENDERER_HOST = (log) => `import { appendFileSync } from "node:fs";
import { respond, serve } from ${IO};
serve((request) => {
  const decode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  const renderer = process.env.ALPHA2_RENDERER ?? "unset";
  appendFileSync(${JSON.stringify(log)}, request.operation + " " + decode + " " + renderer + "\\n");
  const result = request.operation === "capabilities"
    ? { status: "ready", renderer, rendererReason: null, capabilities: {}, effectCatalog: {} }
    : { output: request.output };
  respond(request, { ok: true, result, hostState: "draining" });
});
`;

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
      // Unset, the renderer comes from the environment when the probe starts.
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
import { respond, serve } from ${IO};
serve((request) => {
  const mode = process.env.ALPHA2_VIDEO_DECODE ?? "unset";
  appendFileSync(${JSON.stringify(runs)}, mode + " " + (process.env.ALPHA2_RENDERER ?? "unset") + "\\n");
  const send = (message) => respond(request, message);
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

test("an answer to a request the host could not read is the request's answer", async () => {
  // The host answers an unreadable request with requestId "unknown".
  const root = await scratch("alpha2-node-unread-");
  const hostPath = await fakeHost(
    root,
    ANSWERING_HOST({
      requestId: "unknown",
      ok: false,
      error: {
        code: "INVALID_REQUEST",
        message:
          "the request header is 40000000 bytes; the maximum is 134217728",
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

test("a request header longer than the host reads is refused before a host starts", async () => {
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
        protocolVersion: 2,
        type: "progress",
        requestId: "r",
        phase: "done",
        completed: 1,
        total: 1,
      }),
    ),
  );
});

/** A host that cannot make its GPU device: once ready it reports `fatal` and exits unread. */
const FAILING_HOST = `import { ready, send } from ${IO};
ready();
send({ type: "fatal", error: { code: "GPU_INIT_FAILED", message: "no adapter", fatal: true } }, () => process.exit(70));
`;

/** Larger than a pipe buffer, so it is still partly unwritten when the host exits. */
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
  // Never reads: the request stays half written until the abort ends the host.
  const executable = await fakeHost(
    root,
    `import { ready } from ${IO};
ready();
setInterval(() => {}, 1_000);
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
 * Writes a fake host and makes `directory` the working directory. The client runs
 * `<this node> serve`, which loads `host.mjs`; this works the same on every platform.
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

function assertExited(pid) {
  assert.ok(Number.isSafeInteger(pid) && pid > 0, `no pid: ${pid}`);
  assert.throws(() => process.kill(pid, 0), { code: "ESRCH" });
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitUntil(condition, what) {
  const until = Date.now() + 5_000;
  while (!condition()) {
    if (Date.now() > until) assert.fail(`waited 5 s for ${what}`);
    await new Promise((resolveWait) => setTimeout(resolveWait, 20));
  }
}

/** Records its pid, sends `progress` then success, and exits when stdin ends. */
const RECORDING_HOST = (pidFile) => `import { writeFileSync } from "node:fs";
import { progress, respond, serve } from ${IO};
writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
serve((request) => {
  progress(request, { phase: "frames", completed: 1, total: 2 });
  setTimeout(() => {
    progress(request, { phase: "frames", completed: 2, total: 2 });
    respond(request, { ok: true, result: { output: request.output, frames: 2 } });
  }, 50);
});
`;

/**
 * Runs `body` in a separate Node process so an uncaught error cannot end the test runner, and
 * reports whether the spawned host is still alive once the client is closed.
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
await client.close();
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
    // Exhaust descriptors so the spawn fails with EMFILE before stdio is set up.
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
    // spawn() throws synchronously for errors other than EACCES, EAGAIN, EMFILE, ENFILE and ENOENT.
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

test("an async onProgress that rejects fails the export as PROGRESS_CALLBACK_FAILED", async () => {
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

/** Answers every request with `answer`, or for a `fatal` answer sends it and exits 70. */
const ANSWERING_HOST = (answer) => `import { respond, send, serve } from ${IO};
const answer = ${JSON.stringify(answer)};
serve((request) => {
  if (answer.type === "fatal") send(answer, () => process.exit(70));
  else respond(request, answer);
});
`;

test("a host that cannot make its GPU device is an Alpha2GpuInitializationError, whichever way it says so", async () => {
  const root = await scratch("alpha2-node-gpu-init-");
  const cases = [
    {
      name: "fatal",
      host: `import { send, serve } from ${IO};
serve(() => process.stderr.write("alpha2-host: no adapter\\n", () => send({ type: "fatal", error: { code: "GPU_INIT_FAILED", message: "could not acquire a GPU adapter" } }, () => process.exit(70))));
`,
      message: /could not acquire a GPU adapter/,
    },
    {
      name: "exit 70",
      host: `import { serve } from ${IO};
serve(() => process.stderr.write("alpha2-host: no adapter\\n", () => process.exit(70)));
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

test("parseHostMessage reads state messages and a response's host state", () => {
  const draining = {
    protocolVersion: 2,
    type: "state",
    state: "draining",
    reason: "memory",
    details: { activity: "idle", requestsServed: 3 },
  };
  assert.deepEqual(parseHostMessage(JSON.stringify(draining)), draining);
  const answered = {
    protocolVersion: 2,
    requestId: "r",
    ok: true,
    result: { state: "ready", reason: "started", details: {} },
    hostState: "ready",
  };
  assert.deepEqual(parseHostMessage(JSON.stringify(answered)), answered);
  for (const message of [
    { protocolVersion: 2, type: "ready" },
    { ...draining, state: "stopped" },
    { ...draining, reason: undefined },
    { ...draining, details: [] },
    { ...answered, hostState: "busy" },
    { ...answered, hostState: undefined },
  ])
    assert.throws(
      () => parseHostMessage(JSON.stringify(message)),
      JSON.stringify(message),
    );
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
    `import { ready, requests, respond } from ${IO};
setTimeout(ready, 200);
requests((request) => {
  setTimeout(() => respond(request, { ok: true, result: { output: request.output } }), 1_500);
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

test("a host that answers before its ready state is INVALID_RESPONSE", async () => {
  const root = await scratch("alpha2-node-no-ready-");
  const hostPath = await fakeHost(
    root,
    `import { requests, respond } from ${IO};
respond({ requestId: "unknown" }, { ok: true, result: {} });
requests(() => {});
`,
  );
  await assert.rejects(
    new Alpha2NodeClient({
      hostPath,
      hostStartTimeoutMs: 5_000,
    }).exportProject(exportRequest("plain.mp4")),
    (error) => {
      assert.equal(error.code, "INVALID_RESPONSE", error.message);
      assert.match(error.message, /before its ready state/);
      return true;
    },
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

/** Logs every request, answers a cancel as `serve` does unless `answerCancel: false`. */
const CANCELLABLE_HOST = (
  log,
  { answerCancel = true } = {},
) => `import { appendFileSync } from "node:fs";
import { progress, respond, serve } from ${IO};
const record = (line) => appendFileSync(${JSON.stringify(log)}, line + "\\n");
record("start " + process.pid);
let running;
serve((request) => {
  record(request.operation + " ");
  running = request;
  progress(request, { phase: "frames", completed: 0, total: 100 });
}, {
  onCancel: (cancel) => {
    record("cancel " + cancel.target);
    if (!${answerCancel}) return;
    respond(cancel, { ok: true, result: { target: cancel.target, state: cancel.target === running?.requestId ? "running" : "pending" } });
    setTimeout(() => {
      record("partial removed");
      respond(running, { ok: false, error: { code: "CANCELLED", message: "native export was cancelled: the client cancelled request " + running.requestId } });
    }, 100);
  },
});
`;

test(
  "an abort sends a cancel, settles once the host answers, and keeps the host",
  { timeout: 10_000 },
  async () => {
    const root = await scratch("alpha2-node-cancel-line-");
    const log = join(root, "log");
    const hostPath = await fakeHost(root, CANCELLABLE_HOST(log));
    const controller = new AbortController();
    let aborted;
    const client = new Alpha2NodeClient({ hostPath });
    const error = await client
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
    // The host answered the request, having removed its partial file.
    assert.equal(lines[3], "partial removed");
    const pid = Number(lines[0].split(" ")[1]);
    assert.ok(alive(pid), "the host was ended");
    await client.close();
    assertExited(pid);
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

/**
 * Logs `start`, `request` and `exit` lines with its pid and answers with its pid, modes and request
 * count; `behavior` is code run first for each request, which may answer and return.
 */
const POOLED_HOST = (
  log,
  behavior = "",
) => `import { appendFileSync, existsSync, writeFileSync } from "node:fs";
import { drain, respond, serve } from ${IO};
const record = (line) => appendFileSync(${JSON.stringify(log)}, line + "\\n");
record("start " + process.pid);
process.on("exit", () => record("exit " + process.pid));
let served = 0;
serve((request) => {
  served += 1;
  record("request " + process.pid);
  const result = { pid: process.pid, decode: process.env.ALPHA2_VIDEO_DECODE ?? "unset", renderer: process.env.ALPHA2_RENDERER ?? "unset", served };
  ${behavior}
  respond(request, { ok: true, result });
});
`;

async function exported(client, overrides = {}) {
  let result;
  await client.exportProject(
    exportRequest("out.mp4", {
      ...overrides,
      onResult: (value) => (result = value),
    }),
  );
  return result;
}

test("a client sends its requests one after another to the host it keeps, and close ends it", async () => {
  const root = await scratch("alpha2-node-pool-reuse-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, POOLED_HOST(log)),
  });
  const first = await exported(client);
  const second = await exported(client);
  assert.equal(second.pid, first.pid);
  assert.equal(second.served, 2);
  const [third, fourth] = await Promise.all([
    exported(client),
    exported(client),
  ]);
  assert.deepEqual(
    [third.pid, fourth.pid, third.served + fourth.served],
    [first.pid, first.pid, 7],
  );
  await client.close();
  assertExited(first.pid);
  const after = await exported(client);
  assert.notEqual(after.pid, first.pid);
  assert.equal(after.served, 1);
});

test("a host that reports draining in a state message is sent no more requests", async () => {
  const root = await scratch("alpha2-node-pool-drain-state-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(
      root,
      POOLED_HOST(
        log,
        `if (served === 1) setTimeout(() => drain("idle"), 50);`,
      ),
    ),
  });
  const first = await exported(client);
  // The host exits only once the client closes its stdin.
  await waitUntil(() => !alive(first.pid), "the draining host to exit");
  const second = await exported(client);
  assert.notEqual(second.pid, first.pid);
  assert.equal(second.served, 1);
});

test("a response whose host state is draining removes its host", async () => {
  const root = await scratch("alpha2-node-pool-drain-answer-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(
      root,
      POOLED_HOST(
        log,
        `if (served === 1) return respond(request, { ok: true, result, hostState: "draining" });`,
      ),
    ),
  });
  const first = await exported(client);
  const second = await exported(client);
  assert.notEqual(second.pid, first.pid);
  assert.equal(second.served, 1);
  await waitUntil(() => !alive(first.pid), "the draining host to exit");
});

test("a host that exits while idle is replaced by a new one", async () => {
  const root = await scratch("alpha2-node-pool-idle-exit-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(
      root,
      POOLED_HOST(
        log,
        `if (served === 1) setTimeout(() => process.exit(0), 50);`,
      ),
    ),
  });
  const first = await exported(client);
  await waitUntil(() => !alive(first.pid), "the host to exit");
  // Windows reports the process gone before Node delivers its exit event to the client.
  await new Promise((resolveWait) => setTimeout(resolveWait, 1_000));
  const second = await exported(client);
  assert.notEqual(second.pid, first.pid);
});

test("a request answered HOST_RETIRING is sent again to another host, a few times at most", async () => {
  const root = await scratch("alpha2-node-pool-retiring-");
  const log = join(root, "log");
  const marker = join(root, "retired");
  const retiring = `respond(request, { ok: false, hostState: "draining", error: { code: "HOST_RETIRING", message: "the host is draining and did not start the request" } });`;
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(
      root,
      POOLED_HOST(
        log,
        `if (!existsSync(${JSON.stringify(marker)})) { writeFileSync(${JSON.stringify(marker)}, ""); return ${retiring} }`,
      ),
    ),
  });
  const result = await exported(client);
  const lines = await startsIn(log);
  const requests = lines.filter((line) => line.startsWith("request"));
  assert.equal(requests.length, 2, lines.join());
  assert.notEqual(requests[0], requests[1]);
  assert.equal(requests[1], `request ${result.pid}`);

  const always = join(root, "always");
  const stubborn = new Alpha2NodeClient({
    hostPath: await fakeHost(
      always,
      POOLED_HOST(join(always, "log"), `return ${retiring}`),
    ),
  });
  await assert.rejects(stubborn.exportProject(exportRequest("out.mp4")), {
    code: "HOST_RETIRING",
  });
  assert.equal(
    (await startsIn(join(always, "log"))).filter((line) =>
      line.startsWith("request"),
    ).length,
    4,
  );
});

test("hosts are kept apart by decode mode and renderer", async () => {
  const root = await scratch("alpha2-node-pool-modes-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, POOLED_HOST(log)),
    concurrency: 2,
    renderer: "cpu",
  });
  const software = await exported(client, { videoDecode: "software" });
  const auto = await exported(client, { videoDecode: "auto" });
  assert.notEqual(auto.pid, software.pid);
  assert.deepEqual(
    [software.decode, software.renderer, auto.decode, auto.renderer],
    ["software", "cpu", "auto", "cpu"],
  );
  assert.equal(
    (await exported(client, { videoDecode: "software" })).pid,
    software.pid,
  );
  const gpu = await exported(client, {
    videoDecode: "software",
    renderer: "gpu",
  });
  assert.ok(![software.pid, auto.pid].includes(gpu.pid));
  assert.equal(gpu.renderer, "gpu");
});

test("at the concurrency limit an idle host of other modes is ended before a new one starts", async () => {
  const root = await scratch("alpha2-node-pool-limit-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, POOLED_HOST(log)),
    concurrency: 1,
  });
  const software = await exported(client, { videoDecode: "software" });
  const auto = await exported(client, { videoDecode: "auto" });
  assert.notEqual(auto.pid, software.pid);
  assert.deepEqual(
    (await startsIn(log)).filter((line) => !line.startsWith("request")),
    [`start ${software.pid}`, `exit ${software.pid}`, `start ${auto.pid}`],
  );
});

test("an idle host does not keep the process that started it alive", async () => {
  const root = await scratch("alpha2-node-pool-unref-");
  const executable = await fakeHost(root, POOLED_HOST(join(root, "log")));
  const script = join(root, "client.mjs");
  await writeFile(
    script,
    `import { Alpha2NodeClient } from ${JSON.stringify(new URL("../dist/index.js", import.meta.url).href)};
const exportRequest = ${exportRequest.toString()};
const client = new Alpha2NodeClient({ hostPath: ${JSON.stringify(executable)} });
let result;
await client.exportProject(exportRequest("out.mp4", { onResult: (value) => (result = value) }));
process.stdout.write(JSON.stringify(result) + "\\n");
`,
  );
  const { spawn } = await import("node:child_process");
  const child = spawn(process.execPath, [script], {
    cwd: root,
    stdio: ["ignore", "pipe", "inherit"],
  });
  let stdout = "";
  let answered;
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
    answered ??= Date.now();
  });
  const timer = setTimeout(() => child.kill("SIGKILL"), 10_000);
  const [code] = await new Promise((resolveClose) =>
    child.on("close", (...args) => resolveClose(args)),
  );
  clearTimeout(timer);
  assert.equal(code, 0, stdout);
  const waited = Date.now() - answered;
  assert.ok(waited < 3_000, `exited ${waited} ms after the answer`);
  const { pid } = JSON.parse(stdout);
  // The host's stdin ends with its client.
  await waitUntil(() => !alive(pid), "the host to exit after its client");
});

test("a request goes only to a host started with the environment it would start one with now", async () => {
  const root = await scratch("alpha2-node-pool-environment-");
  const log = join(root, "log");
  const client = new Alpha2NodeClient({
    hostPath: await fakeHost(root, POOLED_HOST(log)),
    concurrency: 2,
  });
  const saved = process.env.ALPHA2_NODE_TEST_SETTING;
  delete process.env.ALPHA2_NODE_TEST_SETTING;
  try {
    const first = await exported(client);
    assert.equal((await exported(client)).pid, first.pid);
    process.env.ALPHA2_NODE_TEST_SETTING = "1";
    const changed = await exported(client);
    assert.notEqual(changed.pid, first.pid);
    assert.equal((await exported(client)).pid, changed.pid);
    delete process.env.ALPHA2_NODE_TEST_SETTING;
    assert.equal((await exported(client)).pid, first.pid);
  } finally {
    if (saved === undefined) delete process.env.ALPHA2_NODE_TEST_SETTING;
    else process.env.ALPHA2_NODE_TEST_SETTING = saved;
  }
});

const CLIENT_SCENARIOS = JSON.parse(
  await readFile(
    new URL(
      "../../../tests/cases/client-behavior-corpus.json",
      import.meta.url,
    ),
    "utf8",
  ),
).filter((scenario) => scenario.clients?.includes("node") ?? true);

/** Logs its start and every request with its pid and working directory, and answers each. */
const PATHS_HOST = (log) => `import { appendFileSync } from "node:fs";
import { respond, serve } from ${IO};
const record = (entry) => appendFileSync(${JSON.stringify(log)}, JSON.stringify({ pid: process.pid, cwd: process.cwd(), ...entry }) + "\\n");
record({ start: true });
const results = {
  capabilities: { status: "ready", capabilities: {}, effectCatalog: {} },
  still: { stills: [] },
  conform: { catalog: {}, report: {} },
  extract: { dataSets: [], timings: {} },
  compose: {},
};
serve((request) => {
  record({ request });
  respond(request, { ok: true, result: results[request.operation] ?? { output: request.output } });
});
`;

function withRoot(value, root) {
  if (typeof value === "string") return value.replace("{root}", root);
  if (Array.isArray(value)) return value.map((item) => withRoot(item, root));
  if (value !== null && typeof value === "object")
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, withRoot(item, root)]),
    );
  return value;
}

function atPointer(value, pointer) {
  return pointer
    .split("/")
    .slice(1)
    .reduce((node, key) => node?.[key], value);
}

const SCENARIO_CALLS = {
  export: (client, fields) =>
    client.exportProject(exportRequest(fields.output, fields)),
  still: (client, fields) =>
    client.stills({ project: exportRequest().project, ...fields }),
  conform: (client, fields) =>
    client.conform({ project: exportRequest().project, ...fields }),
  extract: (client, fields) =>
    client.extract({
      project: exportRequest().project,
      request: { extractor: "example.levels", version: 1 },
      ...fields,
      plugin: { ...fields.plugin, sha256: "0".repeat(64) },
    }),
  compose: (client, { action, ...fields }) => client.compose(action, fields),
};

const SCENARIO_DRIVERS = {
  async "request-paths"(scenario) {
    const root = await scratch("alpha2-node-request-paths-");
    const log = join(root, "log");
    const client = new Alpha2NodeClient({
      hostPath: await fakeHost(join(root, "host"), PATHS_HOST(log)),
    });
    await client.getCapabilities();
    const hostDirectory = process.cwd();
    const sent = [];
    for (const directory of scenario.given.directories) {
      await mkdir(join(root, directory));
      process.chdir(join(root, directory));
      for (const [index, request] of scenario.given.requests.entries()) {
        const given = withRoot(request.fields, root);
        const expect = scenario.expect.requests[index];
        sent.push({ cwd: process.cwd(), given, expect });
        await SCENARIO_CALLS[request.operation](client, given);
      }
    }
    const lines = (await startsIn(log)).map((line) => JSON.parse(line));
    const starts = lines.filter((line) => line.start);
    assert.equal(starts.length, scenario.expect.hosts);
    const received = lines.filter(
      (line) => line.request && line.request.operation !== "capabilities",
    );
    assert.equal(received.length, sent.length);
    received.forEach(({ pid, cwd, request }, index) => {
      assert.equal(pid, starts[0].pid);
      assert.equal(cwd, hostDirectory);
      const { cwd: caller, given, expect } = sent[index];
      for (const pointer of expect.resolved) {
        const path = atPointer(given, pointer);
        const absolute =
          process.platform === "win32"
            ? resolve(caller, path)
            : `${caller}/${path}`;
        assert.equal(atPointer(request, pointer), absolute, pointer);
      }
      for (const pointer of expect.unchanged)
        assert.deepEqual(
          atPointer(request, pointer),
          atPointer(given, pointer),
          pointer,
        );
    });
  },
};

for (const scenario of CLIENT_SCENARIOS)
  test(`client behavior scenario ${scenario.id}`, async () => {
    const driver = SCENARIO_DRIVERS[scenario.kind];
    assert.ok(driver, `no driver for scenario kind ${scenario.kind}`);
    await driver(scenario);
  });
