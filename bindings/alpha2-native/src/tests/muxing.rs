//! Muxing: sample descriptions, in-band parameter sets, and deferred headers.

use super::*;

/// The sample entry an MP4 holds HEVC under: an encode writes `hvc1`, as
/// the browser exporter and Apple's devices do, not `FFmpeg`'s default
/// `hev1`, and a copy keeps the entry its source has.
#[test]
fn hevc_is_written_as_hvc1_and_a_copy_keeps_its_entry() {
    let tag = |name: &[u8; 4]| u32::from_le_bytes(*name);
    let hevc = ffi::AV_CODEC_ID_HEVC;
    assert_eq!(
        crate::convert::container_codec_tag(alpha2_media::Container::Mp4, hevc, 0, false),
        tag(b"hvc1")
    );
    assert_eq!(
        crate::convert::container_codec_tag(alpha2_media::Container::Mp4, hevc, 0, true),
        tag(b"hvc1")
    );
    assert_eq!(
        crate::convert::container_codec_tag(
            alpha2_media::Container::Mp4,
            ffi::AV_CODEC_ID_H264,
            tag(b"avc1"),
            true
        ),
        0
    );
    assert_eq!(
        crate::convert::container_codec_tag(
            alpha2_media::Container::Mp4,
            ffi::AV_CODEC_ID_MPEG4,
            7,
            false
        ),
        7
    );
    for (source, entry) in [("hevc-topleft.mp4", b"hev1"), ("hevc-hvc1.mp4", b"hvc1")] {
        let mut input = crate::decode::open_input(&corpus(source)).expect("input");
        let (parameters, time_base) = {
            let stream = &input.streams()[0];
            (stream.codecpar().clone(), stream.time_base)
        };
        let path = output_path(&format!("copy-{source}"));
        let _ = fs::remove_file(&path);
        let mut muxer = crate::mux::Muxer::open_streams(
            &path,
            alpha2_media::Container::Mp4,
            crate::mux::VideoStream {
                parameters,
                time_base,
                copied: true,
            },
            None,
        )
        .expect("muxer");
        while let Some(mut packet) = input.read_packet().expect("packet") {
            muxer.write_video(&mut packet, time_base).expect("write");
        }
        muxer.close().expect("close");
        let written = crate::decode::open_input(&path).expect("output");
        assert_eq!(
            written.streams()[0].codecpar().codec_tag,
            tag(entry),
            "{source}"
        );
        drop(written);
        fs::remove_file(path).expect("cleanup");
    }
}

/// BT.601 primaries and matrix, the sRGB curve and full range: no field of
/// it is what `h264-bt709-copy.mp4`'s sequence parameter set says, so a
/// stream that decodes as this was rewritten.
const BT601_FULL_SRGB: ColorMetadata = ColorMetadata {
    primaries: alpha2_media::ColorPrimaries::Bt601,
    transfer: alpha2_media::TransferFunction::Srgb,
    matrix: ColorMatrix::Bt601,
    range: ColorRange::Full,
    dynamic_range: alpha2_media::DynamicRange::Sdr,
};

