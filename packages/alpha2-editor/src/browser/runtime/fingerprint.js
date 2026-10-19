// Evidence of an imported file's media, recorded in the project so any host can tell later whether
// media it finds is the same: the bytes' hash, their length and date, and what the demuxer sees.
// Whether two fingerprints match is the engine's decision (`alpha2/assets`), never made here.
import { sha256Blob } from "alpha2/assets";
import {
  colorMetadataFromVideoColorSpace,
  sampleAspectForStream,
} from "alpha2/hdr-frames";
import { TICKS_PER_SECOND, validFrameRate } from "alpha2/project";
import { ALL_FORMATS, BlobSource, EncodedPacketSink, Input } from "mediabunny";

/**
 * The fingerprint of `file` as media of `kind`. `size` is an image's decoded size; video and audio
 * are probed. `hash` makes the content hash, `sha256Blob` on this thread unless the caller hashes
 * elsewhere (`hashInWorker`).
 * @param {File} file
 * @param {string} kind
 * @param {{ signal?: AbortSignal, onProgress?: (bytes: number) => void, size?: { width: number, height: number }, hash?: (file: Blob, options: { signal?: AbortSignal, onProgress?: (bytes: number) => void }) => Promise<string> }} [options]
 * @returns {Promise<import("alpha2/project").MediaFingerprint>}
 */
export async function fingerprintFile(file, kind, options = {}) {
  const hash = options.hash ?? sha256Blob;
  const fingerprint = {
    content_sha256: await hash(file, {
      signal: options.signal,
      onProgress: options.onProgress,
    }),
    byte_length: file.size,
    modified_ms: file.lastModified,
  };
  if (kind === "image" && options.size)
    return {
      ...fingerprint,
      streams: {
        video: { width: options.size.width, height: options.size.height },
      },
    };
  if (kind !== "video" && kind !== "audio") return fingerprint;
  return { ...fingerprint, ...(await probeEvidence(file)) };
}

/** What the demuxer sees: the primary streams' shapes, the duration, and a declared color. */
export async function probeEvidence(file) {
  const input = new Input({
    source: new BlobSource(file),
    formats: ALL_FORMATS,
  });
  try {
    const video = await input.getPrimaryVideoTrack();
    const audio = await input.getPrimaryAudioTrack();
    const streams = {};
    let color;
    if (video) {
      const rate = rationalRate(await presentedRate(video).catch(() => null));
      const shown = await displaySize(video);
      streams.video = {
        width: shown.width,
        height: shown.height,
        ...(rate ? { frame_rate: rate } : {}),
      };
      const space = await video.getColorSpace().catch(() => null);
      if (
        space &&
        (space.primaries || space.transfer || space.matrix || space.fullRange)
      )
        color = colorMetadataFromVideoColorSpace(space);
    }
    if (audio)
      streams.audio = {
        channels: audio.numberOfChannels,
        sample_rate: audio.sampleRate,
      };
    // The latest end of any track: a stream starting at 1.4 s and lasting 2 s ends at 3.4 s.
    const seconds = await input.computeDuration().catch(() => null);
    return {
      ...(video || audio ? { streams } : {}),
      ...(Number.isFinite(seconds)
        ? { duration_ticks: Math.round(seconds * TICKS_PER_SECOND) }
        : {}),
      ...(color ? { color } : {}),
    };
  } finally {
    input.dispose();
  }
}

/**
 * The size a video track is shown at, in square pixels and turned as it is shown, as every host
 * finds it: the sample aspect ratio is the container's when it declares one that is not square,
 * else the one the H.264 or HEVC parameter sets signal (FFmpeg's `av_guess_sample_aspect_ratio`);
 * mediabunny reads only the container's. The stretch is the engine's
 * (`SampleAspectRatio::square_pixel_size`): the longer side of a sample grows, rounded to the
 * nearest pixel.
 * @param {import("mediabunny").InputVideoTrack} track
 * @returns {Promise<{ width: number, height: number }>}
 */
export async function displaySize(track) {
  const declared = await track.getPixelAspectRatio().catch(() => null);
  let aspect = declared && declared.num !== declared.den ? declared : undefined;
  if (!aspect) {
    const config = await track.getDecoderConfig().catch(() => null);
    const description = config?.description ?? undefined;
    const first =
      description === undefined
        ? await new EncodedPacketSink(track).getFirstPacket().catch(() => null)
        : null;
    aspect = sampleAspectForStream(
      config?.codec ?? track.codec,
      description,
      first?.data,
    );
  }
  let width = track.codedWidth;
  let height = track.codedHeight;
  if (aspect && aspect.num > aspect.den)
    width = Math.floor(
      (width * aspect.num * 2 + aspect.den) / (2 * aspect.den),
    );
  else if (aspect && aspect.num < aspect.den)
    height = Math.floor(
      (height * aspect.den * 2 + aspect.num) / (2 * aspect.num),
    );
  return track.rotation % 180 === 0
    ? { width, height }
    : { width: height, height: width };
}

/** How many frames the rate is measured over, at least: about four seconds at 30 fps. */
const RATE_FRAMES = 120;

/**
 * The rate `track` presents its first frames at, from packet times alone (durations differ between
 * demuxers); past {@link RATE_FRAMES}, packets run up to the first presented after every one
 * taken, so reordered frames stay in the span. `null` with fewer than two frames or no span. Twin
 * of the engine's `alpha2_assets::FrameRateEvidence` (held equal by
 * `tests/cases/media-evidence-corpus.json`); a declared rate is no common ground across hosts.
 * @param {import("mediabunny").InputVideoTrack} track
 * @returns {Promise<number | null>}
 */
async function presentedRate(track) {
  let count = 0;
  let first = Infinity;
  let last = -Infinity;
  for await (const packet of new EncodedPacketSink(track).packets(
    undefined,
    undefined,
    { metadataOnly: true },
  )) {
    const time = packet.timestamp;
    if (!Number.isFinite(time)) continue;
    if (count >= RATE_FRAMES && time > last) break;
    first = Math.min(first, time);
    last = Math.max(last, time);
    count += 1;
  }
  return count >= 2 && last > first ? (count - 1) / (last - first) : null;
}

/**
 * A measured frame rate as the rational a stream declares: the NTSC rates and whole rates exactly,
 * others to a thousandth (the engine's `alpha2_assets::rational_rate` is its twin). `null` when the
 * measure is no rate a project holds (so slow it rounds to no frames, or a frame shorter than a
 * tick, which only broken timestamps give): the fingerprint then records no rate rather than one
 * the engine refuses.
 */
export function rationalRate(rate) {
  if (!Number.isFinite(rate) || rate <= 0) return null;
  for (const base of [24, 30, 48, 60, 120]) {
    if (Math.abs(rate - (base * 1000) / 1001) < 0.005)
      return { num: base * 1000, den: 1001 };
  }
  const rational =
    Math.abs(rate - Math.round(rate)) < 0.005
      ? { num: Math.round(rate), den: 1 }
      : { num: Math.round(rate * 1000), den: 1000 };
  return validFrameRate(rational) ? rational : null;
}
