/**
 * How far past the decode cursor a request may be (2 s) and still be reached by decoding forward;
 * a request further ahead seeks.
 */
const MAX_SEQUENTIAL_DECODE_GAP_TICKS = 240_000n;

/**
 * How much later than a requested time a decoded frame's timestamp may be and still be the frame
 * shown at it, for a stream whose time resolution is not known: 0.5 ms and one tick, a millisecond
 * time base's slack. It is also the most a stream whose timestamps show no rounding of their own is
 * given. The engine's `PRESENTATION_SLACK`.
 */
export const PRESENTATION_SLACK_TICKS = 61n;

/**
 * How many of a stream's first packets {@link roundingQuantum} reads the timestamps of: at most
 * this many, and none once the packets read hold {@link ROUNDING_SCAN_BYTES}. The engine's
 * `ROUNDING_SCAN_FRAMES`.
 */
export const ROUNDING_SCAN_FRAMES = 64;

/**
 * The packet bytes (16 MiB) past which no more of a stream's first packets are read for {@link
 * roundingQuantum}, as a native host stops. The engine's `ROUNDING_SCAN_BYTES`.
 */
export const ROUNDING_SCAN_BYTES = 16 * 1024 * 1024;

/**
 * The unit a stream's timestamps are rounded to, as their spacing shows, in units of its time
 * resolution; 0 when they show none. The engine's `rounding_quantum`, the binding's
 * `roundingQuantum`.
 *
 * `timestamps` are the stream's first packets' presentation times as whole units of its time
 * resolution, in any order: {@link ROUNDING_SCAN_FRAMES} of them, fewer when those before hold
 * {@link ROUNDING_SCAN_BYTES}. A frame interval that is not a whole number of a rounding unit comes
 * out of the rounding as two lengths a unit apart (29.97 fps in milliseconds is 33 and 34 ms), so
 * the unit is the greatest common divisor of the differences between the intervals. Intervals all
 * of one length show none, and so do intervals whose differences are as long as half the shortest
 * (a variable frame rate, such as 60 and 30 fps frames in an exact 1/90000 time base), and fewer
 * than three distinct timestamps. Throws `RangeError` for a timestamp that is not a safe integer.
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
 * The source samples an audio stream's frames start at, as its first packets' rounded timestamps
 * show ({@link audioFrameGrid}): every `period` samples from `anchor`, at the stream's rate. The
 * engine's `AudioFrameGrid`.
 *
 * A container that rounds timestamps (Matroska and WebM to the millisecond) stores a frame up to
 * half its unit from where the frame starts, 24 samples at 48 kHz, so a frame placed by its own
 * timestamp after a seek and the same frame reached by decoding straight through land apart. A
 * codec whose frames are all one length (AAC's 1024 samples, Opus's 20 ms) puts every frame on one
 * grid, which the first timestamps together locate: a first frame placed on it gives the samples
 * a decode straight through gives, however it is reached.
 */
export interface AudioFrameGrid {
  /** A source sample, at the stream's rate, that a frame starts at. */
  readonly anchor: number;
  /** The samples in every frame. */
  readonly period: number;
  /**
   * The farthest a frame's timestamp, as a sample, is from the start it stands for: half the
   * rounding unit and the half sample of rounding it to a sample, rounded down after adding one.
   */
  readonly startTolerance: number;
  /**
   * The farthest the end of a frame shorter than `period` is from the grid: a decoder that drops a
   * codec's delay from its first frame after a start or a seek (Opus's pre-skip) moves its
   * timestamp by the samples dropped, rounded again, so twice the half unit and a sample.
   */
  readonly trimmedTolerance: number;
}

