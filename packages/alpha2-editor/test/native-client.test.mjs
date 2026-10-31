import { test } from "node:test";
import assert from "node:assert/strict";
import { validateProject } from "alpha2";
import { createDocument, createEditor } from "../dist/index.js";
import {
  createNativeClient,
  nativeArtifactStatus,
} from "../dist/native-client/index.js";
import { assertScalesBelow } from "../../../scripts/lib/timing.mjs";

test("native cancellation waits for acknowledged cleanup, including allocation race", async () => {
  let allocate,
    removed = false;
  const client = createNativeClient({
    baseUrl: "http://host/api/native",
    fetch: async (_url, init) => {
      if (init?.method === "PUT") await new Promise((r) => (allocate = r));
      if (init?.method === "DELETE") {
        await new Promise((r) => setTimeout(r, 20));
        removed = true;
      }
      return Response.json({});
    },
  });
  const editor = createEditor();
  const job = client.startExport(editor.getSnapshot(), []);
  await new Promise((r) => setTimeout(r, 0));
  let cancelled = false;
  const cancellation = job.cancel().then(() => (cancelled = true));
  assert.equal(cancelled, false);
  allocate();
  await cancellation;
  assert.equal(removed, true);
  assert.equal(job.progress.phase, "cancelled");
  await assert.rejects(job.result, /cancelled/);
  await client.dispose();
  await editor.dispose();
});

test("a finished native export is a kept file, released only on request", async () => {
  let sent;
  const methods = [];
  const artifact = {
    name: "Sequence.mp4",
    bytes: 3,
    sha256: "abc",
    createdAt: 1,
    expiresAt: 3_600_001,
  };
  const client = createNativeClient({
    baseUrl: "http://host/api/native",
    fetch: async (url, init) => {
      methods.push(`${init?.method ?? "GET"} ${new URL(url).pathname}`);
      if (init?.method === "POST") sent = JSON.parse(init.body);
      return Response.json({ phase: "completed", fraction: 1, artifact });
    },
  });
  const editor = createEditor();
  const name = editor.document.project.name;
  const job = client.startExport(editor.getSnapshot(), []);
  editor.dispatch({
    type: "project.update",
    patch: { name: "Edited during export" },
  });
  const result = await job.result;
  assert.equal(sent.project.name, name);
  assert.equal(result.kind, "url");
  assert.match(
    result.url,
    /^http:\/\/host\/api\/native\/jobs\/[\w-]+\/artifact$/,
  );
  assert.equal(result.name, "Sequence.mp4");
  assert.equal(result.expiresAt, 3_600_001);
  // Nothing was downloaded into the page, and nothing deleted the file.
  assert.deepEqual(
    methods.map((m) => m.split(" ")[0]),
    ["PUT", "POST", "GET"],
  );
  await result.release();
  assert.equal(methods.at(-1).split(" ")[0], "DELETE");
  await client.dispose();
  await editor.dispose();
});

test("native cancellation reports an unacknowledged cleanup", async () => {
  let allocated;
  const gate = new Promise((r) => (allocated = r));
  const client = createNativeClient({
    baseUrl: "http://host",
    fetch: async (_url, init) => {
      if (init?.method === "PUT") {
        await gate;
        return Response.json({});
      }
      if (init?.method === "DELETE") throw Error("host disconnected");
      return Response.json({});
    },
  });
  const editor = createEditor();
  const job = client.startExport(editor.getSnapshot(), []);
  await new Promise((r) => setTimeout(r, 0));
  const cancellation = job.cancel();
  allocated();
  await assert.rejects(cancellation, /cleanup could not be confirmed/);
  assert.equal(job.progress.phase, "failed");
  await editor.dispose();
  await client.dispose();
});

test("a host failure survives a cleanup that also fails, as its secondary", async () => {
  const hostFailure = {
    code: "READBACK_FAILED",
    message: "GPU texture readback failed: map-failed",
    origin: "native-host",
    operation: "export",
    backend: "native",
    phase: "readback",
    stage: "map-failed",
    frameIndex: 12,
    resource: "unknown",
    stderr: "alpha2-host uncaptured GPU internal error",
  };
  const client = createNativeClient({
    baseUrl: "http://host",
    fetch: async (_url, init) => {
      if (init?.method === "DELETE") throw Error("host disconnected");
      if ((init?.method ?? "GET") === "GET")
        return Response.json({
          phase: "failed",
          fraction: 0.4,
          error: hostFailure.message,
          failure: hostFailure,
        });
      return Response.json({});
    },
  });
  const editor = createEditor();
  const job = client.startExport(editor.getSnapshot(), []);
  await assert.rejects(job.result, /map-failed/);
  assert.equal(job.progress.phase, "failed");
  assert.equal(job.failure.code, "READBACK_FAILED");
  assert.equal(job.failure.frameIndex, 12);
  assert.equal(job.failure.stderr, hostFailure.stderr);
  assert.equal(job.failure.jobId, job.id);
  assert.deepEqual(
    job.failure.secondary.map(({ phase }) => phase),
    ["cleanup"],
  );
  await client.dispose();
  await editor.dispose();
});

