import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { createWriteStream, existsSync, readFileSync, statSync } from "node:fs";
import {
  mkdir,
  mkdtemp,
  readFile,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, join, relative, resolve } from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";

import { bunzip2 } from "./bzip2.js";

/**
 * The file name `alpha2-host` has on `platform`: Windows executables carry `.exe`, and nothing
 * there adds it to a path that lacks it.
 */
export function hostExecutableName(
  platform: NodeJS.Platform = process.platform,
): string {
  return platform === "win32" ? "alpha2-host.exe" : "alpha2-host";
}

/** A native archive's kind: `bundled` carries FFmpeg, `thin` loads the system's FFmpeg 8. */
export type HostArchiveKind = "bundled" | "thin";

/** Where release archives are downloaded from: `<base>/v<version>/<archive>`. */
export const HOST_DOWNLOAD_URL =
  "https://github.com/alpha2-sh/alpha2/releases/download";

/** The release targets, by Node's platform and architecture, and the kinds each is published in. */
const TARGETS: Readonly<
  Record<string, { target: string; kinds: readonly HostArchiveKind[] }>
> = {
  "linux-x64": { target: "linux-x64", kinds: ["bundled", "thin"] },
  "linux-arm64": { target: "linux-arm64", kinds: ["bundled", "thin"] },
  "darwin-arm64": { target: "macos-arm64", kinds: ["bundled", "thin"] },
  "win32-x64": { target: "win-x64", kinds: ["bundled"] },
};

/**
 * The release target that carries `alpha2-host` for `platform` and `arch`, such as `macos-arm64`,
 * or `undefined` when no archive is published for them.
 */
export function hostTarget(
  platform: NodeJS.Platform = process.platform,
  arch: string = process.arch,
): string | undefined {
  return TARGETS[`${platform}-${arch}`]?.target;
}

/**
 * The directory `installHost` unpacks archives into and `resolveHostBinary` looks in:
 * `ALPHA2_HOST_CACHE_DIR` when it is set, else `alpha2` in the platform's cache directory
 * (`$XDG_CACHE_HOME` or `~/.cache` on Linux, `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on
 * Windows). Each archive keeps its own name there, `alpha2-<version>-<target>-<kind>/`, so one
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
function archiveBase(
  version: string,
  target: string,
  kind: HostArchiveKind,
): string {
  return `alpha2-${version}-${target}-${kind}`;
}

/**
 * The installed `alpha2-host` of this package's version for this platform in `cacheDirectory`,
 * the bundled archive's before the thin one's, or `undefined` when neither is there.
 */
