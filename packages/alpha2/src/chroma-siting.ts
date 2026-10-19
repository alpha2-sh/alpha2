// Reads chroma siting and sample shape from a stream's parameter sets as FFmpeg does, so browser
// decodes convert and probe the same as a native decode of the stream.

/**
 * Where a subsampled frame's chroma samples sit relative to its luma.
 *
 * WebCodecs does not report it, so the demuxer, which knows the stream, supplies it. The
 * conversion must place chroma where the encoder did, or every chroma edge lands half a sample
 * off. `unspecified` is converted as centered.
 */
export type ChromaSiting = "left" | "center" | "top-left" | "unspecified";

/**
 * The chroma siting a codec's streams have unless they signal otherwise.
 *
 * H.264, HEVC, and MPEG-4 Part 2 default 4:2:0 chroma to the left (H.273
 * chroma sample location type 0), as FFmpeg reports them natively. Other
 * codecs stay unspecified, which the engine reads as centered, again as
 * natively.
 */
export function chromaSitingForCodec(
  codec: string | null | undefined,
): ChromaSiting {
  switch (codecFamily(codec)) {
    case "avc":
    case "hevc":
    case "mp4v":
      return "left";
    default:
      return "unspecified";
  }
}

/**
 * The chroma siting a stream signals, as FFmpeg's decoder reports it on the
 * frames it decodes.
 *
 * H.264 and HEVC signal it in the sequence parameter set's VUI
 * (`chroma_sample_loc_type_top_field`), AV1 in the sequence header
 * (`chroma_sample_position`). They are read from the decoder configuration's
 * `description` (`avcC`, `hvcC`, `av1C`) or, when it has none (Annex B
 * streams, AV1 in MP4), from the stream's first packet. With neither, or for
 * another codec, the codec's default ({@link chromaSitingForCodec}).
 *
 * The values follow the native decoder, so both hosts convert alike: FFmpeg's
 * reading, except that an H.264 stream with no VUI or a reserved type (above
 * 5) is `left`, as H.264 infers it without one (E.2.1) and the native host
 * reads what FFmpeg leaves unspecified. An H.264 or HEVC type from 3 to 5 is
 * `unspecified`, as are AV1's unknown position and HEVC that is not 4:2:0.
 */
export function chromaSitingForStream(
  codec: string | null | undefined,
  description?: AllowSharedBufferSource | null,
  firstPacket?: AllowSharedBufferSource | null,
): ChromaSiting {
  const family = codecFamily(codec);
  const signalled = (() => {
    try {
      switch (family) {
        case "avc":
          return avcSiting(description, firstPacket);
        case "hevc":
          return hevcSiting(description, firstPacket);
        case "av1":
          return av1Siting(description, firstPacket);
        default:
          return undefined;
      }
    } catch {
      // A truncated or malformed parameter set says nothing a decoder would use.
      return undefined;
    }
  })();
  return signalled ?? chromaSitingForCodec(codec);
}

/** A sample (pixel) aspect ratio: a sample is `num / den` as wide as it is tall. */
export interface SampleAspect {
  readonly num: number;
  readonly den: number;
}

/**
 * The sample aspect ratio an H.264 or HEVC stream signals in its sequence parameter set's VUI
 * (`aspect_ratio_idc`, or `sar_width`/`sar_height` for Extended_SAR), read from the decoder
 * configuration's `description` (`avcC`, `hvcC`) or, without one, from the stream's first packet.
 * `undefined` when the stream signals none (no VUI, `aspect_ratio_idc` 0, a reserved index, or a
 * zero term), for another codec, or when the parameter set does not parse: as FFmpeg's decoder
 * reports it, which a native probe falls back to when the container declares no ratio
 * (`av_guess_sample_aspect_ratio`).
 */
export function sampleAspectForStream(
  codec: string | null | undefined,
  description?: AllowSharedBufferSource | null,
  firstPacket?: AllowSharedBufferSource | null,
): SampleAspect | undefined {
  try {
    switch (codecFamily(codec)) {
      case "avc": {
        const sps = avcSps(description, firstPacket);
        return sps === undefined ? undefined : h264Sps(sps).aspect;
      }
      case "hevc": {
        const sps = hevcSps(description, firstPacket);
        return sps === undefined ? undefined : hevcSpsFacts(sps).aspect;
      }
      default:
        return undefined;
    }
  } catch {
    return undefined;
  }
}

