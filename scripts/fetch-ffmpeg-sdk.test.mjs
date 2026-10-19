// fetch-ffmpeg-sdk.sh against a local fake release: a scratch workspace whose manifest pins
// fixture archives and a `curl` on PATH that copies them; no network.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { delimiter, join, relative, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const realManifest = readFileSync(
  join(root, "release/ffmpeg-lgpl/manifest.toml"),
  "utf8",
);
const tag = /^release_tag = "(.*)"$/m.exec(realManifest)[1];
const policy = realManifest.slice(0, realManifest.indexOf("[assets."));

function array(key) {
  const start = realManifest.indexOf(`${key} = [`);
  const end = realManifest.indexOf("]", start);
  return [...realManifest.slice(start, end).matchAll(/^\s*"([^"]+)"/gm)].map(
    (match) => match[1],
  );
}

// The report's "Applied patches" section for the lock file's patches, as the SDK build writes it.
const appliedPatches = [
  "Applied patches",
  "---------------",
  ...[
    ...realManifest.matchAll(
      /file = "([^"]+)", upstream = "([0-9a-f]+)", sha256 = "([0-9a-f]+)"/g,
    ),
  ].map(
    ([, file, upstream, hash]) => `${file} sha256 ${hash} upstream ${upstream}`,
  ),
  "",
];

const targets = {
  "linux-x64": {
    libraries: array("libraries_linux").map((name) => `lib/${name}`),
    notices: [...array("notices"), ...array("notices_linux")],
    hardware: [
      "--enable-vulkan --disable-vaapi --disable-vdpau --disable-libdrm --disable-v4l2-m2m --disable-libmfx --disable-libvpl",
      "hwdevice: vulkan",
    ],
  },
  "win-x64": {
    libraries: array("libraries_windows").map((name) => `bin/${name}`),
    notices: array("notices"),
    hardware: [
      "--enable-d3d11va --enable-d3d12va --enable-dxva2 --enable-mediafoundation",
      "hwdevice: d3d11va",
      "hwdevice: d3d12va",
      "hwdevice: dxva2",
    ],
  },
};

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function executable(path, body) {
  writeFileSync(path, body);
  chmodSync(path, 0o755);
}

const onWindows = process.platform === "win32";

/** Runs a shell script: Windows starts no file by its shebang, so there Git Bash's bash runs it. */
function script(path, args, options) {
  return onWindows
    ? run("bash", [path, ...args], options)
    : run(path, args, options);
}

/**
 * A path a script printed, as Node names it: Git Bash prints its own form (/c/...), which cygpath
 * turns back into C:\....
 */
function printedPath(text) {
  const path = text.trim();
  return onWindows
    ? spawnSync("cygpath", ["-w", path], { encoding: "utf8" }).stdout.trim()
    : path;
}

function run(command, args, options) {
  return new Promise((done) => {
    const child = spawn(command, args, options);
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("close", (status, signal) =>
      done({ status, signal, stdout, stderr }),
    );
  });
}

// A workspace with copies of the scripts, a fake release, and `bin/` tools ahead of PATH.
function workspace() {
  const dir = mkdtempSync(join(tmpdir(), "alpha2-fetch-sdk-"));
  mkdirSync(join(dir, "scripts/lib"), { recursive: true });
  mkdirSync(join(dir, "release/ffmpeg-lgpl"), { recursive: true });
  mkdirSync(join(dir, "releases"));
  mkdirSync(join(dir, "bin"));
  copyFileSync(
    join(root, "scripts/fetch-ffmpeg-sdk.sh"),
    join(dir, "scripts/fetch-ffmpeg-sdk.sh"),
  );
  chmodSync(join(dir, "scripts/fetch-ffmpeg-sdk.sh"), 0o755);
  copyFileSync(
    join(root, "scripts/lib/windows-tools.sh"),
    join(dir, "scripts/lib/windows-tools.sh"),
  );
  copyFileSync(
    join(root, "release/ffmpeg-lgpl/verify-runtime.sh"),
    join(dir, "release/ffmpeg-lgpl/verify-runtime.sh"),
  );
  chmodSync(join(dir, "release/ffmpeg-lgpl/verify-runtime.sh"), 0o755);
  executable(
    join(dir, "bin/curl"),
    `#!/bin/sh
output=
while [ $# -gt 1 ]; do
  [ "$1" = --output ] && { output=$2; shift; }
  shift
done
echo "$1" >> "${dir}/curl.log"
cp "${dir}/releases/$(basename "$1")" "$output"
`,
  );
  return dir;
}

