import assert from "node:assert/strict";
import test from "node:test";
import {
  pluginsSections,
  setCargoVersions,
  workspaceSections,
} from "./cargo-versions.mjs";

test("sibling crates are pinned to the exact version and test crates are left alone", () => {
  const manifest = [
    "[workspace.package]",
    'version = "0.6.0"',
    "",
    "[workspace.dependencies]",
    'serde = { version = "1.0.229", features = ["derive"] }',
    'alpha2-time = { version = "0.6.0", path = "crates/alpha2-time" }',
    'alpha2-host-core = { version = "=0.6.0", path = "bindings/alpha2-host-core", default-features = false }',
    'alpha2-test-support = { path = "tests/test_support" }',
    "",
    "[package]",
    'version = "0.1.0"',
  ].join("\n");
  const { text, lines } = setCargoVersions(
    manifest,
    workspaceSections("0.7.0-rc.1"),
  );
  assert.equal(lines, 3);
  assert.deepEqual(text.split("\n"), [
    "[workspace.package]",
    'version = "0.7.0-rc.1"',
    "",
    "[workspace.dependencies]",
    'serde = { version = "1.0.229", features = ["derive"] }',
    'alpha2-time = { version = "=0.7.0-rc.1", path = "crates/alpha2-time" }',
    'alpha2-host-core = { version = "=0.7.0-rc.1", path = "bindings/alpha2-host-core", default-features = false }',
    'alpha2-test-support = { path = "tests/test_support" }',
    "",
    "[package]",
    'version = "0.1.0"',
  ]);
});

test("the plugins workspace takes the version as written", () => {
  const manifest = '[workspace.package]\nversion = "0.6.0"\nedition = "2024"';
  const { text, lines } = setCargoVersions(manifest, pluginsSections("0.6.1"));
  assert.equal(lines, 1);
  assert.equal(
    text,
    '[workspace.package]\nversion = "0.6.1"\nedition = "2024"',
  );
});