function codecFamily(codec: string | null | undefined): string | undefined {
  const family = codec?.split(".")[0]?.toLowerCase();
  switch (family) {
    case "avc1":
    case "avc3":
      return "avc";
    case "hvc1":
    case "hev1":
      return "hevc";
    case "av01":
      return "av1";
    default:
      return family;
  }
}

function bytesOf(
  source: AllowSharedBufferSource | null | undefined,
): Uint8Array | undefined {
  if (source === undefined || source === null) return undefined;
  if (ArrayBuffer.isView(source)) {
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  }
  return new Uint8Array(source);
}

/**
 * FFmpeg's `AVChromaLocation` for an H.273 chroma sample location type, using the engine's names
 * for the ones it samples.
 */
function sitingOfLocationType(type: number): ChromaSiting {
  switch (type) {
    case 0:
      return "left";
    case 1:
      return "center";
    case 2:
      return "top-left";
    default:
      // Top, bottom-left, and bottom (3 to 5) are sampled centered natively; anything above 5 is
      // reserved.
      return "unspecified";
  }
}

/** An H.264 stream's first SPS NAL unit: its `avcC` record's, or its first packet's. */
function avcSps(
  description: AllowSharedBufferSource | null | undefined,
  firstPacket: AllowSharedBufferSource | null | undefined,
): Uint8Array | undefined {
  return (
    avcConfigurationSps(bytesOf(description)) ??
    packetNalUnits(bytesOf(firstPacket)).find(
      (unit) => unit.length > 0 && (unit[0] & 0x1f) === 7,
    )
  );
}

function avcSiting(
  description: AllowSharedBufferSource | null | undefined,
  firstPacket: AllowSharedBufferSource | null | undefined,
): ChromaSiting | undefined {
  const sps = avcSps(description, firstPacket);
  return sps === undefined ? undefined : h264SpsSiting(sps);
}

/** The first SPS NAL unit of an `avcC` record. */
function avcConfigurationSps(
  record: Uint8Array | undefined,
): Uint8Array | undefined {
  if (record === undefined || record.length < 8 || record[0] !== 1) {
    return undefined;
  }
  const count = record[5] & 0x1f;
  if (count === 0) return undefined;
  const length = (record[6] << 8) | record[7];
  return record.length >= 8 + length
    ? record.subarray(8, 8 + length)
    : undefined;
}

/**
 * The profiles whose SPS carries `chroma_format_idc` and the fields after it, as FFmpeg lists them.
 */
const H264_HIGH_PROFILES = new Set([
  100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 144,
]);

/**
 * `vui.chroma_location` as FFmpeg's H.264 parser derives it from one SPS NAL unit, starting at
 * its one-byte NAL header. A location FFmpeg leaves unspecified is read as H.264 infers it: left.
 *
 * Throws `RangeError` when the SPS ends early.
 */
export function h264SpsSiting(nal: Uint8Array): ChromaSiting {
  return h264Sps(nal).siting;
}

/** What one H.264 SPS NAL unit says of its chroma siting and its sample aspect ratio. */
function h264Sps(nal: Uint8Array): VuiFacts {
  const bits = new BitReader(unescape(nal.subarray(1)));
  const profile = bits.bits(8);
  bits.skip(16); // constraint flags, level_idc
  bits.ue(); // seq_parameter_set_id
  if (H264_HIGH_PROFILES.has(profile)) {
    const chromaFormat = bits.ue();
    if (chromaFormat === 3) bits.skip(1); // separate_colour_plane_flag
    bits.ue(); // bit_depth_luma_minus8
    bits.ue(); // bit_depth_chroma_minus8
    bits.skip(1); // qpprime_y_zero_transform_bypass_flag
    if (bits.flag()) {
      // seq_scaling_matrix_present_flag
      const lists = chromaFormat === 3 ? 12 : 8;
      for (let list = 0; list < lists; list += 1) {
        if (bits.flag()) skipH264ScalingList(bits, list < 6 ? 16 : 64);
      }
    }
  }
  bits.ue(); // log2_max_frame_num_minus4
  const pocType = bits.ue();
  if (pocType === 0) {
    bits.ue(); // log2_max_pic_order_cnt_lsb_minus4
  } else if (pocType === 1) {
    bits.skip(1); // delta_pic_order_always_zero_flag
    bits.se(); // offset_for_non_ref_pic
    bits.se(); // offset_for_top_to_bottom_field
    const cycle = bits.ue();
    for (let index = 0; index < cycle; index += 1) bits.se();
  }
  bits.ue(); // max_num_ref_frames
  bits.skip(1); // gaps_in_frame_num_value_allowed_flag
  bits.ue(); // pic_width_in_mbs_minus1
  bits.ue(); // pic_height_in_map_units_minus1
  if (!bits.flag()) bits.skip(1); // frame_mbs_only_flag, mb_adaptive_frame_field_flag
  bits.skip(1); // direct_8x8_inference_flag
  if (bits.flag()) {
    // frame_cropping_flag
    bits.ue();
    bits.ue();
    bits.ue();
    bits.ue();
  }
  // FFmpeg's SPS starts zeroed, so it reports no chroma location for a stream without VUI, where
  // H.264 infers type 0.
  if (!bits.flag()) return { siting: "left", aspect: undefined };
  return commonVui(bits, "left");
}