/**
 * The frame grid an audio stream's first packet timestamps show at `sampleRate`, for a stream
 * whose timestamps are whole units of `1 / timeResolution` seconds; `undefined` when they show
 * none to place frames on. The engine's `audio_frame_grid(timestamps, 1, timeResolution,
 * sampleRate)`.
 *
 * `timestamps` are the first packets' presentation times as whole units of the resolution, in any
 * order, read as for {@link roundingQuantum}. Their rounding unit is its quantum, or one unit when
 * they show none. The grid is one every timestamp is within half a unit of: of the whole-sample
 * periods within a unit over their count of their span over it, the one that leaves the most room,
 * the shortest of those that leave as much; and of the anchors it leaves, the middle one, rounded
 * to a sample. There is none when half the unit is under a sample (a time base of the sample rate),
 * when fewer than three distinct timestamps are given or no grid holds them all (frames of several
 * lengths, a missing packet), when no period longer than twice the trimmed tolerance holds them, or
 * when the resolution or the rate is not a positive integer. Throws `RangeError` for a timestamp
 * that is not a safe integer.
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
 * Where a frame of `frames` samples whose timestamp puts it at source sample `at` starts: on
 * `grid` when its timestamp is within the grid's rounding of it, otherwise at `at`. A frame
 * shorter than the period is first taken as the end of one a decoder dropped the start of (Opus's
 * pre-skip): it ends on the grid. The engine's `AudioFrameGrid::place`.
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
 * shown at it, for a stream whose timestamps are whole multiples of `1 / timeResolution` seconds
 * (mediabunny's `getTimeResolution()`) rounded to `quantum` of those units ({@link
 * roundingQuantum}; 0 for none shown). The engine's `presentation_slack`, the binding's
 * `presentationSlackTicks(1, timeResolution, quantum)`.
 *
 * A stream whose timestamps show a rounding is given half of it, rounded up to a tick, and one tick
 * more, however coarse (61 ticks for milliseconds, 101 for 29.97 fps in 1/600). One that shows none
 * is given half its unit, rounded up to a tick, and one tick more, at most {@link
 * PRESENTATION_SLACK_TICKS}: its frames are exact in the time base. A resolution not known is
 * {@link PRESENTATION_SLACK_TICKS}.
 *
 * The frame shown at a time is the last one at or before `time + slack`: a timestamp is rounded to
 * its time base (Matroska's millisecond) and to the nearest tick, while a project frame's time is
 * floored, so the frame truly shown can carry a timestamp a few ticks late. A native host decides
 * the same way.
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
/**
 * Decoded frames the reverse-window cache keeps at most: the frame presented at the last request
 * and the one after it.
 *
 * Each holds one of the decoder's output surfaces, and a hardware decoder has a fixed pool of them.
 * Chromium's Windows decoder allocates its DPB size plus six, and the DPB can occupy all of its own
 * share: a stream without a VUI is reordered by the whole DPB, and long-term references stay in it
 * for a whole GOP. That leaves about five surfaces for everything outside the decoder; a cache of
 * eight holds them all, and the decoder stops producing output every few frames.
 */
const DEFAULT_MAX_CACHED_FRAMES = 2;
/**
 * The fewest frames the cache works with, and the fewest its byte limit evicts down to: the frame
 * at or before a time and the one after it, which proves that frame is still presented.
 */
const MIN_CACHED_FRAMES = 2;
/** How long a decoder may produce nothing before the refill gives up. */
const DEFAULT_STALL_TIMEOUT_MS = 5_000;
/**
 * The longest `stallTimeoutMs` a decoder takes, about 24.8 days: timers hold their delay in a
 * signed 32-bit integer, and a longer one fires at once (Node) or wraps (browsers), which would
 * fail every read as a stall.
 */
export const MAX_STALL_TIMEOUT_MS = 2_147_483_647;

/**
 * Throws `RangeError` unless `value` is a whole number of milliseconds from 1 to
 * {@link MAX_STALL_TIMEOUT_MS}.
 *
 * @internal Shared by the decoder adapters.
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

/** Why the engine needs a decoded source frame. */
export type DecoderRefillReason =
  "prefetch" | "seek" | "evicted" | "device-recovery";

/** A request for the frame of one source presented at a time, shared by every video decoder. */
export interface DecoderRefillRequest {
  /** The source the decoder was opened for; a decoder rejects any other. */
  readonly sourceId: string;
  /** Source time, in ticks (120 000 per second). */
  readonly mediaTimeTicks: bigint;
  /** `seek` and `device-recovery` always restart decoding from a keyframe. */
  readonly reason: DecoderRefillReason;
  /**
   * The seek generation the request belongs to. A request from a new generation drops the
   * decoder's cursor and cached frames; a frame answered for an older one is stale.
   */
  readonly generation: number;
  /**
   * Who reads the source sequentially ({@link decoderConsumerId}): requests with different
   * consumers keep different decode cursors, as the engine requires of two clips, or two
   * composition steps, reading one source at different times. Absent, the source has one cursor.
   */
  readonly consumerId?: string;
}

