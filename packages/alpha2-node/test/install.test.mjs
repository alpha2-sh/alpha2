import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { promisify } from "node:util";

import {
  hostCacheDirectory,
  hostExecutableName,
  hostTarget,
  installHost,
  installOpenH264,
  resolveHostBinary,
} from "../dist/index.js";
import { OPENH264_TARGETS } from "../dist/install.js";

const { version } = JSON.parse(
  await readFile(new URL("../package.json", import.meta.url), "utf8"),
);

/** Directories the tests made, removed once they have all run. */
const scratchDirectories = [];

async function scratch(prefix) {
  const directory = await mkdtemp(join(tmpdir(), prefix));
  scratchDirectories.push(directory);
  return directory;
}

after(async () => {
  await Promise.all(
    scratchDirectories.map((directory) =>
      rm(directory, { recursive: true, force: true }),
    ),
  );
});

/**
 * A release of this package's version for this platform, as `installHost` downloads it: the
 * archive and `checksums.sha256` under `/v<version>/`, served from localhost. `requests` lists
 * every path asked for.
 */
async function serveRelease({ kind = "bundled", checksum } = {}) {
  const root = await scratch("alpha2-node-release-");
  const base = `alpha2-${version}-${hostTarget()}-${kind}`;
  const archive = `${base}.${process.platform === "win32" ? "zip" : "tar.gz"}`;
  await mkdir(join(root, base, "bin"), { recursive: true });
  await mkdir(join(root, base, "lib"));
  await writeFile(join(root, base, "bin", hostExecutableName()), "host");
  await writeFile(join(root, base, "lib", "libavutil.so.60.1.100"), "lib");
  if (process.platform !== "win32")
    await symlink(
      "libavutil.so.60.1.100",
      join(root, base, "lib", "libavutil.so.60"),
    );
  const archivePath = join(root, archive);
  if (process.platform === "win32")
    execFileSync(join(process.env.SystemRoot, "System32", "tar.exe"), [
      "-a",
      "-cf",
      archivePath,
      "-C",
      root,
      base,
    ]);
  else execFileSync("tar", ["-czf", archivePath, "-C", root, base]);
  const bytes = await readFile(archivePath);
  const digest = checksum ?? createHash("sha256").update(bytes).digest("hex");
  const files = new Map([
    [`/v${version}/${archive}`, bytes],
    [
      `/v${version}/checksums.sha256`,
      Buffer.from(`${"0".repeat(64)}  other.tar.gz\n${digest}  ${archive}\n`),
    ],
  ]);
  const requests = [];
  const server = createServer((request, response) => {
    requests.push(request.url);
    const body = files.get(request.url);
    response.writeHead(body ? 200 : 404, {
      "content-length": body?.length ?? 0,
    });
    response.end(body);
  });
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  after(() => server.close());
  return {
    base,
    requests,
    downloadUrl: `http://127.0.0.1:${server.address().port}`,
  };
}

const supported = hostTarget() !== undefined;

test("the release target is named for each published platform", () => {
  assert.equal(hostTarget("linux", "x64"), "linux-x64");
  assert.equal(hostTarget("linux", "arm64"), "linux-arm64");
  assert.equal(hostTarget("darwin", "arm64"), "macos-arm64");
  assert.equal(hostTarget("win32", "x64"), "win-x64");
  assert.equal(hostTarget("darwin", "x64"), undefined);
  assert.equal(hostTarget("freebsd", "x64"), undefined);
});

test("the cache is ALPHA2_HOST_CACHE_DIR when it is set, and a blank one is unset", async () => {
  const root = await scratch("alpha2-node-cache-");
  const saved = process.env.ALPHA2_HOST_CACHE_DIR;
  try {
    process.env.ALPHA2_HOST_CACHE_DIR = "cache";
    assert.equal(hostCacheDirectory(root), join(root, "cache"));
    process.env.ALPHA2_HOST_CACHE_DIR = " ";
    assert.match(hostCacheDirectory(root), /alpha2$/);
    assert.notEqual(hostCacheDirectory(root), join(root, "cache"));
  } finally {
    if (saved === undefined) delete process.env.ALPHA2_HOST_CACHE_DIR;
    else process.env.ALPHA2_HOST_CACHE_DIR = saved;
  }
});