test("a kept file's status is asked without downloading it", async () => {
  const calls = [];
  const answer = (status, body, headers = {}) =>
    new Response(status === 200 ? null : JSON.stringify(body), {
      status,
      headers,
    });
  const status = (response) =>
    nativeArtifactStatus("http://host/f", async (_url, init) => {
      calls.push(init?.method ?? "GET");
      return typeof response === "function" ? response(init) : response;
    });
  assert.deepEqual(
    await status(answer(200, null, { "x-alpha2-expires-at": "42" })),
    { status: "available", expiresAt: 42 },
  );
  assert.deepEqual(await status(() => answer(410, { reason: "evicted" })), {
    status: "gone",
    reason: "evicted",
  });
  assert.deepEqual(await status(answer(404, {})), { status: "missing" });
  assert.deepEqual(
    await nativeArtifactStatus("http://host/f", async () => {
      throw TypeError("offline");
    }),
    { status: "unreachable" },
  );
  assert.ok(calls.includes("HEAD"));
});

test("a native export sends a project the host accepts, whatever the graphs leave unread", async () => {
  const span = { start: 0, end: 120000 };
  const document = createDocument({
    schema_version: 1,
    id: "p",
    name: "Unread",
    settings: {
      width: 64,
      height: 48,
      frame_rate: { num: 30, den: 1 },
      sample_rate: 48000,
    },
    sources: [
      { id: "s", kind: "video", locator: "test://s" },
      { id: "loose", kind: "image", locator: "test://loose.png" },
      { id: "unplaced", kind: "image", locator: "test://unplaced.png" },
      { id: "hidden", kind: "image", locator: "test://hidden.png" },
    ],
    timeline: {
      tracks: [
        {
          id: "t",
          name: "Video",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "s",
              timeline_range: span,
              source_range: span,
            },
          ],
        },
        {
          id: "t2",
          name: "Hidden",
          kind: "video",
          clips: [
            {
              id: "c2",
              source_id: "s",
              timeline_range: span,
              source_range: span,
            },
          ],
        },
      ],
    },
  });
  const editor = createEditor({ document });
  const ok = (command) => {
    const result = editor.dispatch(command);
    assert.equal(result.ok, true, result.error?.message);
  };
  const media = (id, source) => ({
    id,
    op: { type: "media", source_id: source },
  });
  // A media node not yet connected.
  ok({ type: "graph.create", clipId: "c" });
  ok({
    type: "graph.node.add",
    compositionId: "c-fx",
    node: media("loose-node", "loose"),
  });
  // A library composition not on the timeline.
  ok({
    type: "composition.create",
    compositionId: "lower-third",
    sourceId: "lower-third-source",
    name: "Lower third",
  });
  ok({
    type: "graph.node.add",
    compositionId: "lower-third",
    node: media("logo", "unplaced"),
  });
  ok({ type: "graph.output", compositionId: "lower-third", nodeId: "logo" });
  // A graph on a hidden track.
  ok({ type: "graph.create", clipId: "c2" });
  ok({
    type: "graph.node.add",
    compositionId: "c2-fx",
    node: media("inset", "hidden"),
  });
  ok({ type: "graph.output", compositionId: "c2-fx", nodeId: "inset" });
  ok({ type: "track.metadata", id: "t2", patch: { enabled: false } });

  let sent;
  const client = createNativeClient({
    baseUrl: "http://host/api/native",
    fetch: async (_url, init) => {
      if (init?.method === "POST") sent = JSON.parse(init.body);
      return Response.json({
        phase: "completed",
        fraction: 1,
        artifact: {
          name: "out.mp4",
          bytes: 1,
          sha256: "a",
          createdAt: 1,
          expiresAt: 2,
        },
      });
    },
  });
  const job = client.startExport(editor.getSnapshot(), []);
  await job.result;
  assert.deepEqual(validateProject(sent.project), []);
  await client.dispose();
  await editor.dispose();
});

