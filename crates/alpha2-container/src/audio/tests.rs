use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use alpha2_media::{Speaker, SpeakerLayout};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::*;
use crate::{AudioCodec, AudioTrack, PcmFormat};

const AUDIO_EXTENSIONS: [&str; 5] = ["wav", "aac", "flac", "ogg", "mp3"];

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn expectations() -> serde_json::Map<String, Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/container-audio-expected.json");
    let text = std::fs::read_to_string(path).expect("expectations");
    let mut json: Value = serde_json::from_str(&text).expect("expectations JSON");
    json["files"].as_object_mut().expect("files").clone()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn codec_name(codec: AudioCodec) -> String {
    match codec {
        AudioCodec::Pcm(format) => {
            let kind = if format.float {
                "f"
            } else if format.signed {
                "s"
            } else {
                "u"
            };
            let order = match (format.bits, format.big_endian) {
                (8, _) => "",
                (_, true) => "be",
                (_, false) => "le",
            };
            format!("pcm_{kind}{}{order}", format.bits)
        }
        AudioCodec::Aac => "aac".into(),
        AudioCodec::Mp3 => "mp3".into(),
        AudioCodec::Opus => "opus".into(),
        AudioCodec::Vorbis => "vorbis".into(),
        AudioCodec::Flac => "flac".into(),
        other => format!("{other:?}"),
    }
}

fn read_all(reader: &mut dyn ContainerReader) -> Vec<Packet> {
    let mut packets = Vec::new();
    while let Some(packet) = reader.next_packet(TRACK).expect("packet") {
        packets.push(packet);
    }
    packets
}

/// A packet as the expectations spell it.
fn row(packet: &Packet) -> Value {
    serde_json::json!([
        packet.pts,
        packet.dts,
        packet.duration,
        packet.keyframe,
        packet.data.len(),
        sha256(&packet.data),
        packet.discard.0,
        packet.discard.1,
    ])
}

fn rows_hash(rows: &[Value]) -> String {
    let mut hash = Sha256::new();
    for row in rows {
        let fields = row.as_array().expect("row");
        let key = u8::from(fields[3].as_bool().expect("keyframe"));
        let line = format!(
            "{} {} {} {key} {} {} {} {}\n",
            fields[0],
            fields[1],
            fields[2],
            fields[4],
            fields[5].as_str().expect("hash"),
            fields[6],
            fields[7],
        );
        hash.update(line.as_bytes());
    }
    hex(&hash.finalize())
}

fn names(layout: &SpeakerLayout) -> Vec<String> {
    layout
        .speakers()
        .iter()
        .map(|speaker| speaker.name().to_owned())
        .collect()
}