function skipH264ScalingList(bits: BitReader, size: number): void {
  let last = 8;
  let next = 8;
  for (let index = 0; index < size; index += 1) {
    if (next !== 0) next = (last + bits.se() + 256) % 256;
    last = next === 0 ? last : next;
  }
}

/** What a sequence parameter set's VUI says: its chroma siting and its sample aspect ratio. */
interface VuiFacts {
  readonly siting: ChromaSiting;
  readonly aspect: SampleAspect | undefined;
}

/**
 * The sample aspect ratios of `aspect_ratio_idc` 1 to 16 (H.264 table E-1, HEVC table E.1), as
 * FFmpeg's `ff_h2645_pixel_aspect` lists them.
 */
const PIXEL_ASPECT: readonly (readonly [number, number])[] = [
  [1, 1],
  [12, 11],
  [10, 11],
  [16, 11],
  [40, 33],
  [24, 11],
  [20, 11],
  [32, 11],
  [80, 33],
  [18, 11],
  [15, 11],
  [64, 33],
  [160, 99],
  [4, 3],
  [3, 2],
  [2, 1],
];

/**
 * `ff_h2645_decode_common_vui_params` up to the chroma location: the sample aspect ratio, and the
 * siting, left unless the VUI says otherwise; a reserved type (above 5) maps to `reserved`.
 */
function commonVui(
  bits: BitReader,
  reserved: ChromaSiting = "unspecified",
): VuiFacts {
  let aspect: SampleAspect | undefined;
  if (bits.flag()) {
    // aspect_ratio_info_present_flag
    const index = bits.bits(8); // aspect_ratio_idc
    const [num, den] =
      index === 255
        ? [bits.bits(16), bits.bits(16)] // Extended_SAR: sar_width, sar_height
        : (PIXEL_ASPECT[index - 1] ?? [0, 0]);
    if (num > 0 && den > 0) aspect = { num, den };
  }
  if (bits.flag()) bits.skip(1); // overscan_info_present_flag, overscan_appropriate_flag
  if (bits.flag()) {
    // video_signal_type_present_flag: video_format, video_full_range_flag
    bits.skip(4);
    if (bits.flag()) bits.skip(24); // colour_description_present_flag and its three codes
  }
  if (!bits.flag()) return { siting: "left", aspect }; // chroma_loc_info_present_flag
  const type = bits.ue();
  return {
    siting: type > 5 ? reserved : sitingOfLocationType(type),
    aspect,
  };
}

/** An HEVC stream's first SPS NAL unit: its `hvcC` record's, or its first packet's. */
function hevcSps(
  description: AllowSharedBufferSource | null | undefined,
  firstPacket: AllowSharedBufferSource | null | undefined,
): Uint8Array | undefined {
  return (
    hevcConfigurationSps(bytesOf(description)) ??
    packetNalUnits(bytesOf(firstPacket)).find(
      (unit) => unit.length > 1 && ((unit[0] >> 1) & 0x3f) === 33,
    )
  );
}

