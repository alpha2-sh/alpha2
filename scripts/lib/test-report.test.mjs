import test from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  collectRecords,
  compareVersions,
  expired,
  judge,
  markdownSummary,
  parseJunit,
  parseTestLog,
  validateLedger,
  workspaceVersion,
} from "./test-report.mjs";

const where = { lane: "rust-tests", step: "rust" };
/** @param {import("./test-report.mjs").TestRecord[]} records */
const brief = (records) =>
  records.map(({ suite, test, outcome }) => `${outcome} ${suite} ${test}`);

const cargoOutput = `     Running unittests src/lib.rs (target/debug/deps/lt_sample-b9fc001a1564a972)

running 3 tests
test tests::fails ... FAILED
test tests::ignored ... ignored, needs gpu
test tests::passes ... ok

failures:

---- tests::fails stdout ----
    indented captured line
ok not-a-harness-case

thread 'tests::fails' (2371438) panicked at src/lib.rs:4:66:
assertion \`left == right\` failed
note: run with \`RUST_BACKTRACE=1\` environment variable to display a backtrace


failures:
    tests::fails

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass \`--lib\`
     Running tests/crash.rs (target/debug/deps/crash-6843d79565033900)

running 2 tests
test before ... ok
test zz_aborts ... error: test failed, to rerun pass \`--test crash\`

Caused by:
  process didn't exit successfully: \`/work/target/debug/deps/crash-6843d79565033900 --test-threads=1\` (signal: 6, SIGABRT: process abort signal)
   Doc-tests lt_sample

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: 2 targets failed:
    \`--lib\`
    \`--test crash\`
`;

test("cargo test output gives each test its binary and outcome, and names the test a crash ended", () => {
  assert.deepEqual(brief(parseTestLog(cargoOutput, where)), [
    "failed lt_sample tests::fails",
    "skipped lt_sample tests::ignored",
    "passed lt_sample tests::passes",
    "passed crash before",
    "crashed crash zz_aborts",
  ]);
});

test("a failure only the final failures list names is recorded once", () => {
  const output = `running 2 tests
test a ... FAILED
test b ... ok

failures:
    a
    hidden

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
`;
  assert.deepEqual(brief(parseTestLog(output, where)), [
    "failed rust a",
    "passed rust b",
    "failed rust hidden",
  ]);
});

test("a single-threaded run whose status lands on a later line still records it", () => {
  const output = `running 1 test
test slow::case ...
[driver] some uncaptured stderr
ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.00s
`;
  assert.deepEqual(brief(parseTestLog(output, where)), [
    "passed rust slow::case",
  ]);
});

test("a binary cut off by the job timeout reports its long-running test as hung", () => {
  const output = `     Running unittests src/lib.rs (target/debug/deps/alpha2_host-0123456789abcdef)

running 4 tests
test fast ... ok
test stuck ...
test stuck has been running for over 60 seconds
`;
  assert.deepEqual(brief(parseTestLog(output, where)), [
    "passed alpha2_host fast",
    "hung alpha2_host stuck",
  ]);
});

test("a watchdog line marks the tests it names as hung", () => {
  const output = `running 2 tests
test done ... ok
test-watchdog: 900s elapsed; still running: a::stuck, b::other
`;
  assert.deepEqual(brief(parseTestLog(output, where)), [
    "passed rust done",
    "crashed rust (test binary exited without a result)",
    "hung rust a::stuck",
    "hung rust b::other",
  ]);
});

test("a compile error is a build failure", () => {
  const output = `error[E0425]: cannot find value \`x\` in this scope
error: could not compile \`alpha2-render\` (lib test) due to 1 previous error
`;
  assert.deepEqual(brief(parseTestLog(output, where)), [
    "failed build compile alpha2-render",
  ]);
});

