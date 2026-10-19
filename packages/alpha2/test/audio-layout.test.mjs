// The speaker layout the browser reads from a file's bytes, rule for rule as FFmpeg's demuxers and
// decoders read it.
import test from "node:test";
import assert from "node:assert/strict";

import {
  aacChannelConfiguration,
  aacSpeakers,
  ac3Speakers,
  chnlSpeakers,
  codecSpeakers,
  dtsSpeakers,
  movChanSpeakers,
  readAudioSpeakers,
  speakersOfMask,
  wavSpeakers,
} from "../dist/audio-layout.js";

/** Writes big-endian bit fields; `bytes()` pads the last byte with zeros. */
class BitWriter {
  bits = [];
  put(value, count) {
    for (let bit = count - 1; bit >= 0; bit -= 1)
      this.bits.push(Math.floor(value / 2 ** bit) % 2);
    return this;
  }
  align() {
    while (this.bits.length % 8) this.bits.push(0);
    return this;
  }
  bytes(length = 0) {
    this.align();
    const bytes = new Uint8Array(Math.max(length, this.bits.length / 8));
    for (let index = 0; index < this.bits.length; index += 1)
      bytes[index >> 3] |= this.bits[index] << (7 - (index & 7));
    return bytes;
  }
}

/**
 * An `AudioSpecificConfig`: object type, sampling-frequency index, channel configuration, then
 * GASpecificConfig's three zero bits.
 */
function audioSpecificConfig(objectType, frequency, configuration) {
  const bits = [];
  const put = (value, count) => {
    for (let bit = count - 1; bit >= 0; bit -= 1) bits.push((value >> bit) & 1);
  };
  if (objectType >= 31) {
    put(31, 5);
    put(objectType - 32, 6);
  } else put(objectType, 5);
  put(frequency, 4);
  if (frequency === 15) put(48_000, 24);
  put(configuration, 4);
  put(0, 3);
  while (bits.length % 8) bits.push(0);
  return Uint8Array.from({ length: bits.length / 8 }, (_, byte) =>
    bits.slice(byte * 8, byte * 8 + 8).reduce((value, bit) => value * 2 + bit),
  );
}

function opusHead(channels, family) {
  const head = new Uint8Array(family === 0 ? 19 : 21 + channels);
  head.set(new TextEncoder().encode("OpusHead"));
  head[8] = 1;
  head[9] = channels;
  head[18] = family;
  return head;
}

/**
 * A `chan` payload after its version and flags: a layout tag, a bitmap, and channel descriptions by
 * label.
 */
function chan(tag, bitmap = 0, labels = []) {
  const bytes = new Uint8Array(12 + labels.length * 20);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, tag);
  view.setUint32(4, bitmap);
  view.setUint32(8, labels.length);
  labels.forEach((label, index) => view.setUint32(12 + index * 20, label));
  return bytes;
}

/**
 * A WAVE `fmt ` chunk's payload: WAVE_FORMAT_EXTENSIBLE with this channel mask, or another format
 * tag.
 */
function fmt(channels, mask, formatTag = 0xfffe) {
  const bytes = new Uint8Array(40);
  const view = new DataView(bytes.buffer);
  view.setUint16(0, formatTag, true);
  view.setUint16(2, channels, true);
  view.setUint16(16, 22, true);
  view.setUint32(20, mask, true);
  return bytes;
}

test("a channel mask names its speakers in bit order", () => {
  assert.deepEqual(speakersOfMask(0x7n), ["FL", "FR", "FC"]);
  assert.deepEqual(speakersOfMask(0x33n), ["FL", "FR", "BL", "BR"]);
  // Bits FFmpeg names no channel for are unnamed; past bit 31 are its extra speakers.
  assert.deepEqual(speakersOfMask((1n << 20n) | (1n << 35n)), ["UNK", "LFE2"]);
});

test("a WAVE file declares a layout by an extensible format's channel mask of its channel count", () => {
  assert.deepEqual(wavSpeakers(fmt(3, 0x7), 3), ["FL", "FR", "FC"]);
  assert.deepEqual(wavSpeakers(fmt(4, 0x33), 4), ["FL", "FR", "BL", "BR"]);
  // FFmpeg's RIFF demuxer ignores a mask of another count, and plain PCM has none.
  assert.equal(wavSpeakers(fmt(4, 0x7), 4), null);
  assert.equal(wavSpeakers(fmt(2, 0), 2), null);
  assert.equal(wavSpeakers(fmt(2, 0x3, 1), 2), null);
});

