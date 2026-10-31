import assert from "node:assert/strict";
import { execFile, execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import {
  cp,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { promisify } from "node:util";

import { startForwardProxy } from "../../../scripts/packaging/forward-proxy.mjs";

import {
  hostCacheDirectory,
  hostExecutableName,
  hostTarget,
  installHost,
  installOpenH264,
  resolveHostBinary,
} from "../dist/index.js";
import { HTTP_SETTINGS } from "../dist/http.js";
import {
  INSTALL_MACHINE,
  LINUX_GLIBC_FLOOR,
  OPENH264_TARGETS,
} from "../dist/install.js";

const { version } = JSON.parse(
  await readFile(new URL("../package.json", import.meta.url), "utf8"),
);

const run = promisify(execFile);

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
async function serveRelease({ checksum, https } = {}) {
  const root = await scratch("alpha2-node-release-");
  const base = `alpha2-${version}-${hostTarget()}-bundled`;
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
  const archiveSha256 = createHash("sha256").update(bytes).digest("hex");
  const digest = checksum ?? archiveSha256;
  const files = new Map([
    [`/v${version}/${archive}`, bytes],
    [
      `/v${version}/checksums.sha256`,
      Buffer.from(`${"0".repeat(64)}  other.tar.gz\n${digest}  ${archive}\n`),
    ],
  ]);
  const requests = [];
  const handler = (request, response) => {
    requests.push(request.url);
    const body = files.get(request.url);
    response.writeHead(body ? 200 : 404, {
      "content-length": body?.length ?? 0,
    });
    response.end(body);
  };
  const server = https
    ? createHttpsServer(await tlsFixture(), handler)
    : createServer(handler);
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  after(() => server.close());
  return {
    base,
    archiveSha256,
    requests,
    downloadUrl: `${https ? "https" : "http"}://127.0.0.1:${server.address().port}`,
  };
}

const tlsDirectory = fileURLToPath(new URL("fixtures/tls/", import.meta.url));

/** A server certificate for 127.0.0.1 and localhost, issued by the test CA in `ca.pem`. */
async function tlsFixture() {
  return {
    key: await readFile(join(tlsDirectory, "server-key.pem")),
    cert: await readFile(join(tlsDirectory, "server.pem")),
  };
}

const supported = hostTarget() !== undefined;

/** Replaces members of the installer's machine seam until the test ends. */
function standIn(t, replacements) {
  const saved = { ...INSTALL_MACHINE };
  Object.assign(INSTALL_MACHINE, replacements);
  t.after(() => Object.assign(INSTALL_MACHINE, saved));
}

const installModule = new URL("../dist/install.js", import.meta.url).href;
const proxyVariables = [
  "HTTP_PROXY",
  "HTTPS_PROXY",
  "NO_PROXY",
  "http_proxy",
  "https_proxy",
  "no_proxy",
];

/**
 * Runs `installHost(options)` in a child Node with `env` added to an environment without proxy
 * variables (agents read them once per process) and resolves with its result or its error's
 * code and message.
 */
async function installInChild(options, env) {
  const environment = { ...process.env };
  for (const name of proxyVariables) delete environment[name];
  const script =
    `import { installHost } from ${JSON.stringify(installModule)};\n` +
    `try { console.log(JSON.stringify(await installHost(${JSON.stringify(options)}))); }\n` +
    "catch (error) { console.log(JSON.stringify({ code: error.code, message: error.message })); }";
  const { stdout } = await run(
    process.execPath,
    ["--input-type=module", "-e", script],
    { env: { ...environment, ...env } },
  );
  return JSON.parse(stdout);
}

test("the target follows the machine: Rosetta uses macos-arm64, Windows arm64 is refused", async (t) => {
  assert.equal(hostTarget("linux", "x64"), "linux-x64");
  assert.equal(hostTarget("linux", "arm64"), "linux-arm64");
  assert.equal(hostTarget("darwin", "arm64"), "macos-arm64");
  assert.equal(hostTarget("win32", "x64"), "win-x64");
  assert.equal(hostTarget("win32", "arm64"), undefined);
  assert.equal(hostTarget("freebsd", "x64"), undefined);
  standIn(t, { appleSilicon: () => true });
  assert.equal(hostTarget("darwin", "x64"), "macos-arm64");
  INSTALL_MACHINE.appleSilicon = () => false;
  assert.equal(hostTarget("darwin", "x64"), undefined);

  const cacheDirectory = await scratch("alpha2-node-cache-");
  const refusals = [
    ["darwin", "x64", /^Intel Macs are not supported$/],
    [
      "win32",
      "arm64",
      /no Windows on ARM alpha2-host; an x64 Node\.js\/Python runs the x64 host under emulation \(not supported or tested\)/,
    ],
    ["freebsd", "x64", /no alpha2-host archive is published for freebsd-x64/],
  ];
  for (const [platform, arch, message] of refusals) {
    INSTALL_MACHINE.platform = () => platform;
    INSTALL_MACHINE.arch = () => arch;
    for (const install of [installHost, installOpenH264])
      await assert.rejects(
        install({ cacheDirectory, downloadUrl: "http://127.0.0.1:9" }),
        { code: "UNSUPPORTED_PLATFORM", message },
      );
  }
  assert.deepEqual(await readdir(cacheDirectory), []);
});

test(
  "musl and old glibc are refused before any download, naming the floor",
  { skip: !supported && "no archive is published for this platform" },
  async (t) => {
    const release = await serveRelease();
    const cacheDirectory = await scratch("alpha2-node-cache-");
    standIn(t, { platform: () => "linux", arch: () => "x64" });
    const floor = LINUX_GLIBC_FLOOR;
    const older = `2.${Number(floor.split(".")[1]) - 1}`;
    const cases = [
      [
        { kind: "musl" },
        `alpha2-host is built for glibc ${floor} or later; this system's C library is musl (Alpine and other musl distributions are not supported)`,
      ],
      [
        { kind: "unknown" },
        `glibc ${floor} or later; this system's C library is not glibc`,
      ],
      [
        { kind: "glibc", version: older },
        `needs glibc ${floor} or later; this system has glibc ${older}. Build it from source (cargo install alpha2-host with FFmpeg 8 development files) and set ALPHA2_HOST_PATH`,
      ],
      // Compared as numbers: 2.9 sorts after the floor as text but is older.
      [{ kind: "glibc", version: "2.9" }, "this system has glibc 2.9."],
    ];
    for (const [libc, text] of cases) {
      INSTALL_MACHINE.linuxLibc = () => libc;
      const message = new RegExp(text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
      for (const install of [installHost, installOpenH264])
        await assert.rejects(
          install({ cacheDirectory, downloadUrl: release.downloadUrl }),
          { code: "UNSUPPORTED_PLATFORM", message },
        );
    }
    assert.deepEqual(release.requests, []);
    assert.deepEqual(await readdir(cacheDirectory), []);
  },
);

test("the glibc floor is the minimum_os the FFmpeg lock file names for the Linux archives", async () => {
  const manifest = await readFile(
    new URL("../../../release/ffmpeg-lgpl/manifest.toml", import.meta.url),
    "utf8",
  );
  const floors = Object.fromEntries(
    [
      ...manifest.matchAll(
        /^\[assets\.(linux-[^\]]+)\]\r?\n(?:[^[\r\n].*\n)*?minimum_os = "([^"]*)"/gm,
      ),
    ].map(([, target, minimumOs]) => [target, minimumOs]),
  );
  assert.deepEqual(floors, {
    "linux-x64": `glibc ${LINUX_GLIBC_FLOOR}`,
    "linux-arm64": `glibc ${LINUX_GLIBC_FLOOR}`,
  });
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
      verifiedBy: "checksums",
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
    assert.deepEqual(
      await installHost({ cacheDirectory, downloadUrl: release.downloadUrl }),
      {
        hostPath,
        directory: join(cacheDirectory, release.base),
        downloaded: false,
      },
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
  "installs into a cache whose path has an emoji and characters outside the code page",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    const release = await serveRelease();
    const root = await scratch("alpha2-node-named-");
    // Hangul and an emoji are outside every Western ANSI code page, and the accented letter is
    // outside the East Asian ones.
    for (const name of [
      "\uC54C\uD30C2 \uCE90\uC2DC \u{1F600}",
      "caf\u00e9 cache",
    ]) {
      const cacheDirectory = join(root, name);
      const result = await installHost({
        cacheDirectory,
        downloadUrl: release.downloadUrl,
      });
      assert.equal(result.downloaded, true);
      assert.equal(result.hostPath.startsWith(cacheDirectory), true);
      assert.equal(await readFile(result.hostPath, "utf8"), "host");
      assert.deepEqual(await readdir(cacheDirectory), [release.base]);
    }
  },
);

test(
  "force over a directory that cannot be renamed fails and leaves it intact",
  { skip: !supported && "no archive is published for this platform" },
  async (t) => {
    const release = await serveRelease();
    const cacheDirectory = await scratch("alpha2-node-cache-");
    const { directory } = await installHost({
      cacheDirectory,
      downloadUrl: release.downloadUrl,
    });
    let holder;
    if (process.platform === "win32") {
      // A file open without delete sharing, as a scanner may hold one, keeps Windows from renaming
      // the directory it is in. (A running program does not: its image allows the rename.)
      const held = join(directory, "bin", hostExecutableName());
      holder = spawn(
        "powershell.exe",
        [
          "-NoProfile",
          "-Command",
          `$file = [System.IO.File]::Open('${held}', 'Open', 'Read', 'Read'); ` +
            "Write-Output held; Start-Sleep 60",
        ],
        { stdio: ["ignore", "pipe", "ignore"] },
      );
      await new Promise((done) => holder.stdout.once("data", done));
      t.after(async () => {
        holder.kill();
        await new Promise((done) => holder.once("exit", done));
      });
    } else {
      standIn(t, {
        rename: (from, to) =>
          from === directory
            ? Promise.reject(
                Object.assign(new Error("resource busy"), { code: "EBUSY" }),
              )
            : rename(from, to),
      });
    }
    const listing = async () =>
      (await readdir(directory, { recursive: true })).sort();
    const before = await listing();
    await assert.rejects(
      installHost({
        cacheDirectory,
        downloadUrl: release.downloadUrl,
        force: true,
      }),
      {
        code: "EXTRACT_FAILED",
        message:
          /it is in use \(is an alpha2-host from it running\?\); the installed copy is unchanged/,
      },
    );
    assert.deepEqual(await listing(), before);
    assert.deepEqual(await readdir(cacheDirectory), [release.base]);
  },
);

test(
  "a rename refused twice then allowed succeeds",
  { skip: !supported && "no archive is published for this platform" },
  async (t) => {
    const release = await serveRelease();
    const cacheDirectory = await scratch("alpha2-node-cache-");
    let refused = 0;
    standIn(t, {
      retriesRenames: () => true,
      rename: (from, to) => {
        if (refused < 2) {
          refused += 1;
          return Promise.reject(
            Object.assign(new Error("operation not permitted"), {
              code: "EPERM",
            }),
          );
        }
        return rename(from, to);
      },
    });
    const result = await installHost({
      cacheDirectory,
      downloadUrl: release.downloadUrl,
    });
    assert.equal(result.downloaded, true);
    assert.equal(refused, 2);
    assert.deepEqual(await readdir(cacheDirectory), [release.base]);
  },
);

test(
  "installs through an HTTP proxy named by HTTP_PROXY, and NO_PROXY bypasses it",
  { skip: !supported && "no archive is published for this platform" },
  async (t) => {
    const release = await serveRelease();
    const proxy = await startForwardProxy();
    t.after(proxy.close);
    const proxyUrl = `http://127.0.0.1:${proxy.port}`;
    const through = await installInChild(
      {
        cacheDirectory: await scratch("alpha2-node-cache-"),
        downloadUrl: release.downloadUrl,
      },
      { HTTP_PROXY: proxyUrl },
    );
    assert.equal(through.downloaded, true, through.message);
    assert.deepEqual(proxy.lines, [
      `GET ${release.downloadUrl}/v${version}/checksums.sha256`,
      `GET ${release.downloadUrl}/v${version}/${release.base}.${process.platform === "win32" ? "zip" : "tar.gz"}`,
    ]);

    const bypassed = await installInChild(
      {
        cacheDirectory: await scratch("alpha2-node-cache-"),
        downloadUrl: release.downloadUrl,
      },
      { HTTP_PROXY: proxyUrl, NO_PROXY: "127.0.0.1" },
    );
    assert.equal(bypassed.downloaded, true, bypassed.message);
    assert.equal(proxy.lines.length, 2);
  },
);

test(
  "installs through HTTPS_PROXY by CONNECT with the mirror's CA from the system store",
  {
    skip:
      (process.platform !== "linux" &&
        "SSL_CERT_FILE moves the system store on Linux only") ||
      (!supported && "no archive is published for this platform"),
  },
  async (t) => {
    const release = await serveRelease({ https: true });
    const proxy = await startForwardProxy();
    t.after(proxy.close);
    const result = await installInChild(
      {
        cacheDirectory: await scratch("alpha2-node-cache-"),
        downloadUrl: release.downloadUrl,
      },
      {
        HTTPS_PROXY: `http://127.0.0.1:${proxy.port}`,
        SSL_CERT_FILE: join(tlsDirectory, "ca.pem"),
      },
    );
    assert.equal(result.downloaded, true, result.message);
    const authority = new URL(release.downloadUrl).host;
    assert.deepEqual(proxy.lines, [
      `CONNECT ${authority}`,
      `CONNECT ${authority}`,
    ]);
  },
);

test("a refused connection names the cause code and the remedy", async () => {
  const result = await installInChild(
    {
      cacheDirectory: await scratch("alpha2-node-cache-"),
      downloadUrl: "https://alpha2.invalid",
    },
    { HTTPS_PROXY: "http://127.0.0.1:9" },
  );
  assert.equal(result.code, "DOWNLOAD_FAILED", result.message);
  assert.match(result.message, /^cannot download https:\/\/alpha2\.invalid\//);
  assert.match(result.message, /ECONNREFUSED/);
  assert.match(result.message, /HTTPS_PROXY/);
  assert.match(result.message, /NODE_EXTRA_CA_CERTS/);
  assert.match(result.message, /ALPHA2_HOST_DOWNLOAD_URL/);
});

test(
  "a redirect to http from https is refused",
  { skip: !supported && "no archive is published for this platform" },
  async (t) => {
    const plain = await serveRelease();
    const server = createHttpsServer(
      await tlsFixture(),
      (request, response) => {
        response.writeHead(302, {
          location: `${plain.downloadUrl}${request.url}`,
        });
        response.end();
      },
    );
    await new Promise((done) => server.listen(0, "127.0.0.1", done));
    t.after(() => server.close());
    const result = await installInChild(
      {
        cacheDirectory: await scratch("alpha2-node-cache-"),
        downloadUrl: `https://127.0.0.1:${server.address().port}`,
      },
      { NODE_EXTRA_CA_CERTS: join(tlsDirectory, "ca.pem") },
    );
    assert.equal(result.code, "DOWNLOAD_FAILED", result.message);
    assert.match(
      result.message,
      /redirected to an insecure URL http:\/\/127\.0\.0\.1/,
    );
    assert.deepEqual(plain.requests, []);
  },
);

test(
  "a stalled download fails after the idle timeout",
  { skip: !supported && "no archive is published for this platform" },
  async (t) => {
    const release = await serveRelease();
    // The checksums come through the real mirror; the archive starts and then stops.
    const server = createServer((request, response) => {
      if (request.url.endsWith("checksums.sha256")) {
        fetch(`${release.downloadUrl}${request.url}`)
          .then((upstream) => upstream.arrayBuffer())
          .then((body) => response.end(Buffer.from(body)));
        return;
      }
      response.writeHead(200, { "content-length": 1000 });
      response.write("x".repeat(10));
    });
    await new Promise((done) => server.listen(0, "127.0.0.1", done));
    t.after(() => {
      server.closeAllConnections();
      server.close();
    });
    const saved = HTTP_SETTINGS.idleTimeoutMs;
    HTTP_SETTINGS.idleTimeoutMs = 200;
    t.after(() => (HTTP_SETTINGS.idleTimeoutMs = saved));
    const cacheDirectory = await scratch("alpha2-node-cache-");
    await assert.rejects(
      installHost({
        cacheDirectory,
        downloadUrl: `http://127.0.0.1:${server.address().port}`,
      }),
      { code: "DOWNLOAD_FAILED", message: /ETIMEDOUT no data for 0\.2 s/ },
    );
    assert.deepEqual(await readdir(cacheDirectory), []);
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

/**
 * This package's built installer copied beside its manifest into a scratch directory, with
 * `pins` (an object, or text as it is) written as its `dist/host-archives.json`, as the published
 * package carries it. Resolves with that copy's `installHost`.
 */
async function pinnedInstaller(pins) {
  const root = await scratch("alpha2-node-pinned-");
  await cp(
    fileURLToPath(new URL("../dist", import.meta.url)),
    join(root, "dist"),
    {
      recursive: true,
    },
  );
  await cp(
    fileURLToPath(new URL("../package.json", import.meta.url)),
    join(root, "package.json"),
  );
  await writeFile(
    join(root, "dist", "host-archives.json"),
    typeof pins === "string" ? pins : JSON.stringify(pins),
  );
  const module = await import(
    pathToFileURL(join(root, "dist", "install.js")).href
  );
  return module.installHost;
}

const checksumsRequest = (requests) =>
  requests.filter((path) => path.endsWith("/checksums.sha256"));

test(
  "a release pin wins over checksums.sha256 and over a mirror",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    // The mirror serves an archive and a checksums file that agree with each other.
    const release = await serveRelease();
    const other = "b".repeat(64);
    const pinnedElsewhere = await pinnedInstaller({
      version,
      archives: { [hostTarget()]: other },
    });
    const cacheDirectory = await scratch("alpha2-node-cache-");
    await assert.rejects(
      pinnedElsewhere({ cacheDirectory, downloadUrl: release.downloadUrl }),
      {
        code: "CHECKSUM_MISMATCH",
        message: `${release.base}.${process.platform === "win32" ? "zip" : "tar.gz"} has SHA-256 ${release.archiveSha256}; the release pins ${other}`,
      },
    );
    assert.deepEqual(await readdir(cacheDirectory), []);

    const pinned = await pinnedInstaller({
      version,
      archives: { "linux-x64": other, [hostTarget()]: release.archiveSha256 },
    });
    const result = await pinned({
      cacheDirectory,
      downloadUrl: release.downloadUrl,
    });
    assert.equal(result.downloaded, true);
    assert.equal(result.verifiedBy, "release-pin");
    assert.deepEqual(checksumsRequest(release.requests), []);
  },
);

test(
  "without a pin the release checksums are used",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    const release = await serveRelease();
    const unpinned = await installHost({
      cacheDirectory: await scratch("alpha2-node-cache-"),
      downloadUrl: release.downloadUrl,
    });
    assert.equal(unpinned.verifiedBy, "checksums");
    assert.equal(checksumsRequest(release.requests).length, 1);

    // Pins of another version, or without this platform, do not apply.
    for (const pins of [
      { version: "0.0.1", archives: { [hostTarget()]: "b".repeat(64) } },
      { version, archives: {} },
    ]) {
      const result = await (
        await pinnedInstaller(pins)
      )({
        cacheDirectory: await scratch("alpha2-node-cache-"),
        downloadUrl: release.downloadUrl,
      });
      assert.equal(result.verifiedBy, "checksums");
    }
    assert.equal(checksumsRequest(release.requests).length, 3);

    const cacheDirectory = await scratch("alpha2-node-cache-");
    const { stdout } = await run(
      process.execPath,
      [fileURLToPath(new URL("../dist/cli.js", import.meta.url)), "install"],
      {
        env: {
          ...process.env,
          ALPHA2_HOST_CACHE_DIR: cacheDirectory,
          ALPHA2_HOST_DOWNLOAD_URL: release.downloadUrl,
        },
      },
    );
    assert.equal(
      stdout,
      `installed ${join(cacheDirectory, release.base, "bin", hostExecutableName())} ` +
        "(checked against the release's checksums.sha256)\n",
    );
  },
);

test(
  "a damaged pins file is refused before anything is downloaded",
  { skip: !supported && "no archive is published for this platform" },
  async () => {
    const release = await serveRelease();
    for (const [pins, reason] of [
      ["{", /cannot read the release pins in .*host-archives\.json \(/],
      [{ archives: {} }, /\(it has no version and archives\)/],
      [
        { version, archives: { [hostTarget()]: "ABC" } },
        /its [a-z0-9-]+ entry is not a SHA-256\); reinstall @alpha2-sh\/node$/,
      ],
    ]) {
      const cacheDirectory = await scratch("alpha2-node-cache-");
      await assert.rejects(
        (await pinnedInstaller(pins))({
          cacheDirectory,
          downloadUrl: release.downloadUrl,
        }),
        { code: "CHECKSUM_MISMATCH", message: reason },
      );
      assert.deepEqual(await readdir(cacheDirectory), []);
    }
    assert.deepEqual(release.requests, []);
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

test("the install command has no --thin", async () => {
  await assert.rejects(
    run(process.execPath, [cli, "install", "--thin"]),
    (error) => {
      assert.equal(error.code, 2);
      assert.match(
        error.stderr,
        /usage: alpha2-node install \[--openh264\] \[--force\]/,
      );
      return true;
    },
  );
});
