/**
 * The speaker layout a browser-decoded audio source declares, read as a native host reads it.
 *
 * WebCodecs `AudioData` and mediabunny's demuxers carry only a channel count, so the layout comes
 * from the bytes: a WAVE `dwChannelMask`, a QuickTime `chan` or ISO `chnl` box, a FLAC
 * `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` comment, or the codec's own headers (an AAC channel
 * configuration or program config element, an Opus head, an AC-3, E-AC-3 or DTS frame header).
 * Each rule is `FFmpeg`'s (`riffdec.c`, `mov_chan.c`, the FLAC and Matroska demuxers, the AAC,
 * Opus, Vorbis, FLAC, AC-3 and DTS decoders), so the speakers reported here are the ones the native
 * decoder reports for the same file. `null` means the source declares none, or the native decoder
 * would leave its layout unspecified; the engine then assumes `FFmpeg`'s default for the channel
 * count, as a native host does.
 */
import type { AudioSpeaker } from "./media.js";

/**
 * `FFmpeg`'s channel names, by channel number (`enum AVChannel`); a number without one is `UNK`.
 */
const SPEAKER_BY_CHANNEL = new Map<number, AudioSpeaker>([
  [0, "FL"],
  [1, "FR"],
  [2, "FC"],
  [3, "LFE"],
  [4, "BL"],
  [5, "BR"],
  [6, "FLC"],
  [7, "FRC"],
  [8, "BC"],
  [9, "SL"],
  [10, "SR"],
  [11, "TC"],
  [12, "TFL"],
  [13, "TFC"],
  [14, "TFR"],
  [15, "TBL"],
  [16, "TBC"],
  [17, "TBR"],
  [29, "DL"],
  [30, "DR"],
  [31, "WL"],
  [32, "WR"],
  [33, "SDL"],
  [34, "SDR"],
  [35, "LFE2"],
  [36, "TSL"],
  [37, "TSR"],
  [38, "BFC"],
  [39, "BFL"],
  [40, "BFR"],
  [41, "SSL"],
  [42, "SSR"],
  [43, "TTL"],
  [44, "TTR"],
  [61, "BIL"],
  [62, "BIR"],
]);

/** Every name a speaker list may hold: the named `FFmpeg` channels plus `UNK`. */
export const AUDIO_SPEAKERS: ReadonlySet<string> = new Set<string>([
  ...SPEAKER_BY_CHANNEL.values(),
  "UNK",
]);

function speakerOfChannel(channel: number): AudioSpeaker {
  return SPEAKER_BY_CHANNEL.get(channel) ?? "UNK";
}

/**
 * The speakers of a native-order channel mask, in bit order, as `FFmpeg`'s
 * `av_channel_layout_from_mask` lists them. A set bit with no named channel is `UNK`.
 */
export function speakersOfMask(mask: bigint): AudioSpeaker[] {
  const speakers: AudioSpeaker[] = [];
  for (let bit = 0; bit < 64; bit += 1)
    if ((mask >> BigInt(bit)) & 1n) speakers.push(speakerOfChannel(bit));
  return speakers;
}

/** The native channel bit of each named speaker. */
const CHANNEL_OF_SPEAKER = new Map<AudioSpeaker, number>(
  [...SPEAKER_BY_CHANNEL].map(([channel, speaker]) => [speaker, channel]),
);

/**
 * `speakers` with an LFE added where a native-order layout has it, and the plane it lands in: the
 * layout a decoder that adds a silent LFE to a source without one delivers (a WebCodecs decoder
 * does so for E-AC-3 5.0). `null` when `speakers` already has an LFE or is not a native-order list
 * of named speakers, so where the decoder put the LFE is unknown.
 */
export function speakersWithLfe(
  speakers: readonly AudioSpeaker[],
): { speakers: AudioSpeaker[]; lfePlane: number } | null {
  const channels = speakers.map((speaker) => CHANNEL_OF_SPEAKER.get(speaker));
  const lfe = CHANNEL_OF_SPEAKER.get("LFE") as number;
  if (
    channels.some(
      (channel, index) =>
        channel === undefined ||
        channel === lfe ||
        (index > 0 && channel <= (channels[index - 1] as number)),
    )
  )
    return null;
  const at = channels.findIndex((channel) => (channel as number) > lfe);
  const lfePlane = at === -1 ? speakers.length : at;
  return {
    speakers: [
      ...speakers.slice(0, lfePlane),
      "LFE",
      ...speakers.slice(lfePlane),
    ],
    lfePlane,
  };
}

const list = (names: string): AudioSpeaker[] =>
  names.split(" ") as AudioSpeaker[];

/**
 * A layout, or `null` when it names no speaker, as `SpeakerLayout::declared_or_default` reads it.
 */
function declared(speakers: AudioSpeaker[] | null): AudioSpeaker[] | null {
  if (speakers === null || speakers.every((speaker) => speaker === "UNK"))
    return null;
  return speakers;
}

/**
 * The speakers of a WAVE `fmt ` chunk payload's `WAVE_FORMAT_EXTENSIBLE` channel mask, as
 * `FFmpeg`'s RIFF demuxer reads it: a mask whose bit count is not `channels` says nothing.
 * `null` for any other WAVE format, a payload shorter than 24 bytes, or a mask naming no speaker.
 */
export function wavSpeakers(
  fmt: Uint8Array,
  channels: number,
): AudioSpeaker[] | null {
  if (fmt.length < 24) return null;
  const view = new DataView(fmt.buffer, fmt.byteOffset, fmt.byteLength);
  if (view.getUint16(0, true) !== 0xfffe) return null;
  const speakers = speakersOfMask(BigInt(view.getUint32(20, true)));
  return speakers.length === channels ? declared(speakers) : null;
}

const MOV_LABELS: Readonly<Record<string, AudioSpeaker>> = {
  L: "FL",
  R: "FR",
  C: "FC",
  LFE: "LFE",
  Rls: "BL",
  Rrs: "BR",
  Lc: "FLC",
  Rc: "FRC",
  Cs: "BC",
  Ls: "SL",
  Rs: "SR",
  Ts: "TC",
  Vhl: "TFL",
  Vhc: "TFC",
  Vhr: "TFR",
  Rlt: "TBL",
  Rrt: "TBR",
  Lt: "DL",
  Rt: "DR",
  Lw: "WL",
  Rw: "WR",
  Lsd: "SDL",
  Rsd: "SDR",
  LFE2: "LFE2",
  LFE1: "LFE",
};

/**
 * QuickTime channel layout tags (`kAudioChannelLayoutTag_*`, `mov_chan.h`) and the channels
 * `mov_chan.c` lays each out as. A label it has no channel for (ambisonic W X Y Z, Csd, HI, VI,
 * Haptic) is unnamed.
 */
