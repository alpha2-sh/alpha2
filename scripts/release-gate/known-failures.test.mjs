import assert from "node:assert/strict";
import { test } from "node:test";

import { excuse, load, validate, workspaceVersion } from "./known-failures.mjs";
import { buildReport, failuresIn, parseSummary } from "./report.mjs";

const entry = {
  test: "tests::compositing::plus_lighter_adds_a_glow",
  machines: ["win"],
  phase: "adapters",
  reason: "a reason",
  owner: "someone",
  added: "2026-10-31",
  until: "0.6.0",
};

test("the tracked list is valid for the workspace version", () => {
  const { problems } = load();
  assert.deepEqual(problems, []);
  assert.match(workspaceVersion(), /^\d+\.\d+\.\d+$/);
});

test("an expired entry fails the gate", () => {
  assert.deepEqual(
    validate({ release: "0.5.0", entries: [entry] }, "0.5.0"),
    [],
  );
  for (const until of ["0.5.0", "0.4.9"])
    assert.deepEqual(
      validate({ release: "0.5.0", entries: [{ ...entry, until }] }, "0.5.0"),
      [
        `entry 0 (${entry.test}): expired (until ${until}, the workspace is 0.5.0)`,
      ],
    );
  const problems = validate(
    {
      release: "0.5.0",
      entries: [{ ...entry, until: "next", owner: undefined, extra: 1 }],
    },
    "0.5.0",
  );
  assert.ok(
    problems.some((problem) => problem.includes("unknown key extra")),
    problems,
  );
  assert.ok(
    problems.some((problem) => problem.includes("until next is not a semver")),
    problems,
  );
  assert.ok(
    problems.some((problem) =>
      problem.includes("owner is not a non-empty string"),
    ),
  );
});

test("a failure listed for another machine is not excused", () => {
  const failure = { test: entry.test, phase: "adapters", step: "intel-render" };
  assert.equal(excuse([entry], "win", failure), entry);
  assert.equal(excuse([entry], "lin", failure), undefined);
  assert.equal(excuse([entry], "win", { ...failure, phase: "p1" }), undefined);
  const stepped = { ...entry, step: "intel-render" };
  assert.equal(excuse([stepped], "win", failure), stepped);
  assert.equal(
    excuse([stepped], "win", { ...failure, step: "nvidia-render" }),
    undefined,
  );
});

test("step logs name libtest, node, pytest and harness failures, build errors and crashes", () => {
  const log = [
    "test tests::a ... ok",
    "test tests::b ... FAILED",
    "not ok 3 - a node test",
    "  \u2716 a spec test (12.5ms)",
    "FAILED tests/test_e2e.py::test_export - AssertionError",
    "FAIL audio-clock-tracking: timed out",
    "FAIL corrupt frames",
    "error: could not compile `alpha2-os` (lib test) due to 1 previous error",
    "  process didn't exit successfully: `/t/deps/alpha2_render-0a1b --show-output` (exit status: 101)",
    "  process didn't exit successfully: `/t/deps/alpha2_host-9f --show-output` (signal: 11, SIGSEGV: invalid memory reference)",
    "test tests::b ... FAILED",
  ].join("\n");
  assert.deepEqual(failuresIn(log), [
    "tests::b",
    "a node test",
    "a spec test",
    "tests/test_e2e.py::test_export",
    "audio-clock-tracking",
    "corrupt frames",
    "build alpha2-os",
    "crash alpha2_host-9f (signal: 11, SIGSEGV: invalid memory reference)",
  ]);
});

test("a run is blocked by any unexcused failure and passes when every failure is known", () => {
  const steps = parseSummary(
    "adapters/intel-render rc=101 30s load=1.0\nadapters/nvidia-render rc=0 28s load=1.1\n",
  );
  const logs = new Map([
    ["adapters/intel-render", `test ${entry.test} ... FAILED\n`],
  ]);
  const knownFailures = { document: { entries: [entry] }, problems: [] };
  const passed = buildReport({ machine: "win", steps, logs, knownFailures });
  assert.equal(passed.verdict, "pass");
  assert.equal(passed.steps[0].failures[0].excused, true);
  const elsewhere = buildReport({ machine: "lin", steps, logs, knownFailures });
  assert.equal(elsewhere.verdict, "blocked");
  const unnamed = buildReport({
    machine: "win",
    steps: parseSummary("p1/build rc=2 3s load=n/a"),
    logs: new Map(),
    knownFailures,
  });
  assert.equal(unnamed.verdict, "blocked");
  assert.deepEqual(unnamed.steps[0].failures, [
    { test: "build", excused: false },
  ]);
});