test("a QuickTime chan box declares a layout as FFmpeg's mov_chan reads it", () => {
  // kAudioChannelLayoutTag_MPEG_3_0_B: C L R, the order the samples are in.
  assert.deepEqual(movChanSpeakers(chan((114 << 16) | 3), 3), [
    "FC",
    "FL",
    "FR",
  ]);
  assert.deepEqual(movChanSpeakers(chan((108 << 16) | 4), 4), [
    "FL",
    "FR",
    "BL",
    "BR",
  ]);
  // A tag of another count, or one mov_chan does not know, says nothing.
  assert.equal(movChanSpeakers(chan((114 << 16) | 3), 4), null);
  assert.equal(movChanSpeakers(chan((147 << 16) | 0), 2), null);
  assert.equal(movChanSpeakers(chan((190 << 16) | 2), 2), null);
  // Ambisonic B-format names no speaker.
  assert.equal(movChanSpeakers(chan((107 << 16) | 4), 4), null);
  assert.deepEqual(movChanSpeakers(chan(0x10000, 0x7), 3), ["FL", "FR", "FC"]);
  assert.equal(movChanSpeakers(chan(0x10000, 0x7), 2), null);
  assert.deepEqual(movChanSpeakers(chan(0, 0, [3, 1, 2]), 3), [
    "FC",
    "FL",
    "FR",
  ]);
  assert.deepEqual(movChanSpeakers(chan(0, 0, [35, 36, 37, 99]), 4), [
    "WL",
    "WR",
    "LFE2",
    "UNK",
  ]);
  assert.equal(movChanSpeakers(chan(0, 0, [3, 1]), 3), null, "too few");
});

test("a codec's own header lays its planes out as FFmpeg's decoder delivers them", () => {
  for (const [configuration, channels, speakers] of [
    [1, 1, ["FC"]],
    [3, 3, ["FL", "FR", "FC"]],
    [4, 4, ["FL", "FR", "FC", "BC"]],
    [6, 6, ["FL", "FR", "FC", "LFE", "BL", "BR"]],
    // FFmpeg decodes configuration 7 as encoders write it, 7.1, not the specification's 7.1(wide).
    [7, 8, ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]],
    [11, 7, ["FL", "FR", "FC", "LFE", "BL", "BR", "BC"]],
    [12, 8, ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"]],
    [
      13,
      24,
      "FL FR FC LFE BL BR FLC FRC BC SL SR TC TFL TFC TFR TBL TBC TBR LFE2 TSL TSR BFC BFL BFR".split(
        " ",
      ),
    ],
    [14, 8, ["FL", "FR", "FC", "LFE", "BL", "BR", "TFL", "TFR"]],
  ]) {
    const config = audioSpecificConfig(2, 3, configuration);
    assert.equal(aacChannelConfiguration(config), configuration);
    assert.deepEqual(codecSpeakers("aac", config, channels), speakers);
  }
  // An escaped object type and an explicit rate; a program config element (0) is not read.
  assert.equal(aacChannelConfiguration(audioSpecificConfig(42, 15, 5)), 5);
  assert.equal(codecSpeakers("aac", audioSpecificConfig(2, 3, 0), 6), null);
  assert.deepEqual(codecSpeakers("aac", audioSpecificConfig(2, 3, 0), 2), [
    "FL",
    "FR",
  ]);
  // Without a description the stream is ADTS, whose configuration its channel count names.
  assert.deepEqual(codecSpeakers("aac", null, 3), ["FL", "FR", "FC"]);
  assert.deepEqual(codecSpeakers("aac", null, 8), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "BL",
    "BR",
    "SL",
    "SR",
  ]);
  assert.equal(codecSpeakers("aac", null, 7), null);
  // Opus: mapping family 1 is Vorbis's order, reordered; family 0 is mono or stereo; others name no
  // speaker.
  assert.deepEqual(codecSpeakers("opus", opusHead(3, 1), 3), [
    "FL",
    "FR",
    "FC",
  ]);
  assert.deepEqual(codecSpeakers("opus", opusHead(6, 1), 6), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "BL",
    "BR",
  ]);
  assert.deepEqual(codecSpeakers("opus", opusHead(2, 0), 2), ["FL", "FR"]);
  assert.equal(codecSpeakers("opus", opusHead(4, 2), 4), null);
  assert.equal(codecSpeakers("opus", opusHead(4, 0), 4), null);
  assert.deepEqual(codecSpeakers("vorbis", null, 7), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "BC",
    "SL",
    "SR",
  ]);
  assert.deepEqual(codecSpeakers("flac", null, 5), [
    "FL",
    "FR",
    "FC",
    "SL",
    "SR",
  ]);
  assert.deepEqual(codecSpeakers("flac", null, 4), ["FL", "FR", "BL", "BR"]);
  assert.deepEqual(codecSpeakers("mp3", null, 1), ["FC"]);
  assert.equal(codecSpeakers("ac3", null, 6), null);
  assert.equal(codecSpeakers("vorbis", null, 9), null);
});

