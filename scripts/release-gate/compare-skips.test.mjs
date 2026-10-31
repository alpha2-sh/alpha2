import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { main } from "./compare-skips.mjs";

const record = (test, capability, reason = "absent") =>
  JSON.stringify({
    lang: "rust",
    suite: "s",
    test,
    capability,
    reason,
    pid: 1,
  });

test("a new skip of a declared capability fails and removed skips are listed", () => {
  const directory = mkdtempSync(join(tmpdir(), "alpha2-compare-skips-"));
  try {
    const previous = join(directory, "previous.jsonl");
    const current = join(directory, "current.jsonl");
    writeFileSync(
      previous,
      `${record("a", "nvenc")}\n${record("b", "metal")}\n`,
    );
    writeFileSync(
      current,
      `${record("a", "nvenc")}\n${record("c", "vulkan-video")}\n`,
    );
    const lines = [];
    assert.equal(
      main([previous, current], (line) => lines.push(line), {}),
      0,
    );
    assert.deepEqual(lines, [
      "added [vulkan-video] s :: c :: absent",
      "removed [metal] s :: b :: absent",
      "1 added, 1 removed",
    ]);
    lines.length = 0;
    const declared = { ALPHA2_REQUIRE: "vulkan-video" };
    assert.equal(
      main([previous, current], (line) => lines.push(line), declared),
      1,
    );
    assert.equal(lines[0], "FAIL added [vulkan-video] s :: c :: absent");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