/// What `FFmpeg`'s D3D12 Video encoders hand over, made from the video of a
/// checked-in MP4: the packets in Annex B with the parameter sets in-band
/// before every keyframe (`*_mp4toannexb`, then `dump_extra`), and an
/// encoder context that describes the stream, carries no extradata and is
/// tagged `color` the way an export opens its encoder. Returns the context,
/// the packets and their time base.
fn in_band_encoder_output(
    name: &str,
    color: ColorMetadata,
) -> (
    AVCodecContext,
    Vec<rsmpeg::avcodec::AVPacket>,
    rsmpeg::avutil::AVRational,
) {
    let mut input = crate::decode::open_input(&corpus(name)).expect(name);
    let index = input
        .streams()
        .iter()
        .position(|stream| stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_VIDEO)
        .expect("a video stream");
    let (parameters, time_base) = {
        let stream = &input.streams()[index];
        (stream.codecpar().clone(), stream.time_base)
    };
    let hevc = parameters.codec_id == ffi::AV_CODEC_ID_HEVC;
    let spec = if hevc {
        c"hevc_mp4toannexb,dump_extra"
    } else {
        c"h264_mp4toannexb,dump_extra"
    };
    let mut annex_b = alpha2_ffmpeg_bridge::bitstream_filter_list(spec, &parameters, time_base)
        .expect("Annex B filters");
    let mut packets = Vec::new();
    let drain = |filter: &mut rsmpeg::avcodec::AVBSFContext,
                 packets: &mut Vec<rsmpeg::avcodec::AVPacket>| {
        loop {
            let mut packet = rsmpeg::avcodec::AVPacket::new();
            if filter.receive_packet(&mut packet).is_err() {
                break;
            }
            packets.push(packet);
        }
    };
    while let Some(mut packet) = input.read_packet().expect("packet") {
        if usize::try_from(packet.stream_index).ok() == Some(index) {
            annex_b.send_packet(Some(&mut packet)).expect("send");
            drain(&mut annex_b, &mut packets);
        }
    }
    annex_b.send_packet(None).expect("flush");
    drain(&mut annex_b, &mut packets);
    let parameter_set = |nal: u8| {
        if hevc {
            (32..=34).contains(&nal)
        } else {
            matches!(nal, 7 | 8)
        }
    };
    assert!(
        parameter_set_count(&packets, hevc, parameter_set) > 0,
        "{name}: the simulated encoder writes its parameter sets in-band"
    );
    let codec = AVCodec::find_decoder(parameters.codec_id).expect("codec");
    let mut encoder = AVCodecContext::new(&codec);
    encoder.set_width(parameters.width);
    encoder.set_height(parameters.height);
    encoder.set_pix_fmt(parameters.format);
    encoder.set_time_base(time_base);
    alpha2_ffmpeg_bridge::open_codec_context(&mut encoder, crate::convert::color_options(color))
        .expect("context");
    assert!(
        alpha2_ffmpeg_bridge::codec_parameters_extradata(&encoder.extract_codecpar()).is_empty(),
        "{name}: the simulated encoder has no extradata"
    );
    (encoder, packets, time_base)
}

/// How many of the Annex B `packets`' NAL units `parameter_set` takes for a
/// parameter set.
fn parameter_set_count(
    packets: &[rsmpeg::avcodec::AVPacket],
    hevc: bool,
    parameter_set: impl Fn(u8) -> bool,
) -> usize {
    packets
        .iter()
        .map(|packet| {
            let data = alpha2_ffmpeg_bridge::packet_bytes(packet);
            data.windows(4)
                .filter(|window| window[..3] == [0, 0, 1])
                .filter(|window| parameter_set(nal_type(hevc, window[3])))
                .count()
        })
        .sum()
}

/// The type of the NAL unit whose header starts with `header`.
fn nal_type(hevc: bool, header: u8) -> u8 {
    if hevc {
        (header >> 1) & 0x3f
    } else {
        header & 0x1f
    }
}

/// The types of the NAL units an `avcC` or `hvcC` sample description holds.
fn sample_description_nal_types(hevc: bool, record: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let length = |at: usize| usize::from(u16::from_be_bytes([record[at], record[at + 1]]));
    if hevc {
        let mut at = 23;
        for _ in 0..record[22] {
            let count = length(at + 1);
            at += 3;
            for _ in 0..count {
                types.push(nal_type(true, record[at + 2]));
                at += 2 + length(at);
            }
        }
    } else {
        let mut at = 6;
        for _ in 0..record[5] & 0x1f {
            types.push(nal_type(false, record[at + 2]));
            at += 2 + length(at);
        }
        for _ in 0..record[at] {
            at += 1;
            types.push(nal_type(false, record[at + 2]));
            at += 1 + length(at);
        }
    }
    types
}