test("an export rate is sent exactly as given, and one the engine cannot read is refused first", async () => {
  const requests = [];
  const client = createNativeClient({
    baseUrl: "http://host/api/native",
    fetch: async (url, init) => {
      requests.push(init?.method ?? "GET");
      if (init?.method === "POST") requests.push(JSON.parse(init.body));
      return Response.json({ phase: "completed", fraction: 1, artifact: {} });
    },
  });
  const editor = createEditor();
  for (const frameRate of [
    29.97,
    0,
    { num: 30_000, den: 0 },
    { num: 2.5, den: 1 },
    { num: 120_001, den: 1 },
    { num: 2 ** 32, den: 1 },
  ]) {
    assert.throws(
      () => client.startExport(editor.getSnapshot(), [], { frameRate }),
      { code: "invalid_frame_rate" },
      JSON.stringify(frameRate),
    );
  }
  assert.deepEqual(requests, [], "nothing reached the service");
  const job = client.startExport(editor.getSnapshot(), [], {
    frameRate: { num: 30_000, den: 1_001 },
  });
  await job.result;
  const sent = requests.find((r) => typeof r === "object");
  assert.deepEqual(sent.frameRate, { num: 30_000, den: 1_001 });
  await client.dispose();
  await editor.dispose();
});

const finished = {
  name: "out.mp4",
  bytes: 1,
  sha256: "a",
  createdAt: 1,
  expiresAt: 2,
};

test("an image sequence's frames map to their uploads in one pass, however long", async () => {
  // An editor's snapshot of a sequence of `count` frames, and its uploads as the bridge's
  // nativeAssets makes them: one per frame, in frame order.
  const sequence = (count) => {
    const locators = Array.from(
      { length: count },
      (_, i) => `browser://seq/frame/${i + 1}`,
    );
    const document = createDocument({
      schema_version: 1,
      id: "p",
      name: "Sequence",
      settings: {
        width: 64,
        height: 48,
        frame_rate: { num: 30, den: 1 },
        sample_rate: 48000,
      },
      sources: [
        {
          id: "seq",
          kind: "video",
          locator: "browser://seq",
          provider: {
            format: "image_sequence",
            frames: locators,
            frame_rate: { num: 30, den: 1 },
            start_number: 1,
            missing: "reject",
          },
        },
      ],
      timeline: { tracks: [] },
    });
    const editor = createEditor({ document });
    const snapshot = editor.getSnapshot();
    editor.dispose();
    const assets = locators.map((locator, i) => ({
      id: `seq:${i}`,
      kind: "frames",
      blob: new Blob(["png"]),
      name: `${i + 1}.png`,
      locator,
    }));
    return { snapshot, assets };
  };
  // Exports the sequence through a service that answers every request at once; returns the
  // export request's body.
  const exported = async ({ snapshot, assets }) => {
    let sent;
    const client = createNativeClient({
      baseUrl: "http://host",
      fetch: async (_url, init) => {
        if (init?.method === "POST") sent = JSON.parse(init.body);
        return Response.json({
          phase: "completed",
          fraction: 1,
          artifact: finished,
        });
      },
    });
    await client.startExport(snapshot, assets).result;
    await client.dispose();
    return sent;
  };
  const count = 40000;
  const sent = await exported(sequence(count));
  assert.equal(sent.frames.seq.length, count);
  assert.equal(sent.frames.seq[0], "seq:0");
  assert.equal(sent.frames.seq[count - 1], `seq:${count - 1}`);
  // Looking every frame up in the upload list took about 2 s at this length, several times the
  // rest of the export.
  await assertScalesBelow(
    "an image sequence's export",
    count / 8,
    sequence,
    exported,
  );
});

/**
 * A native export service that keeps a job's state as the client expects: PUT allocates a job once, an
 * asset is uploaded while it prepares, POST renders only a preparing or failed job, and a render
 * completes after `renderPolls` status requests. `fail(method, path, count)` makes a request throw
 * as a lost connection does.
 */
function service({ renderPolls = 3, fail = () => false } = {}) {
  const calls = [];
  const counts = new Map();
  let job = null;
  const fetch = async (url, init) => {
    const method = init?.method ?? "GET";
    const path = new URL(url).pathname.replace(/[\w-]{36}/, "id");
    const key = `${method} ${path}`;
    counts.set(key, (counts.get(key) ?? 0) + 1);
    calls.push(key);
    if (fail(method, path, counts.get(key)))
      throw new TypeError("network connection lost");
    if (method === "PUT" && path === "/jobs/id") {
      if (job)
        return Response.json({ error: "Job already exists" }, { status: 409 });
      job = { phase: "preparing", polls: 0 };
      return Response.json({}, { status: 201 });
    }
    if (!job) return Response.json({ error: "Unknown" }, { status: 404 });
    if (method === "PUT") return Response.json({ bytes: 1 });
    if (method === "POST") {
      if (!["preparing", "failed"].includes(job.phase))
        return Response.json({ error: "Job is not ready" }, { status: 409 });
      job.phase = "rendering";
      return Response.json({}, { status: 202 });
    }
    if (method === "DELETE") {
      job = null;
      return Response.json({ removed: true });
    }
    if (job.phase === "rendering" && ++job.polls >= renderPolls)
      job.phase = "completed";
    return Response.json({
      phase: job.phase,
      fraction: job.phase === "completed" ? 1 : job.polls / renderPolls,
      ...(job.phase === "completed" ? { artifact: finished } : {}),
    });
  };
  return { fetch, calls, count: (key) => counts.get(key) ?? 0 };
}

