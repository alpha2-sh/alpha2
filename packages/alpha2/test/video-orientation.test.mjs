import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { ALL_FORMATS, BufferSource, Input } from "mediabunny";

import { readVideoOrientation, UPRIGHT } from "../dist/media.js";

const corpus = join(import.meta.dirname, "../../../tests/corpus");

/**
 * The orientation the container states for its primary video track, and the rotation mediabunny
 * reads from it.
 */
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

// Each pair is what native decoding reads: a mirroring display matrix is a mirror and a turn,
// while mediabunny reads only an angle, upside down for a pure mirror.
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
  // A track the file does not have.
  const mp4 = readFileSync(join(corpus, "h264-bt709-hflip.mp4"));
  assert.equal(await readVideoOrientation(mp4, 7), undefined);
});
