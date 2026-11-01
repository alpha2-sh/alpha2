import { closeSync, openSync, readFileSync, readSync, statSync } from "node:fs";

import { INSTALL_MACHINE, LINUX_GLIBC_FLOOR } from "./install.js";

export const DEFAULT_HOST_START_TIMEOUT_MS = 30_000;

/** The default when unset; an `INVALID_INPUT` message for values not above zero. */
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

export function hostStartTimeoutMessage(binary: string, ms: number): string {
  const hint =
    process.platform === "darwin"
      ? "; macOS may be holding it for a Gatekeeper check"
      : process.platform === "win32"
        ? "; an antivirus scan or an App Control policy may be holding it"
        : "";
  return `alpha2-host did not start within ${ms} ms: ${binary}${hint}`;
}

export function hostBlockedMessage(what: string): string {
  return (
    `Windows blocked alpha2-host or a library it loads (${what}): Smart App Control, an App ` +
    "Control (WDAC) or AppLocker policy, or an antivirus refuses unsigned code. Allow the files " +
    "in that policy, or turn Smart App Control off (Windows Security > App & browser control)."
  );
}

/** Formats as `status 0xC0000428`. */
export function windowsStatus(exitCode: number): string {
  const status = exitCode < 0 ? exitCode + 2 ** 32 : exitCode;
  return `status 0x${status.toString(16).toUpperCase().padStart(8, "0")}`;
}

/**
 * Classifies spawn failures that retrying cannot fix. libuv reports a Windows policy refusal as
 * `EACCES`, `EPERM` or `UNKNOWN`; a glibc binary on musl fails `ENOENT` although the file exists.
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

/** An ELF executable's `PT_INTERP` or a script's `#!` interpreter. */
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
  // Only 64-bit little-endian ELF (class and data bytes after the magic).
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

/** Hint text for a too-old glibc; empty for any other refusal. */
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
 * The cgroup v2 `memory.events` `oom_kill` count, or `undefined` off Linux or outside cgroup v2.
 * The host shares this process's cgroup, so a rise while a SIGKILLed host ran means an OOM kill.
 * `ALPHA2_TEST_MEMORY_EVENTS` overrides the file for tests.
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