/// Every picture `path`'s video decodes to, as a hash of its samples, and
/// the colour each one says its sequence header declares. The container's
/// colour tags are cleared before decoding, so only the bitstream speaks.
fn decoded_pictures(
    path: &Path,
) -> Vec<(
    u64,
    (
        ffi::AVColorPrimaries,
        ffi::AVColorTransferCharacteristic,
        ffi::AVColorSpace,
        ffi::AVColorRange,
    ),
)> {
    use std::hash::{Hash, Hasher};
    let mut input = crate::decode::open_input(path).expect("input");
    let index = input
        .streams()
        .iter()
        .position(|stream| stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_VIDEO)
        .expect("a video stream");
    let (parameters, time_base) = {
        let stream = &input.streams()[index];
        (stream.codecpar().clone(), stream.time_base)
    };
    let codec = AVCodec::find_decoder(parameters.codec_id).expect("decoder");
    let mut decoder = AVCodecContext::new(&codec);
    decoder.apply_codecpar(&parameters).expect("parameters");
    decoder.set_pkt_timebase(time_base);
    let untagged =
        AVDictionary::new_int(c"color_primaries", i64::from(ffi::AVCOL_PRI_UNSPECIFIED), 0)
            .set_int(c"color_trc", i64::from(ffi::AVCOL_TRC_UNSPECIFIED), 0)
            .set_int(c"colorspace", i64::from(ffi::AVCOL_SPC_UNSPECIFIED), 0)
            .set_int(c"color_range", i64::from(ffi::AVCOL_RANGE_UNSPECIFIED), 0)
            .set_int(c"threads", 1, 0);
    alpha2_ffmpeg_bridge::open_codec_context(&mut decoder, untagged).expect("open decoder");
    let mut pictures = Vec::new();
    let mut receive = |decoder: &mut AVCodecContext| {
        while let Ok(frame) = decoder.receive_frame() {
            let planes = alpha2_ffmpeg_bridge::yuv_planes(&frame)
                .expect("planes")
                .expect("a YCbCr picture");
            let mut hasher = std::hash::DefaultHasher::new();
            let sample = planes.layout.bytes_per_sample();
            for plane in &planes.planes {
                let row = usize::try_from(plane.width * sample).expect("row");
                for chunk in plane
                    .bytes
                    .chunks(usize::try_from(plane.stride).expect("stride"))
                {
                    chunk[..row].hash(&mut hasher);
                }
            }
            pictures.push((
                hasher.finish(),
                (
                    frame.color_primaries,
                    frame.color_trc,
                    frame.colorspace,
                    frame.color_range,
                ),
            ));
        }
    };
    while let Some(packet) = input.read_packet().expect("packet") {
        if usize::try_from(packet.stream_index).ok() == Some(index) {
            decoder.send_packet(Some(&packet)).expect("decode");
            receive(&mut decoder);
        }
    }
    decoder.send_packet(None).expect("flush");
    receive(&mut decoder);
    pictures
}

/// Muxes an in-band encoder's output (`in_band_encoder_output`) through
/// [`crate::bitstream::EncodedVideoFilter`] and a deferred header, with
/// `audio` samples of a 48 kHz tone: `audio_first` of them before the first
/// video packet, the rest a frame's worth at a time beside the video.
fn mux_in_band(name: &str, color: ColorMetadata, audio: usize, audio_first: usize) -> PathBuf {
    let (encoder, packets, time_base) = in_band_encoder_output(name, color);
    let path = output_path(&format!("deferred-{name}"));
    let _ = fs::remove_file(&path);
    let mut filter =
        crate::bitstream::EncodedVideoFilter::new(&encoder.extract_codecpar(), color, time_base)
            .expect("filter");
    let mut muxer = crate::mux::Muxer::open_deferred(
        &path,
        alpha2_media::Container::Mp4,
        &encoder,
        time_base,
        (audio > 0).then_some(crate::AudioStreamConfig {
            codec: crate::NativeAudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
            bit_rate: 128_000,
        }),
    )
    .expect("muxer");
    assert!(
        partial_beside(&path).exists(),
        "the partial file exists from the open"
    );
    let mut stages = crate::EncodeStageTimings::default();
    let mut written = 0_usize;
    let mut write_audio = |muxer: &mut crate::mux::Muxer, until: usize| {
        while written < until.min(audio) {
            let count = (until.min(audio) - written).min(1_600);
            let block = tone_block(i64::try_from(written).expect("small"), count, 48_000);
            muxer.write_audio(&block, &mut stages).expect("audio");
            written += count;
        }
    };
    write_audio(&mut muxer, audio_first);
    let write = |muxer: &mut crate::mux::Muxer, packets: Vec<rsmpeg::avcodec::AVPacket>| {
        for mut packet in packets {
            muxer.write_video(&mut packet, time_base).expect("video");
        }
    };
    for (frame, packet) in packets.into_iter().enumerate() {
        write(&mut muxer, filter.filter(packet).expect("filter"));
        write_audio(&mut muxer, audio_first + (frame + 1) * 1_600);
    }
    write(&mut muxer, filter.flush().expect("flush"));
    write_audio(&mut muxer, audio);
    muxer.finish_audio().expect("finish audio");
    muxer
        .write_ready_audio(true, &mut stages)
        .expect("write audio");
    muxer.close().expect("close");
    path
}

