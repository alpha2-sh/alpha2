import test from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";

import { collectProvenance, sourceProperties } from "./provenance.mjs";

test("source.properties lines become a map", () => {
  assert.deepEqual(
    sourceProperties(
      "Pkg.Desc = Android NDK\r\nPkg.Revision = 30.0.14904198\n\n",
    ),
    { "Pkg.Desc": "Android NDK", "Pkg.Revision": "30.0.14904198" },
  );
  assert.deepEqual(sourceProperties(null), {});
});

test("an emulator lane records its NDK, emulator and system image revisions", () => {
  const files = {
    [join("/ndk", "source.properties")]: "Pkg.Revision = 30.0.1\n",
    [join("/sdk", "emulator", "source.properties")]: "Pkg.Revision = 36.1.9\n",
    [join(
      "/sdk",
      "system-images",
      "android-29",
      "google_apis",
      "x86_64",
      "source.properties",
    )]: "Pkg.Revision = 12\n",
  };
  const record = collectProvenance({
    env: {
      ImageOS: "ubuntu24",
      ImageVersion: "20261110.1",
      ANDROID_NDK_HOME: "/ndk",
      ANDROID_HOME: "/sdk",
      ALPHA2_ANDROID_API: "29",
      ALPHA2_PARITY_CHROME: "/nonexistent/chrome",
      ALPHA2_EDGE: "/nonexistent/edge",
      ALPHA2_FIREFOX: "/nonexistent/firefox",
    },
    platform: "linux",
    run: (file, args) =>
      file === "rustc"
        ? "rustc 1.99.0"
        : file === "/nonexistent/chrome" && args[0] === "--version"
          ? "Google Chrome for Testing 140.0.1"
          : null,
    read: (path) => files[path] ?? null,
    browserReports: "/nonexistent",
  });
  assert.equal(record.image, "ubuntu24 20261110.1");
  assert.equal(record.rustc, "rustc 1.99.0");
  assert.equal(record.ndk, "30.0.1 (/ndk)");
  assert.equal(record.emulator, "36.1.9");
  assert.equal(
    record["system-image"],
    "android-29 google_apis x86_64 revision 12",
  );
  assert.equal(
    record.chrome,
    "Google Chrome for Testing 140.0.1 (/nonexistent/chrome)",
  );
  assert.equal(record.firefox, undefined);
});
