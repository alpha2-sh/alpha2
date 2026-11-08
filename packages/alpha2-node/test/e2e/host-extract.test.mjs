// Client-side extractors against a real alpha2-host; `npm run test:node-e2e` builds it first.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import test from "node:test";

import { Alpha2NodeClient } from "../../dist/index.js";
import { executable } from "../../../../scripts/lib/cargo-target.mjs";

const root = resolve(import.meta.dirname, "..", "..", "..", "..");
const hostPath =
  process.env.ALPHA2_HOST_BINARY ?? executable("alpha2-host", "debug");
const client = new Alpha2NodeClient({ hostPath });
test.after(() => client.close());

const project = {
  schema_version: 1,
  id: "p",
  name: "P",
  settings: {
    width: 320,
    height: 180,
    frame_rate: { num: 30, den: 1 },
    sample_rate: 48_000,
  },
  sources: [{ id: "v", kind: "video", locator: "v.mp4" }],
  timeline: { tracks: [] },
};

/** 32-bit FNV-1a, as the digest fixture hashes deliveries on every surface. */
function fnv1a(bytes) {
  let hash = 0x811c9dc5;
  for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193) >>> 0;
  return hash;
}

/** The digest fixture: one `scalars` sample per delivery, the hash's halves over 65536. */
function digest(input) {
  return {
    describe: () => ({
      id: "alpha2.test.digest",
      version: 1,
      input,
      params_schema: { type: "object" },
      outputs: [
        {
          name: "digest",
          kind: "scalars",
          channels: ["hi", "lo"],
          interpolation: "hold",
        },
      ],
    }),
    video(frame, out) {
      const hash = fnv1a(frame.data);
      out.sample("digest", frame.time, [
        (hash >>> 16) / 65536,
        (hash & 0xffff) / 65536,
      ]);
    },
    audio(block, out) {
      const bytes = new Uint8Array(block.samples.buffer.slice(0));
      const hash = fnv1a(bytes);
      out.sample("digest", block.start, [
        (hash >>> 16) / 65536,
        (hash & 0xffff) / 65536,
      ]);
    },
  };
}

/** The scripted fixture: emits a given list per delivery, in order, and another at the end. */
function scripted(outputs, perDelivery, atEnd = []) {
  let delivery = 0;
  const emit = (out, items) => {
    for (const item of items) {
      if ("values" in item) out.sample(item.output, item.time, item.values);
      else if ("text" in item)
        out.word(item.output, item.start, item.end, item.text, item.speaker);
      else
        out.interval(item.output, item.start, item.end, item.label, item.score);
    }
  };
  return {
    describe: () => ({
      id: "alpha2.test.scripted",
      version: 1,
      input: { media: "video", from: ["source"], format: "luma8" },
      params_schema: {
        type: "object",
        properties: { threshold: { type: "number", minimum: 0 } },
        additionalProperties: false,
      },
      outputs,
    }),
    video: (_, out) => emit(out, perDelivery[delivery++] ?? []),
    end: (out) => emit(out, atEnd),
  };
}

function run(extractor, name, size, end, extra = {}) {
  return client.extract(
    {
      project,
      assets: { v: join(root, "tests", "corpus", name) },
      request: {
        extractor: extractor.describe().id,
        version: 1,
        input: {
          from: "source",
          source_id: "v",
          range: { start: 0, end },
          size,
        },
        outputs: Object.fromEntries(
          extractor
            .describe()
            .outputs.map((output) => [output.name, output.name]),
        ),
        ...extra,
      },
    },
    extractor,
  );
}

test("the JS digest fixture records the bytes the Rust reader pins", async () => {
  const pinned = JSON.parse(
    await readFile(
      join(root, "tests", "golden", "extract-recipe.json"),
      "utf8",
    ),
  );
  for (const name of ["h264-bt709-copy.mp4", "rotation.mp4", "hdr-pq.mp4"]) {
    const progress = [];
    const extractor = digest({
      media: "video",
      from: ["source"],
      format: "rgba8",
    });
    const result = await client.extract(
      {
        project,
        assets: { v: join(root, "tests", "corpus", name) },
        request: {
          extractor: "alpha2.test.digest",
          version: 1,
          input: {
            from: "source",
            source_id: "v",
            range: { start: 0, end: 40_000 },
            size: 96,
          },
          outputs: { digest: "frames" },
        },
        onProgress: (step) => progress.push(step),
      },
      extractor,
    );
    const [set] = result.dataSets;
    const found = set.times.map((time, index) => {
      const hi = Math.round(set.values[index * 2] * 65536);
      const lo = Math.round(set.values[index * 2 + 1] * 65536);
      return [time, (hi * 65536 + lo).toString(16).padStart(8, "0")];
    });
    assert.deepEqual(found, pinned[name], name);
    assert.ok(set.made_from, `${name}: made_from is the conformed original`);
    assert.ok(
      progress.length > 0 && progress.every((step) => step.phase === "extract"),
    );
  }
});

