#!/usr/bin/env node
// Generates the fixtures a browser audio decoder's channel order is measured with.
//
//   FFMPEG_ENCODERS=<an ffmpeg with libopus and libvorbis> \
//     node scripts/generate-decoder-order-fixtures.mjs
//
// Each fixture is a short 48 kHz file whose decoded plane k, in FFmpeg's decoder order, holds a
// tone of its own (93.75 Hz for the LFE, 375 * (k + 1) Hz otherwise), keyed by `decoderOrderKey`
// in `decoder-order.ts`. Covers AAC configurations and program config elements (MP4 and ADTS),
// Opus and Vorbis in Ogg, and AC-3/E-AC-3 in MP4. Each file is encoded once to map inputs to
// decoded planes, then again so plane k holds its own tone, and checked by decoding it again.
// Writes `packages/alpha2/src/generated/<codec>-order-fixtures.ts`.
//
// Needs `ffmpeg` on PATH, FFMPEG_ENCODERS, python3 and libfdk-aac.so.2. Another FFmpeg may write
// other bytes; any file that passes the check measures the same order.

import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  ALL_FORMATS,
  BufferSource,
  BufferTarget,
  EncodedAudioPacketSource,
  EncodedPacket,
  EncodedPacketSink,
  Input,
  Mp4OutputFormat,
  Output,
} from "mediabunny";
import { format, resolveConfig } from "prettier";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const generated = join(root, "packages/alpha2/src/generated");
const encoders = process.env.FFMPEG_ENCODERS;
if (!encoders)
  throw new Error(
    "FFMPEG_ENCODERS must name an FFmpeg with libopus and libvorbis",
  );
const RATE = 48_000;
const FRAMES = 1_024;
const LFE_TONE = 93.75;
const toneOf = (plane) => 375 * (plane + 1);

/** WAVE channel mask bits, in order (`WAVEFORMATEXTENSIBLE`). */
const MASK_SPEAKERS = [
  "FL",
  "FR",
  "FC",
  "LFE",
  "BL",
  "BR",
  "FLC",
  "FRC",
  "BC",
  "SL",
  "SR",
  "TC",
  "TFL",
  "TFC",
  "TFR",
  "TBL",
  "TBC",
  "TBR",
];

function power(samples, frequency) {
  const coefficient = 2 * Math.cos((2 * Math.PI * frequency) / RATE);
  let previous = 0;
  let before = 0;
  for (const sample of samples) {
    const current = sample + coefficient * previous - before;
    before = previous;
    previous = current;
  }
  return (
    previous * previous + before * before - coefficient * previous * before
  );
}

/** The loudest of `tones` in `samples` when it is at least 100 times the next, else `null`. */
function dominantTone(samples, tones) {
  const powers = tones.map((tone) => power(samples, tone));
  const loudest = powers.indexOf(Math.max(...powers));
  const next = Math.max(...powers.filter((_, tone) => tone !== loudest));
  return powers[loudest] >= 100 * next ? loudest : null;
}

/** Decodes `file` with FFmpeg: its planes and the speakers its WAVE channel mask names. */
function decode(path) {
  const wav = execFileSync("ffmpeg", [
    ...["-hide_banner", "-loglevel", "error", "-i", path],
    ...["-map_metadata", "-1", "-fflags", "+bitexact", "-flags", "+bitexact"],
    ...["-f", "wav", "-acodec", "pcm_f32le", "-"],
  ]);
  let channels = 0;
  let mask = 0;
  let data = null;
  for (let at = 12; at + 8 <= wav.length;) {
    const id = wav.toString("latin1", at, at + 4);
    const size = wav.readUInt32LE(at + 4);
    if (id === "fmt ") {
      channels = wav.readUInt16LE(at + 10);
      if (wav.readUInt16LE(at + 8) === 0xfffe) mask = wav.readUInt32LE(at + 28);
    } else if (id === "data") data = wav.subarray(at + 8, at + 8 + size);
    at += 8 + size + (size & 1);
  }
  if (data === null || mask === 0)
    throw new Error(`${path}: FFmpeg decodes no channel layout`);
  const speakers = MASK_SPEAKERS.filter((_, bit) => mask & (1 << bit));
  if (speakers.length !== channels)
    throw new Error(
      `${path}: a mask of ${speakers.length} speakers for ${channels} channels`,
    );
  const frames = data.length / 4 / channels;
  const planes = Array.from({ length: channels }, (_, plane) =>
    Float32Array.from({ length: frames }, (_, frame) =>
      data.readFloatLE((frame * channels + plane) * 4),
    ),
  );
  return { planes, speakers };
}

/**
 * Encodes a fixture whose FFmpeg-decoded plane k holds its own tone. `encode(tones, path)` writes
 * a file whose input channel i carries `tones[i]`; `start` is where the tones are read.
 */