test(
  "an installed archive is found by binary resolution and is not downloaded again",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    const release = await serveRelease();
    const cacheDirectory = await scratch("alpha2-node-cache-");
    let progress;
    const result = await installHost({
      cacheDirectory,
      downloadUrl: `${release.downloadUrl}/`,
      onProgress: (update) => (progress = update),
    });
    const hostPath = join(
      cacheDirectory,
      release.base,
      "bin",
      hostExecutableName(),
    );
    assert.deepEqual(result, {
      hostPath,
      directory: join(cacheDirectory, release.base),
      downloaded: true,
    });
    assert.equal(progress.received, progress.total);
    assert.deepEqual(await readdir(cacheDirectory), [release.base]);
    if (process.platform !== "win32")
      assert.equal(
        await readFile(
          join(cacheDirectory, release.base, "lib", "libavutil.so.60"),
          "utf8",
        ),
        "lib",
      );

    const saved = {
      path: process.env.PATH,
      host: process.env.ALPHA2_HOST_PATH,
    };
    process.env.PATH = "";
    delete process.env.ALPHA2_HOST_PATH;
    try {
      assert.equal(
        resolveHostBinary({
          cacheDirectory,
          resourcesPath: join(cacheDirectory, "none"),
        }),
        hostPath,
      );
    } finally {
      process.env.PATH = saved.path;
      if (saved.host !== undefined) process.env.ALPHA2_HOST_PATH = saved.host;
    }

    const requests = release.requests.length;
    assert.equal(
      (await installHost({ cacheDirectory, downloadUrl: release.downloadUrl }))
        .downloaded,
      false,
    );
    assert.equal(release.requests.length, requests);
    assert.equal(
      (
        await installHost({
          cacheDirectory,
          downloadUrl: release.downloadUrl,
          force: true,
        })
      ).downloaded,
      true,
    );
    assert.deepEqual(await readdir(cacheDirectory), [release.base]);
  },
);

test(
  "an archive whose checksum does not match is refused and leaves nothing in the cache",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    const release = await serveRelease({ checksum: "a".repeat(64) });
    const cacheDirectory = await scratch("alpha2-node-cache-");
    await assert.rejects(
      installHost({ cacheDirectory, downloadUrl: release.downloadUrl }),
      { name: "Alpha2HostInstallError", code: "CHECKSUM_MISMATCH" },
    );
    assert.deepEqual(await readdir(cacheDirectory), []);
  },
);

test(
  "a release that is not published is a download failure",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    const release = await serveRelease();
    const cacheDirectory = await scratch("alpha2-node-cache-");
    await assert.rejects(
      installHost({
        cacheDirectory,
        downloadUrl: `${release.downloadUrl}/missing`,
      }),
      (error) => {
        assert.equal(error.code, "DOWNLOAD_FAILED");
        assert.match(
          error.message,
          /answered 404; the release may not be published/,
        );
        return true;
      },
    );
    assert.deepEqual(await readdir(cacheDirectory), []);
  },
);

test(
  "a kind the platform is not published in is refused before any download",
  {
    skip:
      process.platform !== "win32" && "every other platform has a thin archive",
  },
  async () => {
    await assert.rejects(installHost({ kind: "thin" }), {
      code: "UNSUPPORTED_PLATFORM",
    });
  },
);

/** `printf abracadabra | bzip2 -9`, which stands in for Cisco's archive. */
const FAKE_OPENH264_ARCHIVE = Buffer.from(
  "425a6839314159265359236d4bd800000101803c001000200030cd00947a" +
    "9ea52c8478bb9229c284811b6a5ec0",
  "hex",
);
const FAKE_OPENH264_LIBRARY = "abracadabra";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