/// Checks a deferred-header export of `name` tagged `color`: the parameter
/// sets are in the sample description and in no packet, the sequence
/// header declares `color` where the source declared something else, and
/// every picture decodes exactly as the source's does.
fn assert_in_band_stream_is_repaired(name: &str, color: ColorMetadata) {
    let path = mux_in_band(name, color, 0, 0);
    let mut input = crate::decode::open_input(&path).expect("output");
    let parameters = input.streams()[0].codecpar().clone();
    let hevc = parameters.codec_id == ffi::AV_CODEC_ID_HEVC;
    let codec = if hevc {
        NativeVideoCodec::Hevc
    } else {
        NativeVideoCodec::H264
    };
    let record = alpha2_ffmpeg_bridge::codec_parameters_extradata(&parameters);
    assert_eq!(
        record.first(),
        Some(&1),
        "{name}: an MP4 sample description"
    );
    let mut described = sample_description_nal_types(hevc, &record);
    described.sort_unstable();
    described.dedup();
    let expected: &[u8] = if hevc { &[32, 33, 34] } else { &[7, 8] };
    assert_eq!(described, expected, "{name}: the sample description");
    let length_size = crate::passthrough::nal_length_size(codec, &record).expect("length size");
    let mut packets = 0;
    while let Some(packet) = input.read_packet().expect("packet") {
        let data = alpha2_ffmpeg_bridge::packet_bytes(&packet);
        let mut at = 0;
        while at + length_size < data.len() {
            let length = data[at..at + length_size]
                .iter()
                .fold(0_usize, |value, byte| (value << 8) | usize::from(*byte));
            let nal = nal_type(hevc, data[at + length_size]);
            assert!(
                !expected.contains(&nal),
                "{name}: packet {packets} carries NAL type {nal}"
            );
            at += length_size + length;
        }
        assert!(
            !alpha2_ffmpeg_bridge::packet_has_side_data(&packet, ffi::AV_PKT_DATA_NEW_EXTRADATA),
            "{name}: packet {packets} changes the sample description"
        );
        packets += 1;
    }
    drop(input);
    let source = decoded_pictures(&corpus(name));
    let written = decoded_pictures(&path);
    let tags = crate::convert::color_tags(color);
    assert!(
        source.iter().all(|(_, declared)| *declared != tags),
        "{name}: the source already declares the colour asked for"
    );
    assert!(
        written.iter().all(|(_, declared)| *declared == tags),
        "{name}: {:?}",
        written.first()
    );
    assert_eq!(written.len(), source.len(), "{name}: pictures");
    for (index, ((written, _), (source, _))) in written.iter().zip(&source).enumerate() {
        assert_eq!(written, source, "{name}: picture {index}");
    }
    fs::remove_file(path).expect("cleanup");
}

/// `FFmpeg`'s D3D12 Video encoders write their parameter sets in-band and no
/// extradata, and no colour description; the export's bitstream filters and
/// deferred header leave an H.264 MP4 a decoder starts from its sample
/// description, tagged in its SPS, with the pictures unchanged.
#[test]
fn an_h264_stream_with_in_band_parameter_sets_muxes_with_them_in_its_sample_description() {
    assert_in_band_stream_is_repaired("h264-bt709-copy.mp4", BT601_FULL_SRGB);
}

/// The same for HEVC, whose `hvc1` sample entry forbids in-band parameter
/// sets outright.
#[test]
fn an_hevc_stream_with_in_band_parameter_sets_muxes_with_them_in_its_sample_description() {
    assert_in_band_stream_is_repaired("hevc-topleft.mp4", PQ_BT2020);
}

/// Audio that is ready before the encoder's first packet waits for the
/// deferred header, then muxes exactly as it does behind a header written at
/// open: the same packets at the same times, interleaved with the video.
#[test]
fn audio_before_a_deferred_header_waits_and_is_interleaved() {
    let samples = 96_000;
    let path = mux_in_band(
        "h264-bt709-copy.mp4",
        ColorMetadata::EXPORT,
        samples,
        24_000,
    );
    let reference = export_muxed_audio("deferred-audio-reference", 48_000, samples);
    let audio_packets = |path: &Path| {
        let mut input = crate::decode::open_input(path).expect("input");
        let index = input
            .streams()
            .iter()
            .position(|stream| stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_AUDIO)
            .expect("an audio stream");
        let mut order = Vec::new();
        let mut packets = Vec::new();
        while let Some(packet) = input.read_packet().expect("packet") {
            let audio = usize::try_from(packet.stream_index).ok() == Some(index);
            order.push(audio);
            if audio {
                packets.push((
                    packet.pts,
                    packet.duration,
                    alpha2_ffmpeg_bridge::packet_bytes(&packet),
                ));
            }
        }
        (packets, order)
    };
    let (written, order) = audio_packets(&path);
    let (expected, _) = audio_packets(&reference);
    assert_eq!(exported_audio(&path), exported_audio(&reference));
    assert_eq!(written.len(), expected.len(), "audio packets");
    assert!(
        written == expected,
        "the audio packets differ from a header written at open"
    );
    let switches = order.windows(2).filter(|pair| pair[0] != pair[1]).count();
    assert!(
        switches >= 2,
        "audio and video are interleaved, not one after the other ({switches} switches)"
    );
    fs::remove_file(path).expect("cleanup");
    fs::remove_file(reference).expect("cleanup");
}

