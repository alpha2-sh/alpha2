import { test } from "node:test";
import assert from "node:assert/strict";
import { ExportFailureError } from "alpha2/export-failure";
import { createEditor, EditorError } from "../dist/index.js";
import { ExportJob } from "../dist/browser/jobs.js";
import { createNativeClient } from "../dist/native-client/index.js";

const failure = (code, extra = {}) =>
  new ExportFailureError({
    code,
    message: `${code} happened`,
    origin: "browser",
    operation: "export",
    backend: "browser-webgpu",
    phase: "render",
    resource: "unknown",
    frameIndex: 5,
    ...extra,
  });
const artifact = { kind: "blob", blob: new Blob(["mp4"]), name: "a.mp4" };
const noWait = { retry: { backoffMs: () => 0 } };

function job(backend, options = noWait) {
  const editor = createEditor();
  return new ExportJob(
    editor.revision,
    editor.document,
    backend,
    () => {},
    options,
  );
}

test("a lost device is retried whole on a fresh run, and the job records why", async () => {
  const attempts = [];
  const seen = [];
  const backend = {
    async run(doc, _signal, progress, attempt) {
      attempts.push({ doc, attempt });
      progress({ phase: "rendering", fraction: 0.5 });
      if (attempt.number === 1) throw failure("DEVICE_LOST");
      return artifact;
    },
  };
  const exported = job(backend);
  exported.subscribe((value) => seen.push(value.phase));
  assert.equal(await exported.result, artifact);
  assert.equal(attempts.length, 2);
  assert.equal(attempts[1].doc, attempts[0].doc);
  assert.equal(attempts[1].attempt.profile.name, "default");
  assert.equal(attempts[1].attempt.previous.code, "DEVICE_LOST");
  assert.equal(attempts[1].attempt.id, `${exported.id}/2`);
  const [first, second] = exported.attempts;
  assert.equal(first.outcome, "failed");
  assert.equal(first.failure.code, "DEVICE_LOST");
  assert.equal(first.failure.attemptId, `${exported.id}/1`);
  assert.equal(first.decision.retry, true);
  assert.equal(second.outcome, "succeeded");
  assert.ok(seen.includes("retrying"));
  assert.equal(exported.progress.phase, "completed");
  assert.equal(exported.progress.attempt, 2);
});

test("memory exhaustion retries with less in flight", async () => {
  const profiles = [];
  const exported = job({
    async run(_doc, _signal, _progress, attempt) {
      profiles.push(attempt.profile);
      if (attempt.number < 3)
        throw failure(
          attempt.number === 1 ? "OUT_OF_MEMORY" : "ALLOCATION_FAILED",
        );
      return artifact;
    },
  });
  await exported.result;
  assert.deepEqual(
    profiles.map((profile) => [profile.name, profile.memoryScale]),
    [
      ["default", 1],
      ["reduced", 0.5],
      ["minimal", 0.25],
    ],
  );
});

test("a failure that persists ends after the bounded attempts, and is cleaned up once", async () => {
  let runs = 0;
  const disposed = [];
  const exported = job({
    async run() {
      runs += 1;
      throw failure("DEVICE_LOST", { frameIndex: runs });
    },
    async dispose(outcome) {
      disposed.push(outcome);
    },
  });
  await assert.rejects(exported.result, /DEVICE_LOST happened/);
  assert.equal(runs, 3);
  assert.equal(exported.progress.phase, "failed");
  assert.equal(exported.failure.code, "DEVICE_LOST");
  assert.equal(exported.failure.attemptId, `${exported.id}/3`);
  assert.match(exported.attempts[2].decision.reason, /all 3 attempts failed/);
  assert.deepEqual(disposed, [{ ok: false, cancelled: false }]);
});

test("invalid input is not retried", async () => {
  let runs = 0;
  const exported = job({
    async run() {
      runs += 1;
      throw failure("INVALID_INPUT");
    },
  });
  await assert.rejects(exported.result);
  assert.equal(runs, 1);
  assert.equal(exported.attempts[0].decision.retry, false);
});

test("an editor's own refusal is invalid input, not retried", async () => {
  // Thrown inside an attempt: an empty range by the bridge's run, media that is not ready or not
  // there by the native asset provider.
  for (const [code, operation] of [
    ["empty_export", "export"],
    ["conform_failed", "export"],
    ["missing_asset", "native-export"],
    ["invalid_projection", "export"],
  ]) {
    let runs = 0;
    const exported = job({
      async run() {
        runs += 1;
        throw new EditorError(code, `${code} refused`, operation);
      },
    });
    const error = await exported.result.then(
      () => assert.fail(`${code} resolved`),
      (cause) => cause,
    );
    assert.equal(runs, 1, code);
    assert.equal(exported.attempts[0].failure.code, "INVALID_INPUT", code);
    assert.equal(exported.attempts[0].decision.retry, false, code);
    assert.equal(error.failure.code, "INVALID_INPUT", code);
  }
  for (const [code, expected] of [
    ["resource_limit", "RESOURCE_LIMIT"],
    ["storage_quota_exceeded", "STORAGE_QUOTA_EXCEEDED"],
  ]) {
    let runs = 0;
    const exported = job({
      async run() {
        runs += 1;
        throw new EditorError(code, `${code} refused`, "export");
      },
    });
    await assert.rejects(exported.result);
    assert.equal(runs, 1, code);
    assert.equal(exported.attempts[0].failure.code, expected, code);
  }
});

