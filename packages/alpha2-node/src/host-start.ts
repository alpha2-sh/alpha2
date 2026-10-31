import { closeSync, openSync, readFileSync, readSync, statSync } from "node:fs";

import { INSTALL_MACHINE, LINUX_GLIBC_FLOOR } from "./install.js";

/** How long a started host has to write its first line, by default. */
export const DEFAULT_HOST_START_TIMEOUT_MS = 30_000;

/**
 * `hostStartTimeoutMs` as a client keeps it: the default when unset, `Infinity` to wait forever.
 * Returns the message of an `INVALID_INPUT` for anything else that is not a number above zero.
 */
export function hostStartTimeout(
  value: unknown,
): { readonly ms: number } | { readonly invalid: string } {
  if (value === undefined) return { ms: DEFAULT_HOST_START_TIMEOUT_MS };
  if (typeof value !== "number" || Number.isNaN(value) || value <= 0)
    return {
      invalid:
        "hostStartTimeoutMs must be a number of milliseconds above zero, or Infinity",
    };
  return { ms: value };
}

/**
 * Why a host wrote nothing within `ms` milliseconds, and what may be holding it on this platform.
 */
export function hostStartTimeoutMessage(binary: string, ms: number): string {
  const hint =
    process.platform === "darwin"
      ? "; macOS may be holding it for a Gatekeeper check"
      : process.platform === "win32"
        ? "; an antivirus scan or an App Control policy may be holding it"
        : "";
  return `alpha2-host did not start within ${ms} ms: ${binary}${hint}`;
}

/**
 * The message of a `HOST_BLOCKED`: Windows refused to run alpha2-host or a library it loads, as
 * `what` (a status, or the spawn error) says.
 */
export function hostBlockedMessage(what: string): string {
  return (
    `Windows blocked alpha2-host or a library it loads (${what}): Smart App Control, an App ` +
    "Control (WDAC) or AppLocker policy, or an antivirus refuses unsigned code. Allow the files " +
    "in that policy, or turn Smart App Control off (Windows Security > App & browser control)."
  );
}

/** A Windows exit status as a policy refusal names it: `status 0xC0000428`. */
export function windowsStatus(exitCode: number): string {
  const status = exitCode < 0 ? exitCode + 2 ** 32 : exitCode;
  return `status 0x${status.toString(16).toUpperCase().padStart(8, "0")}`;
}

/**
 * What a failed spawn of `binary` says about the machine, when it says something no retry mends:
 * a Windows policy that refused the executable (`HOST_BLOCKED`; libuv reports such a refusal as
 * `EACCES`, `EPERM` or `UNKNOWN` without the Win32 error), or a Linux executable whose program
 * interpreter is missing (`HOST_LOAD_FAILED`: a glibc build on a musl system fails `ENOENT` though
 * the file is there).
 */
export function spawnRefusal(
  binary: string,
  cause: unknown,
): { readonly code: string; readonly message: string } | undefined {
  const errno = (cause as { code?: unknown } | null)?.code;
  if (typeof errno !== "string" || !isFile(binary)) return undefined;
  if (
    process.platform === "win32" &&
    (errno === "EACCES" || errno === "EPERM" || errno === "UNKNOWN")
  )
    return {
      code: "HOST_BLOCKED",
      message: hostBlockedMessage(`spawn ${errno}`),
    };
  if (process.platform === "linux" && errno === "ENOENT") {
    const interpreter = programInterpreter(binary);
    return {
      code: "HOST_LOAD_FAILED",
      message:
        `alpha2-host could not be loaded: ${binary}: its program interpreter ` +
        `${interpreter === undefined ? "" : `${interpreter} `}is missing; this alpha2-host is ` +
        "built for glibc, and musl systems such as Alpine are not supported",
    };
  }
  return undefined;
}

/**
 * The program interpreter `path` names: an ELF executable's `PT_INTERP` (the dynamic loader, such
 * as `/lib64/ld-linux-x86-64.so.2`), or a script's `#!` line. `undefined` when it names none or
 * cannot be read.
 */
export function programInterpreter(path: string): string | undefined {
  const head = Buffer.alloc(4096);
  let length: number;
  try {
    const fd = openSync(path, "r");
    try {
      length = readSync(fd, head, 0, head.length, 0);
    } finally {
      closeSync(fd);
    }
  } catch {
    return undefined;
  }
  const bytes = head.subarray(0, length);
  if (bytes[0] === 0x23 && bytes[1] === 0x21) {
    const line = bytes.subarray(2).toString("latin1").split("\n")[0] ?? "";
    return line.trim().split(/\s+/)[0] || undefined;
  }
  // A 64-bit little-endian ELF: the class and data bytes after the magic number.
  if (
    length < 64 ||
    bytes.readUInt32BE(0) !== 0x7f454c46 ||
    bytes[4] !== 2 ||
    bytes[5] !== 1
  )
    return undefined;
  const PT_INTERP = 3;
  const tableOffset = Number(bytes.readBigUInt64LE(0x20));
  const entrySize = bytes.readUInt16LE(0x36);
  const entries = bytes.readUInt16LE(0x38);
  for (let index = 0; index < entries; index += 1) {
    const entry = tableOffset + index * entrySize;
    if (entry + 40 > length) return undefined;
    if (bytes.readUInt32LE(entry) !== PT_INTERP) continue;
    const offset = Number(bytes.readBigUInt64LE(entry + 8));
    const size = Number(bytes.readBigUInt64LE(entry + 32));
    if (offset + size > length) return undefined;
    return (
      bytes
        .subarray(offset, offset + size)
        .toString("latin1")
        .replace(/\0.*$/s, "") || undefined
    );
  }
  return undefined;
}

/**
 * What a loader refusal adds when glibc refused the host for being too old: the floor the host
 * needs and the version this system has. Empty for any other refusal.
 */
export function glibcHint(stderr: string): string {
  if (!/version `GLIBC_2\.\d+' not found/.test(stderr)) return "";
  const libc = INSTALL_MACHINE.linuxLibc();
  const has = libc.kind === "glibc" ? `glibc ${libc.version}` : libc.kind;
  return `; this alpha2-host needs glibc ${LINUX_GLIBC_FLOOR} or later (this system has ${has})`;
}

function isFile(path: string): boolean {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

/**
 * How many processes the kernel's out-of-memory killer has ended in this process's cgroup (the
 * `oom_kill` line of cgroup v2's `memory.events`), or `undefined` where there is none to read:
 * off Linux, outside cgroup v2, or without the file. A host started by this process runs in the
 * same cgroup, so a count that rose while a SIGKILLed host ran says the kernel killed it.
 * `ALPHA2_TEST_MEMORY_EVENTS` names another file to read, for tests only.
 */
export function oomKillCount(): number | undefined {
  const injected = process.env.ALPHA2_TEST_MEMORY_EVENTS;
  let path = injected;
  if (path === undefined) {
    if (process.platform !== "linux") return undefined;
    try {
      const membership = readFileSync("/proc/self/cgroup", "utf8");
      const line = membership
        .split("\n")
        .find((entry) => entry.startsWith("0::"));
      if (line === undefined) return undefined;
      path = `/sys/fs/cgroup${line.slice(3).trim().replace(/\/$/, "")}/memory.events`;
    } catch {
      return undefined;
    }
  }
  try {
    const events = readFileSync(path, "utf8");
    const count = /^oom_kill (\d+)$/m.exec(events)?.[1];
    return count === undefined ? undefined : Number(count);
  } catch {
    return undefined;
  }
}