function hevcSiting(
  description: AllowSharedBufferSource | null | undefined,
  firstPacket: AllowSharedBufferSource | null | undefined,
): ChromaSiting | undefined {
  const sps = hevcSps(description, firstPacket);
  return sps === undefined ? undefined : hevcSpsSiting(sps);
}

/** The first SPS NAL unit of an `hvcC` record. */
function hevcConfigurationSps(
  record: Uint8Array | undefined,
): Uint8Array | undefined {
  if (record === undefined || record.length < 23 || record[0] !== 1) {
    return undefined;
  }
  const arrays = record[22];
  let at = 23;
  for (let array = 0; array < arrays; array += 1) {
    if (at + 3 > record.length) return undefined;
    const type = record[at] & 0x3f;
    const count = (record[at + 1] << 8) | record[at + 2];
    at += 3;
    for (let unit = 0; unit < count; unit += 1) {
      if (at + 2 > record.length) return undefined;
      const length = (record[at] << 8) | record[at + 1];
      at += 2;
      if (at + length > record.length) return undefined;
      if (type === 33) return record.subarray(at, at + length);
      at += length;
    }
  }
  return undefined;
}

/**
 * `avctx->chroma_sample_location` as FFmpeg's HEVC decoder sets it from one SPS NAL unit, starting
 * at its two-byte NAL header.
 *
 * Throws `RangeError` when the SPS ends early.
 */
export function hevcSpsSiting(nal: Uint8Array): ChromaSiting {
  return hevcSpsFacts(nal).siting;
}

/** What one HEVC SPS NAL unit says of its chroma siting and its sample aspect ratio. */
function hevcSpsFacts(nal: Uint8Array): VuiFacts {
  const bits = new BitReader(unescape(nal.subarray(2)));
  bits.skip(4); // sps_video_parameter_set_id
  const subLayers = bits.bits(3); // sps_max_sub_layers_minus1
  bits.skip(1); // sps_temporal_id_nesting_flag
  skipProfileTierLevel(bits, subLayers);
  bits.ue(); // sps_seq_parameter_set_id
  const chromaFormat = bits.ue();
  if (chromaFormat === 3) bits.skip(1); // separate_colour_plane_flag
  bits.ue(); // pic_width_in_luma_samples
  bits.ue(); // pic_height_in_luma_samples
  if (bits.flag()) {
    // conformance_window_flag
    bits.ue();
    bits.ue();
    bits.ue();
    bits.ue();
  }
  bits.ue(); // bit_depth_luma_minus8
  bits.ue(); // bit_depth_chroma_minus8
  const pocLsbBits = bits.ue() + 4;
  const orderingInfo = bits.flag();
  for (
    let layer = orderingInfo ? 0 : subLayers;
    layer <= subLayers;
    layer += 1
  ) {
    bits.ue(); // sps_max_dec_pic_buffering_minus1
    bits.ue(); // sps_max_num_reorder_pics
    bits.ue(); // sps_max_latency_increase_plus1
  }
  bits.ue(); // log2_min_luma_coding_block_size_minus3
  bits.ue(); // log2_diff_max_min_luma_coding_block_size
  bits.ue(); // log2_min_luma_transform_block_size_minus2
  bits.ue(); // log2_diff_max_min_luma_transform_block_size
  bits.ue(); // max_transform_hierarchy_depth_inter
  bits.ue(); // max_transform_hierarchy_depth_intra
  // scaling_list_enabled_flag, sps_scaling_list_data_present_flag
  if (bits.flag() && bits.flag()) skipHevcScalingListData(bits);
  bits.skip(2); // amp_enabled_flag, sample_adaptive_offset_enabled_flag
  if (bits.flag()) {
    // pcm_enabled_flag
    bits.skip(8); // pcm sample bit depths
    bits.ue();
    bits.ue();
    bits.skip(1); // pcm_loop_filter_disabled_flag
  }
  skipShortTermRefPicSets(bits, bits.ue());
  if (bits.flag()) {
    // long_term_ref_pics_present_flag
    const count = bits.ue();
    for (let index = 0; index < count; index += 1) bits.skip(pocLsbBits + 1);
  }
  bits.skip(2); // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
  // FFmpeg reports a location only for 4:2:0; without VUI its zeroed flags read as left.
  const vui = bits.flag() ? commonVui(bits) : undefined;
  return {
    siting: chromaFormat !== 1 ? "unspecified" : (vui?.siting ?? "left"),
    aspect: vui?.aspect,
  };
}