export function cachedHostBinary(cacheDirectory: string): string | undefined {
  const version = packageVersion();
  const entry = TARGETS[`${process.platform}-${process.arch}`];
  if (version === undefined || entry === undefined) return undefined;
  for (const kind of entry.kinds) {
    const candidate = join(
      cacheDirectory,
      archiveBase(version, entry.target, kind),
      "bin",
      hostExecutableName(),
    );
    if (isFile(candidate)) return candidate;
  }
  return undefined;
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
  /** The archive kind. Defaults to `bundled`, which needs no FFmpeg on the machine. */
  readonly kind?: HostArchiveKind;
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
 * same time do not see each other's partial work. Unpacking runs the system's `tar` (on Windows,
 * the one in System32, which also reads `.zip`).
 *
 * Throws an {@link Alpha2HostInstallError}.
 */
export async function installHost(
  options: InstallHostOptions = {},
): Promise<InstallHostResult> {
  const kind = options.kind ?? "bundled";
  const entry = TARGETS[`${process.platform}-${process.arch}`];
  if (entry === undefined)
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      `no alpha2-host archive is published for ${process.platform}-${process.arch}`,
    );
  if (!entry.kinds.includes(kind))
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      `${entry.target} has no ${kind} archive; it is published as ${entry.kinds.join(", ")}`,
    );
  const version = packageVersion();
  if (version === undefined)
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      "cannot read this package's version from its package.json",
    );

  const cacheDirectory = options.cacheDirectory ?? hostCacheDirectory();
  const base = archiveBase(version, entry.target, kind);
  const directory = join(cacheDirectory, base);
  const hostPath = join(directory, "bin", hostExecutableName());
  if (!options.force && isFile(hostPath))
    return { hostPath, directory, downloaded: false };

  const archive = `${base}.${process.platform === "win32" ? "zip" : "tar.gz"}`;
  const fromEnvironment = process.env.ALPHA2_HOST_DOWNLOAD_URL;
  const downloadUrl = (
    options.downloadUrl ??
    (fromEnvironment?.trim() ? fromEnvironment : HOST_DOWNLOAD_URL)
  ).replace(/\/+$/, "");
  const releaseUrl = `${downloadUrl}/v${version}`;

  await mkdir(cacheDirectory, { recursive: true });
  // Staged in the cache itself, so the finished directory moves into place with one rename.
  const staging = await mkdtemp(join(cacheDirectory, ".install-"));
  try {
    const checksums = await (
      await download(`${releaseUrl}/checksums.sha256`, options.signal)
    ).text();
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
      options,
    );
    if (actual !== expected)
      throw new Alpha2HostInstallError(
        "CHECKSUM_MISMATCH",
        `${archive} has SHA-256 ${actual}; checksums.sha256 says ${expected}`,
      );

    const unpacked = join(staging, "unpacked");
    await mkdir(unpacked);
    await extract(archivePath, unpacked, options.signal);
    const staged = join(unpacked, base);
    if (!isFile(join(staged, "bin", hostExecutableName())))
      throw new Alpha2HostInstallError(
        "EXTRACT_FAILED",
        `${archive} does not hold ${base}/bin/${hostExecutableName()}`,
      );

    if (options.force) await rm(directory, { recursive: true, force: true });
    try {
      await rename(staged, directory);
    } catch (error) {
      // Another install moved the same archive into place first.
      if (!isFile(hostPath))
        throw new Alpha2HostInstallError(
          "EXTRACT_FAILED",
          `cannot move ${base} into ${cacheDirectory}`,
          { cause: error },
        );
    }
    return { hostPath, directory, downloaded: true };
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
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
  const target = hostTarget();
  const entry = target === undefined ? undefined : OPENH264_TARGETS[target];
  if (target === undefined || entry === undefined)
    throw new Alpha2HostInstallError(
      "UNSUPPORTED_PLATFORM",
      `there is no OpenH264 binary for alpha2-host on ${process.platform}-${process.arch}`,
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
  // Staged in the cache itself, so the finished directory moves into place with one rename.
  const staging = await mkdtemp(join(cacheDirectory, ".install-"));
  try {
    const archivePath = join(staging, entry.archive);
    const actual = await downloadFile(
      url,
      archivePath,
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

    // A directory there holds no valid library (or `force` replaces it).
    if (options.force || !(await installed()))
      await rm(directory, { recursive: true, force: true });
    try {
      await rename(staged, directory);
    } catch (error) {
      // Another install moved the same library into place first.
      if (!(await installed()))
        throw new Alpha2HostInstallError(
          "EXTRACT_FAILED",
          `cannot move ${base} into ${cacheDirectory}`,
          { cause: error },
        );
    }
    return result(true);
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

async function download(
  url: string,
  signal?: AbortSignal,
  notFound = "; the release may not be published yet, or not for this platform",
): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(url, signal ? { signal } : {});
  } catch (error) {
    throw downloadError(url, error, signal);
  }
  if (!response.ok || response.body === null)
    throw new Alpha2HostInstallError(
      "DOWNLOAD_FAILED",
      `${url} answered ${response.status}` +
        (response.status === 404 ? notFound : ""),
    );
  return response;
}

/** Writes `url` to `path`, reporting progress, and returns its SHA-256 in lower-case hex. */
async function downloadFile(
  url: string,
  path: string,
  options: Pick<InstallHostOptions, "onProgress" | "signal">,
  notFound?: string,
): Promise<string> {
  const response = await download(url, options.signal, notFound);
  const length = Number(response.headers.get("content-length"));
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
      Readable.fromWeb(
        response.body as import("node:stream/web").ReadableStream,
      ),
      meter,
      createWriteStream(path),
      ...(options.signal ? [{ signal: options.signal }] : []),
    );
  } catch (error) {
    throw downloadError(url, error, options.signal);
  }
  return hash.digest("hex");
}

function downloadError(
  url: string,
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
    `cannot download ${url}`,
    {
      cause: error,
    },
  );
}

/** Unpacks `archive` into `destination` with the system's `tar`. */
function extract(
  archive: string,
  destination: string,
  signal?: AbortSignal,
): Promise<void> {
  // Windows' own bsdtar reads .zip; a GNU tar earlier on PATH (Git Bash) would not.
  const tar =
    process.platform === "win32"
      ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe")
      : "tar";
  return new Promise((resolvePromise, reject) => {
    // tar runs in `destination` and names the archive relative to it, so neither path is an
    // argument: Windows passes arguments to bsdtar in the ANSI code page, which turns a cache
    // under a folder named in another script into question marks, while the working directory
    // goes through as Unicode. The relative path holds only the installer's own ASCII names.
    const child = spawn(tar, ["-xf", relative(destination, archive)], {
      cwd: destination,
      stdio: ["ignore", "ignore", "pipe"],
      ...(signal ? { signal } : {}),
    });
    let stderr = "";
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      if (stderr.length < 16_384) stderr += chunk;
    });
    child.on("error", (error) =>
      reject(
        signal?.aborted
          ? new Alpha2HostInstallError("ABORTED", "the install was aborted", {
              cause: error,
            })
          : new Alpha2HostInstallError(
              "EXTRACT_FAILED",
              `cannot run ${tar} to unpack the archive`,
              { cause: error },
            ),
      ),
    );
    child.on("close", (code) => {
      if (code === 0) resolvePromise();
      else
        reject(
          new Alpha2HostInstallError(
            "EXTRACT_FAILED",
            `${tar} exited with ${code} unpacking the archive${stderr ? `:\n${stderr.trim()}` : ""}`,
          ),
        );
    });
  });
}

function isFile(path: string): boolean {
  try {
    return existsSync(path) && statSync(path).isFile();
  } catch {
    return false;
  }
}