/**
 * Serves a stand-in for this platform's OpenH264 archive from localhost, as `<base>/<archive>`,
 * and makes the target table expect it (or the checksums in `overrides`) until the test ends.
 * Returns what a child process needs to see the same table: `preload`, a module to `--import`.
 */
async function serveOpenH264(t, overrides = {}) {
  const target = hostTarget();
  const original = OPENH264_TARGETS[target];
  const entry = {
    ...original,
    archiveSha256: sha256(FAKE_OPENH264_ARCHIVE),
    librarySha256: sha256(FAKE_OPENH264_LIBRARY),
    ...overrides,
  };
  OPENH264_TARGETS[target] = entry;
  t.after(() => {
    OPENH264_TARGETS[target] = original;
  });

  const requests = [];
  const server = createServer((request, response) => {
    requests.push(request.url);
    const body =
      request.url === `/${entry.archive}` ? FAKE_OPENH264_ARCHIVE : undefined;
    response.writeHead(body ? 200 : 404, {
      "content-length": body?.length ?? 0,
    });
    response.end(body);
  });
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  t.after(() => server.close());

  const preload = join(await scratch("alpha2-node-openh264-"), "table.mjs");
  const table = new URL("../dist/install.js", import.meta.url).href;
  await writeFile(
    preload,
    `import { OPENH264_TARGETS } from ${JSON.stringify(table)};\n` +
      `OPENH264_TARGETS[${JSON.stringify(target)}] = ${JSON.stringify(entry)};\n`,
  );
  return {
    entry,
    requests,
    base: `openh264-2.7.0-${target}`,
    downloadUrl: `http://127.0.0.1:${server.address().port}`,
    preload: pathToFileURL(preload).href,
  };
}

const unsupported =
  !supported && "no OpenH264 binary is offered for this platform";

test(
  "OpenH264 installs into the cache and is not downloaded again",
  { skip: unsupported },
  async (t) => {
    const served = await serveOpenH264(t);
    const cacheDirectory = await scratch("alpha2-node-cache-");
    let progress;
    const result = await installOpenH264({
      cacheDirectory,
      downloadUrl: `${served.downloadUrl}/`,
      onProgress: (update) => (progress = update),
    });
    const directory = join(cacheDirectory, served.base);
    const libraryPath = join(directory, served.entry.library);
    assert.deepEqual(result, {
      libraryPath,
      directory,
      downloaded: true,
      version: "2.7.0",
      notice: "OpenH264 Video Codec provided by Cisco Systems, Inc.",
    });
    assert.equal(progress.received, FAKE_OPENH264_ARCHIVE.length);
    assert.equal(await readFile(libraryPath, "utf8"), FAKE_OPENH264_LIBRARY);
    assert.deepEqual(await readdir(cacheDirectory), [served.base]);
    assert.deepEqual(await readdir(directory), [served.entry.library]);

    const requests = served.requests.length;
    const again = await installOpenH264({ cacheDirectory });
    assert.equal(again.downloaded, false);
    assert.equal(served.requests.length, requests);

    // A library that is not the one expected is replaced.
    await writeFile(libraryPath, "damaged");
    const repaired = await installOpenH264({
      cacheDirectory,
      downloadUrl: served.downloadUrl,
    });
    assert.equal(repaired.downloaded, true);
    assert.equal(await readFile(libraryPath, "utf8"), FAKE_OPENH264_LIBRARY);

    const forced = await installOpenH264({
      cacheDirectory,
      downloadUrl: served.downloadUrl,
      force: true,
    });
    assert.equal(forced.downloaded, true);
    assert.equal(served.requests.length, requests + 2);
    assert.deepEqual(await readdir(cacheDirectory), [served.base]);
  },
);

test(
  "an OpenH264 archive or library whose checksum does not match leaves nothing in the cache",
  { skip: unsupported },
  async (t) => {
    for (const overrides of [
      { archiveSha256: "a".repeat(64) },
      { librarySha256: "b".repeat(64) },
    ]) {
      await t.test(Object.keys(overrides)[0], async (t) => {
        const served = await serveOpenH264(t, overrides);
        const cacheDirectory = await scratch("alpha2-node-cache-");
        await assert.rejects(
          installOpenH264({ cacheDirectory, downloadUrl: served.downloadUrl }),
          { name: "Alpha2HostInstallError", code: "CHECKSUM_MISMATCH" },
        );
        assert.deepEqual(await readdir(cacheDirectory), []);
      });
    }
  },
);