test("a late report from an ended attempt moves nothing", async () => {
  let stale;
  let release;
  const gate = new Promise((resolve) => (release = resolve));
  const exported = job({
    async run(_doc, _signal, progress, attempt) {
      if (attempt.number === 1) {
        stale = progress;
        throw failure("HOST_EXITED");
      }
      progress({ phase: "rendering", fraction: 0.2 });
      await gate;
      return artifact;
    },
  });
  // Bounded: a job that never reports attempt 2 (its report dropped, or the failure not retried)
  // fails here instead of polling forever.
  const deadline = Date.now() + 5_000;
  while (
    exported.progress.attempt !== 2 ||
    exported.progress.phase !== "rendering"
  ) {
    assert.ok(
      Date.now() < deadline,
      `attempt 2 never reported rendering: ${JSON.stringify(exported.progress)}`,
    );
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  stale({ phase: "finalizing", fraction: 0.99 });
  assert.equal(exported.progress.phase, "rendering");
  assert.equal(exported.progress.fraction, 0.2);
  assert.equal(exported.progress.attempt, 2);
  release();
  assert.equal(await exported.result, artifact);
});

test("cancelling during the backoff starts no new attempt", async () => {
  let runs = 0;
  const disposed = [];
  let slept;
  const sleeping = new Promise((resolve) => (slept = resolve));
  const exported = job(
    {
      async run() {
        runs += 1;
        throw failure("DEVICE_LOST");
      },
      async dispose(outcome) {
        disposed.push(outcome);
      },
    },
    {
      sleep: (_ms, signal) =>
        new Promise((_resolve, reject) => {
          slept();
          signal.addEventListener("abort", () => reject(signal.reason), {
            once: true,
          });
        }),
    },
  );
  await sleeping;
  assert.equal(exported.progress.phase, "retrying");
  await exported.cancel();
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(runs, 1);
  assert.equal(exported.progress.phase, "cancelled");
  await assert.rejects(exported.result, { code: "cancelled" });
  assert.deepEqual(disposed, [{ ok: false, cancelled: true }]);
});

test("a native retry renders again from the same uploads, asking the host for less", async () => {
  const calls = [];
  let posts = 0;
  const client = createNativeClient({
    baseUrl: "http://host/api/native",
    fetch: async (url, init) => {
      const method = init?.method ?? "GET";
      const path = new URL(url).pathname;
      calls.push(`${method} ${path.replace(/[\w-]{36}/, "id")}`);
      if (method === "POST") {
        posts += 1;
        calls.push(JSON.parse(init.body));
        return Response.json({});
      }
      if (method === "GET")
        return posts === 1
          ? Response.json({
              phase: "failed",
              fraction: 0.3,
              error: "out of memory",
              failure: {
                code: "OUT_OF_MEMORY",
                origin: "native-host",
                operation: "export",
                backend: "native",
                phase: "readback",
                resource: "gpu",
              },
            })
          : Response.json({
              phase: "completed",
              fraction: 1,
              artifact: {
                name: "a.mp4",
                bytes: 3,
                sha256: "x",
                createdAt: 1,
                expiresAt: 2,
              },
            });
      return Response.json({});
    },
  });
  const editor = createEditor();
  const exported = client.startExport(editor.getSnapshot(), [
    {
      id: "clip",
      kind: "assets",
      blob: new Blob(["media"]),
      name: "clip.mp4",
    },
  ]);
  // The reference backoff is a second; the retry is what is checked, not the wait.
  const result = await exported.result;
  assert.equal(result.kind, "url");
  const methods = calls.filter((call) => typeof call === "string");
  assert.equal(
    methods.filter((call) => call === "PUT /api/native/jobs/id").length,
    1,
  );
  assert.equal(
    methods.filter((call) => call.startsWith("PUT /api/native/jobs/id/assets"))
      .length,
    1,
  );
  assert.equal(methods.filter((call) => call.startsWith("DELETE")).length, 0);
  const [first, second] = calls.filter((call) => typeof call === "object");
  assert.equal(first.memory, undefined);
  assert.equal(second.memory.scale, 0.5);
  assert.equal(first.attempt, `${exported.id}/1`);
  assert.equal(second.attempt, `${exported.id}/2`);
  await client.dispose();
  await editor.dispose();
});

test("every editor error code is classified as the shared table says", async () => {
  const { readFile, readdir } = await import("node:fs/promises");
  const table = JSON.parse(
    await readFile(
      new URL("../../../tests/cases/failure-classes.json", import.meta.url),
      "utf8",
    ),
  ).editor;
  for (const [code, expected] of Object.entries(table))
    assert.equal(
      new EditorError(code, code, "test").failureCode,
      expected,
      code,
    );
  assert.equal(
    new EditorError("no_such_code", "?", "test").failureCode,
    "UNKNOWN",
  );
  // Every code the editor's sources make is in the table, so a new one is classified when it is
  // added.
  const root = new URL("../src/", import.meta.url);
  const made = new Set();
  for (const entry of await readdir(root, { recursive: true })) {
    if (!/\.(ts|tsx|js)$/.test(entry)) continue;
    const text = await readFile(new URL(entry, root), "utf8");
    for (const match of text.matchAll(/(?:EditorError|super)\(\s*"([a-z_]+)"/g))
      made.add(match[1]);
  }
  assert.ok(made.size >= 25, [...made].join());
  for (const code of made)
    assert.ok(
      code in table,
      `${code} is not in tests/cases/failure-classes.json`,
    );
});

test("the staging an export frame holds is the renderer's, canary included", async () => {
  // The JS mirror of the renderer's I420 staging (16-byte canary included) is held to the corpus
  // alpha2-render's readback tests read.
  const { estimatedStagingBytes, minimumExportWorkingSet } =
    await import("../dist/browser/runtime/staging.js");
  const { readFile } = await import("node:fs/promises");
  const corpus = JSON.parse(
    await readFile(
      new URL(
        "../../../tests/cases/staging-bytes-corpus.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  assert.ok(corpus.sizes.length >= 8);
  for (const { width, height, stagingBytes, packedBytes } of corpus.sizes) {
    assert.equal(
      estimatedStagingBytes(width, height),
      stagingBytes,
      `${width}x${height}`,
    );
    assert.equal(
      minimumExportWorkingSet(width, height),
      stagingBytes + packedBytes,
      `${width}x${height}`,
    );
  }
});

test("a retry that asks for less in flight never goes below one frame's working set", async () => {
  const { attemptOptions, DEFAULT_IN_FLIGHT_BYTES } =
    await import("../dist/browser/runtime/budget.js");
  const { minimumExportWorkingSet } =
    await import("../dist/browser/runtime/staging.js");
  const uhd8k = { width: 7680, height: 4320 };
  const frame = minimumExportWorkingSet(uhd8k.width, uhd8k.height);
  assert.ok(frame > DEFAULT_IN_FLIGHT_BYTES / 2, "an 8K frame needs more");
  const options = { videoCodec: "avc" };
  assert.equal(
    attemptOptions(options, { profile: { memoryScale: 1 } }, uhd8k),
    options,
  );
  // At 8K a reduced or minimal retry would ask for less than one frame: it gets one frame.
  for (const memoryScale of [0.5, 0.25])
    assert.deepEqual(
      attemptOptions(options, { profile: { memoryScale } }, uhd8k),
      { videoCodec: "avc", memory: { inFlightBytes: frame } },
    );
  // At 1080p the scaled default fits, and the caller's own budget is what is scaled.
  const hd = { width: 1920, height: 1080 };
  assert.equal(
    attemptOptions({}, { profile: { memoryScale: 0.5 } }, hd).memory
      .inFlightBytes,
    DEFAULT_IN_FLIGHT_BYTES / 2,
  );
  assert.equal(
    attemptOptions(
      { memory: { inFlightBytes: 256 * 1024 * 1024 } },
      { profile: { memoryScale: 0.25 } },
      hd,
    ).memory.inFlightBytes,
    64 * 1024 * 1024,
  );
});

test("a cancel that arrives after the attempt finished releases what it made and disposes as cancelled", async () => {
  const released = [];
  const disposed = [];
  let finish;
  const backend = {
    // An attempt past its last cancellation check (publishing, cleaning up): it finishes anyway.
    run: () => new Promise((resolve) => (finish = resolve)),
    async dispose(outcome) {
      disposed.push(outcome);
    },
  };
  const exporting = job(backend);
  await new Promise((resolve) => setTimeout(resolve, 0));
  const cancelling = exporting.cancel();
  finish({
    kind: "file",
    name: "out.mp4",
    file: new File([], "out.mp4"),
    release: async () => {
      released.push("out.mp4");
    },
  });
  await cancelling;
  await assert.rejects(exporting.result, (error) => error.code === "cancelled");
  assert.deepEqual(released, ["out.mp4"], "the finished file is deleted");
  assert.deepEqual(disposed, [{ ok: false, cancelled: true }]);
  assert.equal(exporting.attempts[0].outcome, "cancelled");
  assert.equal(exporting.progress.phase, "cancelled");
});
