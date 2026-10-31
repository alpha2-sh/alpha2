// The browsers and drivers a harness started, ended when the harness is interrupted or exits early:
// an interrupted run must not leave a browser's process group or its temporary profile behind.
// A runner tracks what it starts and untracks it once it stopped it itself.

import { spawnSync } from "node:child_process";
import { rmSync } from "node:fs";

/** The exit status of a process ended by each signal it handles: 128 plus the signal number. */
const SIGNAL_STATUS = Object.freeze({ SIGINT: 130, SIGTERM: 143, SIGHUP: 129 });

const tracked = new Set();
let installed = false;

/**
 * Tracks a started browser or driver until `untrack`. `child` is its process (absent while only the
 * profile exists), `group` says it leads its own process group (spawned detached on POSIX), and
 * `profile` is a directory to remove. The first call installs handlers for SIGINT, SIGTERM and
 * SIGHUP that end every tracked process, remove every tracked profile and exit with the signal's
 * status, and an exit handler that does the same.
 *
 * @param {{ child?: import("node:child_process").ChildProcess, group?: boolean, profile?: string }} entry
 * @returns {typeof entry} The entry, for `untrack`.
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

/** Stops tracking `entry`; its runner ended it. */
export function untrack(entry) {
  tracked.delete(entry);
}

/**
 * Ends each entry's process and removes its profile, synchronously, so it also runs in an exit
 * handler. POSIX kills a group with SIGKILL to its negative pid; Windows ends the process tree with
 * `taskkill /T /F`. A process that already exited is left alone: its pid may name another process.
 *
 * @param {Iterable<{ child?: import("node:child_process").ChildProcess, group?: boolean, profile?: string }>} entries
 * @param {{ platform?: string, spawnSyncProcess?: typeof spawnSync,
 *   kill?: (pid: number, signal: string) => void }} [options] Stand-ins for tests.
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
        // A process that has not died yet may still hold a file; nothing more can be done here.
      }
  }
}
