// The browser a page check runs in when the run names Edge.
import assert from "node:assert/strict";
import test from "node:test";

import { browserForRun } from "./browser-page.mjs";

const only =
  (...paths) =>
  (candidate) =>
    paths.includes(candidate);

test("--browser edge resolves to the CDP runner with Edge's executable", () => {
  const edge =
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe";
  const machine = {
    env: { "PROGRAMFILES(X86)": "C:\\Program Files (x86)" },
    platform: "win32",
    exists: only(edge),
  };
  assert.deepEqual(browserForRun({ argv: ["--browser", "edge"], ...machine }), {
    kind: "chrome",
    name: "edge",
    command: edge,
  });
  assert.deepEqual(
    browserForRun({
      argv: [],
      ...machine,
      env: { ...machine.env, ALPHA2_BROWSER: "edge" },
    }),
    { kind: "chrome", name: "edge", command: edge },
  );
  // Edge never stands in for a missing Chrome, and a missing Edge fails the run that asked for it.
  assert.equal(browserForRun({ argv: [], ...machine }), null);
  assert.throws(
    () =>
      browserForRun({ argv: ["--browser=edge"], ...machine, exists: only() }),
    /no Edge found: set ALPHA2_EDGE to an Edge executable/,
  );
});