test(
  "OpenH264 is downloaded from ALPHA2_OPENH264_DOWNLOAD_URL when no URL is given",
  { skip: unsupported },
  async (t) => {
    const served = await serveOpenH264(t);
    const cacheDirectory = await scratch("alpha2-node-cache-");
    const saved = process.env.ALPHA2_OPENH264_DOWNLOAD_URL;
    process.env.ALPHA2_OPENH264_DOWNLOAD_URL = `${served.downloadUrl}/missing`;
    try {
      await assert.rejects(installOpenH264({ cacheDirectory }), (error) => {
        assert.equal(error.code, "DOWNLOAD_FAILED");
        assert.match(error.message, /\/missing\/.* answered 404/);
        return true;
      });
    } finally {
      if (saved === undefined) delete process.env.ALPHA2_OPENH264_DOWNLOAD_URL;
      else process.env.ALPHA2_OPENH264_DOWNLOAD_URL = saved;
    }
    assert.deepEqual(await readdir(cacheDirectory), []);
  },
);

test(
  "an aborted OpenH264 install leaves nothing in the cache",
  { skip: unsupported },
  async (t) => {
    const served = await serveOpenH264(t);
    const cacheDirectory = await scratch("alpha2-node-cache-");
    await assert.rejects(
      installOpenH264({
        cacheDirectory,
        downloadUrl: served.downloadUrl,
        signal: AbortSignal.abort(),
      }),
      { code: "ABORTED" },
    );
    assert.deepEqual(await readdir(cacheDirectory), []);
  },
);

test(
  "a platform without an OpenH264 binary is refused before any download",
  { skip: unsupported },
  async (t) => {
    const target = hostTarget();
    const original = OPENH264_TARGETS[target];
    delete OPENH264_TARGETS[target];
    t.after(() => {
      OPENH264_TARGETS[target] = original;
    });
    const cacheDirectory = await scratch("alpha2-node-cache-");
    await assert.rejects(installOpenH264({ cacheDirectory }), {
      code: "UNSUPPORTED_PLATFORM",
    });
    assert.deepEqual(await readdir(cacheDirectory), []);
  },
);

const cli = fileURLToPath(new URL("../dist/cli.js", import.meta.url));
const run = promisify(execFile);

test(
  "the command line installs OpenH264 and prints Cisco's notice every time",
  { skip: unsupported },
  async (t) => {
    const served = await serveOpenH264(t);
    const cacheDirectory = await scratch("alpha2-node-cache-");
    const install = () =>
      run(
        process.execPath,
        ["--import", served.preload, cli, "install", "--openh264"],
        {
          env: {
            ...process.env,
            ALPHA2_HOST_CACHE_DIR: cacheDirectory,
            ALPHA2_OPENH264_DOWNLOAD_URL: served.downloadUrl,
          },
        },
      );
    const libraryPath = join(cacheDirectory, served.base, served.entry.library);
    for (const first of [
      `installed OpenH264 2.7.0: ${libraryPath}`,
      `already installed: ${libraryPath}`,
    ]) {
      const lines = (await install()).stdout.trimEnd().split(/\r?\n/);
      assert.equal(lines[0], first);
      assert.equal(
        lines[1],
        "OpenH264 Video Codec provided by Cisco Systems, Inc.",
      );
      assert.match(lines.at(-1), /set ALPHA2_OPENH264=0 to turn it off\.$/);
    }
  },
);

test("the command line refuses --openh264 with --thin", async () => {
  await assert.rejects(
    run(process.execPath, [cli, "install", "--openh264", "--thin"]),
    (error) => {
      assert.equal(error.code, 2);
      assert.match(error.stderr, /--thin/);
      return true;
    },
  );
});