/**
 * The consumer that owns a decode cursor: the clip, or one composition step of it (`readerId`)
 * that reads a source on its own. A decoder shared by two of them would seek on every frame.
 */
export function decoderConsumerId(
  clipId: string,
  readerId?: string | null,
): string {
  return readerId ? `${clipId}\u0000${readerId}` : clipId;
}

/**
 * How a video's pictures are shown upright: mirrored left to right when `mirror` is set, then
 * turned clockwise by `rotationDegrees`. The eight together are every orientation a container
 * states with a multiple of 90 degrees, as the engine's media contract carries it.
 */
export interface VideoOrientation {
  readonly rotationDegrees: 0 | 90 | 180 | 270;
  readonly mirror: boolean;
}

/** Shown as stored. */
export const UPRIGHT: VideoOrientation = Object.freeze({
  rotationDegrees: 0,
  mirror: false,
});

/** A container file, or its bytes. */
export type ContainerBytes = Blob | ArrayBuffer | ArrayBufferView;

/** Bytes this reads before it gives up on a container's header. */
const MAX_HEADER_SCAN_BYTES = 64 * 1024 * 1024;

/**
 * How the container says track `trackId`'s pictures are shown upright, read as native decoding
 * (FFmpeg) reads it, mirror included: an MP4 or QuickTime track's `tkhd` matrix composed with the
 * movie's `mvhd` matrix, or a Matroska or WebM track's rectangular projection pose. `trackId` is
 * the `tkhd` track ID or the Matroska `TrackNumber`, which mediabunny reports as `InputTrack.id`. A
 * matrix that turns by other than a multiple of 90 degrees is shown upright, as natively.
 *
 * Resolves to `undefined` for any other container, or one whose header this cannot find; demuxers
 * report a rotation but not a mirror, so a caller then has only the rotation its demuxer reports.
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

/** Ranged reads over a Blob or bytes in memory. */
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
  /** Where the box's payload starts and ends. */
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

/**
 * A display matrix's linear part, row-major `[a, b, c, d]`: `(x, y)` shows at `(a*x + c*y, b*x +
 * d*y)`.
 */
type Matrix2 = readonly [number, number, number, number];

