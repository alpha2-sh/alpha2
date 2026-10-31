/**
 * Measures the channel order a browser's audio decoder delivers.
 *
 * WebCodecs leaves `AudioData` plane order to the decoder. `codecSpeakers` assumes `FFmpeg`'s
 * order, which Chromium and Firefox keep, but Safari's AAC decoder delivers bitstream order (C L R
 * for configuration 3), and Vorbis or AC-3 native order would differ the same way. A built-in file
 * with a distinct tone per plane is decoded once per stream kind, and the tones map each decoded
 * plane back to `FFmpeg`'s order.
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
 * A short 48 kHz file whose plane `k`, in `FFmpeg`'s order, holds only tone `tones[k]`. Written by
 * `scripts/generate-decoder-order-fixtures.mjs`.
 */
export interface DecoderOrderFixture {
  /** Speakers in `FFmpeg`'s order. */
  readonly speakers: readonly string[];
  /** Tone per plane in hertz, in `FFmpeg`'s order (93.75 Hz for the LFE). */
  readonly tones: readonly number[];
  /** First sample analyzed, past the codec priming and the encoder's first frame. */
  readonly start: number;
  /** Base64 file contents. */
  readonly file: string;
}

/** Fixtures per codec, loaded on first use. */
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

/** Samples analyzed per plane. Each fixture tone sits on a bin of this length at 48 kHz. */
const ANALYSIS_FRAMES = 1_024;
/** Power ratio the loudest tone needs over the next to identify a plane. */
const TONE_DOMINANCE = 10;

/**
 * Fixture key for the stream property that decides decoder plane order, or `null` when order
 * cannot vary (one or two channels, or codecs with a single order such as FLAC and PCM):
 *
 * - AAC: channel configuration (`aac:6`), ADTS configuration without a description
 *   (`aac:adts:6`), or program config elements (`aac:pce:SF CF CB LL`);
 * - Opus mapping family 1: channel count (`opus:1:6`);
 * - Vorbis: channel count (`vorbis:6`);
 * - AC-3 and E-AC-3: speakers in the first frame header (`ac3:FL FR FC LFE SL SR`).
 *
 * Keys without a fixture are assumed to decode in `FFmpeg`'s order.
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

/** Fixture for a {@link decoderOrderKey} key, or `null`. */
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
 * Maps each decoded plane to its index in `FFmpeg`'s order by its dominant tone. Returns `null`
 * unless every plane is identified exactly once.
 *
 * @param planes Decoded fixture planes, in the decoder's order.
 * @param tones Tone per plane in `FFmpeg`'s order.
 * @param start First sample analyzed.
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

/** Decoded fixture planes in the decoder's order. */
export interface DecodedFixture {
  readonly planes: readonly Float32Array[];
  readonly sampleRate: number;
}

export type FixtureDecode = (file: Uint8Array) => Promise<DecodedFixture>;

/** Plane order of `fixture` as `decode` delivers it, or `null` when it cannot be measured. */
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

/** Decodes through the same mediabunny path a source of this codec uses. */
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

/** Cached orders by fixture key; failed measurements cache `null`. */
const measured = new Map<string, Promise<readonly number[] | null>>();

/**
 * Decoder plane order for streams with this key, measured once and cached. Resolves `null` (use
 * `FFmpeg`'s order) when there is no key or fixture, or measurement fails. Never rejects.
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
