// A request up to 2 s past the decode cursor decodes forward instead of seeking.
const MAX_SEQUENTIAL_DECODE_GAP_TICKS = 240_000n;

/** Presentation slack for an unknown time resolution (0.5 ms plus one tick); also the cap. */
export const PRESENTATION_SLACK_TICKS = 61n;

/** Most leading packets read for {@link roundingQuantum}. Matches the engine. */
export const ROUNDING_SCAN_FRAMES = 64;

/** Packet bytes (16 MiB) after which no more leading packets are read. */
export const ROUNDING_SCAN_BYTES = 16 * 1024 * 1024;

/**
 * The unit a stream's timestamps are rounded to, in units of its time resolution, or 0 when none
 * shows. A frame interval that is not a whole number of units rounds to two lengths a unit apart
 * (29.97 fps in ms gives 33 and 34), so the unit is the GCD of the interval differences. Returns 0
 * for uniform or variable frame rates and for fewer than three distinct timestamps.
 * Throws `RangeError` for a timestamp that is not a safe integer.
 */
export function roundingQuantum(timestamps: readonly number[]): number {
  for (const timestamp of timestamps)
    if (!Number.isSafeInteger(timestamp))
      throw new RangeError("a timestamp must be a safe integer");
  const sorted = [...new Set(timestamps)].sort((a, b) => a - b);
  const intervals = sorted
    .slice(1)
    .map((timestamp, index) => BigInt(timestamp) - BigInt(sorted[index]));
  const first = intervals[0];
  if (first === undefined) return 0;
  let quantum = 0n;
  let shortest = first;
  for (const interval of intervals) {
    if (interval < shortest) shortest = interval;
    let a = quantum;
    let b = interval > first ? interval - first : first - interval;
    while (b !== 0n) [a, b] = [b, a % b];
    quantum = a;
  }
  if (quantum === 0n || quantum * 2n >= shortest) return 0;
  return Number(quantum);
}

/**
 * The samples an audio stream's frames start at: every `period` samples from `anchor`. Lets frames
 * with millisecond-rounded timestamps (Matroska, WebM) land on the same samples after a seek as in
 * a straight decode.
 */
export interface AudioFrameGrid {
  readonly anchor: number;
  readonly period: number;
  readonly startTolerance: number;
  /** Wider than `startTolerance` because dropping codec delay (Opus pre-skip) rounds again. */
  readonly trimmedTolerance: number;
}

/**
 * The frame grid an audio stream's first packet timestamps show at `sampleRate`, for timestamps in
 * whole units of `1 / timeResolution` seconds. Matches the engine's `audio_frame_grid`. Returns
 * `undefined` when the timestamps do not fix a grid (too few, mixed frame lengths, a missing
 * packet, sub-sample rounding) or the resolution or rate is not a positive integer.
 * Throws `RangeError` for a timestamp that is not a safe integer.
 */
export function audioFrameGrid(
  timestamps: readonly number[],
  timeResolution: number,
  sampleRate: number,
): AudioFrameGrid | undefined {
  if (
    !Number.isSafeInteger(timeResolution) ||
    timeResolution <= 0 ||
    !Number.isSafeInteger(sampleRate) ||
    sampleRate <= 0
  )
    return undefined;
  const quantum = roundingQuantum(timestamps);
  const sorted = [...new Set(timestamps)]
    .sort((a, b) => a - b)
    .map((timestamp) => BigInt(timestamp));
  const count = BigInt(sorted.length);
  if (count < 3n) return undefined;
  const den = BigInt(timeResolution);
  // Samples of a unit, times `den`; the rounding unit in samples, times `den` (two half units).
  const unit = BigInt(sampleRate);
  const rounding = unit * BigInt(Math.max(quantum, 1));
  if (rounding < 2n * den) return undefined;
  const floorDiv = (a: bigint, b: bigint) => {
    const quotient = a / b;
    return a % b !== 0n && a < 0n !== b < 0n ? quotient - 1n : quotient;
  };
  const trimmed = floorDiv(rounding + den, den);
  const span = (sorted[sorted.length - 1] - sorted[0]) * unit;
  const frames = count - 1n;
  let shortest = floorDiv(span - rounding, den * frames);
  if (shortest < 2n * trimmed + 1n) shortest = 2n * trimmed + 1n;
  const longest = floorDiv(span + rounding, den * frames) + 1n;
  let best: { period: bigint; low: bigint; high: bigint } | undefined;
  for (let period = shortest; period <= longest; period += 1n) {
    let low: bigint | undefined;
    let high: bigint | undefined;
    sorted.forEach((timestamp, index) => {
      const center = 2n * timestamp * unit - 2n * BigInt(index) * period * den;
      if (low === undefined || center - rounding > low) low = center - rounding;
      if (high === undefined || center + rounding < high)
        high = center + rounding;
    });
    if (low === undefined || high === undefined || low > high) continue;
    if (best === undefined || high - low > best.high - best.low)
      best = { period, low, high };
  }
  if (best === undefined) return undefined;
  return {
    anchor: Number(floorDiv(best.low + best.high + 2n * den, 4n * den)),
    period: Number(best.period),
    startTolerance: Number(floorDiv(rounding + 2n * den, 2n * den)),
    trimmedTolerance: Number(trimmed),
  };
}

/**
 * Where a frame of `frames` samples with timestamp sample `at` starts: on `grid` when within its
 * tolerance, otherwise at `at`. A short frame is taken as start-trimmed (Opus pre-skip), so its end
 * goes on the grid.
 */
export function placeOnAudioFrameGrid(
  grid: AudioFrameGrid,
  at: number,
  frames: number,
): number {
  const shiftOnto = (position: number) => {
    const offset =
      (((position - grid.anchor) % grid.period) + grid.period) % grid.period;
    return offset * 2 <= grid.period ? -offset : grid.period - offset;
  };
  if (frames >= 1 && frames < grid.period) {
    const shift = shiftOnto(at + frames);
    if (Math.abs(shift) <= grid.trimmedTolerance) return at + shift;
  }
  const shift = shiftOnto(at);
  return Math.abs(shift) <= grid.startTolerance ? at + shift : at;
}

/**
 * How much later than a requested time a decoded frame's timestamp may be and still be the frame
 * shown at it, in ticks: half the timestamp rounding unit, rounded up, plus one tick.
 * `timeResolution` is mediabunny's `getTimeResolution()`, `quantum` from {@link roundingQuantum}
 * (0 for none). Unknown resolutions get {@link PRESENTATION_SLACK_TICKS}.
 */