// Packs an SDK for `target` whose libraries hold `content`, and pins it in the manifest.
function release(dir, pins) {
  let assets = "";
  for (const [target, content] of Object.entries(pins)) {
    const spec = targets[target];
    const name = `ffmpeg-8.0.3-shared-${target}`;
    const tree = join(dir, "pack", content, name);
    for (const file of [
      ...spec.libraries,
      ...spec.notices,
      "include/libavutil/version.h",
    ]) {
      mkdirSync(join(tree, file, ".."), { recursive: true });
      writeFileSync(join(tree, file), `${content}\n`);
    }
    mkdirSync(join(tree, "lib"), { recursive: true });
    const archive = join(dir, "releases", `${name}.tar.gz`);
    // Relative paths: Git Bash's GNU tar reads "C:" in an absolute path as a remote host.
    const tar = run(
      "tar",
      [
        "-czf",
        relative(dir, archive),
        "-C",
        relative(dir, join(tree, "..")),
        name,
      ],
      { cwd: dir },
    );
    const report = join(dir, "releases", `${name}.configure.txt`);
    writeFileSync(
      report,
      [
        `# ${content}`,
        ...appliedPatches,
        ...array("required_configure"),
        ...spec.hardware,
      ].join("\n") + "\n",
    );
    assets += `
[assets.${target}]
archive = "${name}.tar.gz"
sha256 = "${"@archive"}"
report = "${name}.configure.txt"
report_sha256 = "${sha256(report)}"
`;
    pins[target] = { archive, assets, tar };
  }
  return Promise.all(Object.values(pins).map((pin) => pin.tar)).then(() => {
    const text = Object.values(pins)
      .map((pin) => pin.assets.replace("@archive", sha256(pin.archive)))
      .join("");
    writeFileSync(
      join(dir, "release/ffmpeg-lgpl/manifest.toml"),
      policy + text,
    );
  });
}

// The environment's PATH variable, whose name Windows spells as it likes (Path).
const pathKey =
  Object.keys(process.env).find((key) => key.toUpperCase() === "PATH") ??
  "PATH";

function fetchSdk(dir, target, env = {}) {
  return script(join(dir, "scripts/fetch-ffmpeg-sdk.sh"), [target], {
    env: {
      ...process.env,
      [pathKey]: `${join(dir, "bin")}${delimiter}${process.env[pathKey]}`,
      ALPHA2_FFMPEG_CACHE: join(dir, "cache"),
      ...env,
    },
  });
}

function sdkRoot(dir, target) {
  return join(dir, "cache", tag, "sdk", target);
}

