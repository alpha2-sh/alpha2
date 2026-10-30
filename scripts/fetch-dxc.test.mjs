// fetch-dxc.sh against a local fake release: a scratch workspace whose manifest pins a zip the test
// builds, served from 127.0.0.1; no network.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { crc32 } from "node:zlib";

const root = resolve(import.meta.dirname, "..");
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

/** A zip of `files` (name -> bytes), stored without compression. */
function zip(files) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const [name, bytes] of Object.entries(files)) {
    const nameBytes = Buffer.from(name);
    const crc = crc32(bytes);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(bytes.length, 18);
    local.writeUInt32LE(bytes.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(bytes.length, 20);
    central.writeUInt32LE(bytes.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    locals.push(local, nameBytes, bytes);
    centrals.push(central, nameBytes);
    offset += local.length + nameBytes.length + bytes.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(Object.keys(files).length, 8);
  end.writeUInt16LE(Object.keys(files).length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

const dll = Buffer.from("not really a dxcompiler.dll");
const licence = Buffer.from("MIT licence text");
const archive = zip({
  "bin/x64/dxcompiler.dll": dll,
  "LICENCE-MIT.txt": licence,
});

/**
 * A scratch workspace holding the script and a manifest pinning `archive`, with the given
 * hashes (the real ones unless overridden).
 */
function workspace({
  archiveSha = sha256(archive),
  dllSha = sha256(dll),
} = {}) {
  const dir = mkdtempSync(join(tmpdir(), "alpha2-fetch-dxc-"));
  mkdirSync(join(dir, "scripts/lib"), { recursive: true });
  mkdirSync(join(dir, "release/dxc"), { recursive: true });
  copyFileSync(
    join(root, "scripts/fetch-dxc.sh"),
    join(dir, "scripts/fetch-dxc.sh"),
  );
  copyFileSync(
    join(root, "scripts/lib/windows-tools.sh"),
    join(dir, "scripts/lib/windows-tools.sh"),
  );
  writeFileSync(
    join(dir, "release/dxc/manifest.toml"),
    [
      "schema = 1",
      'version = "9.9.9"',
      'download_url = "https://example.invalid"',
      'archive = "dxc.zip"',
      `archive_sha256 = "${archiveSha}"`,
      "",
      "[files]",
      `"bin/x64/dxcompiler.dll" = "${dllSha}"`,
      "",
      "[licenses]",
      `"LICENCE-MIT.txt" = "${sha256(licence)}"`,
      "",
    ].join("\n"),
  );
  return dir;
}

/** Serves the archive and counts the requests for it. */
async function release() {
  const served = { count: 0 };
  const server = createServer((request, response) => {
    if (request.url === "/dxc.zip") {
      served.count += 1;
      response.end(archive);
    } else {
      response.writeHead(404).end();
    }
  });
  await new Promise((resolveListen) =>
    server.listen(0, "127.0.0.1", resolveListen),
  );
  served.url = `http://127.0.0.1:${server.address().port}`;
  served.close = () =>
    new Promise((resolveClose) => server.close(resolveClose));
  return served;
}

function fetch(dir, served, target) {
  return new Promise((resolveRun) => {
    // Windows starts no file by its shebang: Git Bash's bash runs it there.
    const shell = process.platform === "win32" ? "bash" : "sh";
    const child = spawn(shell, [join(dir, "scripts/fetch-dxc.sh"), target], {
      env: { ...process.env, ALPHA2_DXC_DOWNLOAD_URL: served.url },
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("close", (status) => resolveRun({ status, stdout, stderr }));
  });
}

test("the pinned release is fetched, verified, and its dxcompiler.dll printed", async () => {
  const dir = workspace();
  const served = await release();
  try {
    const target = join(dir, "cache");
    const run = await fetch(dir, served, target);
    assert.equal(run.status, 0, run.stderr);
    assert.equal(run.stdout.trim(), join(target, "dxcompiler.dll"));
    assert.deepEqual(readFileSync(join(target, "dxcompiler.dll")), dll);
    assert.deepEqual(readFileSync(join(target, "LICENCE-MIT.txt")), licence);
    // A verified cache answers without downloading again.
    const again = await fetch(dir, served, target);
    assert.equal(again.status, 0, again.stderr);
    assert.equal(served.count, 1);
  } finally {
    await served.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an archive or a file whose checksum differs is refused", async () => {
  const served = await release();
  try {
    for (const [options, what] of [
      [{ archiveSha: "0".repeat(64) }, "checksum mismatch for dxc.zip"],
      [
        { dllSha: "1".repeat(64) },
        "checksum mismatch for bin/x64/dxcompiler.dll",
      ],
    ]) {
      const dir = workspace(options);
      try {
        const target = join(dir, "cache");
        const run = await fetch(dir, served, target);
        assert.notEqual(run.status, 0, `${what}: ${run.stdout}`);
        assert.match(run.stderr, new RegExp(what.replaceAll(".", "\\.")));
        assert.equal(existsSync(join(target, "dxcompiler.dll")), false, what);
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    }
  } finally {
    await served.close();
  }
});
