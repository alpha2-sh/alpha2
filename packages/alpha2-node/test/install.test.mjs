import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
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

import {
  hostCacheDirectory,
  hostExecutableName,
  hostTarget,
  installHost,
  resolveHostBinary,
} from "../dist/index.js";

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
