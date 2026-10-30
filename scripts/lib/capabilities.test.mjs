import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  CAPABILITIES,
  readSkipLog,
  recordSkip,
  required,
  requiredCapabilities,
  summarizeSkipLog,
} from "./capabilities.mjs";

test("ALPHA2_REQUIRE is a trimmed comma list of known names", () => {
  assert.deepEqual(
    [...requiredCapabilities({ ALPHA2_REQUIRE: " wasm , ,browser," })],
    ["wasm", "browser"],
  );
  assert.deepEqual([...requiredCapabilities({})], []);
  assert.equal(required("wasm", { ALPHA2_REQUIRE: "wasm" }), true);
  assert.equal(required("browser", { ALPHA2_REQUIRE: "wasm" }), false);
  for (const name of CAPABILITIES.keys()) assert.match(name, /^[a-z0-9-]+$/);
});

test("an unknown name is refused wherever it appears", () => {
  assert.throws(
    () => requiredCapabilities({ ALPHA2_REQUIRE: "wasm,wsam" }),
    /unknown capability "wsam"/,
  );
  assert.throws(() => required("wsam", {}), /unknown capability "wsam"/);
  assert.throws(
    () => recordSkip({ suite: "s", capability: "wsam", reason: "r" }, {}),
    /unknown capability "wsam"/,
  );
});

test("a required capability's skip throws instead of recording", () => {
  const dir = mkdtempSync(join(tmpdir(), "alpha2-capabilities-"));
  const log = join(dir, "skips.jsonl");
  try {
    assert.throws(
      () =>
        recordSkip(
          { suite: "s", capability: "wasm", reason: "not built" },
          { ALPHA2_REQUIRE: "wasm", ALPHA2_SKIP_LOG: log },
        ),
      /^Error: wasm is required \(ALPHA2_REQUIRE\): not built$/,
    );
    assert.equal(
      recordSkip(
        { suite: "s", test: "t", capability: "wasm", reason: "not built" },
        { ALPHA2_SKIP_LOG: log },
      ),
      "not built",
    );
    const [record] = readSkipLog(log);
    assert.deepEqual(
      { ...record, pid: 0 },
      {
        lang: "js",
        suite: "s",
        test: "t",
        capability: "wasm",
        reason: "not built",
        pid: 0,
      },
    );
    assert.deepEqual(summarizeSkipLog(readSkipLog(log)), [
      { capability: "wasm", count: 1, suites: [{ suite: "s", count: 1 }] },
    ]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a relative skip log path is refused", () => {
  assert.throws(
    () =>
      recordSkip(
        { suite: "s", capability: "wasm", reason: "r" },
        { ALPHA2_SKIP_LOG: "skips.jsonl" },
      ),
    /must be absolute/,
  );
});

test("records appended by many processes are whole lines", async () => {
  const dir = mkdtempSync(join(tmpdir(), "alpha2-capabilities-"));
  const log = join(dir, "skips.jsonl");
  const module = new URL("./capabilities.mjs", import.meta.url).href;
  const script = `
    const { recordSkip } = await import(${JSON.stringify(module)});
    for (let i = 0; i < 100; i++)
      recordSkip({ suite: "s", test: String(i), capability: "browser", reason: "x".repeat(200) + i });
  `;
  try {
    await Promise.all(
      Array.from(
        { length: 8 },
        () =>
          new Promise((resolveChild, reject) => {
            const child = spawn(
              process.execPath,
              ["--input-type=module", "-e", script],
              {
                env: {
                  ...process.env,
                  ALPHA2_SKIP_LOG: log,
                  ALPHA2_REQUIRE: "",
                },
                stdio: "ignore",
              },
            );
            child.once("error", reject);
            child.once("exit", (code) =>
              code === 0
                ? resolveChild()
                : reject(new Error(`child exited ${code}`)),
            );
          }),
      ),
    );
    const lines = readFileSync(log, "utf8").trimEnd().split("\n");
    assert.equal(lines.length, 800);
    for (const line of lines)
      assert.equal(JSON.parse(line).capability, "browser");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
