// An interrupted harness ends the browsers it started and removes their profiles.
import assert from "node:assert/strict";
import test from "node:test";

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { cleanUp } from "./browser-cleanup.mjs";

function isRunning(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** A harness stand-in: tracks a detached child that started a grandchild, then waits. */
const harness = `
import { spawn } from "node:child_process";
import { track } from ${JSON.stringify(pathToFileURL(join(import.meta.dirname, "browser-cleanup.mjs")).href)};
const profile = process.argv[2];
const child = spawn(process.execPath, ["-e", \`
  const { spawn } = require("node:child_process");
  const grandchild = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], { stdio: "ignore" });
  process.stdout.write(String(grandchild.pid) + "\\\\n");
  setInterval(() => {}, 1000);
\`], { stdio: ["ignore", "pipe", "ignore"], detached: true });
track({ child, group: true, profile });
child.stdout.setEncoding("utf8");
child.stdout.once("data", (line) => {
  process.stdout.write(JSON.stringify({ child: child.pid, grandchild: Number(line.trim()) }) + "\\n");
});
setInterval(() => {}, 1000);
`;

test(
  "SIGTERM ends a tracked browser group and removes its profile",
  { skip: process.platform === "win32" && "POSIX signals and process groups" },
  async () => {
    const directory = await mkdtemp(join(tmpdir(), "alpha2-cleanup-"));
    try {
      const script = join(directory, "harness.mjs");
      const profile = join(directory, "profile");
      await mkdir(profile);
      await writeFile(join(profile, "Local State"), "{}");
      await writeFile(script, harness);
      const wrapper = spawn(process.execPath, [script, profile], {
        stdio: ["ignore", "pipe", "inherit"],
      });
      wrapper.stdout.setEncoding("utf8");
      const pids = await new Promise((resolvePids, reject) => {
        wrapper.stdout.once("data", (line) => resolvePids(JSON.parse(line)));
        wrapper.once("exit", (code) =>
          reject(new Error(`the harness exited ${code} early`)),
        );
      });
      assert.ok(isRunning(pids.child) && isRunning(pids.grandchild));
      const exited = new Promise((resolveExit) =>
        wrapper.once("exit", (code, signal) => resolveExit({ code, signal })),
      );
      wrapper.kill("SIGTERM");
      assert.deepEqual(await exited, { code: 143, signal: null });
      // SIGKILL is delivered at once; give the kernel a moment to reap.
      const deadline = Date.now() + 5_000;
      while (
        (isRunning(pids.child) || isRunning(pids.grandchild)) &&
        Date.now() < deadline
      )
        await new Promise((resolveTimer) => setTimeout(resolveTimer, 50));
      assert.equal(isRunning(pids.child), false, "the browser");
      assert.equal(isRunning(pids.grandchild), false, "its child process");
      assert.equal(existsSync(profile), false, "the profile");
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test("cleanup on Windows ends each running process tree with taskkill", async () => {
  const directory = await mkdtemp(join(tmpdir(), "alpha2-cleanup-"));
  try {
    const calls = [];
    const killed = [];
    cleanUp(
      [
        {
          child: { pid: 4242, exitCode: null, signalCode: null },
          group: false,
          profile: directory,
        },
        // Exited: its pid may name another process by now.
        { child: { pid: 4343, exitCode: 0, signalCode: null } },
      ],
      {
        platform: "win32",
        spawnSyncProcess: (command, args) => calls.push([command, ...args]),
        kill: (pid, signal) => killed.push([pid, signal]),
      },
    );
    assert.deepEqual(calls, [["taskkill", "/T", "/F", "/PID", "4242"]]);
    assert.deepEqual(killed, []);
    assert.equal(existsSync(directory), false);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("cleanup on POSIX kills a group through its negative pid and a lone process by its pid", () => {
  const killed = [];
  cleanUp(
    [
      { child: { pid: 10, exitCode: null, signalCode: null }, group: true },
      { child: { pid: 11, exitCode: null, signalCode: null }, group: false },
      {
        child: { pid: 12, exitCode: null, signalCode: "SIGTERM" },
        group: true,
      },
    ],
    {
      platform: "linux",
      spawnSyncProcess: () => assert.fail("no taskkill on POSIX"),
      kill: (pid, signal) => killed.push([pid, signal]),
    },
  );
  assert.deepEqual(killed, [
    [-10, "SIGKILL"],
    [11, "SIGKILL"],
  ]);
});
