// Client-side extractors against a real alpha2-host; `npm run test:node-e2e` builds it first.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
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

const expectedProgress = (times, end) => {
  const report = (stage, completed, covered) => ({
    phase: "extract",
    stage,
    pass: 0,
    completed,
    total: null,
    covered,
    span: end,
  });
  return [
    report("begin", 0, 0),
    report("pass", 0, 0),
    ...times.map((time, index) => report("pass", index + 1, time)),
    report("end", times.length, end),
  ];
};

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
    assert.deepEqual(
      progress,
      expectedProgress(
        pinned[name].map(([time]) => time),
        40_000,
      ),
      name,
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

/** The Rust target triple of this machine, as manifests name native artifacts. */
function hostTriple() {
  const output = spawnSync("rustc", ["-vV"], { encoding: "utf8" }).stdout;
  return /^host: (.+)$/m.exec(output)[1];
}

/** Writes fixture `name` as a plugin directory; returns the manifest's path and hash. */
async function pluginDir(name, { wasm = true, native = false } = {}) {
  const plugins = join(root, "plugins");
  const triple = hostTriple();
  for (const target of ["wasm32-unknown-unknown", triple]) {
    const built = spawnSync(
      "cargo",
      [
        "build",
        "--release",
        "--target",
        target,
        "--manifest-path",
        join(plugins, "Cargo.toml"),
        "--target-dir",
        join(plugins, "target"),
        "-p",
        `alpha2-plugin-fixture-${name}`,
      ],
      {
        stdio: "inherit",
        env: {
          ...process.env,
          CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS:
            "-Ctarget-feature=-relaxed-simd",
        },
      },
    );
    assert.equal(built.status, 0, `fixture ${name} builds for ${target}`);
  }
  const dir = await mkdtemp(join(tmpdir(), `alpha2-plugin-${name}-`));
  const library = `alpha2_plugin_fixture_${name}`;
  const artifacts = [];
  const place = async (from, url, target, type) => {
    const bytes = await readFile(from);
    await mkdir(join(dir, url, ".."), { recursive: true });
    await writeFile(join(dir, url), bytes);
    const sha256 = createHash("sha256").update(bytes).digest("hex");
    artifacts.push({ target, type, url, sha256, bytes: bytes.length });
  };
  const release = (target) => join(plugins, "target", target, "release");
  if (wasm)
    await place(
      join(release("wasm32-unknown-unknown"), `${library}.wasm`),
      `${library}.wasm`,
      "wasm32",
      "wasm",
    );
  if (native) {
    const file = triple.includes("windows")
      ? `${library}.dll`
      : triple.includes("apple")
        ? `lib${library}.dylib`
        : `lib${library}.so`;
    await place(
      join(release(triple), file),
      `${triple}/${file}`,
      triple,
      "cdylib",
    );
  }
  const descriptor = JSON.parse(
    await readFile(join(plugins, "fixtures", name, "descriptor.json"), "utf8"),
  );
  const slots = await readFile(
    join(plugins, "fixtures", name, "resources.json"),
    "utf8",
  ).then(JSON.parse, () => undefined);
  const text = JSON.stringify({
    id: descriptor.id,
    version: "0.1.0",
    abi: "alpha2-plugin/1",
    kind: "extractor",
    artifacts,
    ...(slots === undefined ? {} : { resources: slots }),
    descriptor,
  });
  await writeFile(join(dir, "manifest.json"), text);
  return {
    dir,
    manifest: join(dir, "manifest.json"),
    sha256: createHash("sha256").update(text).digest("hex"),
  };
}

function pluginRequest(plugin, extractor, outputs, params = {}) {
  return {
    project,
    assets: { v: join(root, "tests", "corpus", "h264-bt709-copy.mp4") },
    plugin,
    request: {
      extractor,
      version: 1,
      params,
      input: {
        from: "source",
        source_id: "v",
        range: { start: 0, end: 40_000 },
        size: 96,
      },
      outputs,
    },
  };
}

test("a plugin the host runs records the bytes the Rust reader pins", async () => {
  const pinned = JSON.parse(
    await readFile(
      join(root, "tests", "golden", "extract-recipe.json"),
      "utf8",
    ),
  );
  const { dir, manifest, sha256 } = await pluginDir("digest");
  try {
    const progress = [];
    const result = await client.extract({
      ...pluginRequest({ manifest, sha256 }, "alpha2.test.digest", {
        digest: "frames",
      }),
      onProgress: (step) => progress.push(step),
    });
    const [set] = result.dataSets;
    const found = set.times.map((time, index) => {
      const hi = Math.round(set.values[index * 2] * 65536);
      const lo = Math.round(set.values[index * 2 + 1] * 65536);
      return [time, (hi * 65536 + lo).toString(16).padStart(8, "0")];
    });
    assert.deepEqual(found, pinned["h264-bt709-copy.mp4"]);
    assert.equal(set.made_by.plugin.version, "0.1.0");
    assert.ok(progress.length > 0);
    await assert.rejects(
      client.extract(
        pluginRequest(
          { manifest, sha256: "0".repeat(64) },
          "alpha2.test.digest",
          { digest: "frames" },
        ),
      ),
      (error) => error.code === "PLUGIN_HASH_MISMATCH",
    );
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("a native plugin that crashes fails its request; only in the host process does it end the host", async () => {
  const { dir, manifest, sha256 } = await pluginDir("scripted", {
    wasm: false,
    native: true,
  });
  const outputs = {
    track: "t",
    levels: "l",
    correction: "c",
    words: "w",
    quiet: "q",
  };
  const native = new Alpha2NodeClient({
    hostPath,
    plugins: { mode: "native", isolation: "in-process" },
  });
  const isolated = new Alpha2NodeClient({
    hostPath,
    plugins: { mode: "native" },
  });
  try {
    await assert.rejects(
      isolated.extract(
        pluginRequest(
          { manifest, sha256, allowNative: true },
          "alpha2.test.scripted",
          outputs,
          { abort_at: 1 },
        ),
      ),
      (error) => {
        assert.equal(error.code, "PLUGIN_TRAPPED");
        if (process.platform !== "win32")
          assert.equal(error.details?.signal, 6);
        return true;
      },
    );
    const survived = await isolated.extract(
      pluginRequest(
        { manifest, sha256, allowNative: true },
        "alpha2.test.scripted",
        outputs,
      ),
    );
    assert.equal(survived.dataSets.length, 5);
    await assert.rejects(
      client.extract(
        pluginRequest({ manifest, sha256 }, "alpha2.test.scripted", outputs, {
          abort_at: 1,
        }),
      ),
      (error) => error.code === "PLUGIN_NATIVE_REFUSED",
    );
    // A host started without `--plugins native` ignores `allowNative`.
    await assert.rejects(
      client.extract(
        pluginRequest(
          { manifest, sha256, allowNative: true },
          "alpha2.test.scripted",
          outputs,
        ),
      ),
      (error) => error.code === "PLUGIN_NATIVE_REFUSED",
    );
    await assert.rejects(
      native.extract(
        pluginRequest(
          { manifest, sha256, allowNative: true },
          "alpha2.test.scripted",
          outputs,
          { abort_at: 1 },
        ),
      ),
      (error) => {
        assert.equal(error.code, "HOST_EXITED");
        return true;
      },
    );
    const after = await native.extract(
      pluginRequest(
        { manifest, sha256, allowNative: true },
        "alpha2.test.scripted",
        outputs,
      ),
    );
    assert.equal(after.dataSets.length, 5);
  } finally {
    await native.close();
    await isolated.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test("a plugin reads the files a request supplies within the resource root", async () => {
  const { dir, manifest, sha256 } = await pluginDir("supplied", {
    native: true,
  });
  const models = join(dir, "models");
  await mkdir(join(models, "extra"), { recursive: true });
  await writeFile(join(models, "model.bin"), Buffer.from([1, 2, 119]));
  await writeFile(join(models, "extra", "a.txt"), "alpha");
  const allowed = new Alpha2NodeClient({
    hostPath,
    plugins: { mode: "native", resourceRoot: models },
  });
  const supplied = {
    model: join(models, "model.bin"),
    extra: join(models, "extra"),
  };
  const run = (target, plugin) =>
    target.extract(
      pluginRequest(
        { manifest, sha256, ...plugin },
        "alpha2.test.supplied",
        { files: "files" },
        { read: ["model", "extra/a.txt"] },
      ),
    );
  try {
    for (const allowNative of [false, true]) {
      const { dataSets } = await run(allowed, { supplied, allowNative });
      assert.deepEqual(
        dataSets[0].words.map((word) => word.text),
        ["model 3 010277", "extra/a.txt 5 616c706861"],
      );
      assert.equal(dataSets[0].made_by.plugin.manifest_sha256, sha256);
      assert.deepEqual(
        dataSets[0].made_by.plugin.supplied.map((file) => file.name),
        ["extra/a.txt", "model"],
      );
    }
    await assert.rejects(run(allowed, {}), (error) => {
      assert.equal(error.code, "PLUGIN_RESOURCE_INVALID");
      assert.deepEqual(error.details, { slot: "model", reason: "missing" });
      return true;
    });
    await assert.rejects(
      run(client, { supplied }),
      (error) =>
        error.code === "PLUGIN_REFUSED" &&
        error.message.includes("--plugin-resource-root"),
    );
  } finally {
    await allowed.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test("conform reports data made from other media once its source is relinked", async () => {
  const video = join(root, "tests", "corpus", "h264-bt709-copy.mp4");
  const placed = {
    ...project,
    timeline: {
      tracks: [
        {
          id: "t",
          name: "T",
          kind: "video",
          clips: [
            {
              id: "c",
              source_id: "v",
              timeline_range: { start: 0, end: 40_000 },
              source_range: { start: 0, end: 40_000 },
            },
          ],
        },
      ],
    },
  };
  const first = await client.conform({ project: placed, assets: { v: video } });
  const withData = {
    ...placed,
    data: [
      {
        id: "track",
        anchor: { type: "source", source_id: "v" },
        kind: "intervals",
        intervals: [],
        made_from: first.catalog.assets[0].original.fingerprint,
      },
    ],
  };
  const same = await client.conform({
    project: withData,
    assets: { v: video },
  });
  assert.deepEqual(same.report.stale_data, []);
  const relinked = await client.conform({
    project: withData,
    assets: { v: join(root, "tests", "corpus", "rotation.mp4") },
  });
  assert.deepEqual(
    relinked.report.stale_data.map((entry) => entry.data),
    ["track"],
  );
});

test("an extraction reads a project with a LUT: a source input without it, the timeline with it", async () => {
  const effects = JSON.parse(
    await readFile(
      join(root, "tests", "cases", "host-parity-effects-project.json"),
      "utf8",
    ),
  );
  const corpus = (name) => join(root, "tests", "corpus", name);
  const assets = {
    video: corpus("parity-rgb.mov"),
    still: corpus("parity-image.png"),
  };
  const input = {
    media: "video",
    from: ["source", "timeline"],
    format: "rgba8",
    max_size: 960,
  };
  const run = (from, effectAssets) =>
    client.extract(
      {
        project: effects,
        assets,
        effectAssets,
        request: {
          extractor: "alpha2.test.digest",
          version: 1,
          // The range covers the LUT clip.
          input: {
            from,
            ...(from === "source" ? { source_id: "video" } : {}),
            range: { start: 20_000, end: 30_000 },
            size: 32,
          },
          outputs: { digest: "frames" },
        },
      },
      digest(input),
    );
  const source = await run("source");
  assert.ok(source.dataSets[0].times.length > 0);
  const dir = await mkdtemp(join(tmpdir(), "alpha2-lut-"));
  try {
    const identity = join(dir, "identity.cube");
    await writeFile(
      identity,
      "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
    );
    const graded = await run("timeline", {
      grade: corpus("parity-grade.cube"),
    });
    const plain = await run("timeline", { grade: identity });
    assert.equal(
      graded.dataSets[0].times.length,
      plain.dataSets[0].times.length,
    );
    assert.notDeepEqual(graded.dataSets[0].values, plain.dataSets[0].values);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