async function orderedFixture(name, channels, start, encode) {
  const path = join(directory, name);
  const window = (plane, from) => plane.subarray(from, from + FRAMES);
  // Which input each decoded plane holds; the LFE, band-limited, holds none of these tones.
  const probe = Array.from({ length: channels }, (_, input) => toneOf(input));
  await encode(probe, path);
  const first = decode(path);
  if (first.planes.length !== channels)
    throw new Error(`${name}: FFmpeg decodes ${first.planes.length} channels`);
  const lfe = first.speakers.indexOf("LFE");
  const inputOf = first.planes.map((plane, index) =>
    index === lfe ? null : dominantTone(window(plane, start), probe),
  );
  if (lfe >= 0)
    inputOf[lfe] = probe.findIndex((_, input) => !inputOf.includes(input));
  if (new Set(inputOf).size !== channels || inputOf.includes(null))
    throw new Error(`${name}: decoded planes hold inputs ${inputOf}`);
  const tones = first.speakers.map((speaker, plane) =>
    speaker === "LFE" ? LFE_TONE : toneOf(plane),
  );
  const inputs = new Array(channels);
  inputOf.forEach((input, plane) => (inputs[input] = tones[plane]));
  await encode(inputs, path);
  const { planes, speakers } = decode(path);
  for (const [plane, samples] of planes.entries())
    for (const from of [start, start + FRAMES])
      if (dominantTone(window(samples, from), tones) !== plane)
        throw new Error(
          `${name}: plane ${plane} does not hold its tone at ${from}`,
        );
  const file = readFileSync(path);
  console.log(`${name}: ${speakers.join(" ")}, ${file.length} bytes`);
  return { speakers, tones, start, file };
}

const expression = (tones) =>
  tones.map((tone) => `0.25*sin(2*PI*${tone}*t)`).join("|");

/** An FFmpeg encode of `layout` (an FFmpeg layout name) from `duration` seconds of tones. */
const ffmpegEncode = (binary, layout, duration, codec) => (tones, path) =>
  execFileSync(binary, [
    ...["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"],
    `aevalsrc=${expression(tones)}:c=${layout}:s=${RATE}:d=${duration}`,
    ...codec,
    ...[
      "-fflags",
      "+bitexact",
      "-flags:a",
      "+bitexact",
      "-map_metadata",
      "-1",
      path,
    ],
  ]);