/**
 * An AAC-LC 48 kHz `AudioSpecificConfig` whose program config element lists these elements: each
 * of `front`, `side` and `back` a string of `S` (SCE) and `C` (CPE), then LFE and coupling channel
 * counts and a comment.
 */
function pceConfig({
  front = "",
  side = "",
  back = "",
  lfe = 0,
  cc = 0,
  comment = "",
}) {
  const writer = new BitWriter().put(2, 5).put(3, 4).put(0, 4);
  writer.put(0, 3); // frame length, core coder, extension
  writer.put(0, 4).put(1, 2).put(3, 4); // element tag, object type, sampling index
  writer.put(front.length, 4).put(side.length, 4).put(back.length, 4);
  writer.put(lfe, 2).put(0, 3).put(cc, 4).put(0, 3);
  for (const elements of [front, side, back])
    elements
      .split("")
      .forEach((element, id) =>
        writer.put(element === "C" ? 1 : 0, 1).put(id, 4),
      );
  for (let id = 0; id < lfe; id += 1) writer.put(id, 4);
  for (let id = 0; id < cc; id += 1) writer.put(0, 1).put(id, 4);
  writer.align().put(comment.length, 8);
  for (const character of comment) writer.put(character.charCodeAt(0), 8);
  return writer.bytes();
}

test("an AAC program config element lays its planes out as FFmpeg's decoder assigns them", () => {
  // Pairs at the front and back: FFmpeg's encoder writes quad so.
  assert.deepEqual(aacSpeakers(pceConfig({ front: "C", back: "C" })), [
    "FL",
    "FR",
    "BL",
    "BR",
  ]);
  assert.deepEqual(aacSpeakers(pceConfig({ front: "SC", back: "C", lfe: 1 })), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "BL",
    "BR",
  ]);
  // A back SCE after a pair is the back centre; a second back pair goes to the sides.
  assert.deepEqual(aacSpeakers(pceConfig({ front: "SC", back: "CS" })), [
    "FL",
    "FR",
    "FC",
    "BL",
    "BR",
    "BC",
  ]);
  assert.deepEqual(
    aacSpeakers(pceConfig({ front: "SC", back: "CC", lfe: 1 })),
    ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"],
  );
  // Coupling channels carry no plane, and the comment is skipped.
  assert.deepEqual(
    aacSpeakers(pceConfig({ front: "SC", cc: 1, comment: "alpha2" })),
    ["FL", "FR", "FC"],
  );
  // Orders FFmpeg cannot assign leave the layout unspecified: a front SCE after a pair (its
  // encoder's 3.1), and side elements, which it places only above the listener.
  assert.equal(aacSpeakers(pceConfig({ front: "CS", lfe: 1 })), null);
  assert.equal(aacSpeakers(pceConfig({ front: "SC", side: "C" })), null);
  assert.equal(
    codecSpeakers("aac", pceConfig({ front: "CS", lfe: 1 }), 4),
    null,
  );
  const truncated = pceConfig({ front: "SC", back: "C" }).subarray(0, 5);
  assert.equal(aacSpeakers(truncated), undefined);
  assert.equal(codecSpeakers("aac", truncated, 5), null);
  // HE-AAC v2 signalled explicitly: parametric stereo makes the mono element two planes.
  const psConfig = new BitWriter()
    .put(29, 5)
    .put(6, 4)
    .put(1, 4)
    .put(3, 4)
    .put(2, 5)
    .put(0, 3)
    .bytes();
  assert.deepEqual(aacSpeakers(psConfig), ["FL", "FR"]);
  assert.deepEqual(codecSpeakers("aac", psConfig, 2), ["FL", "FR"]);
});

/** A `chnl` payload after its version and flags. */
function chnl(configuration, rest) {
  return Uint8Array.of(1, configuration, ...rest);
}