export function presentationSlackTicks(
  timeResolution?: number,
  quantum = 0,
): bigint {
  if (
    timeResolution === undefined ||
    !Number.isFinite(timeResolution) ||
    timeResolution <= 0
  )
    return PRESENTATION_SLACK_TICKS;
  const shown = Number.isSafeInteger(quantum) && quantum > 0;
  const units = shown ? BigInt(quantum) : 1n;
  // Half the rounding in ticks, rounded up: 120 000 * units / (2 * resolution).
  const half = Number.isInteger(timeResolution)
    ? (60_000n * units + BigInt(timeResolution) - 1n) / BigInt(timeResolution)
    : BigInt(Math.ceil((60_000 * Number(units)) / timeResolution));
  const slack = half + 1n;
  return shown || slack < PRESENTATION_SLACK_TICKS
    ? slack
    : PRESENTATION_SLACK_TICKS;
}
const DEFAULT_MAX_DECODED_FRAME_BYTES = 64 * 1024 * 1024;
// The presented frame and the one after. Each holds a hardware decoder surface from a small fixed
// pool, and holding more stalls some Windows decoders.
const DEFAULT_MAX_CACHED_FRAMES = 2;
const MIN_CACHED_FRAMES = 2;
const DEFAULT_STALL_TIMEOUT_MS = 5_000;
/** About 24.8 days; timers misfire past a signed 32-bit delay. */
export const MAX_STALL_TIMEOUT_MS = 2_147_483_647;

/**
 * Throws `RangeError` unless `value` is whole milliseconds in 1..{@link MAX_STALL_TIMEOUT_MS}.
 *
 * @internal
 */
export function validateStallTimeout(value: number): void {
  if (
    !Number.isSafeInteger(value) ||
    value <= 0 ||
    value > MAX_STALL_TIMEOUT_MS
  )
    throw new RangeError(
      `stallTimeoutMs must be a whole number of milliseconds from 1 to ${MAX_STALL_TIMEOUT_MS}`,
    );
}

export type DecoderRefillReason =
  "prefetch" | "seek" | "evicted" | "device-recovery";

/** A request for the frame of one source presented at a time, shared by every video decoder. */
export interface DecoderRefillRequest {
  readonly sourceId: string;
  /** Source time, in ticks (120 000 per second). */
  readonly mediaTimeTicks: bigint;
  /** `seek` and `device-recovery` always restart decoding from a keyframe. */
  readonly reason: DecoderRefillReason;
  /** A request from a newer generation drops the cursor and cache; older frames are stale. */
  readonly generation: number;
  /** Each consumer ({@link decoderConsumerId}) keeps its own decode cursor. */
  readonly consumerId?: string;
  /** `"software"` decodes on the CPU, for a host that cannot take the browser decoder's frame. */
  readonly decode?: "default" | "software";
}

/** The owner of a decode cursor: a clip, or a composition step that reads the source itself. */
export function decoderConsumerId(
  clipId: string,
  readerId?: string | null,
): string {
  return readerId ? `${clipId}\u0000${readerId}` : clipId;
}

/** Mirrored left to right when `mirror` is set, then rotated clockwise by `rotationDegrees`. */
export interface VideoOrientation {
  readonly rotationDegrees: 0 | 90 | 180 | 270;
  readonly mirror: boolean;
}

/** Shown as stored. */
export const UPRIGHT: VideoOrientation = Object.freeze({
  rotationDegrees: 0,
  mirror: false,
});

export type ContainerBytes = Blob | ArrayBuffer | ArrayBufferView;

const MAX_HEADER_SCAN_BYTES = 64 * 1024 * 1024;

/**
 * Reads track `trackId`'s orientation, mirror included, from MP4/QuickTime `tkhd`/`mvhd` matrices
 * or the Matroska projection pose, as `FFmpeg` does. `trackId` is mediabunny's `InputTrack.id`.
 * Resolves to `undefined` for other containers or when the header is not found.
 */
export async function readVideoOrientation(
  container: ContainerBytes,
  trackId: number,
): Promise<VideoOrientation | undefined> {
  const reader = new ByteReader(container);
  const head = await reader.read(0, 12);
  if (head.length >= 4 && readU32(head, 0) === 0x1a45dfa3)
    return matroskaOrientation(reader, trackId);
  if (head.length >= 8) {
    const type = ascii(head, 4, 8);
    if (["ftyp", "moov", "mdat", "free", "wide", "skip"].includes(type))
      return isobmffOrientation(reader, trackId);
  }
  return undefined;
}

class ByteReader {
  readonly #container: ContainerBytes;
  readonly size: number;
  constructor(container: ContainerBytes) {
    this.#container = container;
    this.size =
      container instanceof Blob ? container.size : container.byteLength;
  }
  async read(offset: number, length: number): Promise<Uint8Array> {
    const end = Math.min(this.size, offset + length);
    if (offset >= end) return new Uint8Array(0);
    const container = this.#container;
    if (container instanceof Blob)
      return new Uint8Array(await container.slice(offset, end).arrayBuffer());
    if (container instanceof ArrayBuffer)
      return new Uint8Array(container, offset, end - offset);
    return new Uint8Array(
      container.buffer,
      container.byteOffset + offset,
      end - offset,
    );
  }
}

function readU32(bytes: Uint8Array, at: number): number {
  return new DataView(bytes.buffer, bytes.byteOffset).getUint32(at);
}

function ascii(bytes: Uint8Array, start: number, end: number): string {
  return String.fromCharCode(...bytes.subarray(start, end));
}

interface Box {
  readonly type: string;
  readonly start: number;
  readonly end: number;
}

async function* isobmffBoxes(
  reader: ByteReader,
  start: number,
  end: number,
): AsyncGenerator<Box> {
  let at = start;
  while (at + 8 <= end && at < MAX_HEADER_SCAN_BYTES + start) {
    const header = await reader.read(at, 16);
    if (header.length < 8) return;
    let size = readU32(header, 0);
    let headerSize = 8;
    if (size === 1) {
      if (header.length < 16) return;
      size = readU32(header, 8) * 2 ** 32 + readU32(header, 12);
      headerSize = 16;
    } else if (size === 0) size = end - at;
    if (size < headerSize || at + size > end) return;
    yield { type: ascii(header, 4, 8), start: at + headerSize, end: at + size };
    at += size;
  }
}

async function child(
  reader: ByteReader,
  parent: Box,
  type: string,
): Promise<Box | undefined> {
  for await (const box of isobmffBoxes(reader, parent.start, parent.end))
    if (box.type === type) return box;
  return undefined;
}

/** Row-major `[a, b, c, d]`: `(x, y)` shows at `(a*x + c*y, b*x + d*y)`. */
type Matrix2 = readonly [number, number, number, number];

async function boxMatrix(
  reader: ByteReader,
  box: Box,
  offset: number,
): Promise<Matrix2 | undefined> {
  const bytes = await reader.read(box.start + offset, 20);
  if (bytes.length < 20 || box.start + offset + 36 > box.end) return undefined;
  const view = new DataView(bytes.buffer, bytes.byteOffset);
  const fixed = (at: number) => view.getInt32(at) / 65536;
  return [fixed(0), fixed(4), fixed(12), fixed(16)];
}

