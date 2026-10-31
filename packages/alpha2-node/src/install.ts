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

import { extractArchive } from "./archive.js";
import { bunzip2 } from "./bzip2.js";
import { get } from "./http.js";

/**
 * The file name `alpha2-host` has on `platform`: Windows executables carry `.exe`, and nothing
 * there adds it to a path that lacks it.
 */
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

/** The C library of a Linux system. */
export type LinuxLibc =
  | { readonly kind: "glibc"; readonly version: string }
  | { readonly kind: "musl" | "unknown" };

let detectedLibc: LinuxLibc | undefined;

/** This Linux system's C library, from Node's own report of the glibc it runs on. */
function linuxLibc(): LinuxLibc {
  if (detectedLibc !== undefined) return detectedLibc;
  // `excludeNetwork` (Node 22.13) postdates the pinned @types/node.
  const report = process.report as NodeJS.ProcessReport & {
    excludeNetwork: boolean;
  };
  const excluded = report.excludeNetwork;
  let version: unknown;
  try {
    // The report otherwise resolves the names of every open socket, which can take seconds.
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

/** Whether this Mac has an Apple silicon CPU, which an x64 Node under Rosetta does not report. */
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

/**
 * The machine as the installer sees it, and the rename it retries. Exported for this package's
 * tests, which stand in for other machines; the package's entry point does not export it.
 */
export const INSTALL_MACHINE = {
  platform: (): NodeJS.Platform => process.platform,
  arch: (): string => process.arch,
  linuxLibc,
  appleSilicon,
  rename: (from: string, to: string): Promise<void> => rename(from, to),
  retriesRenames: (): boolean => process.platform === "win32",
};

/** The release target for `platform` and `arch`, or why there is none. */
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
      // The interpreter's architecture decides: an x64 Node on Windows on ARM gets the x64 host.
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
 * The release target that carries `alpha2-host` for `platform` and `arch`, such as `macos-arm64`,
 * or `undefined` when no archive is published for them. An x64 Node on an Apple silicon Mac gets
 * `macos-arm64`.
 */
export function hostTarget(
  platform: NodeJS.Platform = INSTALL_MACHINE.platform(),
  arch: string = INSTALL_MACHINE.arch(),
): string | undefined {
  const found = targetFor(platform, arch);
  return "target" in found ? found.target : undefined;
}

/**
 * The directory `installHost` unpacks archives into and `resolveHostBinary` looks in:
 * `ALPHA2_HOST_CACHE_DIR` when it is set, else `alpha2` in the platform's cache directory
 * (`$XDG_CACHE_HOME` or `~/.cache` on Linux, `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on
 * Windows). Each archive keeps its own name there, `alpha2-<version>-<target>-bundled/`, so one
 * cache serves every version and every client that uses this layout.
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

/**
 * This package's version, which is the version of the `alpha2-host` it speaks to: they are
 * released together. `undefined` when the package's manifest is not beside its code.
 */
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

/** The name of the archive, and of the directory it unpacks into, without its extension. */
function archiveBase(version: string, target: string): string {
  return `alpha2-${version}-${target}-bundled`;
}

/**
 * The installed `alpha2-host` of this package's version for this platform in `cacheDirectory`,
 * or `undefined` when it is not there.
 */
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

/** An {@link installHost} failure. Nothing it was installing is left in the cache. */
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
   * The base the release archives are downloaded from, as `<base>/v<version>/<archive>` and
   * `<base>/v<version>/checksums.sha256`: a mirror of {@link HOST_DOWNLOAD_URL}. Defaults to
   * `ALPHA2_HOST_DOWNLOAD_URL` when it is set, else {@link HOST_DOWNLOAD_URL}.
   */
  readonly downloadUrl?: string;
  /** Called as the archive downloads; `total` is `undefined` when the server did not say. */
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
}

/**
 * Downloads the native archive of this package's version for this platform from the GitHub
 * release, checks it against the release's `checksums.sha256`, and unpacks it into the cache,
 * where `resolveHostBinary` finds it. An archive already installed is not downloaded again unless
 * `force` is set. The archive appears in the cache whole or not at all, so installs running at the
 * same time do not see each other's partial work, and `force` over a copy that is in use leaves
 * that copy as it was. Downloads go through `HTTPS_PROXY`/`HTTP_PROXY` (with `NO_PROXY`) when they
 * are set and trust the system's certificate store as well as Node's own. Linux systems need
 * glibc {@link LINUX_GLIBC_FLOOR} or later; musl systems are refused before anything is
 * downloaded.
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
    const checksums = await downloadText(
      `${releaseUrl}/checksums.sha256`,
      "host",
      options.signal,
    );
    const expected = checksumOf(checksums, archive);
    if (expected === undefined)
      throw new Alpha2HostInstallError(
        "DOWNLOAD_FAILED",
        `${releaseUrl}/checksums.sha256 lists no ${archive}`,
      );

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
        `${archive} has SHA-256 ${actual}; checksums.sha256 says ${expected}`,
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
    return { hostPath, directory, downloaded };
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

/**
 * The release target to install for, after refusing what the archives cannot run on: a Node.js
 * without the proxy and certificate support downloads rely on, a platform without an archive, and
 * a Linux C library other than glibc {@link LINUX_GLIBC_FLOOR} or later.
 */
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

/** Whether dotted version `a` is older than `b`, comparing each part as a number. */
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

/** Where Cisco publishes its OpenH264 binaries: `<base>/<archive>`. */
export const OPENH264_DOWNLOAD_URL = "https://ciscobinary.openh264.org";

/** The text Cisco's binary licence requires wherever the user controls the codec. */
export const OPENH264_NOTICE =
  "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/** Cisco's licence for its OpenH264 binaries. */
export const OPENH264_LICENSE_URL =
  "https://www.openh264.org/BINARY_LICENSE.txt";

/** One target's OpenH264 binary: the `.bz2` file Cisco publishes and the library it unpacks to. */
export interface OpenH264Archive {
  readonly archive: string;
  readonly archiveSha256: string;
  readonly library: string;
  readonly librarySha256: string;
}

/**
 * Cisco's OpenH264 binaries by release target, with their SHA-256s. It mirrors
 * release/openh264/manifest.toml, which the published package does not carry. Exported for this
 * package's tests, which serve their own files; the package's entry point does not export it.
 */
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
   * The base the binary is downloaded from, as `<base>/<archive>`: a mirror of
   * {@link OPENH264_DOWNLOAD_URL}. Defaults to `ALPHA2_OPENH264_DOWNLOAD_URL` when it is set, else
   * {@link OPENH264_DOWNLOAD_URL}.
   */
  readonly downloadUrl?: string;
  /** Called as the binary downloads; `total` is `undefined` when the server did not say. */
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
  /** {@link OPENH264_NOTICE}, which Cisco's licence requires be shown to the user. */
  readonly notice: string;
}

