/**
 * The channel order a browser's audio decoder delivers, measured rather than assumed.
 *
 * WebCodecs leaves the order of an `AudioData`'s planes to the decoder. The speakers
 * `codecSpeakers` reports are in the order `FFmpeg`'s decoder delivers (the order a native host
 * decodes in), which Chromium's and Firefox's decoders keep; Safari's AAC decoder delivers the
 * bitstream's order instead (C L R for configuration 3), and a decoder that delivers libopus's or
 * libvorbis's own order (Vorbis's: L C R) or an AC-3 frame's (L C R) would differ the same way. So
 * the order is measured: a short built-in file whose planes each hold a tone of their own is
 * decoded through the same decoder a source is, once per kind of stream in each page or worker,
 * and each plane's tone says which of `FFmpeg`'s planes it is. A source's blocks are then laid out
 * in `FFmpeg`'s order whatever the decoder delivers.
 */
import { ALL_FORMATS, AudioSampleSink, BufferSource, Input } from "mediabunny";

import {
  ADTS_CONFIGURATIONS,
  aacChannelConfiguration,
  aacProgramConfigElements,
  ac3Speakers,
  opusMappingFamily,
} from "./audio-layout.js";

/**
 * A short 48 kHz file whose plane `k`, decoded in the order `FFmpeg`'s decoder delivers, holds the
 * tone `tones[k]` and nothing else: what a browser decoder's channel order is measured with
 * (`scripts/generate-decoder-order-fixtures.mjs` writes them).
 */
export interface DecoderOrderFixture {
  /** The speakers `FFmpeg` decodes the file into, in its order. */
  readonly speakers: readonly string[];
  /** Each plane's tone in hertz, by plane in `FFmpeg`'s order: 93.75 Hz for the LFE. */
  readonly tones: readonly number[];
  /**
   * The sample of the decoded planes the tones are read from, 1024 samples on: past the codec's
   * priming and the encoder's first frame.
   */
  readonly start: number;
  /** The file, base64. */
  readonly file: string;
}

/** Each codec's fixtures, loaded when a stream of it is first measured. */
const FIXTURES: Readonly<
  Record<
    string,
    () => Promise<{
      readonly ORDER_FIXTURES: Readonly<Record<string, DecoderOrderFixture>>;
    }>
  >
> = {
  aac: () => import("./generated/aac-order-fixtures.js"),
  opus: () => import("./generated/opus-order-fixtures.js"),
  vorbis: () => import("./generated/vorbis-order-fixtures.js"),
  ac3: () => import("./generated/ac3-order-fixtures.js"),
  eac3: () => import("./generated/eac3-order-fixtures.js"),
};

/** How many samples of each plane are read. Each fixture tone sits on a bin of it at 48 kHz. */
const ANALYSIS_FRAMES = 1_024;
/** How many times the next tone's power a plane's loudest tone must have to name the plane. */
const TONE_DOMINANCE = 10;

/**
 * What decides the order `codec`'s decoder delivers a stream's planes in, as a fixture key, or
 * `null` for a stream of one or two channels, which every decoder delivers in one order, and for
 * a codec whose decoders deliver one order (`FFmpeg` keeps FLAC's, PCM's and an unspecified
 * layout's), or that no browser path decodes:
 *
 * - AAC: the channel configuration (`aac:6`), the ADTS header's for a stream without a
 *   description (`aac:adts:6`), or the elements of a program config element (`aac:pce:SF CF CB LL`,
 *   {@link aacProgramConfigElements});
 * - Opus mapping family 1: the channel count (`opus:1:6`);
 * - Vorbis: the channel count (`vorbis:6`);
 * - AC-3 and E-AC-3: the speakers the first packet's frame header declares
 *   (`ac3:FL FR FC LFE SL SR`).
 *
 * A key without a fixture is not measured, and its decoder's order is taken as `FFmpeg`'s.
 */
export function decoderOrderKey(
  codec: string | null,
  description: Uint8Array | null,
  channels: number,
  packet: Uint8Array | null = null,
): string | null {
  if (channels <= 2) return null;
  switch (codec) {
    case "aac": {
      if (description === null) {
        const configuration = ADTS_CONFIGURATIONS[channels];
        return configuration === undefined ? null : `aac:adts:${configuration}`;
      }
      const configuration = aacChannelConfiguration(description);
      if (configuration !== null) return `aac:${configuration}`;
      const elements = aacProgramConfigElements(description);
      return elements === null ? null : `aac:pce:${elements}`;
    }
    case "opus":
      return opusMappingFamily(description) === 1 ? `opus:1:${channels}` : null;
    case "vorbis":
      return `vorbis:${channels}`;
    case "ac3":
    case "eac3": {
      const speakers = packet === null ? undefined : ac3Speakers(packet);
      return speakers === undefined ? null : `${codec}:${speakers.join(" ")}`;
    }
    default:
      return null;
  }
}

/** The fixture that measures streams keyed `key` ({@link decoderOrderKey}), or `null`. */
export async function decoderOrderFixture(
  key: string,
): Promise<DecoderOrderFixture | null> {
  const load = FIXTURES[key.slice(0, key.indexOf(":"))];
  if (load === undefined) return null;
  return (await load()).ORDER_FIXTURES[key] ?? null;
}