const nativeExport = async (fake) => {
  const editor = createEditor();
  const client = createNativeClient({
    baseUrl: "http://host",
    fetch: fake.fetch,
  });
  const job = client.startExport(editor.getSnapshot(), [
    { id: "a", kind: "assets", blob: new Blob(["1"]), name: "a.mp4" },
    { id: "b", kind: "assets", blob: new Blob(["2"]), name: "b.mp4" },
  ]);
  const result = await job.result.then(
    (value) => ({ value }),
    (error) => ({ error }),
  );
  await client.dispose();
  await editor.dispose();
  return result;
};

test("a status request lost once is asked again, and the render goes on", async () => {
  const fake = service({
    fail: (method, path, n) => method === "GET" && n === 2,
  });
  const { value, error } = await nativeExport(fake);
  assert.equal(error, undefined, error?.message);
  assert.equal(value.kind, "url");
  assert.equal(fake.count("POST /jobs/id"), 1);
  assert.equal(fake.count("DELETE /jobs/id"), 0);
});

test("a retry after an upload was lost allocates nothing again and sends only what is missing", async () => {
  const fake = service({
    fail: (method, path, n) =>
      method === "PUT" && path === "/jobs/id/assets/b" && n === 1,
  });
  const { value, error } = await nativeExport(fake);
  assert.equal(error, undefined, error?.message);
  assert.equal(value.kind, "url");
  assert.equal(fake.count("PUT /jobs/id"), 1);
  assert.equal(fake.count("PUT /jobs/id/assets/a"), 1);
  assert.equal(fake.count("PUT /jobs/id/assets/b"), 2);
  assert.equal(fake.count("DELETE /jobs/id"), 0);
});

test("a retry while the service still renders follows that render instead of asking for another", async () => {
  // Every status request of the first attempt is lost: it gives up, and the next follows.
  const fake = service({
    renderPolls: 8,
    fail: (method, path, n) => method === "GET" && n <= 5,
  });
  const { value, error } = await nativeExport(fake);
  assert.equal(error, undefined, error?.message);
  assert.equal(value.kind, "url");
  assert.equal(fake.count("POST /jobs/id"), 1);
  assert.equal(fake.count("DELETE /jobs/id"), 0);
});

test("a client that detaches lets a submitted render go on, and removes a job still uploading", async () => {
  const methods = [];
  let hold;
  const client = createNativeClient({
    baseUrl: "http://host",
    fetch: async (url, init) => {
      const method = init?.method ?? "GET";
      methods.push(`${method} ${new URL(url).pathname}`);
      if (method === "PUT" && new URL(url).pathname.split("/").length > 3)
        // An upload that does not finish before the page leaves.
        await new Promise((resolve, reject) => {
          hold = resolve;
          init.signal?.addEventListener("abort", () =>
            reject(new DOMException("aborted", "AbortError")),
          );
        });
      if (method === "GET")
        return Response.json({ phase: "rendering", fraction: 0.4 });
      return Response.json({});
    },
  });
  const editor = createEditor();
  const rendering = client.startExport(editor.getSnapshot(), []);
  await waitFor(() => rendering.progress.phase === "rendering");
  const uploading = client.startExport(editor.getSnapshot(), [
    { id: "a", kind: "assets", blob: new Blob(["x"]), name: "a.mp4" },
  ]);
  await waitFor(() => hold !== undefined);
  await client.detach();
  await assert.rejects(rendering.result, /cancelled/);
  await assert.rejects(uploading.result, /cancelled/);
  const deleted = methods.filter((m) => m.startsWith("DELETE"));
  assert.deepEqual(deleted, [`DELETE /jobs/${uploading.id}`]);
  assert.throws(() => client.startExport(editor.getSnapshot(), []), /disposed/);
  await editor.dispose();
});

test("a job another page left is removed by id, and a refusal is reported", async () => {
  const methods = [];
  let status = 200;
  const client = createNativeClient({
    baseUrl: "http://host/api/native",
    fetch: async (url, init) => {
      methods.push(`${init?.method ?? "GET"} ${new URL(url).pathname}`);
      return new Response("busy", { status });
    },
  });
  await client.releaseJob("left behind");
  assert.deepEqual(methods, ["DELETE /api/native/jobs/left%20behind"]);
  status = 500;
  await assert.rejects(client.releaseJob("x"), /Native host: busy/);
  await client.dispose();
});

async function waitFor(condition) {
  for (let i = 0; i < 500 && !condition(); i++)
    await new Promise((resolve) => setTimeout(resolve, 2));
  assert.ok(condition(), "timed out");
}
