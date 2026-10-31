import assert from "node:assert/strict";
import {
  lstat,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  readlink,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test, { after } from "node:test";
import { crc32, deflateRawSync, gunzipSync, gzipSync } from "node:zlib";

import { extractArchive } from "../dist/archive.js";

const scratchDirectories = [];

async function scratch() {
  const directory = await mkdtemp(join(tmpdir(), "alpha2-node-archive-"));
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

const posix = process.platform !== "win32";

/** One 512-byte tar header; `magic` defaults to POSIX ustar. */
function tarHeader({
  name,
  type = "0",
  size = 0,
  mode = 0o644,
  uid = 0,
  gid = 0,
  linkName = "",
  prefix = "",
  magic = "ustar\u000000",
}) {
  const header = Buffer.alloc(512);
  header.write(name, 0, 100);
  header.write(`${mode.toString(8).padStart(7, "0")}\0`, 100);
  header.write(`${uid.toString(8).padStart(7, "0")}\0`, 108);
  header.write(`${gid.toString(8).padStart(7, "0")}\0`, 116);
  header.write(`${size.toString(8).padStart(11, "0")}\0`, 124);
  header.write(`${"0".repeat(11)}\0`, 136);
  header.write(type, 156);
  header.write(linkName, 157, 100);
  header.write(magic, 257, 8, "latin1");
  header.write(prefix, 345, 155);
  header.fill(0x20, 148, 156);
  let sum = 0;
  for (const byte of header) sum += byte;
  header.write(`${sum.toString(8).padStart(6, "0")}\0 `, 148);
  return header;
}

const pad = (bytes) =>
  Buffer.concat([bytes, Buffer.alloc((512 - (bytes.length % 512)) % 512)]);

/**
 * A gzipped tar of `entries`: `{ name, data?, type?, mode?, uid?, gid?, linkName?, prefix?,
 * magic?, pax? }`, where `pax` is a record of extended keys written before the entry.
 */
function tarGz(entries, { end = true } = {}) {
  const blocks = [];
  for (const entry of entries) {
    if (entry.pax) {
      const records = Object.entries(entry.pax).map(([key, value]) => {
        const body = ` ${key}=${value}\n`;
        let length = body.length + 1;
        while (`${length}${body}`.length !== length) length += 1;
        return `${length}${body}`;
      });
      const payload = Buffer.from(records.join(""));
      blocks.push(
        tarHeader({ name: "PaxHeader", type: "x", size: payload.length }),
      );
      blocks.push(pad(payload));
    }
    const data = Buffer.from(entry.data ?? "");
    blocks.push(tarHeader({ ...entry, size: data.length }));
    if (data.length) blocks.push(pad(data));
  }
  if (end) blocks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(blocks));
}

/**
 * A zip of `entries`: `{ name, data?, mode?, deflate?, utf8?, crc?, flags? }`, written with a Unix
 * "made by" host so `mode` is kept.
 */
function zip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const entry of entries) {
    const name = Buffer.from(entry.name);
    const data = Buffer.from(entry.data ?? "");
    const stored = entry.deflate ? deflateRawSync(data) : data;
    const flags = (entry.flags ?? 0) | (entry.utf8 ? 0x800 : 0);
    const crc = entry.crc ?? crc32(data);
    const method = entry.deflate ? 8 : 0;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(flags, 6);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(crc >>> 0, 14);
    local.writeUInt32LE(stored.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE((3 << 8) | 20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(flags, 8);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(crc >>> 0, 16);
    central.writeUInt32LE(stored.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt32LE(((entry.mode ?? 0o100644) << 16) >>> 0, 38);
    central.writeUInt32LE(offset, 42);
    locals.push(local, name, stored);
    centrals.push(central, name);
    offset += 30 + name.length + stored.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

async function extract(bytes, extension) {
  const root = await scratch();
  const archive = join(root, `archive.${extension}`);
  await writeFile(archive, bytes);
  const destination = join(root, "out");
  await mkdir(destination);
  await extractArchive(archive, destination);
  return destination;
}

/** Asserts `bytes` are refused with a message matching `message` and nothing lands outside. */
async function assertRefused(bytes, extension, message) {
  const root = await scratch();
  const archive = join(root, `archive.${extension}`);
  await writeFile(archive, bytes);
  const destination = join(root, "out");
  await mkdir(destination);
  await assert.rejects(extractArchive(archive, destination), { message });
  assert.deepEqual((await readdir(root)).sort(), [
    `archive.${extension}`,
    "out",
  ]);
}

test("a tar.gz unpacks files, nested directories, execute bits and relative links", async () => {
  const out = await extract(
    tarGz([
      { name: "top/", type: "5", mode: 0o755 },
      { name: "top/bin/tool", data: "#!", mode: 0o755 },
      { name: "top/a/b/c.txt", data: "deep" },
      { name: "top/lib/libx.so.1.2", data: "lib", mode: 0o755 },
      { name: "top/lib/libx.so.1", type: "2", linkName: "libx.so.1.2" },
      { name: "top/share/link", type: "2", linkName: "../lib/libx.so.1" },
    ]),
    "tar.gz",
  );
  assert.equal(await readFile(join(out, "top/a/b/c.txt"), "utf8"), "deep");
  if (posix) {
    assert.equal((await stat(join(out, "top/bin/tool"))).mode & 0o777, 0o755);
    assert.equal((await stat(join(out, "top/a/b/c.txt"))).mode & 0o111, 0);
    assert.equal(await readlink(join(out, "top/lib/libx.so.1")), "libx.so.1.2");
    assert.equal(await readFile(join(out, "top/share/link"), "utf8"), "lib");
  }
});

test("names and links that would leave the destination are refused", async () => {
  const cases = [
    [{ name: "../x", data: "x" }, /unsafe name: \.\.\/x/],
    [{ name: "/abs", data: "x" }, /unsafe name: \/abs/],
    [{ name: "C:x", data: "x" }, /unsafe name: C:x/],
    [{ name: "a\\b", data: "x" }, /unsafe name: a\\b/],
    [{ name: "top//x", data: "x" }, /unsafe name/],
    [
      { name: "top/etc", type: "2", linkName: "../../etc" },
      /link top\/etc that points outside it/,
    ],
    [
      { name: "top/abs", type: "2", linkName: "/etc/passwd" },
      /non-relative target/,
    ],
    [
      { name: "top/hard", type: "1", linkName: "top/x" },
      /unsupported entry top\/hard \(type 1\)/,
    ],
    [{ name: "top/fifo", type: "6" }, /unsupported entry top\/fifo \(type 6\)/],
  ];
  for (const [entry, message] of cases)
    await assertRefused(tarGz([entry]), "tar.gz", message);
  await assertRefused(
    zip([{ name: "../x", data: "x" }]),
    "zip",
    /unsafe name: \.\.\/x/,
  );
  await assertRefused(
    zip([{ name: "top/link", data: "target", mode: 0o120777 }]),
    "zip",
    /link top\/link, which zip archives may not carry/,
  );
});

test("a damaged archive is refused", async () => {
  const whole = tarGz([{ name: "top/a", data: "x".repeat(5000) }]);
  await assertRefused(whole.subarray(0, whole.length - 40), "tar.gz", /./);
  await assertRefused(
    tarGz([{ name: "top/a", data: "x".repeat(5000) }], { end: false }),
    "tar.gz",
    /is truncated/,
  );
  const header = tarGz([{ name: "top/a", data: "x" }]);
  const raw = gunzipSync(header);
  raw[0] ^= 1;
  await assertRefused(gzipSync(raw), "tar.gz", /checksum does not match/);
  await assertRefused(
    zip([{ name: "top/a", data: "hello", deflate: true, crc: 1234 }]),
    "zip",
    /CRC-32 or size does not match/,
  );
  await assertRefused(
    zip([{ name: "top/a", data: "hello", flags: 1 }]),
    "zip",
    /encrypted entry top\/a/,
  );
});

test("pax path overrides and GNU long names are honoured", async () => {
  const long = `top/${"d".repeat(80)}/${"f".repeat(80)}.txt`;
  const out = await extract(
    tarGz([
      { name: "short", data: "pax", pax: { path: long, mtime: "1.5" } },
      {
        name: "././@LongLink",
        type: "L",
        data: `${long}.gnu\0`,
        magic: "ustar  \0",
      },
      { name: "truncated", data: "gnu", magic: "ustar  \0" },
      { name: "name.txt", prefix: "top/prefixed", data: "ustar" },
    ]),
    "tar.gz",
  );
  assert.equal(await readFile(join(out, long), "utf8"), "pax");
  assert.equal(await readFile(join(out, `${long}.gnu`), "utf8"), "gnu");
  assert.equal(
    await readFile(join(out, "top/prefixed/name.txt"), "utf8"),
    "ustar",
  );
});

test("a zip unpacks stored and deflated entries with UTF-8 names and modes", async () => {
  const out = await extract(
    zip([
      { name: "top/", mode: 0o40755 },
      {
        name: "top/bin/alpha2-host.exe",
        data: "MZ".repeat(1000),
        deflate: true,
        mode: 0o100755,
      },
      { name: "top/caf\u00e9.txt", data: "utf8", utf8: true },
      { name: "top/plain.txt", data: "" },
    ]),
    "zip",
  );
  assert.equal(
    await readFile(join(out, "top/bin/alpha2-host.exe"), "utf8"),
    "MZ".repeat(1000),
  );
  assert.equal(await readFile(join(out, "top/caf\u00e9.txt"), "utf8"), "utf8");
  assert.equal(await readFile(join(out, "top/plain.txt"), "utf8"), "");
  if (posix)
    assert.equal(
      (await stat(join(out, "top/bin/alpha2-host.exe"))).mode & 0o777,
      0o755,
    );
});

test("the release archive layout round-trips", async () => {
  const base = "alpha2-0.5.0-linux-x64-bundled";
  const libraries = {
    libavcodec: "62.28.103",
    libavdevice: "62.3.103",
    libavfilter: "11.14.103",
    libavformat: "62.12.103",
    libavutil: "60.26.103",
    libswresample: "6.3.103",
    libswscale: "9.5.103",
  };
  const entries = [
    { name: `${base}/`, type: "5", mode: 0o775 },
    { name: `${base}/bin/`, type: "5", mode: 0o775 },
    { name: `${base}/bin/alpha2-host`, data: "host", mode: 0o755 },
    { name: `${base}/bin/alpha2`, data: "cli", mode: 0o755 },
    { name: `${base}/lib/`, type: "5", mode: 0o775 },
    { name: `${base}/licenses/FFmpeg/COPYING.LGPLv2.1`, data: "lgpl" },
    { name: `${base}/LICENSE.md`, data: "mit" },
  ];
  for (const [library, full] of Object.entries(libraries)) {
    const file = `${library}.so.${full}`;
    entries.push({ name: `${base}/lib/${file}`, data: library, mode: 0o755 });
    entries.push({
      name: `${base}/lib/${library}.so.${full.split(".")[0]}`,
      type: "2",
      linkName: file,
    });
    entries.push({
      name: `${base}/lib/${library}.so`,
      type: "2",
      linkName: file,
    });
  }
  const out = await extract(tarGz(entries), "tar.gz");
  const listing = (await readdir(join(out, base, "lib"))).sort();
  assert.equal(listing.length, 21);
  if (!posix) return;
  let links = 0;
  for (const name of listing) {
    const info = await lstat(join(out, base, "lib", name));
    if (info.isSymbolicLink()) {
      links += 1;
      assert.equal(
        await readFile(join(out, base, "lib", name), "utf8"),
        name.replace(/\.so.*$/, ""),
      );
    }
  }
  assert.equal(links, 14);
});

test(
  "extracted files belong to the current user whatever owner the archive records",
  { skip: !posix && "Windows files have no numeric owner" },
  async () => {
    const out = await extract(
      tarGz([
        { name: "top/", type: "5", uid: 1001, gid: 1001 },
        { name: "top/file", data: "x", uid: 1001, gid: 1001 },
      ]),
      "tar.gz",
    );
    for (const path of ["top", "top/file"]) {
      const info = await stat(join(out, path));
      assert.equal(info.uid, process.getuid());
      assert.equal(info.gid, process.getgid());
    }
  },
);