function skipProfileTierLevel(bits: BitReader, subLayers: number): void {
  bits.skip(88); // general profile space, tier, idc, compatibility and constraint flags
  bits.skip(8); // general_level_idc
  const profilePresent: boolean[] = [];
  const levelPresent: boolean[] = [];
  for (let layer = 0; layer < subLayers; layer += 1) {
    profilePresent.push(bits.flag());
    levelPresent.push(bits.flag());
  }
  if (subLayers > 0) bits.skip(2 * (8 - subLayers)); // reserved_zero_2bits
  for (let layer = 0; layer < subLayers; layer += 1) {
    if (profilePresent[layer]) bits.skip(88);
    if (levelPresent[layer]) bits.skip(8);
  }
}

function skipHevcScalingListData(bits: BitReader): void {
  for (let size = 0; size < 4; size += 1) {
    for (let matrix = 0; matrix < 6; matrix += size === 3 ? 3 : 1) {
      if (!bits.flag()) {
        bits.ue(); // scaling_list_pred_matrix_id_delta
        continue;
      }
      const coefficients = Math.min(64, 1 << (4 + (size << 1)));
      if (size > 1) bits.se(); // scaling_list_dc_coef_minus8
      for (let index = 0; index < coefficients; index += 1) bits.se();
    }
  }
}

/**
 * Reads `count` `st_ref_pic_set`s. Each may be predicted from the one before, so their delta POCs
 * are kept.
 */
function skipShortTermRefPicSets(bits: BitReader, count: number): void {
  const sets: Array<{ negative: number[]; positive: number[] }> = [];
  for (let index = 0; index < count; index += 1) {
    if (index !== 0 && bits.flag()) {
      // inter_ref_pic_set_prediction_flag: in an SPS the reference is the set before.
      const reference = sets[index - 1];
      const sign = bits.flag() ? -1 : 1;
      const deltaRps = sign * (bits.ue() + 1);
      const total = reference.negative.length + reference.positive.length;
      const useDelta: boolean[] = [];
      for (let entry = 0; entry <= total; entry += 1) {
        const used = bits.flag();
        useDelta.push(used || bits.flag());
      }
      const negativeCount = reference.negative.length;
      const negative: number[] = [];
      const positive: number[] = [];
      for (let entry = reference.positive.length - 1; entry >= 0; entry -= 1) {
        const delta = reference.positive[entry] + deltaRps;
        if (delta < 0 && useDelta[negativeCount + entry]) negative.push(delta);
      }
      if (deltaRps < 0 && useDelta[total]) negative.push(deltaRps);
      for (let entry = 0; entry < negativeCount; entry += 1) {
        const delta = reference.negative[entry] + deltaRps;
        if (delta < 0 && useDelta[entry]) negative.push(delta);
      }
      for (let entry = negativeCount - 1; entry >= 0; entry -= 1) {
        const delta = reference.negative[entry] + deltaRps;
        if (delta > 0 && useDelta[entry]) positive.push(delta);
      }
      if (deltaRps > 0 && useDelta[total]) positive.push(deltaRps);
      for (let entry = 0; entry < reference.positive.length; entry += 1) {
        const delta = reference.positive[entry] + deltaRps;
        if (delta > 0 && useDelta[negativeCount + entry]) positive.push(delta);
      }
      sets.push({ negative, positive });
      continue;
    }
    const negativeCount = bits.ue();
    const positiveCount = bits.ue();
    const negative: number[] = [];
    const positive: number[] = [];
    let poc = 0;
    for (let entry = 0; entry < negativeCount; entry += 1) {
      poc -= bits.ue() + 1;
      bits.skip(1); // used_by_curr_pic_s0_flag
      negative.push(poc);
    }
    poc = 0;
    for (let entry = 0; entry < positiveCount; entry += 1) {
      poc += bits.ue() + 1;
      bits.skip(1); // used_by_curr_pic_s1_flag
      positive.push(poc);
    }
    sets.push({ negative, positive });
  }
}