test("harness lines, emulator binaries, XCTest and instrumentation", () => {
  const output = `browser-run: browser=chrome version=1
ok export-roundtrip {"frames":3}
FAIL audio-clock-tracking: drifted 40 ms
skip webcodecs-hevc: [webcodecs-video] no encoder
ok alpha2_render-0123456789abcdef on lavapipe (vulkan) (3 VUID-x)
FAIL alpha2_engine-0123456789abcdef on lavapipe (vulkan): exited 101: test result: FAILED.
     Running target/mobile-tests/alpha2_mobile-0123456789abcdef

running 1 test
test device_tests::plays ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s

Test Case '-[Alpha2Tests.PlayerTests testEnds]' passed (0.120 seconds).
Test Case '-[Alpha2Tests.PlayerTests testExports]' failed (2.500 seconds).
There was 1 failure:
1) exportsOnce(sh.alpha2.test.ExportTest)
java.lang.AssertionError
`;
  assert.deepEqual(brief(parseTestLog(output, { lane: "x", step: "cases" })), [
    "passed cases export-roundtrip",
    "failed cases audio-clock-tracking",
    "skipped cases webcodecs-hevc",
    "passed cases alpha2_render",
    "failed cases alpha2_engine",
    "passed alpha2_mobile device_tests::plays",
    "passed Alpha2Tests.PlayerTests testEnds",
    "failed Alpha2Tests.PlayerTests testExports",
    "failed sh.alpha2.test.ExportTest exportsOnce",
  ]);
});

test("JUnit from Node's test runner names tests by their describe blocks", () => {
  const xml = `<?xml version="1.0" encoding="utf-8"?>
<testsuites>
	<testsuite name="group" time="0.002" tests="3" failures="1" skipped="1">
		<testcase name="passes" time="0.000571" classname="test"/>
		<testcase name="fails &lt;x>" time="0.000632" classname="test" failure="1 == 2">
			<failure type="testCodeFailure" message="1 == 2">
Error: 1 == 2 at TestContext.&lt;anonymous>
			</failure>
		</testcase>
		<testcase name="skipped" time="0.000103" classname="test">
			<skipped type="skipped" message="no gpu"/>
		</testcase>
	</testsuite>
	<testcase name="top" time="0.000909" classname="test"/>
</testsuites>`;
  const records = parseJunit(xml, { lane: "js", step: "js", suite: "js" });
  assert.deepEqual(brief(records), [
    "passed js group > passes",
    "failed js group > fails <x>",
    "skipped js group > skipped",
    "passed js top",
  ]);
  assert.equal(records[0].seconds, 0.000571);
});

test("JUnit from pytest names tests by module and function", () => {
  const xml = `<?xml version="1.0" encoding="utf-8"?><testsuites><testsuite name="pytest" errors="1" failures="0" skipped="0" tests="2"><testcase classname="tests.test_client" name="test_export" time="1.5" /><testcase classname="tests.test_client" name="test_probe[3.11]" time="0.1"><error message="fixture failed">boom</error></testcase></testsuite></testsuites>`;
  assert.deepEqual(
    brief(parseJunit(xml, { lane: "py", step: "pytest", suite: "pytest" })),
    [
      "passed pytest tests.test_client::test_export",
      "failed pytest tests.test_client::test_probe[3.11]",
    ],
  );
});

test("records come from every log and JUnit file, by step", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "test-report-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  mkdirSync(join(dir, "logs"));
  mkdirSync(join(dir, "results"));
  writeFileSync(join(dir, "logs", "cases.log"), "FAIL one: why\n");
  writeFileSync(
    join(dir, "logs", "validation.alpha2_render-0123456789abcdef.log"),
    "running 1 test\ntest gpu::draws ... ok\n\ntest result: ok. 1 passed; 0 failed\n",
  );
  writeFileSync(
    join(dir, "results", "js.editor.xml"),
    '<testsuites><testcase name="t" classname="test"/></testsuites>',
  );
  const records = collectRecords({
    lane: "browser",
    logsDir: join(dir, "logs"),
    resultsDir: join(dir, "results"),
  });
  assert.deepEqual(
    records.map(({ step, suite, test }) => `${step} ${suite} ${test}`),
    [
      "cases cases one",
      "validation alpha2_render gpu::draws",
      "js js.editor t",
    ],
  );
});

const entry = (overrides = {}) => ({
  lane: "mobile-test-android-29",
  test: "device_tests::exports_twice",
  reason: "emulator encoder keyframes vary",
  added: "2026-11-17",
  until: "0.7.0",
  ...overrides,
});