test("an archive re-pinned under the same release tag is extracted again", async () => {
  const dir = workspace();
  try {
    await release(dir, { "linux-x64": "v1" });
    const first = await fetchSdk(dir, "linux-x64");
    assert.equal(first.status, 0, first.stderr);
    const sdk = printedPath(first.stdout);
    assert.equal(sdk, sdkRoot(dir, "linux-x64"));
    assert.equal(
      readFileSync(join(sdk, "lib/libavcodec.so.62"), "utf8"),
      "v1\n",
    );

    // A rebuilt archive under the same tag, as a maintainer would pin it.
    await release(dir, { "linux-x64": "v2" });
    const second = await fetchSdk(dir, "linux-x64");
    assert.equal(second.status, 0, second.stderr);
    assert.equal(printedPath(second.stdout), sdk);
    assert.equal(
      readFileSync(join(sdk, "lib/libavcodec.so.62"), "utf8"),
      "v2\n",
      "the SDK is still the archive pinned before",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// An SDK tree for `target` with every library and notice the lock file names.
function sdkTree(dir, target, report, patches) {
  const spec = targets[target];
  const tree = join(dir, target);
  for (const file of [...spec.libraries, ...spec.notices]) {
    mkdirSync(join(tree, file, ".."), { recursive: true });
    writeFileSync(join(tree, file), "");
  }
  const path = join(dir, `${target}.configure.txt`);
  writeFileSync(
    path,
    [
      ...(patches ?? appliedPatches),
      ...array("required_configure"),
      ...report,
    ].join("\n") + "\n",
  );
  return [tree, target, path];
}

function verifyRuntime(dir, args) {
  const verify = join(dir, "release/ffmpeg-lgpl/verify-runtime.sh");
  return onWindows
    ? spawnSync("bash", [verify, ...args], { encoding: "utf8" })
    : spawnSync(verify, args, { encoding: "utf8" });
}

test("each target hardware option is found on its own, wherever the report puts it", () => {
  const dir = workspace();
  writeFileSync(join(dir, "release/ffmpeg-lgpl/manifest.toml"), policy);
  try {
    // NVIDIA's options inside the Linux list and the Windows list reordered: every option is there.
    const linux = verifyRuntime(
      dir,
      sdkTree(dir, "linux-x64", [
        "configuration: --enable-vulkan --enable-ffnvcodec --enable-cuda --enable-cuvid --enable-nvdec --enable-nvenc --disable-vaapi --disable-vdpau --disable-libdrm --disable-v4l2-m2m --disable-libmfx --disable-libvpl",
        "hwdevice: vulkan",
      ]),
    );
    assert.equal(linux.status, 0, linux.stderr);
    const windows = verifyRuntime(
      dir,
      sdkTree(dir, "win-x64", [
        "configuration: --enable-dxva2 --enable-amf --enable-mediafoundation --enable-d3d12va --enable-d3d11va",
        "hwdevice: d3d11va",
        "hwdevice: d3d12va",
        "hwdevice: dxva2",
      ]),
    );
    assert.equal(windows.status, 0, windows.stderr);

    // One option missing fails the check, naming that option alone.
    const missing = verifyRuntime(
      dir,
      sdkTree(dir, "linux-x64", [
        "configuration: --enable-vulkan --disable-vaapi --disable-vdpau --disable-v4l2-m2m --disable-libmfx --disable-libvpl",
        "hwdevice: vulkan",
      ]),
    );
    assert.notEqual(missing.status, 0);
    assert.equal(
      missing.stderr.trim(),
      "FFmpeg configure report is missing target hardware option: --disable-libdrm",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a runtime built without the lock file's patches, or with others, fails", () => {
  const dir = workspace();
  writeFileSync(join(dir, "release/ffmpeg-lgpl/manifest.toml"), policy);
  const hardware = [
    "configuration: --enable-vulkan --disable-vaapi --disable-vdpau --disable-libdrm --disable-v4l2-m2m --disable-libmfx --disable-libvpl",
    "hwdevice: vulkan",
  ];
  const [, , first, second] = appliedPatches;
  try {
    for (const patches of [
      ["Applied patches", "---------------", "none", ""],
      [],
      ["Applied patches", "---------------", first, ""],
      ["Applied patches", "---------------", second, first, ""],
      [
        "Applied patches",
        "---------------",
        first.replace(/sha256 [0-9a-f]+/, `sha256 ${"0".repeat(64)}`),
        second,
        "",
      ],
    ]) {
      const result = verifyRuntime(
        dir,
        sdkTree(dir, "linux-x64", hardware, patches),
      );
      assert.notEqual(result.status, 0, patches.join("|"));
      assert.match(result.stderr, /applied patches are not the lock file's/);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

function systemTool(name) {
  return spawnSync("sh", ["-c", `command -v ${name}`], {
    encoding: "utf8",
  }).stdout.trim();
}

/** A path from the shell as Node opens it: on Windows, a Git Bash path is turned into its own. */
function nativePath(path) {
  return onWindows ? printedPath(path) : path;
}

/**
 * An executable at `path` that runs the CommonJS `source` under this Node. On Windows (no shebangs,
 * and Git Bash cannot read one naming a path with a space) a shell script runs it from a file.
 */
function nodeExecutable(path, source) {
  if (!onWindows) {
    executable(path, `#!${process.execPath}\n${source}`);
    return;
  }
  writeFileSync(`${path}.cjs`, source);
  executable(
    path,
    `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} ${JSON.stringify(`${path}.cjs`)} "$@"\n`,
  );
}

// A `tar` that logs each extraction and then holds it open, so a second fetch starts while the
// first is between extracting and publishing.
function slowTar(dir) {
  executable(
    join(dir, "bin/tar"),
    `#!/bin/sh
case "$1" in -x*) echo extract >> "${dir}/tar.log" ;; esac
"${systemTool("tar")}" "$@" || exit
case "$1" in -x*) sleep 0.6 ;; esac
`,
  );
}

function count(path, line) {
  return existsSync(path)
    ? readFileSync(path, "utf8")
        .split("\n")
        .filter((entry) => entry === line).length
    : 0;
}

const delay = (ms) => new Promise((done) => setTimeout(done, ms));

test("a concurrent first fetch extracts once and never replaces the tree another returned", async () => {
  const dir = workspace();
  try {
    await release(dir, { "linux-x64": "v1" });
    slowTar(dir);
    const library = join(sdkRoot(dir, "linux-x64"), "lib/libavcodec.so.62");
    let returned;
    const first = fetchSdk(dir, "linux-x64").then((result) => {
      returned = existsSync(library) ? statSync(library).ino : undefined;
      return result;
    });
    await delay(200);
    const results = await Promise.all([first, fetchSdk(dir, "linux-x64")]);
    for (const result of results) {
      assert.equal(result.status, 0, result.stderr);
      assert.equal(printedPath(result.stdout), sdkRoot(dir, "linux-x64"));
    }
    assert.equal(count(join(dir, "tar.log"), "extract"), 1, "extractions");
    assert.equal(
      statSync(library).ino,
      returned,
      "the tree the first fetch returned was replaced",
    );
    assert.deepEqual(
      readdirSync(sdkRoot(dir, "linux-x64")).filter((name) =>
        name.startsWith("ffmpeg-"),
      ),
      [],
      "a second copy was published inside the first",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// LLVM stand-ins: llvm-readobj lists two functions and an .rdata constant per DLL; llvm-dlltool
// copies the .def into the .lib, slowly, logging each run.
function llvmTools(dir) {
  executable(
    join(dir, "bin/llvm-readobj"),
    `#!/bin/sh
[ -n "\${FAKE_READOBJ_FAIL:-}" ] && { echo "llvm-readobj: cannot run" >&2; exit 1; }
case "$1" in
  --sections) cat <<'SECTIONS'
Sections [
  Section {
    Number: 1
    Name: .text (2E 74 65 78 74 00 00 00)
    VirtualSize: 0x1000
    VirtualAddress: 0x1000
    Characteristics [ (0x60000020)
      IMAGE_SCN_CNT_CODE (0x20)
      IMAGE_SCN_MEM_EXECUTE (0x20000000)
      IMAGE_SCN_MEM_READ (0x40000000)
    ]
  }
  Section {
    Number: 2
    Name: .rdata (2E 72 64 61 74 61 00 00)
    VirtualSize: 0x800
    VirtualAddress: 0x2000
    Characteristics [ (0x40000040)
      IMAGE_SCN_CNT_INITIALIZED_DATA (0x40)
      IMAGE_SCN_MEM_READ (0x40000000)
    ]
  }
]
SECTIONS
  ;;
  --coff-exports)
    [ -n "\${FAKE_READOBJ_EMPTY:-}" ] && exit 0
    name=$(basename "$2" .dll | sed 's/-[0-9]*$//')
    cat <<EXPORTS
File: $2
Format: COFF-x86-64
Export {
  Ordinal: 1
  Name: \${name}_open
  RVA: 0x1010
}
Export {
  Ordinal: 2
  Name: \${name}_size
  RVA: 0x2010
}
Export {
  Ordinal: 3
  Name: \${name}_close
  RVA: 0x1FF0
}
EXPORTS
  ;;
esac
`,
  );
  executable(
    join(dir, "bin/llvm-dlltool"),
    `#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -d) definition=$2; shift ;;
    -l) library=$2; shift ;;
  esac
  shift
done
echo "$definition" >> "${dir}/dlltool.log"
sleep 0.1
cp "$definition" "$library"
`,
  );
}

function importLibraries(sdk) {
  return array("libraries_windows").map((dll) =>
    join(sdk, "lib", `${dll.replace(/-\d+\.dll$/, "")}.lib`),
  );
}

test("concurrent Windows fetches make each import library once, with its exports", async () => {
  const dir = workspace();
  try {
    await release(dir, { "win-x64": "v1" });
    llvmTools(dir);
    const first = fetchSdk(dir, "win-x64");
    await delay(150);
    const results = await Promise.all([first, fetchSdk(dir, "win-x64")]);
    for (const result of results) {
      assert.equal(result.status, 0, result.stderr);
    }
    const libraries = importLibraries(sdkRoot(dir, "win-x64"));
    assert.equal(
      readFileSync(join(dir, "dlltool.log"), "utf8").trim().split("\n").length,
      libraries.length,
      "import libraries made",
    );
    for (const library of libraries) {
      assert.match(readFileSync(library, "utf8"), /^EXPORTS\n {2}\w+_open/m);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A running process to own a lock. Git Bash's `ps` knows only its own PIDs, so on Windows the owner
 * is a Git Bash process.
 */
async function lockOwner() {
  if (!onWindows) {
    const owner = spawn("sleep", ["30"]);
    return { pid: owner.pid, kill: () => owner.kill("SIGKILL") };
  }
  const owner = spawn("sh", ["-c", "echo $$; exec sleep 30"]);
  const pid = await new Promise((done) =>
    owner.stdout.once("data", (chunk) => done(String(chunk).trim())),
  );
  // During `exec`, ps shows neither shell nor sleep: the owner is ready once ps shows the sleep.
  for (
    let tries = 0;
    tries < 100 &&
    spawnSync("sh", ["-c", `ps -p ${pid} | grep -q sleep`]).status !== 0;
    tries += 1
  )
    await delay(50);
  return {
    pid,
    kill: () => spawnSync("sh", ["-c", `kill -9 ${pid}`]),
  };
}

test("a fetch waits for the lock's owner, and takes over a lock whose owner is gone", async () => {
  const dir = workspace();
  try {
    await release(dir, { "linux-x64": "v1" });
    const lock = join(dir, "cache", tag, "sdk", ".lock-linux-x64");
    mkdirSync(lock, { recursive: true });
    const owner = await lockOwner();
    writeFileSync(join(lock, "pid"), `${owner.pid}\n`);
    let finished = false;
    const waiting = fetchSdk(dir, "linux-x64").then((result) => {
      finished = true;
      return result;
    });
    await delay(1500);
    assert.equal(finished, false, "the fetch went ahead of the lock's owner");
    owner.kill();
    const result = await waiting;
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stderr, /waiting for another fetch/);
    assert.equal(existsSync(lock), false, "the lock is released");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an llvm-readobj that fails or lists no exports fails the fetch and publishes nothing", async () => {
  const dir = workspace();
  try {
    await release(dir, { "win-x64": "v1" });
    llvmTools(dir);
    const sdk = sdkRoot(dir, "win-x64");
    for (const env of [
      { FAKE_READOBJ_FAIL: "1" },
      { FAKE_READOBJ_EMPTY: "1" },
    ]) {
      const result = await fetchSdk(dir, "win-x64", env);
      assert.notEqual(result.status, 0, `${JSON.stringify(env)} passed`);
      assert.equal(existsSync(sdk), false, "an SDK was published");
    }
    const result = await fetchSdk(dir, "win-x64");
    assert.equal(result.status, 0, result.stderr);
    for (const library of importLibraries(sdk)) {
      assert.match(readFileSync(library, "utf8"), /^ {2}\w+_open/m);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an export outside the DLL's executable sections is imported as DATA", async () => {
  const dir = workspace();
  try {
    await release(dir, { "win-x64": "v1" });
    llvmTools(dir);
    const result = await fetchSdk(dir, "win-x64");
    assert.equal(result.status, 0, result.stderr);
    assert.equal(
      readFileSync(join(sdkRoot(dir, "win-x64"), "lib/avutil.def"), "utf8"),
      "LIBRARY avutil-60.dll\nEXPORTS\n  avutil_open\n  avutil_size DATA\n  avutil_close\n",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a download that stalls times out and is retried instead of hanging", async () => {
  const dir = workspace();
  const sockets = new Set();
  // Sends a megabyte of headers and a kilobyte, then goes quiet with the connection open.
  const server = createServer((request, response) => {
    response.writeHead(200, { "Content-Length": 1 << 20 });
    response.write(Buffer.alloc(1024));
  });
  server.on("connection", (socket) => sockets.add(socket));
  try {
    await release(dir, { "linux-x64": "v1" });
    await new Promise((done) => server.listen(0, "127.0.0.1", done));
    const { port } = server.address();
    // The real curl with the script's flags, the stall window and retries shortened.
    nodeExecutable(
      join(dir, "bin/curl"),
      `const { spawnSync } = require("node:child_process");
const args = process.argv.slice(2);
const url = new URL(args.pop());
for (const [flag, value] of [["--speed-time", "1"], ["--retry", "1"]]) {
  const at = args.indexOf(flag);
  if (at >= 0) args[at + 1] = value;
}
args.push("http://127.0.0.1:${port}" + url.pathname);
require("node:fs").appendFileSync(${JSON.stringify(join(dir, "curl.log"))}, args.join(" ") + "\\n");
process.exit(spawnSync(${JSON.stringify(nativePath(systemTool("curl")))}, args, { stdio: "inherit" }).status ?? 1);
`,
    );
    const started = Date.now();
    const fetching = fetchSdk(dir, "linux-x64");
    const timer = setTimeout(() => {
      for (const socket of sockets) socket.destroy();
    }, 30_000);
    const result = await fetching;
    clearTimeout(timer);
    const seconds = (Date.now() - started) / 1000;
    assert.ok(seconds < 30, `the stalled download hung for ${seconds} s`);
    assert.notEqual(result.status, 0, "a stalled download succeeded");
    // curl before 8.x says "Will retry in"; later ones say "Retrying in".
    assert.match(
      result.stderr,
      /Will retry|Retrying in/,
      "the stall was not retried",
    );
  } finally {
    for (const socket of sockets) socket.destroy();
    server.close();
    rmSync(dir, { recursive: true, force: true });
  }
});