function tonePower(
  samples: Float32Array,
  frequency: number,
  sampleRate: number,
): number {
  const coefficient = 2 * Math.cos((2 * Math.PI * frequency) / sampleRate);
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

/**
 * For each decoded plane, the plane of `FFmpeg`'s order it holds: the plane whose tone it holds
 * well above every other. `null` unless that names every plane once, or when a plane is too short
 * to read.
 *
 * @param planes A fixture's decoded planes, in the decoder's order.
 * @param tones Each plane's tone in `FFmpeg`'s order.
 * @param start The sample the tones are read from.
 */
export function planeOrderOfTones(
  planes: readonly Float32Array[],
  tones: readonly number[],
  sampleRate: number,
  start: number,
): number[] | null {
  if (planes.length !== tones.length) return null;
  const order: number[] = [];
  for (const plane of planes) {
    if (plane.length < start + ANALYSIS_FRAMES) return null;
    const samples = plane.subarray(start, start + ANALYSIS_FRAMES);
    const powers = tones.map((tone) => tonePower(samples, tone, sampleRate));
    const loudest = powers.indexOf(Math.max(...powers));
    const next = Math.max(...powers.filter((_, tone) => tone !== loudest));
    if (!(powers[loudest] > TONE_DOMINANCE * next)) return null;
    order.push(loudest);
  }
  return new Set(order).size === order.length ? order : null;
}

/** A fixture decoded: its planes in the decoder's order and their sample rate. */
export interface DecodedFixture {
  readonly planes: readonly Float32Array[];
  readonly sampleRate: number;
}

/** Decodes a fixture file's primary audio track whole. */
export type FixtureDecode = (file: Uint8Array) => Promise<DecodedFixture>;

/**
 * The plane order `decode` delivers `fixture` in ({@link planeOrderOfTones}), or `null` when it
 * cannot be measured: the decode fails, or its planes do not each hold one tone.
 */
export async function measurePlaneOrder(
  fixture: DecoderOrderFixture,
  decode: FixtureDecode,
): Promise<number[] | null> {
  let decoded: DecodedFixture;
  try {
    decoded = await decode(base64Bytes(fixture.file));
  } catch {
    return null;
  }
  return planeOrderOfTones(
    decoded.planes,
    fixture.tones,
    decoded.sampleRate,
    fixture.start,
  );
}

function base64Bytes(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1)
    bytes[index] = binary.charCodeAt(index);
  return bytes;
}

/** Decodes `file` with mediabunny, through the decoder a source of the same codec gets. */
async function decodeWithMediabunny(file: Uint8Array): Promise<DecodedFixture> {
  const input = new Input({
    source: new BufferSource(file),
    formats: ALL_FORMATS,
  });
  try {
    const track = await input.getPrimaryAudioTrack();
    if (track === null) throw new Error("the fixture has no audio track");
    const chunks: Float32Array[][] = [];
    let sampleRate = track.sampleRate;
    for await (const sample of new AudioSampleSink(track).samples()) {
      try {
        sampleRate = sample.sampleRate;
        const planes: Float32Array[] = [];
        for (let plane = 0; plane < sample.numberOfChannels; plane += 1) {
          const data = new Float32Array(sample.numberOfFrames);
          sample.copyTo(data, { planeIndex: plane, format: "f32-planar" });
          planes.push(data);
        }
        chunks.push(planes);
      } finally {
        sample.close();
      }
    }
    const channels = chunks[0]?.length ?? 0;
    if (chunks.some((planes) => planes.length !== channels))
      throw new Error("the fixture changed channel count");
    const planes = Array.from({ length: channels }, (_, plane) => {
      const joined = new Float32Array(
        chunks.reduce((frames, planes) => frames + planes[plane].length, 0),
      );
      let at = 0;
      for (const chunk of chunks) {
        joined.set(chunk[plane], at);
        at += chunk[plane].length;
      }
      return joined;
    });
    return { planes, sampleRate };
  } finally {
    input.dispose();
  }
}

/** Measured orders by fixture key; a measurement that failed stays `null`. */
const measured = new Map<string, Promise<readonly number[] | null>>();

/**
 * For each plane mediabunny's decoder delivers for streams keyed `key` ({@link decoderOrderKey}),
 * the plane of `FFmpeg`'s order it holds. Measured once per key ({@link measurePlaneOrder}) and
 * kept; resolves `null` when there is nothing to measure (no key, or no fixture for it) or the
 * measurement fails, and the decoder's order is then taken as `FFmpeg`'s. Never rejects.
 */
export function decoderPlaneOrder(
  key: string | null,
): Promise<readonly number[] | null> {
  if (key === null) return Promise.resolve(null);
  let order = measured.get(key);
  if (order === undefined) {
    order = decoderOrderFixture(key).then(
      (fixture) =>
        fixture === null
          ? null
          : measurePlaneOrder(fixture, decodeWithMediabunny),
      () => null,
    );
    measured.set(key, order);
  }
  return order;
}
