// Ends tracked browsers and removes their profiles when a harness is interrupted or exits early.

import { spawnSync } from "node:child_process";
import { rmSync } from "node:fs";

const SIGNAL_STATUS = Object.freeze({ SIGINT: 130, SIGTERM: 143, SIGHUP: 129 });

const tracked = new Set();
let installed = false;

/**
 * The first call installs signal and exit handlers that clean up every tracked entry.
 *
 * @param {{ child?: import("node:child_process").ChildProcess, group?: boolean, profile?: string }} entry
 * @returns {typeof entry}
 */
export function track(entry) {
  tracked.add(entry);
  if (!installed) {
    installed = true;
    for (const [signal, status] of Object.entries(SIGNAL_STATUS))
      process.once(signal, () => {
        cleanUp([...tracked]);
        tracked.clear();
        process.exit(status);
      });
    process.once("exit", () => {
      cleanUp([...tracked]);
      tracked.clear();
    });
  }
  return entry;
}

export function untrack(entry) {
  tracked.delete(entry);
}

/**
 * Synchronous so it can run in an exit handler. An exited process is skipped because its pid may
 * be reused.
 *
 * @param {Iterable<{ child?: import("node:child_process").ChildProcess, group?: boolean, profile?: string }>} entries
 * @param {{ platform?: string, spawnSyncProcess?: typeof spawnSync,
 *   kill?: (pid: number, signal: string) => void }} [options]
 */
export function cleanUp(
  entries,
  {
    platform = process.platform,
    spawnSyncProcess = spawnSync,
    kill = (pid, signal) => process.kill(pid, signal),
  } = {},
) {
  for (const { child, group, profile } of entries) {
    const running =
      child !== undefined &&
      child.pid !== undefined &&
      child.exitCode === null &&
      child.signalCode === null;
    if (running) {
      if (platform === "win32")
        spawnSyncProcess("taskkill", ["/T", "/F", "/PID", String(child.pid)], {
          stdio: "ignore",
          windowsHide: true,
        });
      else {
        try {
          kill(group ? -child.pid : child.pid, "SIGKILL");
        } catch {
          // Already gone.
        }
      }
    }
    if (profile !== undefined)
      try {
        rmSync(profile, { recursive: true, force: true });
      } catch {
        // A still-dying process may hold a file.
      }
  }
}
