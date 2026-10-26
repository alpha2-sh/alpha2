import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { basename } from "node:path";
import test from "node:test";

import { bunzip2 } from "../dist/bzip2.js";
import { OPENH264_TARGETS } from "../dist/install.js";

/** `printf abracadabra | bzip2 -9`: one block, whose CRC is 0x236d4bd8. */
const ABRACADABRA = Buffer.from(
  "425a6839314159265359236d4bd800000101803c001000200030cd00947a" +
    "9ea52c8478bb9229c284811b6a5ec0",
  "hex",
);
/** An empty stream: the header, the end-of-stream marker and a combined CRC of 0. */
const EMPTY = Buffer.from("425a683917724538509000000000", "hex");

const invalid = { message: /^invalid bzip2 data: / };

/**
 * `data` compressed at `level` by the system's `bzip2`, else Python's `bz2` module, or `undefined`
 * when neither runs here.
 */
function compress(data, level = 9) {
  const script = `import bz2, sys; sys.stdout.buffer.write(bz2.compress(sys.stdin.buffer.read(), ${level}))`;
  const commands = [
    ["bzip2", [`-${level}`, "-c"]],
    ["python3", ["-I", "-c", script]],
    ["python", ["-I", "-c", script]],
  ];
  for (const [command, args] of commands) {
    try {
      return execFileSync(command, args, {
        input: data,
        maxBuffer: 1 << 28,
        stdio: ["pipe", "pipe", "ignore"],
      });
    } catch {
      // Not on this machine; try the next one.
    }
  }
  return undefined;
}

const noCompressor =
  compress(Buffer.from("probe")) === undefined &&
  "neither bzip2 nor Python is on this machine";

/**
 * `size` deterministic bytes in stretches of runs, noise and a small alphabet, so the compressor
 * uses several Huffman tables, long runs and every byte value.
 */
function sample(size, seed) {
  const bytes = new Uint8Array(size);
  let state = seed;
  const random = () => {
    state = (Math.imul(state, 1103515245) + 12345) >>> 0;
    return state >>> 8;
  };
  for (let i = 0; i < size;) {
    const length = Math.min(size - i, 1 + (random() % 700));
    const kind = random() % 3;
    if (kind === 0) bytes.fill(random() & 0xff, i, i + length);
    else
      for (let k = 0; k < length; k++)
        bytes[i + k] = kind === 1 ? random() & 0xff : 97 + (random() % 7);
    i += length;
  }
  return bytes;
}

test("a hand-checked stream and an empty one decompress", () => {
  assert.equal(Buffer.from(bunzip2(ABRACADABRA)).toString(), "abracadabra");
  assert.equal(bunzip2(EMPTY).length, 0);
  assert.equal(
    Buffer.from(bunzip2(Buffer.concat([EMPTY, ABRACADABRA, EMPTY]))).toString(),
    "abracadabra",
  );
});

test("data that is not exactly bzip2 streams is refused", () => {
  const changed = (index, mask) => {
    const bytes = Buffer.from(ABRACADABRA);
    bytes[index] ^= mask;
    return bytes;
  };
  assert.throws(() => bunzip2(new Uint8Array()), invalid);
  assert.throws(() => bunzip2(Buffer.from("not bzip2")), invalid);
  assert.throws(() => bunzip2(Buffer.from("BZh0")), invalid);
  // The block header, the block's CRC and the stream's CRC.
  assert.throws(() => bunzip2(changed(4, 0x01)), invalid);
  assert.throws(() => bunzip2(changed(10, 0x01)), /block's CRC/);
  assert.throws(
    () => bunzip2(changed(ABRACADABRA.length - 2, 0x01)),
    /stream's CRC/,
  );
  // The move-to-front symbols.
  assert.throws(() => bunzip2(changed(36, 0x10)), invalid);
  for (let end = 0; end < ABRACADABRA.length; end++)
    assert.throws(() => bunzip2(ABRACADABRA.subarray(0, end)), invalid);
  assert.throws(
    () => bunzip2(Buffer.concat([ABRACADABRA, Buffer.from([0])])),
    invalid,
  );
});

test(
  "data bzip2 compressed decompresses to the same bytes",
  { skip: noCompressor },
  () => {
    const cases = [
      // Three blocks of 100 kB.
      [sample(250_000, 1), 1],
      [sample(60_000, 2), 9],
      // Runs far longer than the run-length coding's 4 + 255.
      [new Uint8Array(1_000_000).fill(0x61), 9],
      [Uint8Array.from({ length: 256 * 4 }, (_, i) => i & 0xff), 9],
      [new Uint8Array([7]), 9],
    ];
    for (const [data, level] of cases)
      assert.deepEqual(bunzip2(compress(data, level)), data);

    const first = sample(30_000, 3);
    const second = sample(20_000, 4);
    assert.deepEqual(
      bunzip2(Buffer.concat([compress(first, 2), compress(second, 9)])),
      new Uint8Array(Buffer.concat([first, second])),
    );

    const compressed = compress(sample(60_000, 5));
    for (const index of [100, compressed.length >> 1, compressed.length - 8]) {
      const corrupted = Buffer.from(compressed);
      corrupted[index] ^= 0x04;
      assert.throws(() => bunzip2(corrupted), invalid);
    }
  },
);

const ciscoArchive = process.env.ALPHA2_TEST_OPENH264_ARCHIVE;

test(
  "a Cisco OpenH264 archive decompresses to the library its SHA-256 names",
  {
    skip:
      !ciscoArchive?.trim() &&
      "ALPHA2_TEST_OPENH264_ARCHIVE names no Cisco .bz2 file",
  },
  async () => {
    const entry = Object.values(OPENH264_TARGETS).find(
      (candidate) => candidate.archive === basename(ciscoArchive),
    );
    assert.ok(entry, `${basename(ciscoArchive)} is not a known Cisco archive`);
    const archive = await readFile(ciscoArchive);
    const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
    assert.equal(sha256(archive), entry.archiveSha256);
    assert.equal(sha256(bunzip2(archive)), entry.librarySha256);
  },
);