test("an ISO chnl box declares a layout as FFmpeg's ff_mov_read_chnl reads it", () => {
  assert.deepEqual(chnlSpeakers(chnl(3, [0, 0, 0, 0, 0, 0, 0, 0]), 3), [
    "FC",
    "FL",
    "FR",
  ]);
  assert.deepEqual(chnlSpeakers(chnl(6, [0, 0, 0, 0, 0, 0, 0, 0]), 6), [
    "FC",
    "FL",
    "FR",
    "SL",
    "SR",
    "LFE",
  ]);
  // Omitted channels (bit n is the configuration's channel n) are left out of the count.
  assert.deepEqual(chnlSpeakers(chnl(6, [0, 0, 0, 0, 0, 0, 0, 0b100000]), 5), [
    "FC",
    "FL",
    "FR",
    "SL",
    "SR",
  ]);
  assert.equal(chnlSpeakers(chnl(3, [0, 0, 0, 0, 0, 0, 0, 0]), 4), null);
  assert.equal(chnlSpeakers(chnl(8, [0, 0, 0, 0, 0, 0, 0, 0]), 2), null);
  assert.equal(chnlSpeakers(chnl(3, [0, 0, 0, 0, 0, 0, 0, 0b1000]), 2), null);
  // Speaker positions; an explicit position (126) carries azimuth and elevation and names none.
  assert.deepEqual(chnlSpeakers(chnl(0, [0, 1, 8, 9]), 4), [
    "FL",
    "FR",
    "BL",
    "BR",
  ]);
  assert.deepEqual(chnlSpeakers(chnl(0, [126, 0, 0, 0, 2, 50]), 3), [
    "UNK",
    "FC",
    "UNK",
  ]);
  assert.equal(chnlSpeakers(chnl(0, [0, 1]), 3), null, "truncated");
  assert.equal(chnlSpeakers(chnl(0, [127, 127]), 2), null, "no named speaker");
  assert.equal(chnlSpeakers(Uint8Array.of(2, 0, 0, 1), 2), null);
});

/**
 * An E-AC-3 frame of `size` bytes: substream 0, 48 kHz, six blocks, a channel mode and LFE, and
 * for a dependent frame a channel map.
 */
function eac3Frame({
  dependent = false,
  mode,
  lfe = false,
  channelMap = null,
  blocks = 3,
  size = 64,
}) {
  const writer = new BitWriter()
    .put(0x0b77, 16)
    .put(dependent ? 1 : 0, 2)
    .put(0, 3);
  writer
    .put(size / 2 - 1, 11)
    .put(0, 2)
    .put(blocks, 2)
    .put(mode, 3)
    .put(lfe ? 1 : 0, 1);
  writer.put(16, 5).put(27, 5).put(0, 1); // bsid, dialnorm, no compression
  if (mode === 0) writer.put(27, 5).put(0, 1);
  if (dependent) {
    writer.put(channelMap === null ? 0 : 1, 1);
    if (channelMap !== null) writer.put(channelMap, 16);
  }
  return writer.bytes(size);
}

/** An AC-3 frame header: 48 kHz, a channel mode and LFE. */
function ac3Frame(mode, lfe) {
  const writer = new BitWriter()
    .put(0x0b77, 16)
    .put(0, 16)
    .put(0, 2)
    .put(10, 6);
  writer.put(8, 5).put(0, 3).put(mode, 3);
  if (mode === 2) writer.put(0, 2);
  else {
    if (mode & 1 && mode !== 1) writer.put(0, 2);
    if (mode & 4) writer.put(0, 2);
  }
  return writer.put(lfe ? 1 : 0, 1).bytes(32);
}

