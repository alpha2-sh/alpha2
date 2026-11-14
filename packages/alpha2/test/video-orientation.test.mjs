import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { ALL_FORMATS, BufferSource, Input } from "mediabunny";

import { readVideoOrientation, UPRIGHT } from "../dist/media.js";

const corpus = join(import.meta.dirname, "../../../tests/corpus");

async function orientationOf(name, asBlob) {
  const bytes = readFileSync(join(corpus, name));
  const input = new Input({
    source: new BufferSource(bytes),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryVideoTrack();
    const container = asBlob ? new Blob([bytes]) : bytes;
    return {
      orientation: await readVideoOrientation(container, track.id),
      mediabunny: track.rotation,
    };
  } finally {
    input.dispose();
  }
}

// Native decoding reads a mirroring matrix as mirror plus turn; mediabunny reads only an angle.
const cases = [
  ["h264-bt709-copy.mp4", UPRIGHT, 0],
  ["rotation.mp4", { rotationDegrees: 270, mirror: false }, 270],
  ["h264-bt709-hflip.mp4", { rotationDegrees: 0, mirror: true }, 180],
  ["h264-bt709-vflip.mp4", { rotationDegrees: 180, mirror: true }, 0],
  ["h264-bt709-transverse.mp4", { rotationDegrees: 90, mirror: true }, 270],
  ["h264-bt709-copy.mkv", UPRIGHT, 0],
  ["h264-bt709-transverse.mkv", { rotationDegrees: 90, mirror: true }, 90],
];

for (const [name, expected, mediabunny] of cases) {
  test(`${name} is shown ${JSON.stringify(expected)}, as native decoding shows it`, async () => {
    for (const asBlob of [false, true]) {
      const read = await orientationOf(name, asBlob);
      assert.deepEqual(read.orientation, expected);
      assert.equal(read.mediabunny, mediabunny);
    }
  });
}

test("a container this does not read states no orientation", async () => {
  const png = readFileSync(join(corpus, "parity-image.png"));
  assert.equal(await readVideoOrientation(png, 1), undefined);
  const mp4 = readFileSync(join(corpus, "h264-bt709-hflip.mp4"));
  assert.equal(await readVideoOrientation(mp4, 7), undefined);
});

// Serves a large file from a few placed byte ranges and records the longest read asked for.
class SparseBlob extends Blob {
  constructor(size, ranges) {
    super([]);
    this.fakeSize = size;
    this.ranges = ranges;
    this.longestRead = 0;
  }
  get size() {
    return this.fakeSize;
  }
  slice(start = 0, end = this.fakeSize) {
    this.longestRead = Math.max(this.longestRead, end - start);
    const length = Math.min(end - start, 1 << 20);
    const bytes = new Uint8Array(Math.max(0, length));
    for (const { at, data } of this.ranges)
      for (let index = 0; index < data.length; index += 1) {
        const offset = at + index - start;
        if (offset >= 0 && offset < bytes.length) bytes[offset] = data[index];
      }
    return new Blob([bytes]);
  }
}

test("a Matroska track number of an impossible size is not read", async () => {
  const header = Uint8Array.from([
    0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67, 0xff, 0x16, 0x54,
    0xae, 0x6b, 0xff, 0xae, 0xff,
    // TrackNumber with a declared size of 2^31 bytes.
    0xd7, 0x01, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
  ]);
  const file = new SparseBlob(2 ** 32, [{ at: 0, data: header }]);
  assert.equal(await readVideoOrientation(file, 1), undefined);
  assert.ok(file.longestRead <= 64, `read ${file.longestRead} bytes at once`);
});

test("an MP4 whose moov follows a large mdat still states its mirror", async () => {
  const bytes = readFileSync(join(corpus, "h264-bt709-hflip.mp4"));
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const ftyp = bytes.subarray(0, view.getUint32(0));
  const moovAt = bytes.length - 3445;
  assert.equal(
    String.fromCharCode(...bytes.subarray(moovAt + 4, moovAt + 8)),
    "moov",
  );
  const moov = bytes.subarray(moovAt);
  const mdatSize = 100 * 1024 * 1024 + 8;
  const mdat = new Uint8Array(8);
  new DataView(mdat.buffer).setUint32(0, mdatSize);
  mdat.set([0x6d, 0x64, 0x61, 0x74], 4);
  const at = ftyp.length + mdatSize;
  const file = new SparseBlob(at + moov.length, [
    { at: 0, data: ftyp },
    { at: ftyp.length, data: mdat },
    { at, data: moov },
  ]);
  const { orientation } = await orientationOf("h264-bt709-hflip.mp4", false);
  const input = new Input({
    source: new BufferSource(bytes),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryVideoTrack();
    assert.deepEqual(await readVideoOrientation(file, track.id), orientation);
  } finally {
    input.dispose();
  }
  assert.deepEqual(orientation, { rotationDegrees: 0, mirror: true });
});