/// A deferred header leaves nothing behind when the export ends before the
/// encoder's first packet: an abort removes the partial file, and a close
/// with no video packet is an error that leaves no output either.
#[test]
fn a_deferred_header_that_never_comes_leaves_no_file() {
    let (encoder, _, time_base) =
        in_band_encoder_output("h264-bt709-copy.mp4", ColorMetadata::EXPORT);
    let audio = Some(crate::AudioStreamConfig {
        codec: crate::NativeAudioCodec::Aac,
        sample_rate: 48_000,
        channels: 2,
        bit_rate: 128_000,
    });
    let mut stages = crate::EncodeStageTimings::default();
    for close in [false, true] {
        let path = output_path(&format!("deferred-no-video-{close}"));
        let _ = fs::remove_file(&path);
        let mut muxer = crate::mux::Muxer::open_deferred(
            &path,
            alpha2_media::Container::Mp4,
            &encoder,
            time_base,
            audio,
        )
        .expect("muxer");
        assert!(
            partial_beside(&path).exists(),
            "the partial file exists from the open"
        );
        muxer
            .write_audio(&tone_block(0, 4_800, 48_000), &mut stages)
            .expect("audio");
        if close {
            muxer.finish_audio().expect("finish audio");
            muxer
                .write_ready_audio(true, &mut stages)
                .expect("audio waits");
            let error = muxer.close().expect_err("no video packet");
            assert!(
                matches!(&error, EncodeError::Adapter(message) if message.contains("the encoder produced no parameter sets")),
                "{error:?}"
            );
            drop(muxer);
        } else {
            muxer.abort();
            assert!(
                !partial_beside(&path).exists(),
                "the abort removed the partial file"
            );
            drop(muxer);
        }
        assert!(!partial_beside(&path).exists());
        assert!(!path.exists());
    }
}

/// A copy reads NAL units by their length prefix, which only MP4's sample
/// description gives: an Annex B stream is refused, HEVC's prefix width is
/// read from its `hvcC`, and only IDR pictures (not CRA) start a run.
#[test]
fn a_copy_frames_nal_units_only_as_mp4_describes_them() {
    use crate::passthrough::{is_idr, nal_length_size};
    let mut hvcc = vec![1_u8; 23];
    hvcc[21] = 0b1111_1101; // lengthSizeMinusOne = 1
    assert_eq!(nal_length_size(NativeVideoCodec::Hevc, &hvcc), Ok(2));
    let avcc = [1_u8, 100, 0, 10, 0xff, 0xe1, 0];
    assert_eq!(nal_length_size(NativeVideoCodec::H264, &avcc), Ok(4));
    let annex_b = [0_u8, 0, 0, 1, 0x67, 100, 0, 10];
    for codec in [NativeVideoCodec::H264, NativeVideoCodec::Hevc] {
        let reason = nal_length_size(codec, &annex_b).expect_err("Annex B");
        assert!(reason.contains("not in MP4 form"), "{reason}");
    }
    assert!(nal_length_size(NativeVideoCodec::Mpeg4Part2, &avcc).is_err());
    // HEVC NAL types sit in bits 1-6 of the first header byte: 19 and 20 are IDR, 21 is CRA.
    let unit = |kind: u8| [0_u8, 3, kind << 1, 1, 0xaa];
    assert!(is_idr(NativeVideoCodec::Hevc, 2, &unit(19)));
    assert!(is_idr(NativeVideoCodec::Hevc, 2, &unit(20)));
    assert!(!is_idr(NativeVideoCodec::Hevc, 2, &unit(21)));
    // An IDR slice after an access unit delimiter in the same packet still counts.
    let h264 = [0_u8, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 2, 0x65, 0x88];
    assert!(is_idr(NativeVideoCodec::H264, 4, &h264));
    assert!(!is_idr(NativeVideoCodec::H264, 4, &h264[..6]));
}