async function isobmffOrientation(
  reader: ByteReader,
  trackId: number,
): Promise<VideoOrientation | undefined> {
  const file: Box = { type: "", start: 0, end: reader.size };
  const moov = await child(reader, file, "moov");
  if (moov === undefined) return undefined;
  let movie: Matrix2 = [1, 0, 0, 1];
  let track: Matrix2 | undefined;
  for await (const box of isobmffBoxes(reader, moov.start, moov.end)) {
    if (box.type === "mvhd") {
      const version = (await reader.read(box.start, 1))[0];
      // Skips creation and modification time, timescale, duration, rate, volume, 10 reserved bytes.
      movie = (await boxMatrix(reader, box, version === 1 ? 48 : 36)) ?? movie;
    } else if (box.type === "trak") {
      const tkhd = await child(reader, box, "tkhd");
      if (tkhd === undefined) continue;
      const header = await reader.read(tkhd.start, 24);
      if (header.length < 24) continue;
      const version = header[0];
      if (readU32(header, version === 1 ? 20 : 12) !== trackId) continue;
      // Skips reserved, duration, 8 reserved, layer, alternate group, volume, 2 reserved bytes.
      track = await boxMatrix(reader, tkhd, version === 1 ? 52 : 40);
    }
  }
  if (track === undefined) return undefined;
  // FFmpeg composes them as rows: the track's matrix, then the movie's.
  const [a, b, c, d] = track;
  const [e, f, g, h] = movie;
  return matrixOrientation([
    a * e + b * g,
    a * f + b * h,
    c * e + d * g,
    c * f + d * h,
  ]);
}

/** Reads a display matrix as `av_display_rotation_get` does, a mirror as a flipped first column. */
function matrixOrientation(matrix: Matrix2): VideoOrientation {
  const [b, d] = [matrix[1], matrix[3]];
  const mirror = matrix[0] * d - b * matrix[2] < 0;
  // FFmpeg removes the flip by negating the first column.
  const [a, c] = mirror ? [-matrix[0], -matrix[2]] : [matrix[0], matrix[2]];
  // `av_display_rotation_get`'s counter-clockwise angle, of each axis scaled to unit length.
  const counterClockwise =
    -(Math.atan2(b / Math.hypot(b, d), a / Math.hypot(a, c)) * 180) / Math.PI;
  if (!Number.isFinite(counterClockwise)) return UPRIGHT;
  return rotationOrientation(
    mirror ? counterClockwise : -counterClockwise,
    mirror,
  );
}

function rotationOrientation(
  clockwise: number,
  mirror: boolean,
): VideoOrientation {
  const normalized = ((clockwise % 360) + 360) % 360;
  const quarter = Math.round(normalized / 90) % 4;
  if (Math.abs(normalized - Math.round(normalized / 90) * 90) >= 0.5)
    return UPRIGHT;
  return {
    rotationDegrees: ([0, 90, 180, 270] as const)[quarter],
    mirror,
  };
}

/** IDs keep their length marker, as the specification writes them. */
interface Element {
  readonly id: number;
  readonly start: number;
  /** The end of its parent when the size is unknown. */
  readonly end: number;
}

async function* ebmlElements(
  reader: ByteReader,
  start: number,
  end: number,
): AsyncGenerator<Element> {
  let at = start;
  while (at < end && at < MAX_HEADER_SCAN_BYTES) {
    const header = await reader.read(at, 12);
    const id = vint(header, 0, true);
    if (id === undefined) return;
    const size = vint(header, id.length, false);
    if (size === undefined) return;
    const payload = at + id.length + size.length;
    const elementEnd = size.value === undefined ? end : payload + size.value;
    if (elementEnd > end) return;
    yield { id: id.value ?? 0, start: payload, end: elementEnd };
    at = elementEnd;
  }
}

/** IDs keep their length marker; an all-ones size is unknown (`value` undefined). */
function vint(
  bytes: Uint8Array,
  at: number,
  keepMarker: boolean,
): { value: number | undefined; length: number } | undefined {
  const first = bytes[at];
  if (first === undefined || first === 0) return undefined;
  const length = Math.clz32(first) - 23;
  if (at + length > bytes.length) return undefined;
  let value = keepMarker ? first : first & (0xff >> length);
  let allOnes = value === 0xff >> length;
  for (let index = 1; index < length; index += 1) {
    const byte = bytes[at + index] ?? 0;
    value = value * 256 + byte;
    allOnes &&= byte === 0xff;
  }
  return {
    value: !keepMarker && allOnes ? undefined : value,
    length,
  };
}

async function ebmlUnsigned(reader: ByteReader, element: Element) {
  const bytes = await reader.read(element.start, element.end - element.start);
  return bytes.reduce((value, byte) => value * 256 + byte, 0);
}

async function ebmlFloat(reader: ByteReader, element: Element) {
  const length = element.end - element.start;
  if (length === 0) return 0;
  const bytes = await reader.read(element.start, length);
  const view = new DataView(bytes.buffer, bytes.byteOffset);
  if (length === 4) return view.getFloat32(0);
  if (length === 8) return view.getFloat64(0);
  return Number.NaN;
}

const EBML = {
  segment: 0x18538067,
  tracks: 0x1654ae6b,
  cluster: 0x1f43b675,
  trackEntry: 0xae,
  trackNumber: 0xd7,
  codecDelay: 0x56aa,
  video: 0xe0,
  projection: 0x7670,
  projectionType: 0x7671,
  yaw: 0x7673,
  pitch: 0x7674,
  roll: 0x7675,
};

/** Track entries of the first `Tracks` element before the first cluster. */
async function* matroskaTrackEntries(reader: ByteReader): AsyncGenerator<{
  readonly number: number | undefined;
  readonly entry: Element;
}> {
  for await (const top of ebmlElements(reader, 0, reader.size)) {
    if (top.id !== EBML.segment) continue;
    for await (const level1 of ebmlElements(reader, top.start, top.end)) {
      if (level1.id === EBML.cluster) return;
      if (level1.id !== EBML.tracks) continue;
      for await (const entry of ebmlElements(
        reader,
        level1.start,
        level1.end,
      )) {
        if (entry.id !== EBML.trackEntry) continue;
        let number: number | undefined;
        for await (const field of ebmlElements(reader, entry.start, entry.end))
          if (field.id === EBML.trackNumber)
            number = await ebmlUnsigned(reader, field);
        yield { number, entry };
      }
      return;
    }
  }
}

/** FFmpeg reads a roll as a counter-clockwise turn and a 180 degree yaw as a mirror. */
async function matroskaOrientation(
  reader: ByteReader,
  trackId: number,
): Promise<VideoOrientation | undefined> {
  for await (const { number, entry } of matroskaTrackEntries(reader)) {
    if (number !== trackId) continue;
    let pose:
      { type: number; yaw: number; pitch: number; roll: number } | undefined;
    for await (const field of ebmlElements(reader, entry.start, entry.end)) {
      if (field.id !== EBML.video) continue;
      for await (const video of ebmlElements(reader, field.start, field.end)) {
        if (video.id !== EBML.projection) continue;
        pose = { type: 0, yaw: 0, pitch: 0, roll: 0 };
        for await (const value of ebmlElements(
          reader,
          video.start,
          video.end,
        )) {
          if (value.id === EBML.projectionType)
            pose.type = await ebmlUnsigned(reader, value);
          else if (value.id === EBML.yaw)
            pose.yaw = await ebmlFloat(reader, value);
          else if (value.id === EBML.pitch)
            pose.pitch = await ebmlFloat(reader, value);
          else if (value.id === EBML.roll)
            pose.roll = await ebmlFloat(reader, value);
        }
      }
    }
    if (
      pose === undefined ||
      pose.type !== 0 ||
      pose.pitch !== 0 ||
      (pose.yaw !== 0 && Math.abs(pose.yaw) !== 180) ||
      !Number.isFinite(pose.roll)
    )
      return UPRIGHT;
    return rotationOrientation(-pose.roll, pose.yaw !== 0);
  }
  return undefined;
}