test("the ledger is validated as a whole", () => {
  assert.deepEqual(validateLedger({ failures: [entry()] }), [entry()]);
  assert.throws(() => validateLedger([]), /"failures" array/);
  assert.throws(
    () =>
      validateLedger({
        failures: [
          entry({ until: "next", extra: 1 }),
          entry({ test: "*" }),
          entry({ maxFailing: 2 }),
          entry({ added: "17.11.2026" }),
        ],
      }),
    (/** @type {Error} */ error) => {
      assert.match(error.message, /failures\[0\] has unknown key extra/);
      assert.match(error.message, /failures\[0\]\.until is not a release/);
      assert.match(error.message, /failures\[1\]\.maxFailing must be/);
      assert.match(error.message, /failures\[2\]\.maxFailing is only for/);
      assert.match(error.message, /failures\[3\]\.added is not YYYY-MM-DD/);
      return true;
    },
  );
});

test("an entry lives through its release's development and expires at that release", () => {
  assert.equal(compareVersions("0.10.0", "0.9.9"), 1);
  assert.equal(compareVersions("0.6.0", "0.6.0"), 0);
  const e = entry({ until: "0.6.0" });
  assert.equal(expired(e, { version: "0.6.0", release: false }), false);
  assert.equal(expired(e, { version: "0.6.0", release: true }), true);
  assert.equal(expired(e, { version: "0.6.1", release: false }), true);
  assert.equal(expired(e, { version: "0.5.3", release: true }), false);
});

/** @param {string} test @param {import("./test-report.mjs").Outcome} [outcome] @param {string} [step] */
const record = (test, outcome = "failed", step = "device") => ({
  lane: "mobile-test-android-29",
  step,
  suite: "alpha2_mobile",
  test,
  outcome,
  seconds: null,
});
const at = { lane: "mobile-test-android-29", version: "0.6.0", release: false };

test("an observe lane passes when the ledger explains every failure", () => {
  const verdict = judge(
    [record("device_tests::exports_twice"), record("ok", "passed")],
    { device: { outcome: "failure", conclusion: "success" } },
    [entry()],
    at,
  );
  assert.equal(verdict.unexplained.length, 0);
  assert.equal(verdict.known.length, 1);
  assert.equal(verdict.expired.length, 0);
});

test("a failure the ledger does not name, an expired entry or a silent failed step fail the lane", () => {
  const ledger = [entry(), entry({ test: "old", until: "0.5.3" })];
  const verdict = judge(
    [record("device_tests::exports_twice"), record("new_failure")],
    {
      device: { outcome: "failure", conclusion: "success" },
      boot: { outcome: "failure", conclusion: "success" },
      setup: { outcome: "success", conclusion: "success" },
    },
    ledger,
    at,
  );
  assert.deepEqual(
    verdict.unexplained.map(({ test }) => test),
    ["new_failure", "step boot failed without a failed test"],
  );
  assert.deepEqual(
    verdict.expired.map(({ test }) => test),
    ["old"],
  );
});

test("a wildcard entry explains at most maxFailing failures, and other lanes' entries none", () => {
  const wildcard = entry({ test: "*", maxFailing: 2 });
  const two = [record("a"), record("b")];
  assert.equal(judge(two, {}, [wildcard], at).unexplained.length, 0);
  assert.equal(
    judge([...two, record("c")], {}, [wildcard], at).unexplained.length,
    3,
  );
  assert.equal(
    judge(two, {}, [entry({ test: "*", maxFailing: 2, lane: "other" })], at)
      .unexplained.length,
    2,
  );
});

test("the summary lists failures, and for an observe lane the ledger's view", () => {
  const records = [record("a"), record("b", "passed"), record("c", "hung")];
  const gate = markdownSummary(records, { lane: "rust-tests" });
  assert.match(gate, /### Tests: rust-tests/);
  assert.match(gate, /1 failed, 1 passed, 1 hung/);
  assert.match(gate, /\| hung \| alpha2_mobile \| c \|/);
  const verdict = judge(records, {}, [entry({ test: "a" })], at);
  const observe = markdownSummary(records, { lane: "x", verdict });
  assert.match(observe, /Not in the known-failure ledger\*\* \(1\)/);
  assert.match(observe, /\| failed \| alpha2_mobile \| a \| emulator encoder/);
});

test("the workspace version is read from Cargo.toml", () => {
  assert.match(workspaceVersion(), /^\d+\.\d+\.\d+$/);
});
