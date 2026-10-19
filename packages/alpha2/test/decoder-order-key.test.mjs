// What decides the order a browser decoder delivers a stream's planes in, and the built-in
// fixtures that measure each kind of stream.
import test from "node:test";
import assert from "node:assert/strict";

import {
  ALL_FORMATS,
  BufferSource,
  EncodedPacketSink,
  Input,
} from "mediabunny";

import { decoderOrderFixture, decoderOrderKey } from "../dist/decoder-order.js";

const FIXTURES = {};
for (const codec of ["aac", "opus", "vorbis", "ac3", "eac3"])
  Object.assign(
    FIXTURES,
    (await import(`../dist/generated/${codec}-order-fixtures.js`))
      .ORDER_FIXTURES,
  );

/** AudioSpecificConfig: AAC-LC, 48 kHz, then the channel configuration. */
const asc = (configuration) =>
  new Uint8Array([0x11, 0x80 | (configuration << 3)]);

/** An OpusHead of `channels` channels in mapping `family`. */
function opusHead(channels, family) {
  const head = new Uint8Array(21 + channels);
  head.set(new TextEncoder().encode("OpusHead"));
  head[8] = 1;
  head[9] = channels;
  head[18] = family;
  return head;
}

/** The decoder description and first packet of a fixture file's audio track. */
async function streamOf(fixture) {
  const input = new Input({
    source: new BufferSource(
      Uint8Array.from(atob(fixture.file), (character) =>
        character.charCodeAt(0),
      ),
    ),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    const config = await track.getDecoderConfig();
    return {
      codec: track.codec,
      channels: track.numberOfChannels,
      description:
        config.description === undefined
          ? null
          : new Uint8Array(config.description),
      packet: (await new EncodedPacketSink(track).getFirstPacket()).data,
    };
  } finally {
    input.dispose();
  }
}

test("an AAC stream is keyed by its channel configuration, ADTS header or program config element", () => {
  for (const [configuration, channels] of [
    [3, 3],
    [6, 6],
    [7, 8],
    [11, 7],
    [12, 8],
    [14, 8],
  ])
    assert.equal(
      decoderOrderKey("aac", asc(configuration), channels),
      `aac:${configuration}`,
    );
  // Without a description the stream is ADTS, whose header names configurations 1 to 7.
  assert.equal(decoderOrderKey("aac", null, 6), "aac:adts:6");
  assert.equal(decoderOrderKey("aac", null, 8), "aac:adts:7");
  assert.equal(decoderOrderKey("aac", null, 7), null);
  // A program config element: its elements. A config FFmpeg would not decode is not keyed.
  assert.equal(decoderOrderKey("aac", Uint8Array.of(0x11, 0x80), 6), null);
  // Mono and stereo are delivered in one order.
  assert.equal(decoderOrderKey("aac", asc(2), 2), null);
  assert.equal(decoderOrderKey("aac", null, 2), null);
});

test("Opus mapping family 1 and Vorbis are keyed by channel count, AC-3 by its frame's speakers", async () => {
  assert.equal(decoderOrderKey("opus", opusHead(6, 1), 6), "opus:1:6");
  assert.equal(decoderOrderKey("opus", opusHead(6, 255), 6), null);
  assert.equal(decoderOrderKey("opus", opusHead(2, 0), 2), null);
  assert.equal(decoderOrderKey("opus", null, 6), null);
  assert.equal(decoderOrderKey("vorbis", null, 3), "vorbis:3");
  const { packet } = await streamOf(FIXTURES["ac3:FL FR FC LFE SL SR"]);
  assert.equal(
    decoderOrderKey("ac3", null, 6, packet),
    "ac3:FL FR FC LFE SL SR",
  );
  assert.equal(decoderOrderKey("ac3", null, 6), null);
  assert.equal(decoderOrderKey("ac3", null, 6, new Uint8Array(8)), null);
  // FLAC, PCM and DTS decoders all deliver one order.
  assert.equal(decoderOrderKey("flac", null, 6), null);
  assert.equal(decoderOrderKey("pcm-s16", null, 6), null);
  assert.equal(decoderOrderKey("dts", null, 6, packet), null);
});

test("each fixture is keyed as a source like it is, and its LFE carries the low tone", async () => {
  const keys = Object.keys(FIXTURES);
  // Every AAC configuration FFmpeg names over two channels but 13 (22.2, which no encoder here
  // writes), ADTS's, every Opus family 1 and Vorbis count, and every AC-3 channel mode.
  for (const configuration of [3, 4, 5, 6, 7, 11, 12, 14])
    assert.ok(keys.includes(`aac:${configuration}`));
  for (const configuration of [3, 4, 5, 6, 7])
    assert.ok(keys.includes(`aac:adts:${configuration}`));
  for (const channels of [3, 4, 5, 6, 7, 8])
    assert.ok(
      keys.includes(`opus:1:${channels}`) &&
        keys.includes(`vorbis:${channels}`),
    );
  assert.equal(keys.filter((key) => key.startsWith("ac3:")).length, 10);
  assert.equal(keys.filter((key) => key.startsWith("eac3:")).length, 10);
  for (const [key, fixture] of Object.entries(FIXTURES)) {
    const stream = await streamOf(fixture);
    assert.equal(
      decoderOrderKey(
        stream.codec,
        stream.description,
        stream.channels,
        stream.packet,
      ),
      key,
    );
    assert.equal(stream.channels, fixture.tones.length, key);
    assert.equal(fixture.speakers.length, fixture.tones.length, key);
    fixture.speakers.forEach((speaker, plane) =>
      assert.equal(
        fixture.tones[plane] === 93.75,
        speaker === "LFE",
        `${key} plane ${plane}`,
      ),
    );
    assert.equal(await decoderOrderFixture(key), fixture);
  }
  assert.equal(await decoderOrderFixture("aac:13"), null);
  assert.equal(await decoderOrderFixture("flac:6"), null);
});