/**
 * The `CodecDelay` a Matroska or WebM file declares for track `trackId` (mediabunny's
 * `InputTrack.id`), in nanoseconds; 0 when none. Readers should subtract it; mediabunny does not.
 */
export async function readMatroskaCodecDelay(
  container: ContainerBytes,
  trackId: number,
): Promise<number> {
  const reader = new ByteReader(container);
  const head = await reader.read(0, 4);
  if (head.length < 4 || readU32(head, 0) !== 0x1a45dfa3) return 0;
  for await (const { number, entry } of matroskaTrackEntries(reader)) {
    if (number !== trackId) continue;
    for await (const field of ebmlElements(reader, entry.start, entry.end))
      if (field.id === EBML.codecDelay) return ebmlUnsigned(reader, field);
    return 0;
  }
  return 0;
}

/** A demuxer-independent encoded video source consumed by WebCodecs. */
export interface EncodedVideoSource {
  readonly config: VideoDecoderConfig;
  /** Clockwise turn after {@link mirror}; {@link readVideoOrientation} reads both. */
  readonly rotationDegrees?: 0 | 90 | 180 | 270;
  readonly mirror?: boolean;
  /** Units per second of the chunk timestamps (mediabunny's `getTimeResolution()`), if known. */
  readonly timeResolution?: number;
  /** The timestamps' rounding unit ({@link roundingQuantum}); widens the presentation slack. */
  readonly roundingQuantum?: number;
  /** Chunks in decode order from the keyframe at or before `mediaTimeTicks` (else the first). */
  chunksFrom(
    mediaTimeTicks: bigint,
    signal: AbortSignal,
  ): AsyncIterable<EncodedVideoChunk>;
}

/** Decoded output with explicit ownership: the receiver must close `frame`. */
export interface BrowserDecodedFrame {
  readonly sourceId: string;
  readonly requestedTimeTicks: bigint;
  readonly decodedTimeTicks: bigint;
  readonly generation: number;
  readonly frame: VideoFrame;
  /** Clockwise turn applied after {@link mirror}. */
  readonly rotationDegrees?: 0 | 90 | 180 | 270;
  readonly mirror?: boolean;
  /** WebCodecs does not report it; see {@link chromaSitingForStream}. */
  readonly chromaSiting?: ChromaSiting;
  /** Set on a frame a `decode: "software"` request was answered with. */
  readonly decode?: "software";
}

/** Public browser decoder contract; no optional demuxer types leak through it. */
export interface BrowserVideoDecoder {
  /**
   * Resolves to the last frame at or before `request.mediaTimeTicks` plus the presentation slack;
   * the first frame before the stream starts and the last past its end.
   */
  refill(
    request: DecoderRefillRequest,
    signal?: AbortSignal,
  ): Promise<BrowserDecodedFrame>;
  /** Drops the decode cursor and every cached frame; the next request seeks. */
  flush(): Promise<void>;
  /** Releases the decoder and its frames. Frames already returned stay the caller's. */
  close(): void;
}

/** Each must be a positive safe integer; `stallTimeoutMs` at most {@link MAX_STALL_TIMEOUT_MS}. */
export interface BrowserVideoDecoderOptions {
  /** Bytes of decoded frames cached (default 64 MiB). Never evicts below two frames. */
  readonly maxCachedFrameBytes?: number;
  /** Bytes of decoded frames queued ahead of the cursor (default 64 MiB). */
  readonly maxReadyFrameBytes?: number;
  /** Frames cached at most (default 2, at least 2). Each holds a decoder surface. */
  readonly maxCachedFrames?: number;
  /** Milliseconds without decoder output before the refill fails (default 5000). */
  readonly stallTimeoutMs?: number;
}

/** Loudspeaker by `FFmpeg` channel name. `UNK` planes (unknown, ambisonic) are not mixed. */
export type AudioSpeaker =
  | "FL"
  | "FR"
  | "FC"
  | "LFE"
  | "BL"
  | "BR"
  | "FLC"
  | "FRC"
  | "BC"
  | "SL"
  | "SR"
  | "TC"
  | "TFL"
  | "TFC"
  | "TFR"
  | "TBL"
  | "TBC"
  | "TBR"
  | "DL"
  | "DR"
  | "WL"
  | "WR"
  | "SDL"
  | "SDR"
  | "LFE2"
  | "TSL"
  | "TSR"
  | "BFC"
  | "BFL"
  | "BFR"
  | "SSL"
  | "SSR"
  | "TTL"
  | "TTR"
  | "BIL"
  | "BIR"
  | "UNK";

/** A 22.2 layout's 24; sources with more are refused. */
export const MAX_AUDIO_CHANNELS = 24;

/** Owned project-rate planar audio returned by browser adapters. */
export interface BrowserAudioBlock {
  readonly sourceId: string;
  /**
   * First sample at `sampleRate`: in the source for a decoded block, on the project timeline for a
   * mixed one, from the output's start for an encode sink. Samples because ticks are not whole
   * samples at 44.1 kHz.
   */
  readonly startSample: bigint;
  readonly generation: number;
  readonly sampleRate: number;
  /** Speaker per plane; absent means `FFmpeg`'s default layout for the channel count. */
  readonly speakers?: readonly AudioSpeaker[];
  readonly channels: readonly Float32Array[];
}

/** Source-time interval decoded into one project-rate audio block. */
export interface AudioDecoderRefillRequest {
  readonly sourceId: string;
  readonly startMediaTimeTicks: bigint;
  readonly endMediaTimeTicks: bigint;
  readonly generation: number;
}

/** Demuxer-independent browser audio decoder. */
export interface BrowserAudioDecoder {
  /** Rejects when closed, for another source, or when `signal` aborts. */
  refill(
    request: AudioDecoderRefillRequest,
    projectSampleRate: number,
    signal?: AbortSignal,
  ): Promise<BrowserAudioBlock>;
  close(): void;
}