/** The 2x2 part of the 3x3 matrix a `mvhd` or `tkhd` box stores at `offset`. */
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
      // After version and flags: creation and modification time, timescale, duration, rate, volume,
      // and ten reserved bytes.
      movie = (await boxMatrix(reader, box, version === 1 ? 48 : 36)) ?? movie;
    } else if (box.type === "trak") {
      const tkhd = await child(reader, box, "tkhd");
      if (tkhd === undefined) continue;
      const header = await reader.read(tkhd.start, 24);
      if (header.length < 24) continue;
      const version = header[0];
      if (readU32(header, version === 1 ? 20 : 12) !== trackId) continue;
      // After the track ID: a reserved word, duration, eight reserved bytes, layer, alternate
      // group, volume, and two reserved bytes.
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

/**
 * The orientation a display matrix shows, as `av_display_rotation_get` reads it. A matrix that
 * mirrors (its determinant is negative) is read as FFmpeg reads one: a rotation followed by a flip
 * of its first column, which is a mirror before the opposite rotation.
 */
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

/**
 * A clockwise angle that is a multiple of 90 degrees (within half a degree), as an orientation; any
 * other angle is shown upright.
 */
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

/** An EBML element: its ID with the length marker kept, as the specification writes IDs. */
interface Element {
  readonly id: number;
  readonly start: number;
  /** Where its payload ends; the end of its parent when the size is unknown. */
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

/**
 * A variable-length integer at `at`: an ID keeps its length marker; a size of all ones is unknown
 * (`value` undefined).
 */
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

/**
 * The track entries of a Matroska file's first `Tracks` element, each with its `TrackNumber`;
 * none when the file has no `Tracks` before its first cluster.
 */
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

/**
 * A Matroska track's orientation, as FFmpeg reads a rectangular projection's pose: a roll is a
 * counter-clockwise turn, and a yaw of 180 degrees mirrors. Any other pose is a spherical one,
 * shown upright.
 */
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
 * The `CodecDelay` a Matroska or WebM file declares for track `trackId`, in nanoseconds: how far
 * each of the track's block times is after the time its samples are presented at. The
 * specification has a reader subtract it from every block time, and native decoding (FFmpeg) does,
 * rounded to the track's time base; mediabunny does not. `trackId` is the Matroska `TrackNumber`,
 * which mediabunny reports as `InputTrack.id`.
 *
 * Resolves to 0 for a track that declares none, and for any other container.
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
  /**
   * Clockwise turn that shows the pictures upright, after {@link mirror}. {@link
   * readVideoOrientation} reads both from an MP4, QuickTime, or Matroska file.
   */
  readonly rotationDegrees?: 0 | 90 | 180 | 270;
  /** Whether the pictures are mirrored left to right before they turn. */
  readonly mirror?: boolean;
  /**
   * The units per second the chunks' timestamps are whole multiples of, when the demuxer knows it
   * (mediabunny's `getTimeResolution()`): it bounds how late a frame's timestamp may be and still
   * be the frame shown at a time ({@link presentationSlackTicks}).
   */
  readonly timeResolution?: number;
  /**
   * The unit, of {@link timeResolution}, the stream's timestamps are rounded to, as the first ones
   * show ({@link roundingQuantum} of the first chunks' times, up to {@link ROUNDING_SCAN_FRAMES}
   * and {@link ROUNDING_SCAN_BYTES}); 0 or absent when they show none. It widens the slack past
   * what the resolution alone gives: an MP4 that kept a Matroska file's milliseconds in a 1/16000
   * time base needs a millisecond's.
   */
  readonly roundingQuantum?: number;
  /**
   * The stream's chunks in decode order, starting at the keyframe at or before `mediaTimeTicks`
   * (the first keyframe when none is). Stops early when `signal` aborts.
   */
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
  /** Clockwise turn that shows the frame upright, after {@link mirror}. */
  readonly rotationDegrees?: 0 | 90 | 180 | 270;
  /** Whether the frame is mirrored left to right before it turns. */
  readonly mirror?: boolean;
  /**
   * Where chroma samples sit, which WebCodecs does not report: see {@link chromaSitingForStream}.
   */
  readonly chromaSiting?: ChromaSiting;
}

/** Public browser decoder contract; no optional demuxer types leak through it. */
export interface BrowserVideoDecoder {
  /**
   * Resolves to the frame presented at `request.mediaTimeTicks`: the one whose presentation
   * interval contains it, the last frame whose timestamp is at or before that time plus the
   * stream's presentation slack (`presentationSlackTicks`), as the native decoder resolves a time.
   * A stream whose first frame is later answers with that frame, and past the end the last frame
   * answers. `decodedTimeTicks` is the frame's own time.
   */
  refill(
    request: DecoderRefillRequest,
    signal?: AbortSignal,
  ): Promise<BrowserDecodedFrame>;
  /** Drops the decode cursor and every cached frame; the next request seeks. */
  flush(): Promise<void>;
  /** Releases the decoder and every frame it holds. Frames already returned stay the caller's. */
  close(): void;
}

/**
 * Limits for {@link WebCodecsVideoDecoder}. Each must be a positive safe integer, and
 * `stallTimeoutMs` at most {@link MAX_STALL_TIMEOUT_MS}.
 */
export interface BrowserVideoDecoderOptions {
  /**
   * Bytes of decoded frames the cache keeps (default 64 MiB). It never evicts below two frames,
   * which answering a time takes, however large they are.
   */
  readonly maxCachedFrameBytes?: number;
  /**
   * Bytes of decoded frames queued ahead of the cursor (default 64 MiB). Past it, queued frames
   * earlier than the requested time move to the cache, keeping at least one queued.
   */
  readonly maxReadyFrameBytes?: number;
  /**
   * Frames the reverse-window cache keeps at most (default 2, at least 2). Each holds a decoder
   * surface.
   */
  readonly maxCachedFrames?: number;
  /**
   * Milliseconds a decoder may produce no output while chunks are queued before the refill fails
   * (default 5000, at most {@link MAX_STALL_TIMEOUT_MS}). Before failing, the cache is released,
   * since its frames hold decoder surfaces. Also bounds EOF flush; abort or timeout closes that
   * decoder and releases its cached frames.
   */
  readonly stallTimeoutMs?: number;
}

/**
 * The loudspeaker one plane of an audio block feeds, by `FFmpeg`'s channel name: front left, right
 * and center, low-frequency, back, left and right of center, side, top and bottom positions, a
 * matrix downmix's Lt/Rt (`DL`, `DR`), wide, surround direct, binaural. `UNK` is a plane no speaker
 * names (unknown, ambisonic), which the engine leaves out of its fold.
 */
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

/**
 * Most channels a decoded {@link BrowserAudioBlock} carries: a 22.2 layout's 24, the most of any
 * layout `FFmpeg` names. A source with more is refused by browser and native hosts alike.
 */
export const MAX_AUDIO_CHANNELS = 24;

/** Owned project-rate planar audio returned by browser adapters. */
export interface BrowserAudioBlock {
  readonly sourceId: string;
  /**
   * Index of the first sample at `sampleRate`: of the source for a decoded block, of the project
   * timeline for a mixed one. A block written to an encode sink counts from the output's zero
   * instead, where its first video frame starts (`Alpha2Exporter.audioWindow` gives the mix
   * position written there). Never ticks, which are not a whole number of samples at 44.1 kHz.
   */
  readonly startSample: bigint;
  readonly generation: number;
  readonly sampleRate: number;
  /**
   * The speaker each plane of `channels` feeds, in plane order: the layout the source declares, in
   * the order its planes arrive. Absent when the source declares none: the engine then assumes
   * `FFmpeg`'s default for the channel count (mono, stereo, 2.1, 4.0, 5.0, 5.1, 6.1, 7.1, ...), as
   * a native host does. The engine folds the planes into the project's channels by these speakers.
   */
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
  /**
   * Decodes the requested source interval, resampled to `projectSampleRate`. Rejects when the
   * decoder is closed, the request names another source, or `signal` aborts.
   */
  refill(
    request: AudioDecoderRefillRequest,
    projectSampleRate: number,
    signal?: AbortSignal,
  ): Promise<BrowserAudioBlock>;
  /** Releases the decoder; a later `refill` rejects. */
  close(): void;
}

/** WebCodecs adapter for elementary chunks supplied by any demuxer. */
export class WebCodecsVideoDecoder implements BrowserVideoDecoder {
  readonly #sourceId: string;
  readonly #source: EncodedVideoSource;
  #decoder?: VideoDecoder;
  #iterator?: AsyncIterator<EncodedVideoChunk>;
  #ready: VideoFrame[] = [];
  /**
   * Decoded frames held back until a frame no earlier than all of them arrives: see {@link
   * #arrive}.
   */
  #arriving: VideoFrame[] = [];
  /** Whether this codec's streams can output frames out of presentation order. */
  readonly #reorders: boolean;
  /** Where the stream's chroma samples sit, read from its parameter sets. */
  #chromaSiting: ChromaSiting;
  /** Whether the configuration had no parameter sets, so the first key chunk is read for them. */
  #sitingFromKeyChunk: boolean;
  #outputWaiters: Array<() => void> = [];
  #generation?: number;
  /**
   * The last time the decode cursor answered: the frames up to the one after it have been read. A
   * request at or before it, or far past it, seeks.
   */
  #cursorTicks?: bigint;
  /**
   * When the stream's first frame is presented, once a seek has shown that the stream starts after
   * the time it sought.
   */
  #leadTicks?: bigint;
  /** No frame has been taken since the cursor last sought. */
  #firstSinceSeek = false;
  #decoderError?: Error;
  readonly #cache: VideoFrameWindow;
  readonly #maxReadyFrameBytes: number;
  readonly #stallTimeoutMs: number;
  /**
   * How late a frame may be and still be the one shown at a time ({@link presentationSlackTicks}).
   */
  readonly #slack: bigint;
  #readyBytes = 0;
  #activeTargetTicks?: bigint;
  #reverseCacheHits = 0;

  /**
   * Throws `RangeError` when an option is not a positive safe integer, `maxCachedFrames` is below
   * 2, or `stallTimeoutMs` is above {@link MAX_STALL_TIMEOUT_MS}.
   */
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
  }

  /** Bytes the frame cache holds. */
  get cachedFrameBytes(): number {
    return this.#cache.bytes;
  }

  /**
   * Reads the chroma siting from the first key chunk of a stream whose configuration carries no
   * parameter sets (Annex B, AV1 in MP4), which repeats them there.
   */
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

  /** Requests answered from the cache without decoding. */
  get reverseCacheHits(): number {
    return this.#reverseCacheHits;
  }

  async refill(
    request: DecoderRefillRequest,
    signal: AbortSignal = new AbortController().signal,
  ): Promise<BrowserDecodedFrame> {
    if (request.sourceId !== this.#sourceId) {
      throw new Error(`decoder source mismatch: ${request.sourceId}`);
    }
    throwIfAborted(signal);
    if (
      this.#generation !== undefined &&
      this.#generation !== request.generation
    ) {
      this.#resetCursor();
      this.#cache.clear();
    }
    // Requests before the stream's first frame all present that frame.
    const target =
      this.#leadTicks !== undefined && request.mediaTimeTicks < this.#leadTicks
        ? this.#leadTicks
        : request.mediaTimeTicks;
    const cached = this.#cache.presentedAt(target);
    if (cached !== undefined) {
      // A hit moves no cursor: the decode is still where it last answered.
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
      // From the keyframe at or before the deadline, since the frame shown may lie up to the slack
      // later.
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
      // A failed read may have consumed a chunk no decoder saw, and a broken decoder stays broken:
      // the next request seeks from a keyframe.
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
        // Unless frames after the target are still queued, the window's newest frame is the
        // stream's last.
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
          // Output stopped with every surface held. Give back the cache's frames except the one the
          // target may need; closing queued frames would answer with a later frame.
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
    decoder.configure(this.#source.config);
    this.#decoder = decoder;
    return decoder;
  }

  /**
   * Takes a decoded frame in. WebCodecs outputs in presentation order, but Safari 16.4 to 26.3
   * outputs H.264 B-frames in decode order, after the later reference frame that decides the frame
   * before them. A codec that can reorder therefore holds frames back, sorted, until one arrives
   * that is no earlier than all of them: that one cannot be a B-frame of the ones held, so every
   * earlier time is complete. It costs one frame of lookahead, and one more decoder surface, only
   * for codecs with B-frames.
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

  /**
   * Every frame held back goes to the ready queue: the stream ended, or a later frame proved them
   * complete.
   */
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
    // A generator's return can be queued behind a pending next that never settles. Detach it so
    // cancellation never blocks the next generation.
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
    let timer: ReturnType<typeof setTimeout> | undefined;
    let abort: (() => void) | undefined;
    try {
      throwIfAborted(signal);
      const interrupted = new Promise<never>((_, reject) => {
        abort = () => reject(abortError(signal));
        signal.addEventListener("abort", abort, { once: true });
        timer = setTimeout(
          () =>
            reject(
              new Error(
                `video decoder ${this.#sourceId} stalled during EOF flush after ${this.#stallTimeoutMs} ms`,
              ),
            ),
          this.#stallTimeoutMs,
        );
      });
      await Promise.race([decoder.flush(), interrupted]);
      this.#releaseArriving();
      throwIfAborted(signal);
      if (this.#decoderError !== undefined) throw this.#decoderError;
    } catch (error) {
      // Closing rejects the native flush; Promise.race already observes that late rejection.
      // Callbacks also check decoder identity before retaining.
      this.close();
      throw error;
    } finally {
      clearTimeout(timer);
      if (abort !== undefined) signal.removeEventListener("abort", abort);
    }
  }

  /**
   * The decoded frame presented at `targetTicks`: the last one at or before its deadline, the
   * target plus the slack. Deciding that takes the frame after, which stays queued for the next
   * request. Frames before the target pass into the reverse window.
   *
   * `ended` answers with the last frame once the stream has no frame after it. A stream whose first
   * frame is after the target answers with that frame, and is known to start there from then on:
   * the first frame a seek decodes is the stream's first when it is later than the time sought,
   * since a source's chunks start at the keyframe at or before that time.
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

  /** Moves the first queued frame into the reverse window and returns it. */
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

  /** Resolves true on output, false when none arrives within the stall timeout. */
  #waitForOutput(signal: AbortSignal): Promise<boolean> {
    return new Promise((resolve, reject) => {
      const settle = (): void => {
        clearTimeout(timer);
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
      const timer = setTimeout(() => {
        settle();
        resolve(false);
      }, this.#stallTimeoutMs);
      this.#outputWaiters.push(wake);
      signal.addEventListener("abort", abort, { once: true });
    });
  }
}

/** Limits for a {@link DecoderRefillCoordinator}. Each must be a positive safe integer. */
export interface DecoderRefillCoordinatorOptions {
  /** Refills decoding at once, across sources (default 2). */
  readonly maxConcurrency?: number;
  /**
   * Decoders kept open per source, one per consumer reading it (default 4). Past it, the one used
   * least recently is closed; each open decoder holds decoder surfaces.
   */
  readonly maxCursorsPerSource?: number;
}

/**
 * Runs refills against decoders it opens: one per source and consumer
 * ({@link DecoderRefillRequest.consumerId}), so overlapping clips of one source each decode
 * forward from where they were. Refills of one source run one at a time, at most `maxConcurrency`
 * at once, delivering each frame through `deliver`. A newer generation aborts the requests in
 * flight; a frame decoded for an older one is closed instead of delivered.
 */
export class DecoderRefillCoordinator {
  readonly #open: (
    sourceId: string,
  ) => BrowserVideoDecoder | Promise<BrowserVideoDecoder>;
  readonly #deliver: (frame: BrowserDecodedFrame) => Promise<void>;
  #generation = 0;
  #controller = new AbortController();
  readonly #maxConcurrency: number;
  readonly #maxCursorsPerSource: number;
  #active = 0;
  #capacityWaiters: Array<() => void> = [];
  #sourceTails = new Map<string, Promise<void>>();
  /** Each source's decoders by consumer, least recently used first. */
  readonly #cursors = new Map<
    string,
    Map<string, Promise<BrowserVideoDecoder>>
  >();
  #closed = false;

  /**
   * `open` makes a decoder for a source, called once per consumer that reads it; it may reject for
   * a source it cannot decode, which rejects that refill. Throws `RangeError` when an option is not
   * a positive safe integer.
   */
  constructor(
    open: (
      sourceId: string,
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

  /** Aborts the requests in flight and returns the new generation number for later requests. */
  beginGeneration(): number {
    this.#controller.abort(new DOMException("Superseded", "AbortError"));
    this.#controller = new AbortController();
    return ++this.#generation;
  }

  /**
   * Decodes `request` and delivers its frame. Resolves without delivering when the request's
   * generation is already superseded; rejects with the decoder's, the delivery's, or the abort's
   * error, and once the coordinator is closed.
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
      const decoder = await this.#cursor(
        request.sourceId,
        request.consumerId ?? "",
      );
      const frame = await decoder.refill(request, this.#controller.signal);
      if (frame.generation !== this.#generation) {
        frame.frame.close();
        return;
      }
      await this.#deliver(frame);
    } finally {
      this.#release();
    }
  }

  /**
   * The consumer's decoder for a source, opened on first use. Runs inside the source's queue, so
   * the decoder it closes past the limit is not decoding.
   */
  async #cursor(
    sourceId: string,
    consumerId: string,
  ): Promise<BrowserVideoDecoder> {
    let cursors = this.#cursors.get(sourceId);
    if (cursors === undefined) {
      cursors = new Map();
      this.#cursors.set(sourceId, cursors);
    }
    let decoder = cursors.get(consumerId);
    if (decoder === undefined) {
      decoder = Promise.resolve().then(() => this.#open(sourceId));
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

  /** Aborts every request in flight and closes every decoder it opened. */
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

/** Shape used to wrap mediabunny without importing its types into alpha2. */
export interface OptionalMediaAdapterFactory {
  openVideo(sourceId: string, input: unknown): Promise<BrowserVideoDecoder>;
  openAudio?(sourceId: string, input: unknown): Promise<BrowserAudioDecoder>;
}

/**
 * The tick nearest a WebCodecs microsecond timestamp.
 *
 * A tick is 1/120 000 s (8.33 microseconds), so a frame boundary that is a whole tick arrives from
 * the container rounded to the microsecond: 30 fps frame one is 33 333 microseconds, or 3999.96
 * ticks. Flooring would make it 3999, below its own tick, so a request for 4000 would skip it.
 * Every decoder path converts through this one function, so a frame's tick never depends on which
 * path produced it.
 */
export function microsecondsToTicks(microseconds: number): bigint {
  const scaled = BigInt(Math.round(microseconds)) * 120_000n;
  // Half away from zero, exact in BigInt; negative timestamps are legal.
  const half = scaled < 0n ? -500_000n : 500_000n;
  return (scaled + half) / 1_000_000n;
}

function throwIfAborted(signal: AbortSignal): void {
  if (signal.aborted) throw abortError(signal);
}

/**
 * The reason a signal carries, as an `Error` a caller can reject with.
 *
 * `AbortSignal.reason` is typed `any` and a caller may abort with a value that is not an `Error`,
 * which would reject without a stack. An `Error` passes through untouched, so an `AbortError` keeps
 * its name.
 */
function abortError(signal: AbortSignal): Error {
  const reason: unknown = signal.reason;
  if (reason instanceof Error) return reason;
  // A string reason is the only non-Error form that carries readable text.
  if (typeof reason === "string") return new DOMException(reason, "AbortError");
  return new DOMException("Aborted", "AbortError");
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
  /**
   * How late a frame may be and still be the one shown at a time ({@link presentationSlackTicks}).
   */
  readonly #slack: bigint;
  /**
   * The stream's last frame, once decoding reached the stream's end: past it no frame comes, so
   * that frame answers every later time without a decode. A property of the stream, kept when the
   * window is cleared; it applies while that frame is the newest one held.
   */
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

  /** Records that the newest frame held is the stream's last: see {@link #endTicks}. */
  markEnd(): void {
    this.#endTicks = this.#entries.at(-1)?.ticks;
  }

  /**
   * A clone of the frame presented at `target`, the last one at or before its deadline (the target
   * plus the slack), or `undefined` when the window cannot tell: nothing at or before the deadline,
   * or only the newest frame before the target with no later frame to show that it is still
   * presented. `final` says no later frame is coming, which the window knows itself when its newest
   * frame is the stream's last.
   */
  presentedAt(target: bigint, final = false): VideoFrame | undefined {
    const deadline = target + this.#slack;
    let index = -1;
    for (const [position, entry] of this.#entries.entries()) {
      if (entry.ticks > deadline) break;
      index = position;
    }
    const entry = this.#entries[index];
    if (entry === undefined) return undefined;
    // A frame from the target to its deadline is the one shown; one before it is, unless a later
    // frame still comes by the deadline.
    if (
      entry.ticks < target &&
      index === this.#entries.length - 1 &&
      !final &&
      entry.ticks !== this.#endTicks
    )
      return undefined;
    return entry.frame.clone();
  }

  /**
   * Closes every frame but the one presented at `target`, the last at or before its deadline (the
   * target plus the slack), which answers `target` when the next frame decoded is later. Returns
   * how many were closed.
   */
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

/**
 * Whether a codec's streams can have B-frames, which a decoder outputs out of presentation order
 * when it does not follow WebCodecs: H.264 and HEVC. VP8, VP9, and AV1 output every shown frame in
 * order.
 */
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
import { type ChromaSiting, chromaSitingForStream } from "./chroma-siting.js";
