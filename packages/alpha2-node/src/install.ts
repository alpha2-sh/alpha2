import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import {
  createWriteStream,
  existsSync,
  readdirSync,
  readFileSync,
  statSync,
} from "node:fs";
import {
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import type { IncomingMessage } from "node:http";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { extractArchive } from "./archive.js";
import { bunzip2 } from "./bzip2.js";
import { get } from "./http.js";

/** The `alpha2-host` file name on `platform` (`.exe` on Windows). */
export function hostExecutableName(
  platform: NodeJS.Platform = process.platform,
): string {
  return platform === "win32" ? "alpha2-host.exe" : "alpha2-host";
}

/** Where release archives are downloaded from: `<base>/v<version>/<archive>`. */
export const HOST_DOWNLOAD_URL =
  "https://github.com/alpha2-sh/alpha2/releases/download";

/** The oldest glibc the Linux archives run on. */
export const LINUX_GLIBC_FLOOR = "2.35";

export type LinuxLibc =
  | { readonly kind: "glibc"; readonly version: string }
  | { readonly kind: "musl" | "unknown" };

let detectedLibc: LinuxLibc | undefined;

function linuxLibc(): LinuxLibc {
  if (detectedLibc !== undefined) return detectedLibc;
  // `excludeNetwork` (Node 22.13) postdates the pinned @types/node.
  const report = process.report as NodeJS.ProcessReport & {
    excludeNetwork: boolean;
  };
  const excluded = report.excludeNetwork;
  let version: unknown;
  try {
    // Otherwise the report resolves every open socket's name, which can take seconds.
    report.excludeNetwork = true;
    const header = (report.getReport() as { header?: Record<string, unknown> })
      .header;
    version = header?.glibcVersionRuntime;
  } finally {
    report.excludeNetwork = excluded;
  }
  if (typeof version === "string") detectedLibc = { kind: "glibc", version };
  else {
    let musl = false;
    try {
      musl = readdirSync("/lib").some((name) =>
        /^ld-musl-.+\.so\.1$/.test(name),
      );
    } catch {
      // No /lib to look in.
    }
    detectedLibc = { kind: musl ? "musl" : "unknown" };
  }
  return detectedLibc;
}

let detectedAppleSilicon: boolean | undefined;

/** Detects Apple silicon even when an x64 Node runs under Rosetta. */
function appleSilicon(): boolean {
  if (detectedAppleSilicon === undefined) {
    try {
      detectedAppleSilicon =
        execFileSync("/usr/sbin/sysctl", ["-n", "hw.optional.arm64"], {
          encoding: "utf8",
          stdio: ["ignore", "pipe", "ignore"],
        }).trim() === "1";
    } catch {
      detectedAppleSilicon = false;
    }
  }
  return detectedAppleSilicon;
}

/** Machine probes, replaceable by tests; not exported from the package entry point. */
export const INSTALL_MACHINE = {
  platform: (): NodeJS.Platform => process.platform,
  arch: (): string => process.arch,
  linuxLibc,
  appleSilicon,
  rename: (from: string, to: string): Promise<void> => rename(from, to),
  retriesRenames: (): boolean => process.platform === "win32",
};

function targetFor(
  platform: NodeJS.Platform,
  arch: string,
): { target: string } | { refusal: string } {
  const unpublished = {
    refusal: `no alpha2-host archive is published for ${platform}-${arch}`,
  };
  switch (platform) {
    case "darwin":
      if (arch === "arm64") return { target: "macos-arm64" };
      // An x64 Node under Rosetta runs the arm64 host as its own, native process.
      if (arch === "x64")
        return INSTALL_MACHINE.appleSilicon()
          ? { target: "macos-arm64" }
          : { refusal: "Intel Macs are not supported" };
      return unpublished;
    case "win32":
      // An x64 Node on Windows on ARM gets the x64 host.
      if (arch === "x64") return { target: "win-x64" };
      if (arch === "arm64")
        return {
          refusal:
            "there is no Windows on ARM alpha2-host; an x64 Node.js/Python runs the x64 host " +
            "under emulation (not supported or tested)",
        };
      return unpublished;
    case "linux":
      if (arch === "x64") return { target: "linux-x64" };
      if (arch === "arm64") return { target: "linux-arm64" };
      return unpublished;
    default:
      return unpublished;
  }
}

/**
 * The release target for `platform` and `arch`, such as `macos-arm64`, or `undefined` when none
 * is published. An x64 Node on an Apple silicon Mac gets `macos-arm64`.
 */
export function hostTarget(
  platform: NodeJS.Platform = INSTALL_MACHINE.platform(),
  arch: string = INSTALL_MACHINE.arch(),
): string | undefined {
  const found = targetFor(platform, arch);
  return "target" in found ? found.target : undefined;
}

/**
 * The cache `installHost` unpacks into and `resolveHostBinary` searches: `ALPHA2_HOST_CACHE_DIR`,
 * else `alpha2` in the platform cache directory (`$XDG_CACHE_HOME` or `~/.cache`,
 * `~/Library/Caches`, `%LOCALAPPDATA%`). Archives live in `alpha2-<version>-<target>-bundled/`.
 */
export function hostCacheDirectory(cwd: string = process.cwd()): string {
  const fromEnvironment = process.env.ALPHA2_HOST_CACHE_DIR;
  if (fromEnvironment?.trim()) return resolve(cwd, fromEnvironment);
  if (process.platform === "win32") {
    const local = process.env.LOCALAPPDATA;
    return join(
      local?.trim() ? local : join(homedir(), "AppData", "Local"),
      "alpha2",
    );
  }
  if (process.platform === "darwin")
    return join(homedir(), "Library", "Caches", "alpha2");
  const xdg = process.env.XDG_CACHE_HOME;
  return join(
    xdg && isAbsolute(xdg) ? xdg : join(homedir(), ".cache"),
    "alpha2",
  );
}

/** This package's version, which is also its host's. `undefined` without a manifest. */
export function packageVersion(): string | undefined {
  try {
    const manifest: unknown = JSON.parse(
      readFileSync(new URL("../package.json", import.meta.url), "utf8"),
    );
    const version = (manifest as { version?: unknown }).version;
    return typeof version === "string" ? version : undefined;
  } catch {
    return undefined;
  }
}

function archiveBase(version: string, target: string): string {
  return `alpha2-${version}-${target}-bundled`;
}

/** The cached `alpha2-host` of this package's version and platform, if installed. */
export function cachedHostBinary(cacheDirectory: string): string | undefined {
  const version = packageVersion();
  const target = hostTarget();
  if (version === undefined || target === undefined) return undefined;
  const candidate = join(
    cacheDirectory,
    archiveBase(version, target),
    "bin",
    hostExecutableName(),
  );
  return isFile(candidate) ? candidate : undefined;
}

/** Why {@link installHost} failed. */
export type HostInstallErrorCode =
  | "UNSUPPORTED_PLATFORM"
  | "DOWNLOAD_FAILED"
  | "CHECKSUM_MISMATCH"
  | "EXTRACT_FAILED"
  | "ABORTED";

/** An install failure. Nothing partial is left in the cache. */
export class Alpha2HostInstallError extends Error {
  readonly code: HostInstallErrorCode;

  constructor(
    code: HostInstallErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "Alpha2HostInstallError";
    this.code = code;
  }
}

/** Options of {@link installHost}. */
export interface InstallHostOptions {
  /** The cache to install into. Defaults to {@link hostCacheDirectory}. */
  readonly cacheDirectory?: string;
  /** Downloads and unpacks again when the archive is already installed. */
  readonly force?: boolean;
  /**
   * Mirror of {@link HOST_DOWNLOAD_URL}, laid out as `<base>/v<version>/<archive>`. Defaults to
   * `ALPHA2_HOST_DOWNLOAD_URL`, else {@link HOST_DOWNLOAD_URL}.
   */
  readonly downloadUrl?: string;
  /** Download progress; `total` is `undefined` when the server does not report it. */
  readonly onProgress?: (progress: {
    received: number;
    total: number | undefined;
  }) => void;
  readonly signal?: AbortSignal;
}

/** What {@link installHost} installed. */
export interface InstallHostResult {
  /** The installed `alpha2-host`, which `resolveHostBinary` now finds. */
  readonly hostPath: string;
  /** The archive's directory in the cache, which holds `bin/` and `lib/`. */
  readonly directory: string;
  /** Whether it was downloaded now; `false` when it was already installed. */
  readonly downloaded: boolean;
  /**
   * `"release-pin"` when checked against the package's pinned SHA-256, `"checksums"` when against
   * the release's `checksums.sha256` (no pin, as in a source checkout). Absent without a download.
   */
  readonly verifiedBy?: HostArchiveVerification;
}

/** What {@link installHost} checked a downloaded archive against. */
export type HostArchiveVerification = "release-pin" | "checksums";

/**
 * The archive SHA-256 pinned in `host-archives.json`, or `undefined` when there is no such file
 * (source checkout). An unreadable file is an error so a damaged package never falls back to the
 * mirror's checksums.
 */
function pinnedArchiveSha256(
  version: string,
  target: string,
): string | undefined {
  const file = new URL("./host-archives.json", import.meta.url);
  let text: string;
  try {
    text = readFileSync(file, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw pinsError(file, (error as Error).message, error);
  }
  let pins: unknown;
  try {
    pins = JSON.parse(text);
  } catch (error) {
    throw pinsError(file, (error as Error).message, error);
  }
  const { version: pinnedVersion, archives } = (pins ?? {}) as {
    version?: unknown;
    archives?: unknown;
  };
  if (
    typeof pinnedVersion !== "string" ||
    typeof archives !== "object" ||
    archives === null
  )
    throw pinsError(file, "it has no version and archives");
  if (pinnedVersion !== version) return undefined;
  const pinned = (archives as Record<string, unknown>)[target];
  if (pinned === undefined) return undefined;
  if (typeof pinned !== "string" || !/^[0-9a-f]{64}$/.test(pinned))
    throw pinsError(file, `its ${target} entry is not a SHA-256`);
  return pinned;
}

function pinsError(
  file: URL,
  reason: string,
  cause?: unknown,
): Alpha2HostInstallError {
  return new Alpha2HostInstallError(
    "CHECKSUM_MISMATCH",
    `cannot read the release pins in ${fileURLToPath(file)} (${reason}); reinstall @alpha2-sh/node`,
    cause === undefined ? undefined : { cause },
  );
}

/**
 * Downloads this package's native archive for this platform (from the GitHub release or a
 * mirror), verifies it against the package's pinned SHA-256 (or the release's `checksums.sha256`
 * when unpinned), and unpacks it into the cache. Skips an installed archive unless `force` is set.
 * Installs are atomic, and `force` over a copy in use leaves that copy intact.
 *
 * Honors `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` and trusts the system certificate store. Linux
 * needs glibc {@link LINUX_GLIBC_FLOOR} or later; musl is refused before downloading.
 *
 * Throws an {@link Alpha2HostInstallError}.
 */
export async function installHost(
  options: InstallHostOptions = {},
): Promise<InstallHostResult> {
  const version = packageVersion();
  if (version === undefined)
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      "cannot read this package's version from its package.json",
    );
  const target = installTarget(version);

  const cacheDirectory = options.cacheDirectory ?? hostCacheDirectory();
  const base = archiveBase(version, target);
  const directory = join(cacheDirectory, base);
  const hostPath = join(directory, "bin", hostExecutableName());
  const installed = () => Promise.resolve(isFile(hostPath));
  if (!options.force && (await installed()))
    return { hostPath, directory, downloaded: false };

  const pinned = pinnedArchiveSha256(version, target);
  const archive = `${base}.${target === "win-x64" ? "zip" : "tar.gz"}`;
  const fromEnvironment = process.env.ALPHA2_HOST_DOWNLOAD_URL;
  const downloadUrl = (
    options.downloadUrl ??
    (fromEnvironment?.trim() ? fromEnvironment : HOST_DOWNLOAD_URL)
  ).replace(/\/+$/, "");
  const releaseUrl = `${downloadUrl}/v${version}`;

  await mkdir(cacheDirectory, { recursive: true });
  await removeStale(cacheDirectory, base);
  // Staged in the cache itself, so the finished directory moves into place with one rename.
  const staging = await mkdtemp(join(cacheDirectory, ".install-"));
  try {
    let expected = pinned;
    if (expected === undefined) {
      const checksums = await downloadText(
        `${releaseUrl}/checksums.sha256`,
        "host",
        options.signal,
      );
      expected = checksumOf(checksums, archive);
      if (expected === undefined)
        throw new Alpha2HostInstallError(
          "DOWNLOAD_FAILED",
          `${releaseUrl}/checksums.sha256 lists no ${archive}`,
        );
    }

    const archivePath = join(staging, archive);
    const actual = await downloadFile(
      `${releaseUrl}/${archive}`,
      archivePath,
      "host",
      options,
    );
    if (actual !== expected)
      throw new Alpha2HostInstallError(
        "CHECKSUM_MISMATCH",
        `${archive} has SHA-256 ${actual}; ` +
          (pinned === undefined
            ? `checksums.sha256 says ${expected}`
            : `the release pins ${expected}`),
      );

    const unpacked = join(staging, "unpacked");
    await mkdir(unpacked);
    try {
      await extractArchive(archivePath, unpacked, options.signal);
    } catch (error) {
      throw options.signal?.aborted
        ? new Alpha2HostInstallError("ABORTED", "the install was aborted", {
            cause: error,
          })
        : new Alpha2HostInstallError(
            "EXTRACT_FAILED",
            (error as Error).message,
            { cause: error },
          );
    }
    const staged = join(unpacked, base);
    if (!isFile(join(staged, "bin", hostExecutableName())))
      throw new Alpha2HostInstallError(
        "EXTRACT_FAILED",
        `${archive} does not hold ${base}/bin/${hostExecutableName()}`,
      );

    const downloaded = await replaceDirectory(staged, directory, {
      force: options.force === true,
      isInstalled: installed,
    });
    if (!downloaded) return { hostPath, directory, downloaded };
    return {
      hostPath,
      directory,
      downloaded,
      verifiedBy: pinned === undefined ? "checksums" : "release-pin",
    };
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

/** Refuses unsupported Node.js versions, platforms and C libraries before downloading. */
function installTarget(version: string | undefined): string {
  const node = process.versions.node.split(".").map(Number);
  if (
    !(node[0] === 22
      ? node[1] >= 21
      : node[0] > 24 || (node[0] === 24 && node[1] >= 5))
  )
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      "Node.js 22.21 or later (24.5 or later on Node 24) is required to download alpha2-host; " +
        `this is Node.js ${process.versions.node}`,
    );
  const platform = INSTALL_MACHINE.platform();
  const found = targetFor(platform, INSTALL_MACHINE.arch());
  if ("refusal" in found)
    throw new Alpha2HostInstallError("UNSUPPORTED_PLATFORM", found.refusal);
  if (platform === "linux") {
    const libc = INSTALL_MACHINE.linuxLibc();
    if (libc.kind !== "glibc")
      throw new Alpha2HostInstallError(
        "UNSUPPORTED_PLATFORM",
        `alpha2-host is built for glibc ${LINUX_GLIBC_FLOOR} or later; this system's C library ` +
          (libc.kind === "musl"
            ? "is musl (Alpine and other musl distributions are not supported)"
            : "is not glibc"),
      );
    if (olderVersion(libc.version, LINUX_GLIBC_FLOOR))
      throw new Alpha2HostInstallError(
        "UNSUPPORTED_PLATFORM",
        `alpha2-host${version === undefined ? "" : ` ${version}`} needs glibc ` +
          `${LINUX_GLIBC_FLOOR} or later; this system has ` +
          `glibc ${libc.version}. Build it from source (cargo install alpha2-host with FFmpeg 8 ` +
          "development files) and set ALPHA2_HOST_PATH",
      );
  }
  return found.target;
}

function olderVersion(a: string, b: string): boolean {
  const left = a.split(".").map(Number);
  const right = b.split(".").map(Number);
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const difference = (left[index] || 0) - (right[index] || 0);
    if (difference !== 0) return difference < 0;
  }
  return false;
}

/** The OpenH264 release {@link installOpenH264} installs. */
export const OPENH264_VERSION = "2.7.0";

/** Cisco's OpenH264 binaries, as `<base>/<archive>`. */
export const OPENH264_DOWNLOAD_URL = "https://ciscobinary.openh264.org";

/** The text Cisco's binary licence requires wherever the user controls the codec. */
export const OPENH264_NOTICE =
  "OpenH264 Video Codec provided by Cisco Systems, Inc.";

export const OPENH264_LICENSE_URL =
  "https://www.openh264.org/BINARY_LICENSE.txt";

/** One target's OpenH264 binary: the `.bz2` file Cisco publishes and the library it unpacks to. */
export interface OpenH264Archive {
  readonly archive: string;
  readonly archiveSha256: string;
  readonly library: string;
  readonly librarySha256: string;
}

/** Cisco's OpenH264 binaries by release target. Must match release/openh264/manifest.toml. */
export const OPENH264_TARGETS: Record<string, OpenH264Archive> = {
  "linux-x64": {
    archive: "libopenh264-2.7.0-linux64.8.so.bz2",
    archiveSha256:
      "44ad0dd2c5af2bac84d26f89f2eb4e268708b8739323d15d33837f7221ce4980",
    library: "libopenh264-2.7.0-linux64.8.so",
    librarySha256:
      "1522f873a482923c5ec018a4415e1866898197528828c50f90fd300c94620df3",
  },
  "linux-arm64": {
    archive: "libopenh264-2.7.0-linux-arm64.8.so.bz2",
    archiveSha256:
      "74c7a1d02003d80daf2b4bc7bbb02f73e5548debd4c158b495e1f83d80ebd136",
    library: "libopenh264-2.7.0-linux-arm64.8.so",
    librarySha256:
      "9ac664b0154fd0868491c2a904397b8a05779874c0603b8b1f16ad15de1ffd82",
  },
  "macos-arm64": {
    archive: "libopenh264-2.7.0-mac-arm64.dylib.bz2",
    archiveSha256:
      "c9f2338359093a3e0628e01717be373c6948994a6070a706d303f052013e2b5f",
    library: "libopenh264-2.7.0-mac-arm64.dylib",
    librarySha256:
      "2e1ae3cd10c08328ca03a1f304fd2a1a2a7e5658c6d5a8582101a963f7995ef0",
  },
  "win-x64": {
    archive: "openh264-2.7.0-win64.dll.bz2",
    archiveSha256:
      "7073fda55e904ac515f787d9728c4bcf9965ec7c91c9f84eae3f260ef9b9b37c",
    library: "openh264-2.7.0-win64.dll",
    librarySha256:
      "0ef5e7f3b38753b15c13afa0418d4d7805fa228e6adfd3e425bbf758cff1c288",
  },
};

/** Options of {@link installOpenH264}. */
export interface InstallOpenH264Options {
  /** The cache to install into. Defaults to {@link hostCacheDirectory}. */
  readonly cacheDirectory?: string;
  /** Downloads and unpacks again when the library is already installed. */
  readonly force?: boolean;
  /**
   * Mirror of {@link OPENH264_DOWNLOAD_URL}, as `<base>/<archive>`. Defaults to
   * `ALPHA2_OPENH264_DOWNLOAD_URL`, else {@link OPENH264_DOWNLOAD_URL}.
   */
  readonly downloadUrl?: string;
  /** Download progress; `total` is `undefined` when the server does not report it. */
  readonly onProgress?: (progress: {
    received: number;
    total: number | undefined;
  }) => void;
  readonly signal?: AbortSignal;
}

/** What {@link installOpenH264} installed. */
export interface InstallOpenH264Result {
  /** The installed library, which `alpha2-host` now loads. */
  readonly libraryPath: string;
  /** Its directory in the cache, `openh264-<version>-<target>/`. */
  readonly directory: string;
  /** Whether it was downloaded now; `false` when it was already installed. */
  readonly downloaded: boolean;
  /** The OpenH264 release, {@link OPENH264_VERSION}. */
  readonly version: string;
  /** {@link OPENH264_NOTICE}, which must be shown to the user. */
  readonly notice: string;
}

/**
 * Downloads Cisco's prebuilt OpenH264 library into the cache as
 * `openh264-<version>-<target>/<library>`, where `alpha2-host` loads it as its software H.264
 * encoder. Cisco's patent licence covers the binary only when the user fetches it from Cisco, so
 * nothing else downloads it. Both the `.bz2` and the library are SHA-256 checked. Skips an
 * installed library unless `force` is set; installs are atomic.
 *
 * Throws an {@link Alpha2HostInstallError}.
 */
export async function installOpenH264(
  options: InstallOpenH264Options = {},
): Promise<InstallOpenH264Result> {
  const target = installTarget(packageVersion());
  const entry = OPENH264_TARGETS[target];
  if (entry === undefined)
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      `there is no OpenH264 binary for alpha2-host on ${target}`,
    );

  const cacheDirectory = options.cacheDirectory ?? hostCacheDirectory();
  const base = `openh264-${OPENH264_VERSION}-${target}`;
  const directory = join(cacheDirectory, base);
  const libraryPath = join(directory, entry.library);
  const result = (downloaded: boolean): InstallOpenH264Result => ({
    libraryPath,
    directory,
    downloaded,
    version: OPENH264_VERSION,
    notice: OPENH264_NOTICE,
  });
  const installed = async () =>
    (await sha256OfFile(libraryPath)) === entry.librarySha256;
  if (!options.force && (await installed())) return result(false);

  const fromEnvironment = process.env.ALPHA2_OPENH264_DOWNLOAD_URL;
  const downloadUrl = (
    options.downloadUrl ??
    (fromEnvironment?.trim() ? fromEnvironment : OPENH264_DOWNLOAD_URL)
  ).replace(/\/+$/, "");
  const url = `${downloadUrl}/${entry.archive}`;

  await mkdir(cacheDirectory, { recursive: true });
  await removeStale(cacheDirectory, base);
  // Staged in the cache itself, so the finished directory moves into place with one rename.
  const staging = await mkdtemp(join(cacheDirectory, ".install-"));
  try {
    const archivePath = join(staging, entry.archive);
    const actual = await downloadFile(
      url,
      archivePath,
      "openh264",
      options,
      `; ${entry.archive} is not there`,
    );
    if (actual !== entry.archiveSha256)
      throw new Alpha2HostInstallError(
        "CHECKSUM_MISMATCH",
        `${entry.archive} has SHA-256 ${actual}; OpenH264 ${OPENH264_VERSION} has ${entry.archiveSha256}`,
      );
    if (options.signal?.aborted)
      throw new Alpha2HostInstallError("ABORTED", "the install was aborted");

    let library: Uint8Array;
    try {
      library = bunzip2(await readFile(archivePath));
    } catch (error) {
      throw new Alpha2HostInstallError(
        "EXTRACT_FAILED",
        `cannot decompress ${entry.archive}`,
        { cause: error },
      );
    }
    const libraryHash = createHash("sha256").update(library).digest("hex");
    if (libraryHash !== entry.librarySha256)
      throw new Alpha2HostInstallError(
        "CHECKSUM_MISMATCH",
        `${entry.library} has SHA-256 ${libraryHash}; OpenH264 ${OPENH264_VERSION} has ${entry.librarySha256}`,
      );
    const staged = join(staging, base);
    await mkdir(staged);
    await writeFile(join(staged, entry.library), library);

    return result(
      await replaceDirectory(staged, directory, {
        force: options.force === true,
        isInstalled: installed,
      }),
    );
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

async function sha256OfFile(path: string): Promise<string | undefined> {
  let bytes: Buffer;
  try {
    bytes = await readFile(path);
  } catch {
    return undefined;
  }
  return createHash("sha256").update(bytes).digest("hex");
}

function checksumOf(listing: string, file: string): string | undefined {
  for (const line of listing.split(/\r?\n/)) {
    const match = /^([0-9a-fA-F]{64}) [ *](.+)$/.exec(line);
    if (match?.[2] === file) return match[1].toLowerCase();
  }
  return undefined;
}

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * Removes staging directories and set-aside copies older than a day; younger ones may belong to
 * an install still running.
 */
async function removeStale(
  cacheDirectory: string,
  base: string,
): Promise<void> {
  let names: string[];
  try {
    names = await readdir(cacheDirectory);
  } catch {
    return;
  }
  const now = Date.now();
  for (const name of names) {
    if (!name.startsWith(".install-") && !name.startsWith(`${base}.old-`))
      continue;
    const path = join(cacheDirectory, name);
    try {
      if (now - (await stat(path)).mtimeMs > DAY_MS)
        await rm(path, { recursive: true, force: true });
    } catch {
      // Still in use; the next install retries.
    }
  }
}

/** Windows rename retry delays; the last repeats until ten seconds have passed. */
const RENAME_DELAYS_MS = [50, 100, 200, 400, 800, 1000];
const RENAME_BUDGET_MS = 10_000;

/**
 * On Windows, a scanner or indexer briefly holding a fresh file makes renames fail with EPERM,
 * EACCES or EBUSY, so those are retried for up to ten seconds.
 */
async function renameWithRetry(from: string, to: string): Promise<void> {
  let waited = 0;
  for (let attempt = 0; ; attempt += 1) {
    try {
      await INSTALL_MACHINE.rename(from, to);
      return;
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      const delay =
        RENAME_DELAYS_MS[Math.min(attempt, RENAME_DELAYS_MS.length - 1)];
      if (
        !INSTALL_MACHINE.retriesRenames() ||
        !["EPERM", "EACCES", "EBUSY"].includes(code ?? "") ||
        waited + delay > RENAME_BUDGET_MS
      )
        throw error;
      await sleep(delay);
      waited += delay;
    }
  }
}

/**
 * Moves `staged` into place; `false` when a concurrent install won. An existing copy is replaced
 * when `force` is set or it is incomplete. It is renamed aside first so a copy in use stays whole.
 */
async function replaceDirectory(
  staged: string,
  directory: string,
  options: { force: boolean; isInstalled: () => Promise<boolean> },
): Promise<boolean> {
  let aside: string | undefined;
  if (
    existsSync(directory) &&
    (options.force || !(await options.isInstalled()))
  ) {
    aside = `${directory}.old-${randomBytes(4).toString("hex")}`;
    try {
      await renameWithRetry(directory, aside);
    } catch (error) {
      throw new Alpha2HostInstallError(
        "EXTRACT_FAILED",
        `cannot replace ${directory}: it is in use (is an alpha2-host from it running?); ` +
          "the installed copy is unchanged",
        { cause: error },
      );
    }
  }
  try {
    await renameWithRetry(staged, directory);
  } catch (error) {
    if (!options.force && (await options.isInstalled())) return false;
    if (aside !== undefined) await rename(aside, directory).catch(() => {});
    throw new Alpha2HostInstallError(
      "EXTRACT_FAILED",
      `cannot move ${basename(directory)} into ${dirname(directory)}: ` +
        (error as Error).message,
      { cause: error },
    );
  }
  if (aside !== undefined)
    await rm(aside, { recursive: true, force: true }).catch(() => {});
  return true;
}

const RELEASE_NOT_FOUND =
  "; the release may not be published yet, or not for this platform";

function downloadRemedy(kind: "host" | "openh264"): string {
  const [base, variable] =
    kind === "host"
      ? [HOST_DOWNLOAD_URL, "ALPHA2_HOST_DOWNLOAD_URL"]
      : [OPENH264_DOWNLOAD_URL, "ALPHA2_OPENH264_DOWNLOAD_URL"];
  return (
    "Behind a proxy set HTTPS_PROXY (and NO_PROXY); a proxy that inspects TLS needs its root " +
    "certificate in the system store or in NODE_EXTRA_CA_CERTS; or download from a mirror laid " +
    `out like ${base} with ${variable}`
  );
}

async function download(
  url: string,
  kind: "host" | "openh264",
  signal?: AbortSignal,
  notFound = RELEASE_NOT_FOUND,
): Promise<IncomingMessage> {
  let response: IncomingMessage;
  try {
    response = await get(url, signal);
  } catch (error) {
    throw downloadError(url, kind, error, signal);
  }
  const status = response.statusCode ?? 0;
  if (status !== 200) {
    response.resume();
    throw new Alpha2HostInstallError(
      "DOWNLOAD_FAILED",
      `${url} answered ${status}` +
        (status === 404 ? notFound : `\n${downloadRemedy(kind)}`),
    );
  }
  return response;
}

async function downloadText(
  url: string,
  kind: "host" | "openh264",
  signal?: AbortSignal,
): Promise<string> {
  const response = await download(url, kind, signal);
  const chunks: Buffer[] = [];
  try {
    for await (const chunk of response) chunks.push(chunk as Buffer);
  } catch (error) {
    throw downloadError(url, kind, error, signal);
  }
  return Buffer.concat(chunks).toString("utf8");
}

/** Returns the SHA-256 in lower-case hex. */
async function downloadFile(
  url: string,
  path: string,
  kind: "host" | "openh264",
  options: Pick<InstallHostOptions, "onProgress" | "signal">,
  notFound?: string,
): Promise<string> {
  const response = await download(url, kind, options.signal, notFound);
  const length = Number(response.headers["content-length"]);
  const total = Number.isFinite(length) && length > 0 ? length : undefined;
  const hash = createHash("sha256");
  let received = 0;
  const meter = new Transform({
    transform(chunk: Buffer, _encoding, callback) {
      hash.update(chunk);
      received += chunk.length;
      options.onProgress?.({ received, total });
      callback(null, chunk);
    },
  });
  try {
    await pipeline(
      response,
      meter,
      createWriteStream(path),
      ...(options.signal ? [{ signal: options.signal }] : []),
    );
  } catch (error) {
    throw downloadError(url, kind, error, options.signal);
  }
  return hash.digest("hex");
}

/** Formats the whole cause chain so proxy or certificate failures are not hidden. */
function causeChain(error: unknown): string {
  const parts: string[] = [];
  let current: unknown = error;
  for (let depth = 0; current instanceof Error && depth < 8; depth += 1) {
    const code = (current as NodeJS.ErrnoException).code;
    let message = current.message;
    // A connection that tried several addresses has one error per address.
    if (!message && current instanceof AggregateError)
      message = current.errors
        .map((inner: unknown) => (inner as Error).message)
        .join("; ");
    const part = typeof code === "string" ? `${code} ${message}` : message;
    if (part && parts.at(-1) !== part) parts.push(part.trim());
    current = current.cause;
  }
  return parts.join(": ") || String(error);
}

function downloadError(
  url: string,
  kind: "host" | "openh264",
  error: unknown,
  signal?: AbortSignal,
): Alpha2HostInstallError {
  if (error instanceof Alpha2HostInstallError) return error;
  if (signal?.aborted)
    return new Alpha2HostInstallError("ABORTED", "the install was aborted", {
      cause: error,
    });
  return new Alpha2HostInstallError(
    "DOWNLOAD_FAILED",
    `cannot download ${url}: ${causeChain(error)}\n${downloadRemedy(kind)}`,
    { cause: error },
  );
}

function isFile(path: string): boolean {
  try {
    return existsSync(path) && statSync(path).isFile();
  } catch {
    return false;
  }
}