const MOV_LAYOUT_TAGS: ReadonlyMap<number, string> = new Map([
  [100, "C"],
  [101, "L R"],
  [102, "L R"],
  [103, "Lt Rt"],
  [104, "L R"],
  [105, "L R"],
  [106, "L R"],
  [107, "W X Y Z"],
  [108, "L R Rls Rrs"],
  [109, "L R Rls Rrs C"],
  [110, "L R Rls Rrs C Cs"],
  [111, "L R Rls Rrs C Cs Ls Rs"],
  [112, "L R Rls Rrs Vhl Vhr Rlt Rrt"],
  [113, "L R C"],
  [114, "C L R"],
  [115, "L R C Cs"],
  [116, "C L R Cs"],
  [117, "L R C Ls Rs"],
  [118, "L R Ls Rs C"],
  [119, "L C R Ls Rs"],
  [120, "C L R Ls Rs"],
  [121, "L R C LFE Ls Rs"],
  [122, "L R Ls Rs C LFE"],
  [123, "L C R Ls Rs LFE"],
  [124, "C L R Ls Rs LFE"],
  [125, "L R C LFE Ls Rs Cs"],
  [126, "L R C LFE Ls Rs Lc Rc"],
  [127, "C Lc Rc L R Ls Rs LFE"],
  [128, "L R C LFE Ls Rs Rls Rrs"],
  [129, "L R Ls Rs C LFE Lc Rc"],
  [130, "L R C LFE Ls Rs Lt Rt"],
  [131, "L R Cs"],
  [132, "L R Ls Rs"],
  [133, "L R LFE"],
  [134, "L R LFE Cs"],
  [135, "L R LFE Ls Rs"],
  [136, "L R C LFE"],
  [137, "L R C LFE Cs"],
  [138, "L R Ls Rs LFE"],
  [139, "L R Ls Rs C Cs"],
  [140, "L R Ls Rs C Rls Rrs"],
  [141, "C L R Ls Rs Cs"],
  [142, "C L R Ls Rs Cs LFE"],
  [143, "C L R Ls Rs Rls Rrs"],
  [144, "C L R Ls Rs Rls Rrs Cs"],
  [145, "L R C Vhc Lsd Rsd Ls Rs Vhl Vhr Lw Rw Csd Cs LFE1 LFE2"],
  [
    146,
    "L R C Vhc Lsd Rsd Ls Rs Vhl Vhr Lw Rw Csd Cs LFE1 LFE2 Lc Rc HI VI Haptic",
  ],
  [148, "L R Ls Rs C Lc Rc"],
  [149, "C LFE"],
  [150, "L C R"],
  [151, "L C R Cs"],
  [152, "L C R LFE"],
  [153, "L R Cs LFE"],
  [154, "L C R Cs LFE"],
  [155, "L C R Ls Rs Cs"],
  [156, "L C R Ls Rs Rls Rrs"],
  [157, "L C R Ls Rs LFE Cs"],
  [158, "L C R Ls Rs LFE Ts"],
  [159, "L C R Ls Rs LFE Vhc"],
  [160, "L C R Ls Rs LFE Rls Rrs"],
  [161, "L C R Ls Rs LFE Lc Rc"],
  [162, "L C R Ls Rs LFE Lsd Rsd"],
  [163, "L C R Ls Rs LFE Lw Rw"],
  [164, "L C R Ls Rs LFE Vhl Vhr"],
  [165, "L C R Ls Rs LFE Cs Ts"],
  [166, "L C R Ls Rs LFE Cs Vhc"],
  [167, "L C R Ls Rs LFE Ts Vhc"],
  [168, "C L R LFE"],
  [169, "C L R Cs LFE"],
  [170, "Lc Rc L R Ls Rs"],
  [171, "C L R Rls Rrs Ts"],
  [172, "C Cs L R Rls Rrs"],
  [173, "Lc Rc L R Ls Rs LFE"],
  [174, "C L R Rls Rrs Ts LFE"],
  [175, "C Cs L R Rls Rrs LFE"],
  [176, "Lc C Rc L R Ls Rs"],
  [177, "Lc C Rc L R Ls Rs LFE"],
  [178, "Lc Rc L R Ls Rs Rls Rrs"],
  [179, "Lc C Rc L R Ls Cs Rs"],
  [180, "Lc Rc L R Ls Rs Rls Rrs LFE"],
  [181, "Lc C Rc L R Ls Cs Rs LFE"],
  [182, "C L R Ls Rs LFE Cs"],
]);

/** A QuickTime channel label's channel (`mov_get_channel_id`). */
function movLabelSpeaker(label: number): AudioSpeaker {
  if (label >= 1 && label <= 18) return speakerOfChannel(label - 1);
  return (
    (
      { 35: "WL", 36: "WR", 37: "LFE2", 38: "DL", 39: "DR" } as Record<
        number,
        AudioSpeaker
      >
    )[label] ?? "UNK"
  );
}

/**
 * The speakers a QuickTime `chan` box's payload (after its version and flags) declares, as
 * `FFmpeg`'s `ff_mov_read_chan` reads it: channel descriptions, a channel bitmap whose bit count
 * is the channel count, or a layout tag of that count that `mov_chan.c` knows. `null` for anything
 * else, including a truncated payload.
 */
export function movChanSpeakers(
  chan: Uint8Array,
  channels: number,
): AudioSpeaker[] | null {
  if (chan.length < 12) return null;
  const view = new DataView(chan.buffer, chan.byteOffset, chan.byteLength);
  const tag = view.getUint32(0);
  const bitmap = view.getUint32(4);
  const descriptions = view.getUint32(8);
  if (chan.length < 12 + descriptions * 20) return null;
  if (tag === 0) {
    if (descriptions === 0 || descriptions < channels) return null;
    const speakers: AudioSpeaker[] = [];
    for (let index = 0; index < channels; index += 1)
      speakers.push(movLabelSpeaker(view.getUint32(12 + index * 20)));
    return declared(speakers);
  }
  if (tag === 0x10000) {
    if (bitmap >= 0x40000) return null;
    const speakers = speakersOfMask(BigInt(bitmap));
    return speakers.length === channels ? declared(speakers) : null;
  }
  if ((tag & 0xffff) !== channels) return null;
  const labels = MOV_LAYOUT_TAGS.get(tag >>> 16);
  if (labels === undefined) return null;
  return declared(labels.split(" ").map((label) => MOV_LABELS[label] ?? "UNK"));
}

/**
 * Reads `length` bytes of the source at `offset`. Resolves with fewer bytes (possibly none) when
 * the source ends first.
 */
export type ByteReader = (
  offset: number,
  length: number,
) => Promise<Uint8Array>;

const CONTAINERS = new Set(["moov", "trak", "mdia", "minf", "stbl", "wave"]);
/** The largest `moov` box read whole to find a `chan`; a larger one declares none. */
const MAX_MOOV_BYTES = 64 * 1024 * 1024;

function boxType(bytes: Uint8Array, at: number): string {
  return String.fromCharCode(...bytes.subarray(at, at + 4));
}

/**
 * The boxes inside `bytes[start, end)`: type, payload start and end. Stops at the first malformed
 * one.
 */
function* boxes(
  bytes: Uint8Array,
  start: number,
  end: number,
): Generator<{ type: string; payload: number; end: number }> {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let at = start;
  while (at + 8 <= end) {
    let size = view.getUint32(at);
    let header = 8;
    if (size === 1) {
      if (at + 16 > end) return;
      size = Number(view.getBigUint64(at + 8));
      header = 16;
    } else if (size === 0) size = end - at;
    if (size < header || at + size > end) return;
    yield {
      type: boxType(bytes, at + 4),
      payload: at + header,
      end: at + size,
    };
    at += size;
  }
}