fn audio_corpus_files() -> Vec<String> {
    let mut files = std::fs::read_dir(corpus())
        .expect("corpus")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let extension = name.rsplit_once('.')?.1;
            AUDIO_EXTENSIONS.contains(&extension).then_some(name)
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

#[test]
fn every_audio_corpus_file_has_expectations() {
    let expected = expectations();
    let files = audio_corpus_files();
    assert!(files.len() >= 20, "{files:?}");
    for name in &files {
        assert!(
            expected.contains_key(name),
            "{name} has no FFmpeg expectations"
        );
    }
}

#[test]
fn audio_files_demux_like_ffmpeg() {
    for (name, want) in expectations() {
        let mut reader =
            crate::open(&corpus().join(&name)).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(reader.tracks().len(), 1, "{name}");
        let track = reader.tracks()[0].clone();
        let audio = track.audio.clone().expect("audio facts");
        let codec = codec_name(audio.codec);
        assert_eq!(codec, want["codec"], "{name}: codec");
        assert_eq!(
            audio.sample_rate, want["sample_rate"],
            "{name}: sample rate"
        );
        assert_eq!(audio.channels, want["channels"], "{name}: channels");
        assert_eq!(track.timescale, want["timescale"], "{name}: timescale");
        assert_eq!(track.default, want["default"], "{name}: default");
        assert_eq!(
            track.title.as_deref(),
            want["title"].as_str(),
            "{name}: title"
        );
        let language = want["language"]
            .as_str()
            .filter(|language| *language != "und");
        assert_eq!(track.language.as_deref(), language, "{name}: language");
        if !want["bits_per_sample"].is_null() {
            assert_eq!(
                audio.bits_per_sample,
                want["bits_per_sample"]
                    .as_u64()
                    .and_then(|bits| u16::try_from(bits).ok()),
                "{name}: bits"
            );
        }

        let packets = read_all(reader.as_mut());
        let rows = packets.iter().map(row).collect::<Vec<_>>();
        let spelled = want["packets"].as_array().expect("packets");
        for (index, (ours, theirs)) in rows.iter().zip(spelled).enumerate() {
            assert_eq!(ours, theirs, "{name}: packet {index}");
        }
        assert_eq!(rows.len(), want["packet_count"], "{name}: packet count");
        assert_eq!(
            rows_hash(&rows),
            want["packets_sha256"],
            "{name}: packet stream"
        );

        if codec == "aac" {
            // FFmpeg estimates an ADTS stream's duration from the bit rate of the frames it
            // probes; the reader counts frames.
            let frames = i64::try_from(packets.len()).expect("count");
            assert_eq!(
                track.duration,
                Some(frames * packets[0].duration),
                "{name}: duration"
            );
        } else {
            assert_eq!(
                track.duration,
                want["duration"].as_i64(),
                "{name}: duration"
            );
        }

        // FFmpeg signals MP3 gapless playback only through the first packet's skip samples.
        let priming = if codec == "mp3" {
            &spelled[0][6]
        } else {
            &want["initial_padding"]
        };
        assert_eq!(audio.priming, *priming, "{name}: priming");

        check_setup(&name, &codec, &audio, &want);
    }
}

/// Checks the codec setup data and speakers against what `FFmpeg` reports. The codec
/// configuration, not the container, declares AAC and MP3 layouts.
fn check_setup(name: &str, codec: &str, audio: &AudioTrack, want: &Value) {
    match codec {
        "vorbis" | "opus" => {
            assert_eq!(
                sha256(&audio.codec_private),
                want["extradata_sha256"],
                "{name}: private"
            );
        }
        "flac" => {
            // FFmpeg keeps STREAMINFO alone; the reader keeps `fLaC` and the metadata blocks.
            assert_eq!(&audio.codec_private[..4], b"fLaC", "{name}");
            assert_eq!(audio.codec_private[4] & 0x7f, 0, "{name}: STREAMINFO first");
            assert_eq!(
                sha256(&audio.codec_private[8..42]),
                want["extradata_sha256"],
                "{name}: private"
            );
        }
        _ => {}
    }

    let layout = want["layout"].as_array().map(|layout| {
        layout
            .iter()
            .map(|speaker| speaker.as_str().expect("speaker").to_owned())
            .collect::<Vec<_>>()
    });
    match (codec, layout) {
        // Decoders of these output FFmpeg's channel order; the container declares their own.
        ("vorbis" | "opus", Some(layout)) => {
            let ours = names(&audio.speakers.expect("speakers"));
            assert_eq!(
                ours.iter().collect::<BTreeSet<_>>(),
                layout.iter().collect(),
                "{name}: speakers"
            );
        }
        ("aac" | "mp3", _) | (_, None) => assert_eq!(audio.speakers, None, "{name}"),
        (_, Some(layout)) => {
            let channels = usize::try_from(audio.channels).expect("channels");
            let ours = audio
                .speakers
                .or_else(|| SpeakerLayout::default_for(channels));
            assert_eq!(names(&ours.expect("layout")), layout, "{name}: speakers");
        }
    }
}

#[test]
fn seeks_land_on_the_latest_packet_at_or_before_the_target() {
    for name in audio_corpus_files() {
        let path = corpus().join(&name);
        let mut reader = crate::open(&path).expect("open");
        let packets = read_all(reader.as_mut());
        let mut targets = vec![packets[0].pts - 1000, i64::MAX / 4];
        let step = (packets.len() / 6).max(1);
        for packet in packets.iter().step_by(step) {
            targets.extend([packet.pts, packet.pts + 1, packet.pts + packet.duration - 1]);
        }
        for target in targets {
            let index = packets
                .iter()
                .rposition(|packet| packet.pts <= target)
                .unwrap_or(0);
            let found = reader.seek(TRACK, target).expect("seek");
            assert_eq!(found, Some(packets[index].pts), "{name}: seek to {target}");
            for expected in &packets[index..packets.len().min(index + 2)] {
                let packet = reader
                    .next_packet(TRACK)
                    .expect("packet")
                    .expect("a packet");
                assert_eq!(
                    &packet, expected,
                    "{name}: packet after seeking to {target}"
                );
            }
        }
        assert!(matches!(
            reader.seek(TrackId(9), 0),
            Err(DemuxError::NoSuchTrack(9))
        ));
        assert!(matches!(
            reader.next_packet(TrackId(9)),
            Err(DemuxError::NoSuchTrack(9))
        ));
    }
}

#[test]
fn truncated_and_damaged_audio_files_fail_cleanly() {
    for name in audio_corpus_files() {
        let data = std::fs::read(corpus().join(&name)).expect("read");
        let mut cuts = (1..12)
            .map(|part| data.len() * part / 12)
            .collect::<Vec<_>>();
        cuts.extend([4, 12, 44, 100]);
        for cut in cuts {
            let mut damaged = data[..cut.min(data.len())].to_vec();
            exercise(damaged.clone());
            if damaged.len() > 64 {
                let middle = damaged.len() / 2;
                damaged[middle..middle + 16].fill(0xff);
                exercise(damaged);
            }
        }
    }
}

/// Reads and seeks through `data` without caring whether it parses.
fn exercise(data: Vec<u8>) {
    let Ok(mut reader) = open_bytes(data) else {
        return;
    };
    for _ in 0..10_000 {
        match reader.next_packet(TRACK) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let _ = reader.seek(TRACK, 1_000);
    let _ = reader.next_packet(TRACK);
}

#[test]
fn adts_streams_carry_raw_frames_and_an_audio_specific_config() {
    let mut reader = crate::open(&corpus().join("audio-adts.aac")).expect("open");
    let audio = reader.tracks()[0].audio.clone().expect("audio");
    // AAC LC, 48 kHz (index 3), mono.
    assert_eq!(&audio.codec_private[..], [0x11, 0x88]);
    let packet = reader
        .next_packet(TRACK)
        .expect("packet")
        .expect("a packet");
    assert_ne!(&packet.data[..2], [0xff, 0xf1]);
}

#[test]
fn adts_after_an_id3v2_tag_is_read_as_adts() {
    let mut data = b"ID3\x04\x00\x00\x00\x00\x00\x05hello".to_vec();
    data.extend(std::fs::read(corpus().join("audio-adts.aac")).expect("read"));
    let mut reader = open_bytes(data).expect("open");
    assert_eq!(
        reader.tracks()[0].audio.as_ref().expect("audio").codec,
        AudioCodec::Aac
    );
    assert_eq!(read_all(reader.as_mut()).len(), 20);
}

#[test]
fn mp3_streams_skip_junk_before_the_first_frame() {
    let original = std::fs::read(corpus().join("audio-mp3-mpeg2-id3v1.mp3")).expect("read");
    let mut data = b"ID3\x03\x00\x00\x00\x00\x00\x00".to_vec();
    data.extend([0x00, 0x12, 0x34, 0xff, 0x00]);
    data.extend(&original);
    let mut reader = open_bytes(data).expect("open");
    let packets = read_all(reader.as_mut());
    let mut plain = open_bytes(original).expect("open");
    let expected = read_all(plain.as_mut());
    assert_eq!(packets, expected);
}

/// A WAVE file with an extensible `fmt ` chunk, a padded odd chunk and `data`.
fn wave(bits: u16, valid: u16, mask: u32, channels: u16, data: &[u8]) -> Vec<u8> {
    let align = channels * bits / 8;
    let mut fmt = Vec::new();
    fmt.extend(0xfffe_u16.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend(48_000_u32.to_le_bytes());
    fmt.extend((48_000 * u32::from(align)).to_le_bytes());
    fmt.extend(align.to_le_bytes());
    fmt.extend(bits.to_le_bytes());
    fmt.extend(22_u16.to_le_bytes());
    fmt.extend(valid.to_le_bytes());
    fmt.extend(mask.to_le_bytes());
    fmt.extend(1_u32.to_le_bytes());
    fmt.extend([
        0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ]);
    let mut body = b"WAVE".to_vec();
    for (tag, chunk) in [
        (b"fmt ", fmt),
        (b"junk", vec![1, 2, 3]),
        (b"data", data.to_vec()),
    ] {
        body.extend(tag);
        body.extend(u32::try_from(chunk.len()).expect("size").to_le_bytes());
        body.extend(&chunk);
        if chunk.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut file = b"RIFF".to_vec();
    file.extend(u32::try_from(body.len()).expect("size").to_le_bytes());
    file.extend(body);
    file
}

#[test]
fn wave_extensible_formats_declare_their_samples_and_speakers() {
    let data = (0..=255_u8).cycle().take(6 * 1000).collect::<Vec<_>>();
    let mut reader = open_bytes(wave(24, 20, 0x60f, 2, &data)).expect("open");
    let audio = reader.tracks()[0].audio.clone().expect("audio");
    assert_eq!(
        audio.codec,
        AudioCodec::Pcm(PcmFormat {
            bits: 24,
            float: false,
            big_endian: false,
            signed: true,
        })
    );
    // A mask naming more speakers than channels is ignored.
    assert_eq!(audio.speakers, None);
    assert_eq!(reader.tracks()[0].duration, Some(1000));
    let packets = read_all(reader.as_mut());
    assert_eq!(
        packets
            .iter()
            .map(|packet| packet.data.len())
            .sum::<usize>(),
        data.len()
    );

    let reader = open_bytes(wave(32, 0, 0x3, 2, &data[..8 * 10])).expect("open");
    let audio = reader.tracks()[0].audio.clone().expect("audio");
    assert_eq!(
        audio.speakers.map(|layout| names(&layout)),
        Some(vec!["FL".to_owned(), "FR".to_owned()])
    );

    assert!(matches!(
        open_bytes(wave(12, 0, 0x4, 1, &data[..30])),
        Err(DemuxError::Unsupported(_))
    ));
}

#[test]
fn a_wave_data_chunk_longer_than_the_file_reads_to_its_end() {
    let mut file = wave(16, 0, 0x4, 1, &[1, 2, 3, 4, 5, 6]);
    let size_at = file.len() - 6 - 4;
    file[size_at..size_at + 4].copy_from_slice(&1000_u32.to_le_bytes());
    let mut reader = open_bytes(file).expect("open");
    let packets = read_all(reader.as_mut());
    assert_eq!(packets.len(), 1);
    assert_eq!(&packets[0].data[..], [1, 2, 3, 4, 5, 6]);
    assert_eq!(packets[0].duration, 3);
}

#[test]
fn ogg_pages_with_a_bad_crc_are_skipped() {
    let mut data = std::fs::read(corpus().join("audio-opus-pages.ogg")).expect("read");
    let mut good = open_bytes(data.clone()).expect("open");
    let all = read_all(good.as_mut());
    // Damage the body of the fourth page (the second audio page).
    let fourth = data
        .windows(4)
        .enumerate()
        .filter(|(_, bytes)| *bytes == b"OggS")
        .nth(3)
        .expect("page")
        .0;
    data[fourth + 40] ^= 0x55;
    let mut reader = open_bytes(data).expect("open");
    let packets = read_all(reader.as_mut());
    assert_eq!(packets.len(), all.len() - 3);
    assert_eq!(packets.last(), all.last());
}

#[test]
fn speaker_helpers_follow_the_wave_mask_and_vorbis_order() {
    let layout = speakers_from_mask(0x2_d60f, 10).expect("5.1.4");
    assert_eq!(layout.speakers()[6], Speaker::TopFrontLeft);
    assert_eq!(speakers_from_mask(0x7, 2), None);
    let six = vorbis_order(6).expect("5.1");
    assert_eq!(names(&six), ["FL", "FC", "FR", "BL", "BR", "LFE"]);
    assert_eq!(vorbis_order(9), None);
}

#[test]
fn rescaling_rounds_like_ffmpeg() {
    assert_eq!(rescale(3, 1, 2), 2);
    assert_eq!(rescale(-3, 1, 2), -2);
    assert_eq!(rescale_down(-3, 1, 2), -2);
    assert_eq!(rescale_down(5, 1, 2), 2);
}

#[test]
fn flac_frame_numbers_of_three_utf8_bytes_decode() {
    // 4096-sample mono 16-bit frame at 44.1 kHz, frame 2650 (0xa5a), then a constant subframe.
    let mut header = vec![0xff, 0xf8, 0xc9, 0x08, 0xe0, 0xa9, 0x9a];
    header.push(crc::crc8(&header));
    header.push(0x00);
    let decoded = flac::frame_header(&header).expect("a valid header");
    assert_eq!(decoded.number, 2650);
    assert_eq!(decoded.blocksize, 4096);
    assert_eq!(decoded.sample_rate, 44_100);
    header[7] ^= 1;
    assert!(flac::frame_header(&header).is_none());
}