function av1Siting(
  description: AllowSharedBufferSource | null | undefined,
  firstPacket: AllowSharedBufferSource | null | undefined,
): ChromaSiting | undefined {
  // `av1C` is four bytes of fields, then the configuration OBUs, which may hold the sequence
  // header.
  const record = bytesOf(description);
  const header =
    (record !== undefined && record.length > 4 && (record[0] & 0x7f) === 1
      ? sequenceHeaderObu(record.subarray(4))
      : undefined) ?? sequenceHeaderObu(bytesOf(firstPacket));
  return header === undefined ? undefined : av1SequenceHeaderSiting(header);
}

/** The payload of the first sequence header OBU in a low-overhead bitstream. */
function sequenceHeaderObu(
  data: Uint8Array | undefined,
): Uint8Array | undefined {
  if (data === undefined) return undefined;
  let at = 0;
  while (at < data.length) {
    const header = data[at];
    const type = (header >> 3) & 0x0f;
    const extension = (header & 0x04) !== 0;
    const sized = (header & 0x02) !== 0;
    at += extension ? 2 : 1;
    let size = data.length - at;
    if (sized) {
      size = 0;
      for (let byte = 0; byte < 8; byte += 1) {
        if (at >= data.length) return undefined;
        const value = data[at++];
        size += (value & 0x7f) * 2 ** (7 * byte);
        if ((value & 0x80) === 0) break;
      }
    }
    if (at + size > data.length) return undefined;
    if (type === 1) return data.subarray(at, at + size);
    at += size;
  }
  return undefined;
}

/**
 * `avctx->chroma_sample_location` as FFmpeg's AV1 decoders set it from a sequence header OBU's
 * payload.
 *
 * Throws `RangeError` when the header ends early.
 */
export function av1SequenceHeaderSiting(payload: Uint8Array): ChromaSiting {
  const bits = new BitReader(payload);
  const profile = bits.bits(3);
  bits.skip(1); // still_picture
  const reduced = bits.flag();
  if (reduced) {
    bits.skip(5); // seq_level_idx[0]
  } else {
    let decoderModel = false;
    let bufferDelayBits = 0;
    if (bits.flag()) {
      // timing_info_present_flag
      bits.skip(64); // num_units_in_display_tick, time_scale
      if (bits.flag()) bits.uvlc(); // equal_picture_interval, num_ticks_per_picture_minus_1
      decoderModel = bits.flag();
      if (decoderModel) {
        bufferDelayBits = bits.bits(5) + 1;
        bits.skip(32); // num_units_in_decoding_tick
        // buffer_removal_time_length_minus_1, frame_presentation_time_length_minus_1
        bits.skip(10);
      }
    }
    const initialDisplayDelay = bits.flag();
    const operatingPoints = bits.bits(5) + 1;
    for (let point = 0; point < operatingPoints; point += 1) {
      bits.skip(12); // operating_point_idc
      if (bits.bits(5) > 7) bits.skip(1); // seq_level_idx, seq_tier
      if (decoderModel && bits.flag()) bits.skip(2 * bufferDelayBits + 1);
      if (initialDisplayDelay && bits.flag()) bits.skip(4);
    }
  }
  const widthBits = bits.bits(4) + 1;
  const heightBits = bits.bits(4) + 1;
  bits.skip(widthBits + heightBits); // max_frame_width_minus_1, max_frame_height_minus_1
  if (!reduced && bits.flag()) bits.skip(7); // frame_id_numbers_present_flag and its lengths
  bits.skip(3); // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
  if (!reduced) {
    bits.skip(4); // interintra, masked compound, warped motion, dual filter
    const orderHint = bits.flag();
    if (orderHint) bits.skip(2); // enable_jnt_comp, enable_ref_frame_mvs
    const forceScreenContent = bits.flag() ? 2 : bits.bits(1);
    // seq_choose_integer_mv, seq_force_integer_mv
    if (forceScreenContent > 0 && !bits.flag()) bits.skip(1);
    if (orderHint) bits.skip(3); // order_hint_bits_minus_1
  }
  bits.skip(3); // enable_superres, enable_cdef, enable_restoration
  // color_config
  const highBitDepth = bits.flag();
  const twelveBit = profile === 2 && highBitDepth ? bits.flag() : false;
  const monochrome = profile === 1 ? false : bits.flag();
  let primaries = 2;
  let transfer = 2;
  let matrix = 2;
  if (bits.flag()) {
    primaries = bits.bits(8);
    transfer = bits.bits(8);
    matrix = bits.bits(8);
  }
  if (monochrome) return "unspecified";
  if (primaries === 1 && transfer === 13 && matrix === 0) return "unspecified"; // sRGB 4:4:4
  bits.skip(1); // color_range
  let subsampledX = true;
  let subsampledY = true;
  if (profile === 1) {
    subsampledX = false;
    subsampledY = false;
  } else if (profile === 2) {
    if (twelveBit) {
      subsampledX = bits.flag();
      subsampledY = subsampledX ? bits.flag() : false;
    } else {
      subsampledY = false;
    }
  }
  if (!(subsampledX && subsampledY)) return "unspecified";
  switch (bits.bits(2)) {
    case 1:
      return "left"; // CSP_VERTICAL
    case 2:
      return "top-left"; // CSP_COLOCATED
    default:
      return "unspecified";
  }
}