/** A sample description's channel layout box: a QuickTime `chan` or an ISO `chnl`. */
type LayoutBox =
  | { readonly type: "chan"; readonly payload: Uint8Array }
  | { readonly type: "chnl"; readonly payload: Uint8Array };

/**
 * The first channel layout box in a `trak`'s sound sample description, searched where `FFmpeg`
 * reads one: a `chan` (its payload after the version and flags) or a version 0 `chnl` (after its
 * version and flags; `FFmpeg` ignores any other version).
 */
function findLayoutBox(
  bytes: Uint8Array,
  start: number,
  end: number,
): LayoutBox | null {
  for (const box of boxes(bytes, start, end)) {
    if (box.type === "chan")
      return {
        type: "chan",
        payload: bytes.subarray(box.payload + 4, box.end),
      };
    if (
      box.type === "chnl" &&
      box.end - box.payload >= 4 &&
      bytes.subarray(box.payload, box.payload + 4).every((byte) => byte === 0)
    )
      return {
        type: "chnl",
        payload: bytes.subarray(box.payload + 4, box.end),
      };
    if (CONTAINERS.has(box.type)) {
      const found = findLayoutBox(bytes, box.payload, box.end);
      if (found) return found;
    }
    if (box.type === "stsd") {
      // Full box, then the entry count; each entry is a sample description whose extensions
      // follow its fixed fields.
      for (const entry of boxes(bytes, box.payload + 8, box.end)) {
        const view = new DataView(bytes.buffer, bytes.byteOffset);
        const version = view.getUint16(entry.payload + 8);
        // QuickTime sound descriptions grow by 16 (version 1) or 36 (version 2) bytes; an ISO
        // entry of version 1 does not, so both offsets are tried.
        const offsets = version === 2 ? [56] : version === 1 ? [36, 20] : [20];
        for (const offset of offsets) {
          const found = findLayoutBox(
            bytes,
            entry.payload + 8 + offset,
            entry.end,
          );
          if (found) return found;
        }
      }
    }
  }
  return null;
}

/** The track ID of a `trak` (`tkhd`), or `null`. */
function trackId(bytes: Uint8Array, trak: { payload: number; end: number }) {
  for (const box of boxes(bytes, trak.payload, trak.end)) {
    if (box.type !== "tkhd") continue;
    const view = new DataView(bytes.buffer, bytes.byteOffset);
    const version = bytes[box.payload];
    return view.getUint32(box.payload + (version === 1 ? 20 : 12));
  }
  return null;
}

/** The ASCII text of `length` bytes at `offset`, or `null` when the source is shorter. */
async function readTag(
  read: ByteReader,
  offset: number,
  length: number,
): Promise<string | null> {
  const bytes = await read(offset, length);
  return bytes.length < length ? null : boxType(bytes, 0).slice(0, length);
}

async function wavFmt(read: ByteReader): Promise<Uint8Array | null> {
  const head = await read(0, 12);
  if (head.length < 12) return null;
  const riff = boxType(head, 0);
  if (!["RIFF", "RF64", "BW64"].includes(riff) || boxType(head, 8) !== "WAVE")
    return null;
  let at = 12;
  // A `fmt ` chunk comes before the data; walk at most 64 chunks to it.
  for (let chunk = 0; chunk < 64; chunk += 1) {
    const header = await read(at, 8);
    if (header.length < 8) return null;
    const size = new DataView(header.buffer, header.byteOffset).getUint32(
      4,
      true,
    );
    if (boxType(header, 0) === "fmt ") return read(at + 8, Math.min(size, 64));
    at += 8 + size + (size & 1);
  }
  return null;
}

async function movLayoutBox(
  read: ByteReader,
  size: number,
  track: number,
): Promise<LayoutBox | null> {
  let at = 0;
  while (at + 8 <= size) {
    const header = await read(at, 16);
    if (header.length < 8) return null;
    const view = new DataView(header.buffer, header.byteOffset);
    let length = view.getUint32(0);
    if (length === 1 && header.length >= 16)
      length = Number(view.getBigUint64(8));
    else if (length === 0) length = size - at;
    if (length < 8) return null;
    if (boxType(header, 4) === "moov") {
      if (length > MAX_MOOV_BYTES) return null;
      const moov = await read(at, length);
      const children = view.getUint32(0) === 1 ? 16 : 8;
      let first: LayoutBox | null = null;
      for (const trak of boxes(moov, children, moov.length)) {
        if (trak.type !== "trak") continue;
        const box = findLayoutBox(moov, trak.payload, trak.end);
        if (box === null) continue;
        if (trackId(moov, trak) === track) return box;
        first ??= box;
      }
      return first;
    }
    at += length;
  }
  return null;
}

/** ISO/IEC 23001-8 speaker positions (table 8), as `mov_chan.c` maps them; others are unnamed. */
const ISO_POSITIONS = list(
  "FL FR FC LFE SL SR FLC FRC BL BR BC SDL SDR SSL SSR WL WR TFL TFR TFC TBL TBR TBC TSL TSR TC LFE2 BFL BFR BFC TTL TTR",
);
/** ISO/IEC 23001-8 channel configurations, as `mov_chan.c` lays each out. */
const ISO_CONFIGURATIONS: Readonly<Record<number, string>> = {
  1: "FC",
  2: "FL FR",
  3: "FC FL FR",
  4: "FC FL FR BC",
  5: "FC FL FR SL SR",
  6: "FC FL FR SL SR LFE",
  7: "FC FLC FRC FL FR SL SR LFE",
  9: "FL FR BC",
  10: "FL FR SL SR",
  11: "FC FL FR SL SR BC LFE",
  12: "FC FL FR SL SR BL BR LFE",
  13: "FC FLC FRC FL FR SSL SSR BL BR BC LFE LFE2 TFC TFL TFR TSL TSR TC TBL TBR TBC BFC BFL BFR",
  14: "FC FL FR SL SR LFE TFL TFR",
};

/**
 * The speakers an ISO/IEC 23001-8 `chnl` box's payload (after its version and flags) declares, as
 * `FFmpeg`'s `ff_mov_read_chnl` reads it: a speaker position per channel, or a channel
 * configuration of the channel count less its omitted channels. `null` for a box that carries no
 * channels, a configuration `FFmpeg` does not know or of another count, a truncated payload, or
 * one naming no speaker.
 */
export function chnlSpeakers(
  chnl: Uint8Array,
  channels: number,
): AudioSpeaker[] | null {
  if (chnl.length < 2 || !(chnl[0] & 1)) return null;
  const configuration = chnl[1];
  if (configuration === 0) {
    const speakers: AudioSpeaker[] = [];
    let at = 2;
    for (let channel = 0; channel < channels; channel += 1) {
      const position = chnl[at];
      if (position === undefined) return null;
      // An explicit position carries its azimuth and elevation, which name no speaker.
      at += position === 126 ? 4 : 1;
      speakers.push(ISO_POSITIONS[position] ?? "UNK");
    }
    return declared(speakers);
  }
  if (chnl.length < 10) return null;
  const omitted = new DataView(
    chnl.buffer,
    chnl.byteOffset + 2,
    8,
  ).getBigUint64(0);
  const layout = ISO_CONFIGURATIONS[configuration];
  if (layout === undefined) return null;
  const all = list(layout);
  let count = 0;
  for (let bit = 0n; bit < 64n; bit += 1n)
    if ((omitted >> bit) & 1n) count += 1;
  if (all.length !== channels + count || omitted >> BigInt(all.length) !== 0n)
    return null;
  return declared(
    all.filter((_, channel) => !((omitted >> BigInt(channel)) & 1n)),
  );
}