/** WebCodecs adapter for elementary chunks supplied by any demuxer. */
export class WebCodecsVideoDecoder implements BrowserVideoDecoder {
  readonly #sourceId: string;
  readonly #source: EncodedVideoSource;
  #decoder?: VideoDecoder;
  #iterator?: AsyncIterator<EncodedVideoChunk>;
  #ready: VideoFrame[] = [];
  #arriving: VideoFrame[] = [];
  readonly #reorders: boolean;
  #chromaSiting: ChromaSiting;
  #sitingFromKeyChunk: boolean;
  #outputWaiters: Array<() => void> = [];
  #generation?: number;
  #cursorTicks?: bigint;
  #leadTicks?: bigint;
  #firstSinceSeek = false;
  #decoderError?: Error;
  readonly #cache: VideoFrameWindow;
  readonly #maxReadyFrameBytes: number;
  readonly #stallTimeoutMs: number;
  readonly #slack: bigint;
  #readyBytes = 0;
  #activeTargetTicks?: bigint;
  #reverseCacheHits = 0;
  #software: boolean;
  #softwareFallback?: string;
  #decodedAny = false;

  /** Throws `RangeError` for invalid options (see {@link BrowserVideoDecoderOptions}). */
  constructor(
    sourceId: string,
    source: EncodedVideoSource,
    options: BrowserVideoDecoderOptions = {},
  ) {
    this.#sourceId = sourceId;
    this.#source = source;
    const maxCachedFrameBytes =
      options.maxCachedFrameBytes ?? DEFAULT_MAX_DECODED_FRAME_BYTES;
    this.#maxReadyFrameBytes =
      options.maxReadyFrameBytes ?? DEFAULT_MAX_DECODED_FRAME_BYTES;
    const maxCachedFrames =
      options.maxCachedFrames ?? DEFAULT_MAX_CACHED_FRAMES;
    this.#stallTimeoutMs = options.stallTimeoutMs ?? DEFAULT_STALL_TIMEOUT_MS;
    validatePositiveByteLimit(maxCachedFrameBytes, "maxCachedFrameBytes");
    validatePositiveByteLimit(this.#maxReadyFrameBytes, "maxReadyFrameBytes");
    validatePositiveByteLimit(maxCachedFrames, "maxCachedFrames");
    if (maxCachedFrames < MIN_CACHED_FRAMES)
      throw new RangeError(
        `maxCachedFrames must be at least ${MIN_CACHED_FRAMES}`,
      );
    validateStallTimeout(this.#stallTimeoutMs);
    this.#slack = presentationSlackTicks(
      source.timeResolution,
      source.roundingQuantum,
    );
    this.#cache = new VideoFrameWindow(
      maxCachedFrameBytes,
      maxCachedFrames,
      this.#slack,
    );
    this.#reorders = reordersOutput(source.config.codec);
    this.#chromaSiting = chromaSitingForStream(
      source.config.codec,
      source.config.description,
    );
    this.#sitingFromKeyChunk = source.config.description === undefined;
    this.#software = source.config.hardwareAcceleration === "prefer-software";
  }

  /** Why decoding moved to software after the browser's first pick failed, if it did. */
  get softwareFallback(): string | undefined {
    return this.#softwareFallback;
  }

  get cachedFrameBytes(): number {
    return this.#cache.bytes;
  }

  /** Annex B and AV1-in-MP4 streams carry their parameter sets in the first key chunk. */
  #readSitingFrom(chunk: EncodedVideoChunk): void {
    if (!this.#sitingFromKeyChunk || chunk.type !== "key") return;
    this.#sitingFromKeyChunk = false;
    if (typeof chunk.copyTo !== "function") return;
    const bytes = new Uint8Array(chunk.byteLength);
    chunk.copyTo(bytes);
    this.#chromaSiting = chromaSitingForStream(
      this.#source.config.codec,
      undefined,
      bytes,
    );
  }

  get reverseCacheHits(): number {
    return this.#reverseCacheHits;
  }

  /**
   * A decoder failure retries once from the keyframe with `"prefer-software"`. If that fails too,
   * rejects with `DecodeUnavailableError` when no frame was ever decoded, else with the failure.
   */
  async refill(
    request: DecoderRefillRequest,
    signal: AbortSignal = new AbortController().signal,
  ): Promise<BrowserDecodedFrame> {
    if (request.sourceId !== this.#sourceId) {
      throw new Error(`decoder source mismatch: ${request.sourceId}`);
    }
    try {
      const frame = await withSoftwareFallback(
        () => this.#refillOnce(request, signal),
        this.#software || signal.aborted
          ? undefined
          : (cause) => {
              this.#software = true;
              this.#softwareFallback = `${cause.name}: ${cause.message}`;
              this.#resetCursor();
              this.#cache.clear();
              return Promise.resolve();
            },
      );
      this.#decodedAny = true;
      return frame;
    } catch (error) {
      if (this.#decodedAny || signal.aborted) throw error;
      throw decodeFailureOf(
        error,
        this.#sourceId,
        { kind: "video", codec: this.#source.config.codec },
        false,
      );
    }
  }

  async #refillOnce(
    request: DecoderRefillRequest,
    signal: AbortSignal,
  ): Promise<BrowserDecodedFrame> {
    throwIfAborted(signal);
    if (
      this.#generation !== undefined &&
      this.#generation !== request.generation
    ) {
      this.#resetCursor();
      this.#cache.clear();
    }
    const target =
      this.#leadTicks !== undefined && request.mediaTimeTicks < this.#leadTicks
        ? this.#leadTicks
        : request.mediaTimeTicks;
    const cached = this.#cache.presentedAt(target);
    if (cached !== undefined) {
      this.#reverseCacheHits += 1;
      return {
        sourceId: this.#sourceId,
        requestedTimeTicks: request.mediaTimeTicks,
        decodedTimeTicks: microsecondsToTicks(cached.timestamp),
        generation: request.generation,
        frame: cached,
        rotationDegrees: this.#source.rotationDegrees,
        mirror: this.#source.mirror ?? false,
        chromaSiting: this.#chromaSiting,
      };
    }
    const mustSeek =
      request.reason === "seek" ||
      request.reason === "device-recovery" ||
      this.#iterator === undefined ||
      this.#generation !== request.generation ||
      this.#cursorTicks === undefined ||
      target <= this.#cursorTicks ||
      target - this.#cursorTicks > MAX_SEQUENTIAL_DECODE_GAP_TICKS;
    if (mustSeek) {
      this.#resetCursor();
      this.#cache.clear();
      // The shown frame may be up to the slack late.
      const chunks = this.#source.chunksFrom(target + this.#slack, signal);
      this.#iterator = chunks[Symbol.asyncIterator]();
      this.#generation = request.generation;
      this.#firstSinceSeek = true;
    }
    this.#cursorTicks = target;
    this.#activeTargetTicks = target;
    try {
      return await this.#decodeTo(request, target, signal);
    } catch (error) {
      // A failed read may have consumed a chunk, so the next request seeks.
      this.#resetCursor();
      this.#cache.clear();
      throw error;
    }
  }

  async #decodeTo(
    request: DecoderRefillRequest,
    target: bigint,
    signal: AbortSignal,
  ): Promise<BrowserDecodedFrame> {
    const decoder = this.#ensureDecoder();
    for (;;) {
      throwIfAborted(signal);
      if (this.#decoderError !== undefined) throw this.#decoderError;
      const ready = this.#takeReady(target);
      if (ready !== undefined) {
        return {
          sourceId: this.#sourceId,
          requestedTimeTicks: request.mediaTimeTicks,
          decodedTimeTicks: microsecondsToTicks(ready.timestamp),
          generation: request.generation,
          frame: ready,
          rotationDegrees: this.#source.rotationDegrees,
          mirror: this.#source.mirror ?? false,
          chromaSiting: this.#chromaSiting,
        };
      }
      const iterator = this.#iterator;
      if (iterator === undefined) {
        throw new RangeError("encoded source ended before the requested frame");
      }
      const next = await nextOrAbort(iterator, signal);
      if (next.done) {
        this.#iterator = undefined;
        await this.#flushAtEnd(decoder, signal);
        const finalFrame = this.#takeReady(target, true);
        if (this.#ready.length === 0) this.#cache.markEnd();
        if (finalFrame !== undefined) {
          return {
            sourceId: this.#sourceId,
            requestedTimeTicks: request.mediaTimeTicks,
            decodedTimeTicks: microsecondsToTicks(finalFrame.timestamp),
            generation: request.generation,
            frame: finalFrame,
            rotationDegrees: this.#source.rotationDegrees,
            mirror: this.#source.mirror ?? false,
            chromaSiting: this.#chromaSiting,
          };
        }
        throw new RangeError("encoded source ended before the requested frame");
      }
      this.#readSitingFrom(next.value);
      decoder.decode(next.value);
      if (decoder.decodeQueueSize > 8) {
        if (!(await this.#waitForOutput(signal))) {
          // Every surface is held. Closing queued frames would answer with a later frame.
          if (this.#cache.releaseAllButPresentedAt(target) === 0)
            throw new Error(
              `video decoder ${this.#sourceId} stalled: no output for ` +
                `${this.#stallTimeoutMs} ms with ${decoder.decodeQueueSize} chunks queued; ` +
                "its surfaces are held by frames that were never closed",
            );
        }
      } else {
        await Promise.resolve();
      }
    }
  }

  flush(): Promise<void> {
    this.#resetCursor();
    this.#cache.clear();
    return Promise.resolve();
  }

  close(): void {
    const closing = this.#iterator?.return?.();
    void closing?.catch(() => {});
    this.#iterator = undefined;
    const decoder = this.#decoder;
    this.#decoder = undefined;
    if (decoder !== undefined && decoder.state !== "closed") decoder.close();
    this.#closeReady();
    this.#cache.clear();
    this.#notifyOutput();
  }

  #ensureDecoder(): VideoDecoder {
    if (this.#decoder !== undefined) return this.#decoder;
    const decoder = new VideoDecoder({
      output: (frame) => {
        if (this.#decoder !== decoder) {
          frame.close();
          return;
        }
        this.#arrive(frame);
        this.#notifyOutput();
      },
      error: (error) => {
        if (this.#decoder !== decoder) return;
        this.#decoderError = error;
        this.#notifyOutput();
      },
    });
    decoder.addEventListener("dequeue", () => this.#notifyOutput());
    decoder.configure(
      this.#software
        ? { ...this.#source.config, hardwareAcceleration: "prefer-software" }
        : this.#source.config,
    );
    this.#decoder = decoder;
    return decoder;
  }

  /**
   * Safari 16.4 to 26.3 outputs H.264 B-frames in decode order, so reordering codecs hold frames
   * back, sorted, until one arrives that is no earlier than all of them.
   */
  #arrive(frame: VideoFrame): void {
    if (!this.#reorders) {
      this.#admit(frame);
      return;
    }
    const latest = this.#arriving.at(-1);
    if (latest !== undefined && frame.timestamp >= latest.timestamp)
      this.#releaseArriving();
    const at = this.#arriving.findIndex(
      (held) => held.timestamp > frame.timestamp,
    );
    this.#arriving.splice(at < 0 ? this.#arriving.length : at, 0, frame);
  }

  #releaseArriving(): void {
    for (const frame of this.#arriving.splice(0)) this.#admit(frame);
  }

  #admit(frame: VideoFrame): void {
    this.#ready.push(frame);
    this.#readyBytes += videoFrameBytes(frame);
    this.#ready.sort((left, right) => left.timestamp - right.timestamp);
    while (
      this.#readyBytes > this.#maxReadyFrameBytes &&
      this.#ready.length > 1 &&
      this.#activeTargetTicks !== undefined &&
      microsecondsToTicks(this.#ready[0]?.timestamp ?? 0) <
        this.#activeTargetTicks
    ) {
      const discarded = this.#ready.shift();
      if (discarded === undefined) break;
      this.#readyBytes -= videoFrameBytes(discarded);
      this.#cache.add(discarded);
      this.#firstSinceSeek = false;
      discarded.close();
    }
  }

  #resetCursor(): void {
    // A return can queue behind a next that never settles; detach so cancellation never blocks.
    const closing = this.#iterator?.return?.();
    void closing?.catch(() => {});
    this.#iterator = undefined;
    const decoder = this.#decoder;
    this.#decoder = undefined;
    if (decoder !== undefined && decoder.state !== "closed") decoder.close();
    this.#closeReady();
    this.#generation = undefined;
    this.#cursorTicks = undefined;
    this.#firstSinceSeek = false;
    this.#decoderError = undefined;
    this.#activeTargetTicks = undefined;
  }

  async #flushAtEnd(decoder: VideoDecoder, signal: AbortSignal): Promise<void> {
    const settled = new AbortController();
    let abort: (() => void) | undefined;
    try {
      throwIfAborted(signal);
      const interrupted = new Promise<never>((_, reject) => {
        abort = () => reject(abortError(signal));
        signal.addEventListener("abort", abort, { once: true });
        void activeTimeout(this.#stallTimeoutMs, settled.signal).then(() =>
          reject(
            new Error(
              `video decoder ${this.#sourceId} stalled during EOF flush after ${this.#stallTimeoutMs} ms`,
            ),
          ),
        );
      });
      await Promise.race([decoder.flush(), interrupted]);
      this.#releaseArriving();
      throwIfAborted(signal);
      if (this.#decoderError !== undefined) throw this.#decoderError;
    } catch (error) {
      // Closing rejects the native flush, which Promise.race already observes.
      this.close();
      throw error;
    } finally {
      settled.abort();
      if (abort !== undefined) signal.removeEventListener("abort", abort);
    }
  }

  /**
   * The last frame at or before the target plus the slack; deciding it needs the next frame, which
   * stays queued. A first frame after a seek that is later than the target is the stream's start.
   */
  #takeReady(targetTicks: bigint, ended = false): VideoFrame | undefined {
    const deadline = targetTicks + this.#slack;
    while (this.#ready.length > 0) {
      const frame = this.#ready[0];
      if (frame === undefined) return undefined;
      const ticks = microsecondsToTicks(frame.timestamp);
      if (ticks > deadline) {
        const presented = this.#cache.presentedAt(targetTicks, true);
        if (presented !== undefined) return presented;
        if (this.#firstSinceSeek) this.#leadTicks = ticks;
        return this.#takeFront();
      }
      const taken = this.#takeFront();
      if (ticks >= targetTicks) return taken;
      taken?.close();
    }
    return ended ? this.#cache.presentedAt(targetTicks, true) : undefined;
  }

  #takeFront(): VideoFrame | undefined {
    const frame = this.#ready.shift();
    if (frame !== undefined) {
      this.#readyBytes -= videoFrameBytes(frame);
      this.#cache.add(frame);
      this.#firstSinceSeek = false;
    }
    return frame;
  }

  #closeReady(): void {
    for (const frame of this.#ready.splice(0)) frame.close();
    for (const frame of this.#arriving.splice(0)) frame.close();
    this.#readyBytes = 0;
  }

  #notifyOutput(): void {
    for (const wake of this.#outputWaiters.splice(0)) wake();
  }

  #waitForOutput(signal: AbortSignal): Promise<boolean> {
    return new Promise((resolve, reject) => {
      const timer = new AbortController();
      const settle = (): void => {
        timer.abort();
        signal.removeEventListener("abort", abort);
        this.#outputWaiters = this.#outputWaiters.filter(
          (candidate) => candidate !== wake,
        );
      };
      const wake = (): void => {
        settle();
        resolve(true);
      };
      const abort = (): void => {
        settle();
        reject(abortError(signal));
      };
      void activeTimeout(this.#stallTimeoutMs, timer.signal).then(() => {
        settle();
        resolve(false);
      });
      this.#outputWaiters.push(wake);
      signal.addEventListener("abort", abort, { once: true });
    });
  }
}