/** NAL units of a packet, Annex B (start codes) or four-byte length-prefixed. */
function packetNalUnits(packet: Uint8Array | undefined): Uint8Array[] {
  if (packet === undefined || packet.length < 4) return [];
  const units: Uint8Array[] = [];
  if (
    packet[0] === 0 &&
    packet[1] === 0 &&
    (packet[2] === 1 || (packet[2] === 0 && packet[3] === 1))
  ) {
    let start = -1;
    for (let at = 0; at + 2 < packet.length; at += 1) {
      if (packet[at] === 0 && packet[at + 1] === 0 && packet[at + 2] === 1) {
        if (start >= 0) {
          let end = at;
          while (end > start && packet[end - 1] === 0) end -= 1;
          units.push(packet.subarray(start, end));
        }
        start = at + 3;
        at += 2;
      }
    }
    if (start >= 0) units.push(packet.subarray(start));
    return units;
  }
  let at = 0;
  while (at + 4 <= packet.length) {
    const length =
      ((packet[at] << 24) >>> 0) +
      (packet[at + 1] << 16) +
      (packet[at + 2] << 8) +
      packet[at + 3];
    at += 4;
    if (length === 0 || at + length > packet.length) break;
    units.push(packet.subarray(at, at + length));
    at += length;
  }
  return units;
}

/** RBSP of a NAL unit payload: emulation prevention bytes removed. */
function unescape(payload: Uint8Array): Uint8Array {
  const output = new Uint8Array(payload.length);
  let length = 0;
  let zeros = 0;
  for (const byte of payload) {
    if (zeros >= 2 && byte === 3) {
      zeros = 0;
      continue;
    }
    output[length++] = byte;
    zeros = byte === 0 ? zeros + 1 : 0;
  }
  return output.subarray(0, length);
}

class BitReader {
  readonly #data: Uint8Array;
  #position = 0;

  constructor(data: Uint8Array) {
    this.#data = data;
  }

  bit(): number {
    const byte = this.#data[this.#position >> 3];
    if (byte === undefined) throw new RangeError("parameter set ends early");
    const value = (byte >> (7 - (this.#position & 7))) & 1;
    this.#position += 1;
    return value;
  }

  flag(): boolean {
    return this.bit() === 1;
  }

  bits(count: number): number {
    let value = 0;
    for (let index = 0; index < count; index += 1) {
      value = value * 2 + this.bit();
    }
    return value;
  }

  skip(count: number): void {
    if ((this.#position + count) >> 3 > this.#data.length) {
      throw new RangeError("parameter set ends early");
    }
    this.#position += count;
  }

  /** Exp-Golomb unsigned. */
  ue(): number {
    let zeros = 0;
    while (this.bit() === 0) {
      zeros += 1;
      if (zeros > 31) throw new RangeError("Exp-Golomb code is too long");
    }
    return 2 ** zeros - 1 + this.bits(zeros);
  }

  /** Exp-Golomb signed. */
  se(): number {
    const code = this.ue();
    return code % 2 === 1 ? (code + 1) / 2 : -code / 2;
  }

  /** AV1 `uvlc()`. */
  uvlc(): number {
    let zeros = 0;
    while (this.bit() === 0) {
      zeros += 1;
      if (zeros >= 32) return 2 ** 32 - 1;
    }
    return 2 ** zeros - 1 + this.bits(zeros);
  }
}