/** The channel layout a QuickTime or ISO sample description declares, or `null`. */
async function movSpeakers(
  read: ByteReader,
  size: number,
  track: number,
  channels: number,
): Promise<AudioSpeaker[] | null> {
  const box = await movLayoutBox(read, size, track);
  if (box === null) return null;
  return box.type === "chan"
    ? movChanSpeakers(box.payload, channels)
    : chnlSpeakers(box.payload, channels);
}

/** A big-endian bit reader. Reading past the end throws a `RangeError`. */
class Bits {
  #bit: number;
  readonly #bytes: Uint8Array;

  constructor(bytes: Uint8Array, bit = 0) {
    this.#bytes = bytes;
    this.#bit = bit;
  }

  get position(): number {
    return this.#bit;
  }

  get left(): number {
    return this.#bytes.length * 8 - this.#bit;
  }

  read(count: number): number {
    if (count > this.left) throw new RangeError("read past the end");
    let value = 0;
    for (let index = 0; index < count; index += 1, this.#bit += 1)
      value =
        value * 2 +
        ((this.#bytes[this.#bit >> 3] >> (7 - (this.#bit & 7))) & 1);
    return value;
  }

  show(count: number): number {
    const bit = this.#bit;
    const value = this.read(Math.min(count, this.left));
    this.#bit = bit;
    return count > this.left ? value * 2 ** (count - this.left) : value;
  }

  skip(count: number): void {
    if (count > this.left) throw new RangeError("read past the end");
    this.#bit += count;
  }

  copy(): Bits {
    return new Bits(this.#bytes, this.#bit);
  }
}

/** `FFmpeg`'s native-order channel bits (`AV_CH_*`) by speaker name. */
const CHANNEL_BIT = new Map<AudioSpeaker, bigint>(
  [...SPEAKER_BY_CHANNEL].map(([channel, speaker]) => [
    speaker,
    1n << BigInt(channel),
  ]),
);
const bitsOf = (names: string): bigint =>
  list(names).reduce((mask, speaker) => mask | CHANNEL_BIT.get(speaker)!, 0n);

/**
 * The speakers of a native-order mask of `channels` bits, or `null` for a mask of another count
 * (`FFmpeg` would deliver planes the mask does not name).
 */
function maskSpeakers(mask: bigint, channels: number): AudioSpeaker[] | null {
  const speakers = speakersOfMask(mask);
  return speakers.length === channels ? declared(speakers) : null;
}

// AAC syntax elements and the positions a program config element gives them (`aacdec.h`).
const SCE = 0;
const CPE = 1;
const CCE = 2;
const LFE_ELEMENT = 3;
const FRONT = 1;
const SIDE = 2;
const BACK = 3;
const LFE = 4;
const CC = 5;
/** An AAC syntax element and its position: a row of `FFmpeg`'s `layout_map`. */
interface AacElement {
  readonly type: number;
  readonly position: number;
}
const AAC_NONE = -1;
const AAC_UNUSED = 0x200;
/**
 * The channels `FFmpeg`'s AAC decoder assigns elements at each position, by height layer, then
 * position (front, side, back, LFE): `ff_aac_channel_map`.
 */
const AAC_CHANNEL_MAP: readonly (readonly (readonly number[])[])[] = [
  [
    [2, 6, 7, 0, 1, AAC_NONE],
    [AAC_UNUSED, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE],
    [AAC_UNUSED, 9, 10, 4, 5, 8],
    [3, 35, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE],
  ],
  [
    [13, AAC_NONE, AAC_NONE, 12, 14, AAC_NONE],
    [AAC_UNUSED, 36, 37, AAC_NONE, AAC_NONE, 11],
    [AAC_UNUSED, AAC_NONE, AAC_NONE, 15, 17, 16],
    [AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE],
  ],
  [
    [38, AAC_NONE, AAC_NONE, 39, 40, AAC_NONE],
    [AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE],
    [AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE],
    [AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE, AAC_NONE],
  ],
];

const elements = (spec: string): AacElement[] =>
  spec.split(" ").map((element) => ({
    type: { S: SCE, C: CPE, L: LFE_ELEMENT }[element[0]]!,
    position: { F: FRONT, S: SIDE, B: BACK, L: LFE }[element[1]]!,
  }));
/**
 * The elements of each AAC channel configuration, as `FFmpeg` lays them out
 * (`ff_aac_channel_layout_map`): an element's type (SCE, CPE, LFE) then its position (front, side,
 * back, LFE). Configuration 7's second front pair is a back pair: `FFmpeg` decodes it as 7.1, as
 * encoders write it, rather than the specification's 7.1(wide), unless asked to be strict.
 */
const AAC_CONFIGURATION_ELEMENTS: Readonly<Record<number, AacElement[]>> = {
  1: elements("SF"),
  2: elements("CF"),
  3: elements("SF CF"),
  4: elements("SF CF SB"),
  5: elements("SF CF CB"),
  6: elements("SF CF CB LL"),
  7: elements("SF CF CB CB LL"),
  11: elements("SF CF CB SB LL"),
  12: elements("SF CF CB CB LL"),
  13: elements("SF CF CF CB CB SB LL LL SF CF CS SS CB SB SF CF"),
  14: elements("SF CF CB LL CF"),
};
/** The channel count `FFmpeg` gives each channel configuration (`ff_mpeg4audio_channels`). */
const AAC_CONFIGURATION_CHANNELS = [
  0, 1, 2, 3, 4, 5, 6, 8, 0, 0, 0, 7, 8, 24, 8,
];
/** The sampling frequency of each index (`ff_mpeg4audio_sample_rates`). */
const AAC_SAMPLE_RATES = [
  96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000,
  12_000, 11_025, 8_000, 7_350,
];
const AOT_AAC_LC = 2;
const AOT_SBR = 5;
const AOT_PS = 29;
/** Object types whose `GASpecificConfig` `FFmpeg` reads, a program config element included. */
const AAC_GA_OBJECT_TYPES = new Set([1, 2, 3, 4, 17, 23]);

/** `count_paired_channels`: the channels of the elements at `position` from `current`. */
function countPairedChannels(
  map: readonly AacElement[],
  position: number,
  current: number,
): number {
  let channels = 0;
  let firstCpe = false;
  let sceParity = false;
  for (let index = current; index < map.length; index += 1) {
    if (map[index].position !== position) break;
    if (map[index].type === CPE) {
      if (sceParity) {
        if (position === FRONT && !firstCpe) sceParity = false;
        else return -1;
      }
      channels += 2;
      firstCpe = true;
    } else {
      channels += 1;
      if (position !== LFE) sceParity = !sceParity;
    }
  }
  if (sceParity && position === FRONT && firstCpe) return -1;
  return channels;
}

/**
 * `assign_channels`: the channels of the elements at `position` in `layer`, from `state.current`,
 * added to `state.mask`. `false` where `FFmpeg` gives up on the layout.
 */
function assignChannels(
  map: readonly AacElement[],
  layer: number,
  position: number,
  state: { current: number; mask: bigint },
): boolean {
  const channelsOf = AAC_CHANNEL_MAP[layer][position - 1];
  let channels = countPairedChannels(map, position, state.current);
  if (channels < 0 || channels > 5) return true;
  const add = (channel: number) => {
    state.mask |= 1n << BigInt(channel);
  };
  if (position === LFE) {
    for (let index = 0; channels > 0; index += 1, channels -= 1) {
      if (channelsOf[index] === AAC_NONE) return false;
      add(channelsOf[index]);
      state.current += 1;
    }
    return true;
  }
  while (channels & 1) {
    if (channelsOf[0] === AAC_NONE) return false;
    if (channelsOf[0] === AAC_UNUSED) break;
    add(channelsOf[0]);
    state.current += 1;
    channels -= 1;
  }
  let pair = position !== SIDE && channels <= 3 ? 3 : 1;
  while (channels >= 2) {
    if (channelsOf[pair] === AAC_NONE || channelsOf[pair + 1] === AAC_NONE)
      return false;
    add(channelsOf[pair]);
    add(channelsOf[pair + 1]);
    // A pair is one CPE or two SCEs.
    state.current += map[state.current].type === CPE ? 1 : 2;
    pair += 2;
    channels -= 2;
  }
  while (channels & 1) {
    if (channelsOf[5] === AAC_NONE) return false;
    add(channelsOf[5]);
    state.current += 1;
    channels -= 1;
  }
  return channels === 0;
}

/**
 * The channel mask `FFmpeg`'s AAC decoder sets for these elements (`sniff_channel_order`), or `0n`
 * when it finds none and leaves the layout unspecified. `FFmpeg` delivers its planes in the mask's
 * native order.
 */
function aacElementsMask(map: readonly AacElement[], ps: boolean): bigint {
  const state = { current: 0, mask: 0n };
  for (let layer = 0; layer < 3 && state.current < map.length; layer += 1)
    for (const position of [FRONT, SIDE, BACK, LFE])
      if (!assignChannels(map, layer, position, state)) return 0n;
  // Parametric stereo makes a lone SCE two channels.
  const channels = map.reduce(
    (count, element) =>
      element.type === CCE
        ? count
        : count +
          (element.type === CPE || (element.type === SCE && ps) ? 2 : 1),
    0,
  );
  if (ps && channels === 2)
    return state.mask === CHANNEL_BIT.get("FC") ? bitsOf("FL FR") : 0n;
  return state.mask;
}

/**
 * A program config element's elements (`decode_pce`), from just after its element instance tag,
 * or `null` when it is truncated. `alignment` is the bit the comment's byte alignment counts from.
 */
function programConfigElements(
  bits: Bits,
  alignment: number,
): AacElement[] | null {
  bits.skip(2 + 4); // object type, sampling frequency index
  const front = bits.read(4);
  const side = bits.read(4);
  const back = bits.read(4);
  const lfe = bits.read(2);
  const data = bits.read(3);
  const cc = bits.read(4);
  if (bits.read(1)) bits.skip(4); // mono mixdown
  if (bits.read(1)) bits.skip(4); // stereo mixdown
  if (bits.read(1)) bits.skip(3); // matrix mixdown
  if (bits.left < 5 * (front + side + back + cc) + 4 * (lfe + data + cc))
    return null;
  const map: AacElement[] = [];
  for (const [count, position] of [
    [front, FRONT],
    [side, SIDE],
    [back, BACK],
  ]) {
    for (let index = 0; index < count; index += 1) {
      map.push({ type: bits.read(1) ? CPE : SCE, position });
      bits.skip(4);
    }
  }
  for (let index = 0; index < lfe; index += 1) {
    map.push({ type: LFE_ELEMENT, position: LFE });
    bits.skip(4);
  }
  bits.skip(4 * data);
  for (let index = 0; index < cc; index += 1) {
    map.push({ type: CCE, position: CC });
    bits.skip(5);
  }
  bits.skip((alignment - bits.position) & 7);
  const comment = bits.read(8) * 8;
  if (bits.left < comment) return null;
  return map;
}

function aacObjectType(bits: Bits): number {
  const type = bits.read(5);
  return type === 31 ? 32 + bits.read(6) : type;
}

/**
 * The elements an `AudioSpecificConfig` declares, its channel configuration's or its program
 * config element's, as `FFmpeg` reads them, and whether parametric stereo makes a lone SCE two
 * channels. `undefined` when `FFmpeg` would not decode the config.
 */
function aacConfigElements(
  config: Uint8Array,
):
  | { readonly configuration: number; map: AacElement[]; ps: boolean }
  | undefined {
  try {
    const bits = new Bits(config);
    // `ff_mpeg4audio_get_config_gb`.
    let objectType = aacObjectType(bits);
    const rateIndex = bits.read(4);
    const sampleRate =
      rateIndex === 15 ? bits.read(24) : (AAC_SAMPLE_RATES[rateIndex] ?? 0);
    const configuration = bits.read(4);
    if (configuration >= AAC_CONFIGURATION_CHANNELS.length) return undefined;
    let sbr = -1;
    let ps = -1;
    let explicitSbr = false;
    if (
      objectType === AOT_SBR ||
      (objectType === AOT_PS &&
        !(bits.show(3) & 0x03 && !(bits.show(9) & 0x3f)))
    ) {
      if (objectType === AOT_PS) ps = 1;
      explicitSbr = true;
      sbr = 1;
      if (bits.read(4) === 15) bits.skip(24);
      objectType = aacObjectType(bits);
      if (objectType === 22) bits.skip(4);
    }
    if (!explicitSbr) {
      // A backward-compatible SBR or PS signal after the specific config.
      const sync = bits.copy();
      while (sync.left > 15) {
        if (sync.show(11) !== 0x2b7) {
          sync.skip(1);
          continue;
        }
        sync.skip(11);
        if (aacObjectType(sync) === AOT_SBR) {
          sbr = sync.read(1);
          if (sbr === 1) {
            const extension = sync.read(4);
            const extensionRate =
              extension === 15 ? sync.read(24) : AAC_SAMPLE_RATES[extension];
            if (extensionRate === sampleRate) sbr = -1;
          }
        }
        if (sync.left > 11 && sync.read(11) === 0x548) ps = sync.read(1);
        break;
      }
    }
    if (!sbr) ps = 0;
    if (
      (ps === -1 && objectType !== AOT_AAC_LC) ||
      AAC_CONFIGURATION_CHANNELS[configuration] & ~1
    )
      ps = 0;
    if (rateIndex > 12) return undefined;
    let map: AacElement[] | null | undefined;
    if (AAC_GA_OBJECT_TYPES.has(objectType)) {
      // `decode_ga_specific_config`: frame length, core coder delay, extension flag.
      bits.skip(1);
      if (bits.read(1)) bits.skip(14);
      bits.skip(1);
      if (configuration === 0) {
        bits.skip(4); // element instance tag
        map = programConfigElements(bits, 0);
      } else map = AAC_CONFIGURATION_ELEMENTS[configuration];
      if (map === null || map === undefined) return undefined;
      const channels = map.reduce(
        (count, element) =>
          element.position === CC
            ? count
            : count + (element.type === CPE ? 2 : 1),
        0,
      );
      if (channels > 1) ps = 0;
      else if (sbr === 1 && ps === -1) ps = 1;
    } else {
      map = AAC_CONFIGURATION_ELEMENTS[configuration];
      if (map === undefined) return undefined;
      ps = 0;
    }
    return { configuration, map, ps: ps === 1 };
  } catch {
    return undefined;
  }
}

/**
 * The speakers `FFmpeg`'s AAC decoder lays an `AudioSpecificConfig` out as: its channel
 * configuration's or program config element's elements, assigned and ordered as
 * `ff_aac_output_configure` does. `null` when `FFmpeg` leaves the layout unspecified;
 * `undefined` when it would not decode the config at all.
 */
export function aacSpeakers(
  config: Uint8Array,
): AudioSpeaker[] | null | undefined {
  const elements = aacConfigElements(config);
  if (elements === undefined) return undefined;
  const mask = aacElementsMask(elements.map, elements.ps);
  return mask === 0n ? null : speakersOfMask(mask);
}

/**
 * The output elements a program config element in an `AudioSpecificConfig` declares, in its
 * order, each its type then its position: `SF CF CB LL` is a front SCE, a front CPE, a back CPE
 * and an LFE (`S`, `C`, `L`; front, side, back, LFE). `null` for a config with a channel
 * configuration or one `FFmpeg` would not decode.
 */
export function aacProgramConfigElements(config: Uint8Array): string | null {
  const elements = aacConfigElements(config);
  if (elements === undefined || elements.configuration !== 0) return null;
  return elements.map
    .filter((element) => element.type !== CCE)
    .map(
      (element) =>
        ["S", "C", "", "L"][element.type] + " FSBL"[element.position],
    )
    .join(" ");
}

/** The channels of each AC-3 channel mode (`acmod`), as `FFmpeg` lays them out. */
const AC3_MODE_MASKS = [
  "FL FR",
  "FC",
  "FL FR",
  "FL FR FC",
  "FL FR BC",
  "FL FR FC BC",
  "FL FR SL SR",
  "FL FR FC SL SR",
].map(bitsOf);
/** What each bit of an E-AC-3 dependent substream's channel map adds, most significant first. */
const EAC3_CHANNEL_MAP = [
  "FL",
  "FC",
  "FR",
  "SL",
  "SR",
  "FLC FRC",
  "BL BR",
  "BC",
  "TC",
  "SDL SDR",
  "WL WR",
  "TFL TFR",
  "TFC",
  "TBL TBR",
  "LFE2",
  "LFE",
].map(bitsOf);
const AC3_SAMPLE_RATES = [48_000, 44_100, 32_000];
const EAC3_DEPENDENT = 1;

/** An AC-3 or E-AC-3 frame header, as `ff_ac3_parse_header` reads what the layout needs. */
interface Ac3Header {
  readonly frameType: number;
  readonly frameSize: number;
  readonly blocks: number;
  readonly sampleRate: number;
  readonly mask: bigint;
  readonly channelMap: number;
}

function ac3Header(frame: Uint8Array): Ac3Header | null {
  try {
    const bits = new Bits(frame);
    if (bits.read(16) !== 0x0b77) return null;
    const bsid = bits.show(29) & 0x1f;
    if (bsid > 16) return null;
    if (bsid <= 10) {
      bits.skip(16); // crc1
      const rate = bits.read(2);
      if (rate === 3 || bits.read(6) > 37) return null;
      bits.skip(5 + 3);
      const mode = bits.read(3);
      if (mode === 2) bits.skip(2);
      else {
        if (mode & 1 && mode !== 1) bits.skip(2);
        if (mode & 4) bits.skip(2);
      }
      const lfe = bits.read(1) === 1;
      return {
        frameType: 2,
        frameSize: 0,
        blocks: 6,
        sampleRate: AC3_SAMPLE_RATES[rate] >> (Math.max(bsid, 8) - 8),
        mask: AC3_MODE_MASKS[mode] | (lfe ? CHANNEL_BIT.get("LFE")! : 0n),
        channelMap: 0,
      };
    }
    const frameType = bits.read(2);
    if (frameType === 3) return null;
    const substream = bits.read(3);
    const frameSize = (bits.read(11) + 1) * 2;
    if (frameSize < 7) return null;
    const rate = bits.read(2);
    let sampleRate: number;
    let blocks = 6;
    if (rate === 3) {
      const halfRate = bits.read(2);
      if (halfRate === 3) return null;
      sampleRate = AC3_SAMPLE_RATES[halfRate] / 2;
    } else {
      blocks = [1, 2, 3, 6][bits.read(2)];
      sampleRate = AC3_SAMPLE_RATES[rate];
    }
    const mode = bits.read(3);
    const lfe = bits.read(1) === 1;
    // `FFmpeg` decodes only the first substream and its dependents.
    if (substream !== 0) return null;
    bits.skip(5);
    for (let program = 0; program < (mode ? 1 : 2); program += 1) {
      bits.skip(5);
      if (bits.read(1)) bits.skip(8);
    }
    let channelMap = 0;
    if (frameType === EAC3_DEPENDENT && bits.read(1)) {
      channelMap = bits.read(16);
      let mask = 0n;
      EAC3_CHANNEL_MAP.forEach((channels, bit) => {
        if (channelMap & (1 << (15 - bit))) mask |= channels;
      });
      if (speakersOfMask(mask).length > 16) return null;
    }
    return {
      frameType,
      frameSize,
      blocks,
      sampleRate,
      mask: AC3_MODE_MASKS[mode] | (lfe ? CHANNEL_BIT.get("LFE")! : 0n),
      channelMap,
    };
  } catch {
    return null;
  }
}

/**
 * The speakers `FFmpeg`'s AC-3 or E-AC-3 decoder lays a packet out as: its first frame's channel
 * mode and LFE, and the channels an E-AC-3 dependent frame after it adds by its channel map.
 * `undefined` when the packet does not start with a frame `FFmpeg` decodes.
 */
export function ac3Speakers(packet: Uint8Array): AudioSpeaker[] | undefined {
  const first = ac3Header(packet);
  if (first === null || first.frameType === EAC3_DEPENDENT) return undefined;
  let mask = first.mask;
  if (first.frameSize > 0 && packet.length - first.frameSize > 16) {
    const next = ac3Header(packet.subarray(first.frameSize));
    if (
      next?.frameType === EAC3_DEPENDENT &&
      next.blocks === first.blocks &&
      next.sampleRate === first.sampleRate
    )
      EAC3_CHANNEL_MAP.forEach((channels, bit) => {
        if (next.channelMap & (1 << (15 - bit))) mask |= channels;
      });
  }
  return speakersOfMask(mask);
}

/** The speakers of each DTS core audio channel arrangement (`AMODE`) `FFmpeg` decodes. */
const DTS_MODE_MASKS = [
  "FC",
  "FL FR",
  "FL FR",
  "FL FR",
  "FL FR",
  "FL FR FC",
  "FL FR BC",
  "FL FR FC BC",
  "FL FR SL SR",
  "FL FR FC SL SR",
].map(bitsOf);
const DTS_CORE_SYNC = 0x7ffe8001;
const DTS_CORE_SYNC_LE = 0xfe7f0180;
const DTS_SUBSTREAM_SYNC = 0x64582025;
/** The core extension types that add channels: XCH and XXCH (X96 adds none). */
const DTS_CHANNEL_EXTENSIONS = new Set([0, 6]);

/**
 * The speakers `FFmpeg`'s DTS decoder lays a packet of a core alone out as: the core's channel
 * arrangement and LFE. `null` when the packet carries more than a core `FFmpeg` would decode
 * (channel extensions, an extension substream, a substream without a core) or is not a 16-bit
 * core; those layouts are not read.
 */
export function dtsSpeakers(packet: Uint8Array): AudioSpeaker[] | null {
  if (packet.length < 16) return null;
  let frame = packet;
  const sync = new DataView(packet.buffer, packet.byteOffset).getUint32(0);
  if (sync === DTS_CORE_SYNC_LE) {
    frame = new Uint8Array(packet.length & ~1);
    for (let at = 0; at < frame.length; at += 2) {
      frame[at] = packet[at + 1];
      frame[at + 1] = packet[at];
    }
  } else if (sync !== DTS_CORE_SYNC) return null;
  try {
    // `ff_dca_parse_core_frame_header`.
    const bits = new Bits(frame, 32);
    bits.skip(1);
    if (bits.read(5) + 1 !== 32) return null;
    bits.skip(1);
    if ((bits.read(7) + 1) & 7) return null;
    const frameSize = bits.read(14) + 1;
    if (frameSize < 96) return null;
    const mode = bits.read(6);
    if (mode >= DTS_MODE_MASKS.length) return null;
    bits.skip(4 + 5);
    if (bits.read(1)) return null;
    bits.skip(4);
    const extensionType = bits.read(3);
    const extension = bits.read(1) === 1;
    bits.skip(1);
    const lfe = bits.read(2);
    if (lfe === 3) return null;
    if (extension && DTS_CHANNEL_EXTENSIONS.has(extensionType)) return null;
    if (
      frame.length >= frameSize + 4 &&
      new DataView(frame.buffer, frame.byteOffset).getUint32(frameSize) ===
        DTS_SUBSTREAM_SYNC
    )
      return null;
    return speakersOfMask(
      DTS_MODE_MASKS[mode] | (lfe ? CHANNEL_BIT.get("LFE")! : 0n),
    );
  } catch {
    return null;
  }
}

/**
 * `strtol(text, NULL, 0)` as a 64-bit two's-complement value: leading space, a sign, then a hex
 * (`0x`), octal (`0`) or decimal number, read up to the first character that is not a digit.
 */
function strtol(text: string): bigint {
  const match =
    /^[\t\n\v\f\r ]*([+-]?)(0[xX](?=[0-9a-fA-F])|0?)([0-9a-fA-F]*)/.exec(text)!;
  const [, sign, prefix, rest] = match;
  const radix = prefix.length === 2 ? 16 : prefix === "0" ? 8 : 10;
  const digits =
    prefix === "0" && rest === "" ? "0" : /^[0-9a-zA-Z]*/.exec(rest)![0];
  let value = 0n;
  for (const digit of digits) {
    const number = Number.parseInt(digit, 16);
    if (Number.isNaN(number) || number >= radix) break;
    value = value * BigInt(radix) + BigInt(number);
  }
  const LONG_MAX = (1n << 63n) - 1n;
  if (sign === "-") value = value > LONG_MAX + 1n ? 1n << 63n : -value;
  else if (value > LONG_MAX) value = LONG_MAX;
  return BigInt.asUintN(64, value);
}

/**
 * The channel mask a FLAC `VORBIS_COMMENT` block's first `WAVEFORMATEXTENSIBLE_CHANNEL_MASK`
 * comment declares, as `FFmpeg`'s FLAC and Matroska demuxers read it, or `null` for no comment or
 * a mask `FFmpeg` refuses (zero, or past its first 18 speakers).
 */
function vorbisCommentMask(block: Uint8Array): bigint | null {
  const view = new DataView(block.buffer, block.byteOffset, block.byteLength);
  const text = new TextDecoder();
  if (block.length < 8) return null;
  let at = 4 + view.getUint32(0, true);
  if (at + 4 > block.length) return null;
  const count = view.getUint32(at, true);
  at += 4;
  for (let index = 0; index < count && at + 4 <= block.length; index += 1) {
    const length = view.getUint32(at, true);
    at += 4;
    if (at + length > block.length) return null;
    const comment = text.decode(block.subarray(at, at + length));
    at += length;
    const equals = comment.indexOf("=");
    if (equals <= 0 || equals === comment.length - 1) continue;
    if (
      comment.slice(0, equals).toUpperCase() !==
      "WAVEFORMATEXTENSIBLE_CHANNEL_MASK"
    )
      continue;
    const mask = strtol(comment.slice(equals + 1));
    return mask === 0n || mask & ~0x3ffffn ? null : mask;
  }
  return null;
}

/**
 * The first FLAC `VORBIS_COMMENT` block among the metadata blocks from `offset`, read through
 * `read`, or `null`.
 */
async function flacVorbisComment(
  read: ByteReader,
  offset: number,
): Promise<Uint8Array | null> {
  let at = offset;
  for (let block = 0; block < 128; block += 1) {
    const header = await read(at, 4);
    if (header.length < 4) return null;
    const length = (header[1] << 16) | (header[2] << 8) | header[3];
    if ((header[0] & 0x7f) === 4) return read(at + 4, length);
    if (header[0] & 0x80) return null;
    at += 4 + length;
  }
  return null;
}

/**
 * The channel mask a FLAC track declares in a `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` comment where
 * `FFmpeg` reads one: a FLAC file's metadata blocks, or Matroska codec private data whose first
 * block is the stream info (`description`). `FFmpeg`'s MP4 and Ogg demuxers read none.
 */
async function flacMask(
  read: ByteReader,
  description: Uint8Array | null,
): Promise<bigint | null> {
  // An ID3v2 tag before the stream is skipped, as `FFmpeg` skips it.
  let start = 0;
  const id3 = await read(0, 10);
  if (id3.length === 10 && boxType(id3, 0).startsWith("ID3"))
    start =
      10 +
      (id3[5] & 0x10 ? 10 : 0) +
      ((id3[6] & 0x7f) << 21) +
      ((id3[7] & 0x7f) << 14) +
      ((id3[8] & 0x7f) << 7) +
      (id3[9] & 0x7f);
  if ((await readTag(read, start, 4)) === "fLaC") {
    const block = await flacVorbisComment(read, start + 4);
    return block === null ? null : vorbisCommentMask(block);
  }
  const head = await read(0, 4);
  const matroska =
    head.length === 4 &&
    new DataView(head.buffer, head.byteOffset).getUint32(0) === 0x1a45dfa3;
  if (!matroska || description === null || description.length < 42) return null;
  if (description[4] & 0x7f) return null;
  const block = await flacVorbisComment(
    (offset, length) =>
      Promise.resolve(description.subarray(offset, offset + length)),
    42,
  );
  return block === null ? null : vorbisCommentMask(block);
}

/** Vorbis's channel orders, and Opus's mapping family 1, as `FFmpeg` reorders them. */
const VORBIS_LAYOUTS = [
  "FC",
  "FL FR",
  "FL FR FC",
  "FL FR BL BR",
  "FL FR FC BL BR",
  "FL FR FC LFE BL BR",
  "FL FR FC LFE BC SL SR",
  "FL FR FC LFE BL BR SL SR",
];
/** FLAC's channel assignments (RFC 9639): `flac_channel_layouts`. */
const FLAC_LAYOUTS = [
  "FC",
  "FL FR",
  "FL FR FC",
  "FL FR BL BR",
  "FL FR FC SL SR",
  "FL FR FC LFE SL SR",
  "FL FR FC LFE BC SL SR",
  "FL FR FC LFE BL BR SL SR",
];
/** The AAC channel configuration of each ADTS channel count (ADTS carries 1 to 7). */
export const ADTS_CONFIGURATIONS: Readonly<Record<number, number>> = {
  1: 1,
  2: 2,
  3: 3,
  4: 4,
  5: 5,
  6: 6,
  8: 7,
};

/**
 * The AAC channel configuration of an `AudioSpecificConfig`. `null` for a truncated config or for
 * configuration 0, which defers to a program config element.
 */
export function aacChannelConfiguration(config: Uint8Array): number | null {
  try {
    const bits = new Bits(config);
    aacObjectType(bits);
    if (bits.read(4) === 15) bits.skip(24);
    const configuration = bits.read(4);
    return configuration === 0 ? null : configuration;
  } catch {
    return null;
  }
}

/** The channel mapping family of an `OpusHead`, 0 when there is none. */
export function opusMappingFamily(description: Uint8Array | null): number {
  return description !== null &&
    description.length >= 19 &&
    boxType(description, 0) + boxType(description, 4) === "OpusHead"
    ? description[18]
    : 0;
}

/**
 * The layout a codec's decoder sets, where the decoder sets one: `undefined` when the codec leaves
 * the layout to the container, `null` when the decoder leaves it unspecified.
 */
function decoderLayout(
  codec: string | null,
  description: Uint8Array | null,
  channels: number,
  packet: Uint8Array | null,
): AudioSpeaker[] | null | undefined {
  const fixed = (layouts: readonly string[]) =>
    layouts[channels - 1] === undefined ? null : list(layouts[channels - 1]);
  switch (codec) {
    case "aac": {
      // Without an `AudioSpecificConfig` the stream is ADTS, whose header names configurations 1
      // to 7 by their distinct channel counts.
      const configuration = ADTS_CONFIGURATIONS[channels];
      const config =
        description ??
        (configuration === undefined
          ? null
          : Uint8Array.of(0x11, 0x80 | (configuration << 3)));
      const speakers = config === null ? undefined : aacSpeakers(config);
      if (speakers === undefined) break;
      if (speakers === null || speakers.length === channels) return speakers;
      return channels <= 2 ? fixed(VORBIS_LAYOUTS) : null;
    }
    case "opus": {
      const family = opusMappingFamily(description);
      if (family === 1) return fixed(VORBIS_LAYOUTS);
      return channels <= 2 && family === 0 ? fixed(VORBIS_LAYOUTS) : null;
    }
    case "vorbis":
      return fixed(VORBIS_LAYOUTS);
    case "flac":
      return fixed(FLAC_LAYOUTS);
    case "ac3":
    case "eac3":
      if (packet === null) break;
      return ac3Speakers(packet);
    case "dts":
      return packet === null ? null : dtsSpeakers(packet);
    default:
      break;
  }
  return channels <= 2 ? fixed(VORBIS_LAYOUTS) : undefined;
}

/**
 * The speakers a codec's own headers declare, in the order `FFmpeg`'s decoder delivers its planes
 * (the native order of the layout it sets): an AAC channel configuration or program config
 * element (an ADTS stream's configuration, without a description), an Opus mapping family 0 or 1
 * head, Vorbis's and FLAC's fixed orders, an AC-3 or E-AC-3 frame's channel mode, LFE and
 * dependent channel map, a DTS core's channel arrangement, and mono or stereo for any codec.
 * `null` when the codec says nothing the decoder would read, or the decoder would leave the layout
 * unspecified.
 *
 * @param codec mediabunny's codec name (`aac`, `opus`, `vorbis`, `flac`, `ac3`, `eac3`, `dts`,
 *   ...).
 * @param description The decoder configuration's description (`AudioSpecificConfig`,
 *   `OpusHead`), when the track has one.
 * @param packet The track's first packet, which an AC-3, E-AC-3 or DTS decoder reads its layout
 *   from.
 *
 * Chromium's decoders, which are `FFmpeg`'s, deliver this order. WebCodecs leaves the order to the
 * decoder, and Safari's AAC decoder keeps the bitstream's; the mediabunny adapter measures the
 * order a decoder delivers and puts its planes back in this one (`decoder-order.ts`).
 */
export function codecSpeakers(
  codec: string | null,
  description: Uint8Array | null,
  channels: number,
  packet: Uint8Array | null = null,
): AudioSpeaker[] | null {
  const speakers = decoderLayout(codec, description, channels, packet);
  return speakers === undefined || speakers === null
    ? null
    : speakers.length === channels
      ? declared(speakers)
      : null;
}

/**
 * The speakers a source's primary audio track declares, or `null` when it declares none.
 *
 * A codec whose decoder sets a layout is laid out as `FFmpeg`'s decoder lays it out
 * ({@link codecSpeakers}), whatever the container says. FLAC's decoder keeps a layout of its
 * channel count from the demuxer: a `WAVEFORMATEXTENSIBLE_CHANNEL_MASK` comment in a FLAC file or
 * Matroska, a QuickTime `chan` or ISO `chnl` box, and FLAC's own order otherwise. PCM, and any
 * other codec, is laid out by its container: a WAVE channel mask, a QuickTime `chan` box, an ISO
 * `chnl` box.
 *
 * @param read Reads the source file's bytes.
 * @param size The source file's size in bytes.
 * @param track The audio track; `id` picks the matching `trak` in a QuickTime file, and the first
 *   track with a `chan` or `chnl` box is used when none matches.
 * @param description The track's decoder configuration description, if any.
 * @param firstPacket Reads the track's first packet, for a codec that declares its layout there
 *   (AC-3, E-AC-3, DTS); resolves `null` when there is none.
 *
 * Rejects only when `read` or `firstPacket` rejects; malformed bytes resolve to `null`.
 */
export async function readAudioSpeakers(
  read: ByteReader,
  size: number,
  track: {
    readonly id: number;
    readonly codec: string | null;
    readonly numberOfChannels: number;
  },
  description: Uint8Array | null,
  firstPacket: () => Promise<Uint8Array | null> = () => Promise.resolve(null),
): Promise<AudioSpeaker[] | null> {
  const channels = track.numberOfChannels;
  const codec = track.codec;
  const pcm =
    codec !== null &&
    (codec.startsWith("pcm-") || codec === "ulaw" || codec === "alaw");
  if (codec === "flac") {
    const mask = await flacMask(read, description);
    const speakers =
      (mask === null ? null : maskSpeakers(mask, channels)) ??
      (await movSpeakers(read, size, track.id, channels));
    if (speakers !== null) return speakers;
  }
  if (!pcm) {
    const packet =
      codec === "ac3" || codec === "eac3" || codec === "dts"
        ? await firstPacket()
        : null;
    const speakers = decoderLayout(codec, description, channels, packet);
    if (speakers !== undefined)
      return speakers === null || speakers.length !== channels
        ? null
        : declared(speakers);
  }
  const fmt = await wavFmt(read);
  if (fmt !== null) return wavSpeakers(fmt, channels);
  return movSpeakers(read, size, track.id, channels);
}