test("an AC-3 or E-AC-3 frame lays its planes out as FFmpeg's decoder does", () => {
  assert.deepEqual(ac3Speakers(ac3Frame(7, true)), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "SL",
    "SR",
  ]);
  assert.deepEqual(ac3Speakers(ac3Frame(4, false)), ["FL", "FR", "BC"]);
  assert.deepEqual(ac3Speakers(ac3Frame(0, false)), ["FL", "FR"], "dual mono");
  assert.deepEqual(codecSpeakers("ac3", null, 6, ac3Frame(7, true)), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "SL",
    "SR",
  ]);
  // A 7.1 as a 5.1 independent frame and a dependent frame whose channel map adds the back pair
  // (bit 6 from the most significant): FFmpeg names the union in native order.
  const independent = eac3Frame({ mode: 7, lfe: true });
  const backPair = eac3Frame({ dependent: true, mode: 2, channelMap: 1 << 9 });
  const packet = Uint8Array.of(...independent, ...backPair);
  assert.deepEqual(ac3Speakers(packet), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "BL",
    "BR",
    "SL",
    "SR",
  ]);
  assert.deepEqual(codecSpeakers("eac3", null, 8, packet), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "BL",
    "BR",
    "SL",
    "SR",
  ]);
  const otherBlocks = eac3Frame({
    dependent: true,
    mode: 2,
    channelMap: 1 << 9,
    blocks: 2,
  });
  assert.deepEqual(ac3Speakers(Uint8Array.of(...independent, ...otherBlocks)), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "SL",
    "SR",
  ]);
  const unmapped = eac3Frame({ dependent: true, mode: 2 });
  assert.equal(
    ac3Speakers(Uint8Array.of(...independent, ...unmapped)).length,
    6,
  );
  assert.equal(ac3Speakers(backPair), undefined);
  assert.equal(codecSpeakers("eac3", null, 2, backPair), null);
  assert.equal(codecSpeakers("ac3", null, 5, ac3Frame(7, true)), null);
});

/** A 16-bit big-endian DTS core frame of 1024 bytes. */
function dtsFrame({ mode = 9, lfe = 1, extension = null } = {}) {
  const writer = new BitWriter()
    .put(0x7ffe8001, 32)
    .put(1, 1)
    .put(31, 5)
    .put(0, 1);
  writer.put(15, 7).put(1023, 14).put(mode, 6).put(13, 4).put(15, 5).put(0, 1);
  writer
    .put(0, 4)
    .put(extension ?? 0, 3)
    .put(extension === null ? 0 : 1, 1)
    .put(0, 1)
    .put(lfe, 2);
  return writer.bytes(1024);
}

test("a DTS core lays its planes out as FFmpeg's decoder does", () => {
  assert.deepEqual(dtsSpeakers(dtsFrame()), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "SL",
    "SR",
  ]);
  assert.deepEqual(dtsSpeakers(dtsFrame({ mode: 7, lfe: 0 })), [
    "FL",
    "FR",
    "FC",
    "BC",
  ]);
  // X96 adds no channel; 16-bit little-endian words are the same frame.
  assert.deepEqual(dtsSpeakers(dtsFrame({ extension: 2 })), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "SL",
    "SR",
  ]);
  const swapped = dtsFrame().map((_, at, frame) => frame[at ^ 1]);
  assert.deepEqual(dtsSpeakers(swapped), ["FL", "FR", "FC", "LFE", "SL", "SR"]);
  assert.deepEqual(codecSpeakers("dts", null, 6, dtsFrame()), [
    "FL",
    "FR",
    "FC",
    "LFE",
    "SL",
    "SR",
  ]);
  // Channel extensions (XCH, XXCH) and an extension substream after the core carry layouts this
  // does not read; an invalid LFE flag is not a core FFmpeg decodes.
  assert.equal(dtsSpeakers(dtsFrame({ extension: 0 })), null);
  assert.equal(dtsSpeakers(dtsFrame({ extension: 6 })), null);
  const substream = Uint8Array.of(
    ...dtsFrame(),
    0x64,
    0x58,
    0x20,
    0x25,
    0,
    0,
    0,
    0,
  );
  assert.equal(dtsSpeakers(substream), null);
  assert.equal(dtsSpeakers(dtsFrame({ lfe: 3 })), null);
  assert.equal(
    dtsSpeakers(Uint8Array.of(0x64, 0x58, 0x20, 0x25, ...new Uint8Array(60))),
    null,
  );
});

/** A FLAC file's head: `fLaC`, a 4-channel STREAMINFO and a VORBIS_COMMENT with these comments. */
function flacHead(comments) {
  const encoder = new TextEncoder();
  const streamInfo = new Uint8Array(34);
  streamInfo[12] = 0b0000_0110; // four channels
  const vendor = encoder.encode("alpha2");
  const entries = comments.map((comment) => encoder.encode(comment));
  const block = new Uint8Array(
    8 +
      vendor.length +
      entries.reduce((total, entry) => total + 4 + entry.length, 0),
  );
  const view = new DataView(block.buffer);
  view.setUint32(0, vendor.length, true);
  block.set(vendor, 4);
  let at = 4 + vendor.length;
  view.setUint32(at, entries.length, true);
  at += 4;
  for (const entry of entries) {
    view.setUint32(at, entry.length, true);
    block.set(entry, at + 4);
    at += 4 + entry.length;
  }
  const header = (type, last, length) =>
    Uint8Array.of(
      (last ? 0x80 : 0) | type,
      length >> 16,
      (length >> 8) & 0xff,
      length & 0xff,
    );
  return Uint8Array.of(
    ...encoder.encode("fLaC"),
    ...header(0, false, 34),
    ...streamInfo,
    ...header(4, true, block.length),
    ...block,
  );
}