test("an invalid emit is refused with the session's reason", async () => {
  const extractor = scripted(
    [{ name: "track", kind: "points", channels: ["a"] }],
    [[{ output: "track", time: 0, values: [20, 0] }]],
  );
  await assert.rejects(
    run(extractor, "h264-bt709-copy.mp4", 64, 40_000),
    (error) => {
      assert.equal(error.code, "INVALID_EMIT");
      return true;
    },
  );
});

test("params are checked against the descriptor's schema before anything runs", async () => {
  const extractor = scripted(
    [{ name: "track", kind: "points", channels: ["a"] }],
    [],
  );
  await assert.rejects(
    run(extractor, "h264-bt709-copy.mp4", 64, 40_000, {
      params: { threshold: -1 },
    }),
    (error) => {
      assert.equal(error.code, "INVALID_INPUT");
      return true;
    },
  );
});

test("an extractor that throws ends the request as EXTRACTOR_FAILED", async () => {
  const extractor = {
    ...scripted([{ name: "track", kind: "points", channels: ["a"] }], []),
    video() {
      throw new Error("model missing");
    },
  };
  await assert.rejects(
    run(extractor, "h264-bt709-copy.mp4", 64, 40_000),
    (error) => {
      assert.equal(error.code, "EXTRACTOR_FAILED");
      assert.match(error.message, /model missing/);
      return true;
    },
  );
});

test("cancelling mid-pass answers CANCELLED", async () => {
  const controller = new AbortController();
  const extractor = {
    ...digest({ media: "video", from: ["source"], format: "luma8" }),
    video(frame, out) {
      if (frame.time >= 8_000) controller.abort();
      out.sample("digest", frame.time, [0, 0]);
    },
  };
  await assert.rejects(
    client.extract(
      {
        project,
        assets: { v: join(root, "tests", "corpus", "h264-bt709-copy.mp4") },
        request: {
          extractor: "alpha2.test.digest",
          version: 1,
          input: {
            from: "source",
            source_id: "v",
            range: { start: 0, end: 200_000 },
            size: 32,
          },
          outputs: { digest: "frames" },
        },
        signal: controller.signal,
      },
      extractor,
    ),
    (error) => {
      assert.equal(error.name, "Alpha2AbortError");
      return true;
    },
  );
});

test(
  "a host that exits mid-run fails the request and the pool starts another",
  { skip: process.platform !== "linux" && "finds the host by its parent pid" },
  async () => {
    const { execFileSync } = await import("node:child_process");
    const extractor = {
      ...digest({ media: "video", from: ["source"], format: "luma8" }),
      video(frame, out) {
        if (frame.time === 8_000) {
          const pids = execFileSync("pgrep", ["-P", String(process.pid)], {
            encoding: "utf8",
          });
          for (const pid of pids.trim().split("\n"))
            process.kill(Number(pid), "SIGKILL");
        }
        out.sample("digest", frame.time, [0, 0]);
      },
    };
    const request = (end) => ({
      project,
      assets: { v: join(root, "tests", "corpus", "h264-bt709-copy.mp4") },
      request: {
        extractor: "alpha2.test.digest",
        version: 1,
        input: {
          from: "source",
          source_id: "v",
          range: { start: 0, end },
          size: 32,
        },
        outputs: { digest: "frames" },
      },
    });
    await assert.rejects(
      client.extract(request(40_000), extractor),
      (error) => {
        assert.equal(error.code, "HOST_EXITED");
        return true;
      },
    );
    const next = await client.extract(
      request(4_000),
      digest({ media: "video", from: ["source"], format: "luma8" }),
    );
    assert.equal(next.dataSets[0].times.length, 1);
  },
);

test("the JS scripted fixture's lists become the data sets the session builds", async () => {
  const extractor = scripted(
    [
      { name: "words", kind: "words" },
      { name: "quiet", kind: "intervals", suggest: "remove_ranges" },
    ],
    // The interval is emitted once the pass has delivered past its end.
    [
      [],
      [],
      [{ output: "quiet", start: 0, end: 4_000, label: "silence", score: 0.5 }],
    ],
    [
      {
        output: "words",
        start: 12_000,
        end: 20_000,
        text: "late",
        speaker: "b",
      },
      { output: "words", start: 0, end: 8_000, text: "early" },
    ],
  );
  const { dataSets } = await run(extractor, "h264-bt709-copy.mp4", 32, 40_000);
  const byId = Object.fromEntries(dataSets.map((set) => [set.id, set]));
  assert.deepEqual(byId.words.words, [
    { start: 0, end: 8_000, text: "early" },
    { start: 12_000, end: 20_000, text: "late", speaker: "b" },
  ]);
  assert.deepEqual(byId.quiet.intervals, [
    { start: 0, end: 4_000, label: "silence", score: 0.5 },
  ]);
  for (const set of dataSets) {
    assert.deepEqual(set.anchor, { type: "source", source_id: "v" });
    assert.equal(set.made_by.extractor, "alpha2.test.scripted");
    assert.equal(set.made_by.plugin, undefined);
  }
});