/**
 * Downloads Cisco's prebuilt OpenH264 library for this platform, the software H.264 encoder
 * `alpha2-host` loads when no hardware encoder is available, into the cache as
 * `openh264-<version>-<target>/<library>`, where the host looks for it. Nothing else downloads it:
 * Cisco's patent licence covers its binary only when the user fetches it from Cisco. The `.bz2`
 * file and the library it unpacks to are both checked against their SHA-256s. A library already
 * installed is not downloaded again unless `force` is set. It appears in the cache whole or not at
 * all, like {@link installHost}'s archives.
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

/** The SHA-256 of the file at `path` in lower-case hex, or `undefined` when there is none. */
async function sha256OfFile(path: string): Promise<string | undefined> {
  let bytes: Buffer;
  try {
    bytes = await readFile(path);
  } catch {
    return undefined;
  }
  return createHash("sha256").update(bytes).digest("hex");
}

/** The SHA-256 `sha256sum` output lists for `file`, in lower case. */
function checksumOf(listing: string, file: string): string | undefined {
  for (const line of listing.split(/\r?\n/)) {
    const match = /^([0-9a-fA-F]{64}) [ *](.+)$/.exec(line);
    if (match?.[2] === file) return match[1].toLowerCase();
  }
  return undefined;
}

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * Removes what earlier installs into `cacheDirectory` left behind more than a day ago: staging
 * directories, and copies of `base` moved aside by a replacement that could not delete them (a
 * host from them was still running). Anything younger may belong to an install still running.
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
      // Gone already, or still in use: the next install tries again.
    }
  }
}

/** Delays between rename attempts on Windows, the last repeated until ten seconds have passed. */
const RENAME_DELAYS_MS = [50, 100, 200, 400, 800, 1000];
const RENAME_BUDGET_MS = 10_000;

/**
 * Renames `from` to `to`. On Windows a scanner or indexer that has just opened a fresh file makes
 * the rename fail with EPERM, EACCES or EBUSY for a moment, so those are retried for up to ten
 * seconds; elsewhere it is one attempt.
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
 * Moves the finished `staged` directory to `directory` and resolves with whether it did (`false`
 * when a concurrent install got there first). A copy already at `directory` is replaced when
 * `force` is set or it is not a complete install: it is renamed aside first, so a copy in use
 * fails the replacement and stays whole, and is deleted only once the new one is in place.
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

/** What a failed download suggests, for the host archive or for OpenH264. */
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

/** The response for `url` when it answered 200, else a {@link Alpha2HostInstallError}. */
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

/** Writes `url` to `path`, reporting progress, and returns its SHA-256 in lower-case hex. */
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

/**
 * Every level of `error`'s cause chain as `<code> <message>`, so a proxy refusal or a certificate
 * failure is named rather than hidden behind a generic outer message.
 */
function causeChain(error: unknown): string {
  const parts: string[] = [];
  let current: unknown = error;
  for (let depth = 0; current instanceof Error && depth < 8; depth += 1) {
    const code = (current as NodeJS.ErrnoException).code;
    let message = current.message;
    // A connection that tried several addresses fails with one error per address.
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
