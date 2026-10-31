import assert from "node:assert/strict";
import { isAbsolute, join } from "node:path";
import { test } from "node:test";

import { cargoTargetDirectory, executable } from "./cargo-target.mjs";

test("executables carry .exe on Windows and live under the cargo target directory", () => {
  const target = cargoTargetDirectory();
  assert.ok(isAbsolute(target), target);
  assert.equal(
    executable("alpha2-host", "release", { platform: "win32" }),
    join(target, "release", "alpha2-host.exe"),
  );
  assert.equal(
    executable("alpha2-host", "debug", { platform: "linux" }),
    join(target, "debug", "alpha2-host"),
  );
  assert.equal(
    executable("alpha2", "debug", { target: "/elsewhere", platform: "darwin" }),
    join("/elsewhere", "debug", "alpha2"),
  );
});