/** The `AudioSpecificConfig` of an MP4 or ADTS file's AAC track, and its access units. */
async function aacTrack(path) {
  const input = new Input({
    source: new BufferSource(readFileSync(path)),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    const config = await track.getDecoderConfig();
    const packets = [];
    for await (const packet of new EncodedPacketSink(track).packets())
      packets.push(packet);
    return {
      description: new Uint8Array(config.description),
      channels: track.numberOfChannels,
      packets,
    };
  } finally {
    input.dispose();
  }
}

/** Writes AAC access units under `description` into an MP4 (FFmpeg's muxer, bit-exact). */
async function writeAacMp4(path, description, channels, packets) {
  const target = new BufferTarget();
  const output = new Output({ format: new Mp4OutputFormat(), target });
  const source = new EncodedAudioPacketSource("aac");
  output.addAudioTrack(source);
  await output.start();
  for (const [index, packet] of packets.entries())
    await source.add(
      new EncodedPacket(packet.data, "key", packet.timestamp, packet.duration),
      index === 0
        ? {
            decoderConfig: {
              codec: "mp4a.40.2",
              numberOfChannels: channels,
              sampleRate: RATE,
              description,
            },
          }
        : undefined,
    );
  await output.finalize();
  // mediabunny stamps the time; FFmpeg's bit-exact remux does not.
  const staged = `${path}.staged.mp4`;
  writeFileSync(staged, new Uint8Array(target.buffer));
  execFileSync("ffmpeg", [
    ...["-hide_banner", "-loglevel", "error", "-y", "-i", staged],
    ...["-c", "copy", "-map_metadata", "-1", "-fflags", "+bitexact", path],
  ]);
  rmSync(staged);
}

class BitWriter {
  bits = [];
  write(value, count) {
    for (let bit = count - 1; bit >= 0; bit -= 1)
      this.bits.push((value >> bit) & 1);
  }
  bytes() {
    const bytes = new Uint8Array(Math.ceil(this.bits.length / 8));
    this.bits.forEach(
      (bit, index) => (bytes[index >> 3] |= bit << (7 - (index & 7))),
    );
    return bytes;
  }
}

/**
 * An AAC-LC 48 kHz `AudioSpecificConfig` declaring `elements` (`SF`, `CB`, `LL`: SCE, CPE or LFE
 * at the front, side, back or LFE position) in a program config element. Each element's instance
 * tag counts the elements of its kind before it, as the encoders here number them.
 */
function pceConfig(elements) {
  const writer = new BitWriter();
  writer.write(2, 5); // AAC-LC
  writer.write(3, 4); // 48 kHz
  writer.write(0, 4); // channel configuration 0: a program config element
  writer.write(0, 3); // frame length, core coder, extension flags
  const at = (position) =>
    elements.filter((element) => element[1] === position);
  writer.write(0, 4); // element instance tag
  writer.write(1, 2); // object type: AAC-LC
  writer.write(3, 4);
  writer.write(at("F").length, 4);
  writer.write(at("S").length, 4);
  writer.write(at("B").length, 4);
  writer.write(at("L").length, 2);
  writer.write(0, 3); // associated data elements
  writer.write(0, 4); // coupling channel elements
  writer.write(0, 3); // no mixdowns
  const tags = { S: 0, C: 0, L: 0 };
  for (const position of ["F", "S", "B", "L"])
    for (const [type] of at(position)) {
      if (position !== "L") writer.write(type === "C" ? 1 : 0, 1);
      writer.write(tags[type]++, 4);
    }
  writer.write(0, (8 - (writer.bits.length & 7)) & 7); // byte alignment
  writer.write(0, 8); // comment field bytes
  return writer.bytes();
}

const directory = mkdtempSync(join(tmpdir(), "alpha2-order-"));
const fixtures = { aac: {}, opus: {}, vorbis: {}, ac3: {}, eac3: {} };
try {
  const aacLayouts = { 3: "3.0", 4: "4.0", 5: "5.0", 6: "5.1", 7: "7.1" };
  const configurationFiles = {};
  for (const [configuration, layout] of Object.entries(aacLayouts)) {
    const channels = configuration === "7" ? 8 : Number(configuration);
    const codec = ["-c:a", "aac", "-b:a", `${12 * channels}k`];
    const mp4 = await orderedFixture(
      `aac-${configuration}.mp4`,
      channels,
      1_024,
      ffmpegEncode("ffmpeg", layout, 0.064, codec),
    );
    fixtures.aac[`aac:${configuration}`] = mp4;
    configurationFiles[configuration] = join(
      directory,
      `aac-${configuration}.mp4`,
    );
    fixtures.aac[`aac:adts:${configuration}`] = await orderedFixture(
      `aac-adts-${configuration}.aac`,
      channels,
      1_024,
      ffmpegEncode("ffmpeg", layout, 0.064, [...codec, "-f", "adts"]),
    );
  }
  // Configurations FFmpeg's encoder writes as program config elements; libfdk-aac writes them.
  const fdkLayouts = { 11: 7, 12: 8, 14: 8 };
  for (const [configuration, channels] of Object.entries(fdkLayouts)) {
    const name = `aac-${configuration}.mp4`;
    fixtures.aac[`aac:${configuration}`] = await orderedFixture(
      name,
      channels,
      3_072,
      (tones, path) => {
        // 128 ms, after libfdk-aac's 2048-sample delay, which an MP4 without an edit list keeps.
        const frames = 6_144;
        const pcm = Buffer.alloc(frames * channels * 2);
        for (let frame = 0; frame < frames; frame += 1)
          tones.forEach((tone, channel) =>
            pcm.writeInt16LE(
              Math.round(8_192 * Math.sin((2 * Math.PI * tone * frame) / RATE)),
              (frame * channels + channel) * 2,
            ),
          );
        const encoded = JSON.parse(
          execFileSync(
            "python3",
            [
              join(root, "scripts/lib/fdk-aac-encode.py"),
              configuration,
              String(channels),
              String(16_000 * channels),
            ],
            { input: pcm, maxBuffer: 64 << 20 },
          ),
        );
        const packets = encoded.units.map((unit, index) => ({
          data: Buffer.from(unit, "hex"),
          timestamp: (index * 1_024) / RATE,
          duration: 1_024 / RATE,
        }));
        return writeAacMp4(
          path,
          Buffer.from(encoded.config, "hex"),
          channels,
          packets,
        );
      },
    );
    configurationFiles[configuration] = join(directory, name);
  }
  // Program config elements FFmpeg's encoder writes.
  for (const [elements, layout] of [
    ["CF LL", "2.1"],
    ["CF SB", "3.0(back)"],
    ["CF CB", "quad"],
  ])
    fixtures.aac[`aac:pce:${elements}`] = await orderedFixture(
      `aac-pce-${layout}.mp4`,
      elements
        .split(" ")
        .reduce((count, element) => count + (element[0] === "C" ? 2 : 1), 0),
      1_024,
      ffmpegEncode("ffmpeg", layout, 0.064, ["-c:a", "aac", "-b:a", "48k"]),
    );
  // The elements of a channel configuration declared in a program config element instead.
  for (const [configuration, elements] of [
    [3, "SF CF"],
    [4, "SF CF SB"],
    [5, "SF CF CB"],
    [6, "SF CF CB LL"],
    [11, "SF CF CB SB LL"],
    [12, "SF CF CB CB LL"],
  ]) {
    const source = fixtures.aac[`aac:${configuration}`];
    fixtures.aac[`aac:pce:${elements}`] = await orderedFixture(
      `aac-pce-${configuration}.mp4`,
      source.tones.length,
      source.start,
      // The configuration's file, whose planes already hold these tones, under a PCE.
      async (tones, path) => {
        const track = await aacTrack(configurationFiles[configuration]);
        await writeAacMp4(
          path,
          pceConfig(elements.split(" ")),
          track.channels,
          track.packets,
        );
      },
    );
  }
  // Opus mapping family 1 and Vorbis, whose decoders FFmpeg reorders from Vorbis's channel order.
  const vorbisLayouts = {
    3: "3.0",
    4: "quad",
    5: "5.0",
    6: "5.1",
    7: "6.1",
    8: "7.1",
  };
  for (const [channels, layout] of Object.entries(vorbisLayouts)) {
    fixtures.opus[`opus:1:${channels}`] = await orderedFixture(
      `opus-${channels}.ogg`,
      Number(channels),
      1_024,
      ffmpegEncode(encoders, layout, 0.096, [
        ...[
          "-c:a",
          "libopus",
          "-mapping_family",
          "1",
          "-b:a",
          `${24 * channels}k`,
          "-f",
          "ogg",
        ],
      ]),
    );
    fixtures.vorbis[`vorbis:${channels}`] = await orderedFixture(
      `vorbis-${channels}.ogg`,
      Number(channels),
      1_024,
      ffmpegEncode(encoders, layout, 0.096, [
        "-c:a",
        "libvorbis",
        "-q:a",
        "0",
        "-f",
        "ogg",
      ]),
    );
  }
  // AC-3 and E-AC-3: each channel mode (2/0 to 3/2) of three or more channels, with and without
  // the LFE, but 2/2 with the LFE, which FFmpeg's encoders do not write.
  const ac3Layouts = [
    "FL+FR+LFE",
    "FL+FR+FC",
    "FL+FR+FC+LFE",
    "FL+FR+BC",
    "FL+FR+LFE+BC",
    "FL+FR+FC+BC",
    "FL+FR+FC+LFE+BC",
    "FL+FR+SL+SR",
    "FL+FR+FC+SL+SR",
    "FL+FR+FC+LFE+SL+SR",
  ];
  for (const codec of ["ac3", "eac3"])
    for (const layout of ac3Layouts) {
      const fixture = await orderedFixture(
        `${codec}-${layout}.mp4`,
        layout.split("+").length,
        1_536,
        ffmpegEncode("ffmpeg", layout, 0.096, ["-c:a", codec, "-b:a", "96k"]),
      );
      fixtures[codec][`${codec}:${fixture.speakers.join(" ")}`] = fixture;
    }
} finally {
  rmSync(directory, { recursive: true, force: true });
}

const DESCRIPTIONS = {
  aac: "AAC-LC: by channel configuration (MP4), ADTS configuration and program config element",
  opus: "Opus mapping family 1 (Ogg): by channel count",
  vorbis: "Vorbis (Ogg): by channel count",
  ac3: "AC-3 (MP4): by the speakers its frame header declares",
  eac3: "E-AC-3 (MP4): by the speakers its frame header declares",
};
for (const [codec, byKey] of Object.entries(fixtures)) {
  const entries = Object.entries(byKey)
    .map(
      ([key, { speakers, tones, start, file }]) =>
        `  ${JSON.stringify(key)}: {\n    speakers: ${JSON.stringify(speakers)},\n    tones: ${JSON.stringify(tones)},\n    start: ${start},\n    file: ${JSON.stringify(file.toString("base64"))},\n  },`,
    )
    .join("\n");
  const outPath = join(generated, `${codec}-order-fixtures.ts`);
  const source = `// Generated by scripts/generate-decoder-order-fixtures.mjs. Do not edit; run the script.

import type { DecoderOrderFixture } from "../decoder-order.js";

/** ${DESCRIPTIONS[codec]}, keyed as \`decoderOrderKey\` keys a source. */
export const ORDER_FIXTURES: Readonly<Record<string, DecoderOrderFixture>> = {
${entries}
};
`;
  writeFileSync(
    outPath,
    await format(source, {
      ...(await resolveConfig(outPath)),
      filepath: outPath,
    }),
  );
  console.log(`wrote ${outPath}`);
}