/** Limits for a {@link DecoderRefillCoordinator}. Each must be a positive safe integer. */
export interface DecoderRefillCoordinatorOptions {
  /** Concurrent refills across sources (default 2). */
  readonly maxConcurrency?: number;
  /** Decoders kept open per source (default 4); the least recently used one closes past it. */
  readonly maxCursorsPerSource?: number;
}

/** What a {@link DecoderRefillCoordinator} asks its opener for. */
export interface DecoderOpenOptions {
  /** An opener without a software decoder rejects with `code: "software_decode_unavailable"`. */
  readonly decode: "default" | "software";
}

/**
 * Runs refills on decoders it opens, one per source and consumer (plus one for software), with
 * one refill per source at a time and at most `maxConcurrency` overall. A newer generation aborts
 * requests in flight and closes their frames instead of delivering them.
 */
export class DecoderRefillCoordinator {
  readonly #open: (
    sourceId: string,
    options: DecoderOpenOptions,
  ) => BrowserVideoDecoder | Promise<BrowserVideoDecoder>;
  readonly #deliver: (frame: BrowserDecodedFrame) => Promise<void>;
  #generation = 0;
  #controller = new AbortController();
  readonly #maxConcurrency: number;
  readonly #maxCursorsPerSource: number;
  #active = 0;
  #capacityWaiters: Array<() => void> = [];
  #sourceTails = new Map<string, Promise<void>>();
  readonly #cursors = new Map<
    string,
    Map<string, Promise<BrowserVideoDecoder>>
  >();
  #closed = false;

  /**
   * A rejected `open` rejects that refill; `software_decode_unavailable` becomes
   * `PlanarDecodeUnavailableError`. Throws `RangeError` for invalid options.
   */
  constructor(
    open: (
      sourceId: string,
      options: DecoderOpenOptions,
    ) => BrowserVideoDecoder | Promise<BrowserVideoDecoder>,
    deliver: (frame: BrowserDecodedFrame) => Promise<void>,
    options: DecoderRefillCoordinatorOptions = {},
  ) {
    const maxConcurrency = options.maxConcurrency ?? 2;
    const maxCursorsPerSource = options.maxCursorsPerSource ?? 4;
    if (!Number.isSafeInteger(maxConcurrency) || maxConcurrency <= 0) {
      throw new RangeError("maxConcurrency must be a positive safe integer");
    }
    if (
      !Number.isSafeInteger(maxCursorsPerSource) ||
      maxCursorsPerSource <= 0
    ) {
      throw new RangeError(
        "maxCursorsPerSource must be a positive safe integer",
      );
    }
    this.#open = open;
    this.#deliver = deliver;
    this.#maxConcurrency = maxConcurrency;
    this.#maxCursorsPerSource = maxCursorsPerSource;
  }

  /** Aborts requests in flight and returns the new generation number. */
  beginGeneration(): number {
    this.#controller.abort(new DOMException("Superseded", "AbortError"));
    this.#controller = new AbortController();
    return ++this.#generation;
  }

  /**
   * Resolves without delivering when the request's generation is superseded; rejects with the
   * decoder's, delivery's or abort's error, and once closed.
   */
  async refill(request: DecoderRefillRequest): Promise<void> {
    if (request.generation < this.#generation) return;
    if (request.generation > this.#generation) {
      this.#controller.abort(new DOMException("Superseded", "AbortError"));
      this.#controller = new AbortController();
      this.#generation = request.generation;
    }
    const previous =
      this.#sourceTails.get(request.sourceId) ?? Promise.resolve();
    const task = previous.catch(() => {}).then(() => this.#refillNow(request));
    this.#sourceTails.set(request.sourceId, task);
    try {
      await task;
    } finally {
      if (this.#sourceTails.get(request.sourceId) === task) {
        this.#sourceTails.delete(request.sourceId);
      }
    }
  }

  async #refillNow(request: DecoderRefillRequest): Promise<void> {
    await this.#acquire();
    try {
      if (this.#closed) throw new Error("decoder refill coordinator is closed");
      if (request.generation !== this.#generation) return;
      throwIfAborted(this.#controller.signal);
      const software = request.decode === "software";
      const decoder = await this.#cursor(
        request.sourceId,
        request.consumerId ?? "",
        software ? "software" : "default",
      );
      const frame = await decoder.refill(request, this.#controller.signal);
      if (frame.generation !== this.#generation) {
        frame.frame.close();
        return;
      }
      await this.#deliver(software ? { ...frame, decode: "software" } : frame);
    } finally {
      this.#release();
    }
  }

  // Runs inside the source's queue, so a decoder closed past the limit is not decoding.
  async #cursor(
    sourceId: string,
    consumer: string,
    decode: "default" | "software",
  ): Promise<BrowserVideoDecoder> {
    let cursors = this.#cursors.get(sourceId);
    if (cursors === undefined) {
      cursors = new Map();
      this.#cursors.set(sourceId, cursors);
    }
    const consumerId =
      decode === "software" ? `${consumer}\u0000software` : consumer;
    let decoder = cursors.get(consumerId);
    if (decoder === undefined) {
      decoder = Promise.resolve()
        .then(() => this.#open(sourceId, { decode }))
        .catch((error: unknown) => {
          throw decode === "software"
            ? softwareRefusal(sourceId, error)
            : error;
        });
      // A source that failed to open is opened again by the next request.
      decoder.catch(() => {
        if (cursors.get(consumerId) === decoder) cursors.delete(consumerId);
      });
    }
    cursors.delete(consumerId);
    cursors.set(consumerId, decoder);
    for (const [consumer, evicted] of cursors) {
      if (cursors.size <= this.#maxCursorsPerSource) break;
      cursors.delete(consumer);
      evicted.then(
        (closed) => closed.close(),
        () => {},
      );
    }
    return decoder;
  }

  async #acquire(): Promise<void> {
    if (this.#active < this.#maxConcurrency) {
      this.#active += 1;
      return;
    }
    await new Promise<void>((resolve) => this.#capacityWaiters.push(resolve));
    this.#active += 1;
  }

  #release(): void {
    this.#active -= 1;
    this.#capacityWaiters.shift()?.();
  }

  close(): void {
    this.#closed = true;
    this.#controller.abort();
    for (const wake of this.#capacityWaiters.splice(0)) wake();
    for (const cursors of this.#cursors.values())
      for (const decoder of cursors.values())
        decoder.then(
          (opened) => opened.close(),
          () => {},
        );
    this.#cursors.clear();
  }
}

function softwareRefusal(sourceId: string, error: unknown): unknown {
  if (
    (error as { code?: unknown } | null)?.code !== "software_decode_unavailable"
  )
    return error;
  const facts = {
    kind: "video" as const,
    codec: null,
    softwareDecoder: "none" as const,
    browserError: error instanceof Error ? error.message : String(error),
  };
  return new PlanarDecodeUnavailableError(
    decodeRefusalMessage(sourceId, facts),
    facts,
    { cause: error },
  );
}

/** Shape used to wrap mediabunny without importing its types into alpha2. */
export interface OptionalMediaAdapterFactory {
  openVideo(sourceId: string, input: unknown): Promise<BrowserVideoDecoder>;
  openAudio?(sourceId: string, input: unknown): Promise<BrowserAudioDecoder>;
}

/**
 * The tick nearest a WebCodecs microsecond timestamp. Rounds, not floors, because tick boundaries
 * arrive just below themselves (30 fps frame one is 3999.96 ticks).
 */
export function microsecondsToTicks(microseconds: number): bigint {
  const scaled = BigInt(Math.round(microseconds)) * 120_000n;
  // Half away from zero, exact in BigInt; negative timestamps are legal.
  const half = scaled < 0n ? -500_000n : 500_000n;
  return (scaled + half) / 1_000_000n;
}

async function nextOrAbort<T>(
  iterator: AsyncIterator<T>,
  signal: AbortSignal,
): Promise<IteratorResult<T>> {
  let abort: (() => void) | undefined;
  const aborted = new Promise<never>((_, reject) => {
    abort = (): void => {
      reject(abortError(signal));
    };
    signal.addEventListener("abort", abort, { once: true });
  });
  try {
    return await Promise.race([iterator.next(), aborted]);
  } finally {
    if (abort !== undefined) signal.removeEventListener("abort", abort);
  }
}

interface CachedVideoFrame {
  readonly ticks: bigint;
  readonly frame: VideoFrame;
  readonly bytes: number;
}

class VideoFrameWindow {
  readonly #limit: number;
  readonly #maxFrames: number;
  readonly #slack: bigint;
  /** The stream's last frame once decoding reached the end; answers every later time. */
  #endTicks?: bigint;
  #entries: CachedVideoFrame[] = [];
  #bytes = 0;

  constructor(limit: number, maxFrames: number, slack: bigint) {
    this.#limit = limit;
    this.#maxFrames = maxFrames;
    this.#slack = slack;
  }

  get bytes(): number {
    return this.#bytes;
  }

  add(frame: VideoFrame): void {
    const ticks = microsecondsToTicks(frame.timestamp);
    const existing = this.#entries.findIndex((entry) => entry.ticks === ticks);
    const clone = frame.clone();
    const bytes = videoFrameBytes(clone);
    if (existing >= 0) {
      const [replaced] = this.#entries.splice(existing, 1);
      if (replaced !== undefined) {
        this.#bytes -= replaced.bytes;
        replaced.frame.close();
      }
    }
    this.#entries.push({ ticks, frame: clone, bytes });
    this.#entries.sort((left, right) =>
      left.ticks < right.ticks ? -1 : left.ticks > right.ticks ? 1 : 0,
    );
    this.#bytes += bytes;
    while (
      (this.#bytes > this.#limit && this.#entries.length > MIN_CACHED_FRAMES) ||
      this.#entries.length > this.#maxFrames
    ) {
      const evicted = this.#entries.shift();
      if (evicted === undefined) break;
      this.#bytes -= evicted.bytes;
      evicted.frame.close();
    }
  }

  markEnd(): void {
    this.#endTicks = this.#entries.at(-1)?.ticks;
  }

  /** A clone of the presented frame, or `undefined` when no later frame proves it yet. */
  presentedAt(target: bigint, final = false): VideoFrame | undefined {
    const deadline = target + this.#slack;
    let index = -1;
    for (const [position, entry] of this.#entries.entries()) {
      if (entry.ticks > deadline) break;
      index = position;
    }
    const entry = this.#entries[index];
    if (entry === undefined) return undefined;
    if (
      entry.ticks < target &&
      index === this.#entries.length - 1 &&
      !final &&
      entry.ticks !== this.#endTicks
    )
      return undefined;
    return entry.frame.clone();
  }

  /** Closes every frame but the one presented at `target`. Returns how many were closed. */
  releaseAllButPresentedAt(target: bigint): number {
    const deadline = target + this.#slack;
    let kept = -1;
    for (const [position, entry] of this.#entries.entries()) {
      if (entry.ticks > deadline) break;
      kept = position;
    }
    const released = this.#entries.filter((_, position) => position !== kept);
    for (const entry of released) {
      this.#bytes -= entry.bytes;
      entry.frame.close();
    }
    this.#entries = this.#entries.filter((_, position) => position === kept);
    return released.length;
  }

  clear(): void {
    for (const entry of this.#entries.splice(0)) entry.frame.close();
    this.#bytes = 0;
  }
}

/** Codecs with B-frames, which Safari outputs out of presentation order. */
function reordersOutput(codec: string): boolean {
  const family = codec.split(".")[0]?.toLowerCase();
  return ["avc1", "avc3", "hvc1", "hev1"].includes(family ?? "");
}

function videoFrameBytes(frame: VideoFrame): number {
  const width = Math.max(1, frame.codedWidth || frame.displayWidth || 1);
  const height = Math.max(1, frame.codedHeight || frame.displayHeight || 1);
  return width * height * 4;
}

function validatePositiveByteLimit(value: number, name: string): void {
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new RangeError(`${name} must be a positive safe integer`);
  }
}
import { abortError, throwIfAborted } from "./abort.js";
import { activeTimeout } from "./active-clock.js";
import { type ChromaSiting, chromaSitingForStream } from "./chroma-siting.js";
import {
  decodeRefusalMessage,
  PlanarDecodeUnavailableError,
} from "./hdr-frames.js";
import { decodeFailureOf, withSoftwareFallback } from "./decoder-fallback.js";