const reader = (bytes) => async (offset, length) =>
  bytes.subarray(offset, offset + length);

function flacSpeakers(file, description = null, channels = 4) {
  return readAudioSpeakers(
    reader(file),
    file.length,
    { id: 1, codec: "flac", numberOfChannels: channels },
    description,
  );
}

test("a FLAC channel-mask comment declares a layout where FFmpeg's demuxers read one", async () => {
  const fourZero = ["FL", "FR", "FC", "BC"];
  const quad = ["FL", "FR", "BL", "BR"];
  assert.deepEqual(
    await flacSpeakers(flacHead(["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x107"])),
    fourZero,
  );
  // strtol's bases, case-insensitive keys, leading space and trailing text; the first comment.
  for (const value of ["263", "0407", "0X107", " +0x107;0x33", "263 speakers"])
    assert.deepEqual(
      await flacSpeakers(
        flacHead(["TITLE=x", `waveformatextensible_channel_mask=${value}`]),
      ),
      fourZero,
      value,
    );
  assert.deepEqual(
    await flacSpeakers(
      flacHead([
        "WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x107",
        "WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x33",
      ]),
    ),
    fourZero,
  );
  // A mask FFmpeg refuses (zero, past its first 18 speakers, negative) or of another count leaves
  // FLAC's own four-channel order.
  for (const value of ["0", "0x40000", "-263", "0x7", "speakers"])
    assert.deepEqual(
      await flacSpeakers(
        flacHead([`WAVEFORMATEXTENSIBLE_CHANNEL_MASK=${value}`]),
      ),
      quad,
      value,
    );
  const id3 = Uint8Array.of(
    0x49,
    0x44,
    0x33,
    4,
    0,
    0,
    0,
    0,
    0,
    5,
    1,
    2,
    3,
    4,
    5,
  );
  assert.deepEqual(
    await flacSpeakers(
      Uint8Array.of(
        ...id3,
        ...flacHead(["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x107"]),
      ),
    ),
    fourZero,
  );
  // Matroska codec private data holds the blocks; an MP4 description (no EBML head) is not read.
  const blocks = flacHead(["WAVEFORMATEXTENSIBLE_CHANNEL_MASK=0x107"]);
  const matroska = Uint8Array.of(0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0);
  assert.deepEqual(await flacSpeakers(matroska, blocks), fourZero);
  assert.deepEqual(await flacSpeakers(new Uint8Array(16), blocks), quad);
});

test("a codec whose decoder sets the layout is not laid out by its container", async () => {
  // A WAVE mask beside an AAC program config element FFmpeg cannot assign: the decoder leaves
  // the layout unspecified, whatever the container says.
  const wav = new Uint8Array(12 + 8 + 40);
  wav.set(new TextEncoder().encode("RIFF\0\0\0\0WAVEfmt "));
  new DataView(wav.buffer).setUint32(16, 40, true);
  wav.set(fmt(4, 0x107), 20);
  const track = (codec) => ({ id: 1, codec, numberOfChannels: 4 });
  assert.deepEqual(
    await readAudioSpeakers(reader(wav), wav.length, track("pcm-s16"), null),
    ["FL", "FR", "FC", "BC"],
  );
  assert.equal(
    await readAudioSpeakers(
      reader(wav),
      wav.length,
      track("aac"),
      pceConfig({ front: "CS", lfe: 1 }),
    ),
    null,
  );
  let asked = 0;
  const packet = async () => {
    asked += 1;
    return ac3Frame(6, false);
  };
  assert.deepEqual(
    await readAudioSpeakers(
      reader(wav),
      wav.length,
      track("ac3"),
      null,
      packet,
    ),
    ["FL", "FR", "SL", "SR"],
  );
  assert.equal(asked, 1);
  await readAudioSpeakers(
    reader(wav),
    wav.length,
    track("pcm-s16"),
    null,
    packet,
  );
  assert.equal(asked, 1, "a PCM track reads no packet");
});
