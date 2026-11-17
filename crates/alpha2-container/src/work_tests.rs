//! Open, read and seek work grows with the file, not with the product of two of its counts.

use std::io::Cursor;

use crate::{ContainerReader, MatroskaReader, Mp4Reader};

fn bx(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(payload.len() + 8)
        .expect("box")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&kind);
    out.extend_from_slice(payload);
    out
}

fn full(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut payload = vec![0; 4];
    payload.extend_from_slice(body);
    bx(kind, &payload)
}

fn be(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

const IDENTITY: [u32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];

/// An MP4 file of one video track of `samples` one-byte samples whose `stss` lists only the
/// first, with `edits` edits that each show the last sample: every edit walks back to the start.
fn walking_edits(samples: u32, edits: usize) -> Vec<u8> {
    let mut tkhd = be(&[0, 0, 1, 0, samples, 0, 0, 0, 0]);
    tkhd.extend_from_slice(&be(&IDENTITY));
    tkhd.extend_from_slice(&be(&[64 << 16, 48 << 16]));
    let mut mdhd = be(&[0, 0, 1000, samples]);
    mdhd.extend_from_slice(&[0x55, 0xc4, 0, 0]);
    let mut hdlr = be(&[0]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0; 13]);
    let mut entry = vec![0; 78];
    entry[7] = 1;
    entry[24..26].copy_from_slice(&64_u16.to_be_bytes());
    entry[26..28].copy_from_slice(&48_u16.to_be_bytes());
    let mut stsd = be(&[1]);
    stsd.extend_from_slice(&bx(*b"mp4v", &entry));
    let build = |offset: u32| {
        let mut stbl = full(*b"stsd", &stsd);
        stbl.extend_from_slice(&full(*b"stts", &be(&[1, samples, 1])));
        stbl.extend_from_slice(&full(*b"stss", &be(&[1, 1])));
        stbl.extend_from_slice(&full(*b"stsc", &be(&[1, 1, samples, 1])));
        stbl.extend_from_slice(&full(*b"stsz", &be(&[1, samples])));
        stbl.extend_from_slice(&full(*b"stco", &be(&[1, offset])));
        let mut mdia = full(*b"mdhd", &mdhd);
        mdia.extend_from_slice(&full(*b"hdlr", &hdlr));
        mdia.extend_from_slice(&bx(*b"minf", &bx(*b"stbl", &stbl)));
        let mut elst = be(&[u32::try_from(edits).expect("edits")]);
        for _ in 0..edits {
            elst.extend_from_slice(&be(&[1, samples - 1, 0x1_0000]));
        }
        let mut trak = full(*b"tkhd", &tkhd);
        trak.extend_from_slice(&bx(*b"edts", &full(*b"elst", &elst)));
        trak.extend_from_slice(&bx(*b"mdia", &mdia));
        let mut movie_header = be(&[0, 0, 1000, samples, 0x1_0000, 0x0100_0000, 0, 0]);
        movie_header.extend_from_slice(&be(&IDENTITY));
        movie_header.extend_from_slice(&be(&[0, 0, 0, 0, 0, 0, 2]));
        let mut moov = full(*b"mvhd", &movie_header);
        moov.extend_from_slice(&bx(*b"trak", &trak));
        bx(*b"moov", &moov)
    };
    let ftyp = bx(*b"ftyp", b"isom\0\0\0\0isom");
    let offset = u32::try_from(ftyp.len() + build(0).len() + 8).expect("offset");
    let mut out = ftyp;
    out.extend_from_slice(&build(offset));
    out.extend_from_slice(&(samples + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.resize(out.len() + samples as usize, 0);
    out
}

#[test]
fn an_edit_list_is_applied_in_time_linear_in_the_file() {
    // Samples and edits grow together; walking back per edit would be quadratic.
    let file = |edits: usize| walking_edits(u32::try_from(edits * 64).expect("samples"), edits);
    // The index stops growing at eight times the samples, and the walks stop with it.
    let reader = Mp4Reader::new(Cursor::new(file(1_600))).expect("opens");
    assert_eq!(reader.tracks().len(), 1);
    alpha2_scaling_support::assert_linear_shared("edit list", 200, file, |data| {
        let _ = std::hint::black_box(Mp4Reader::new(Cursor::new(data.clone())));
    });
}

fn flac_crc8(bytes: &[u8]) -> u8 {
    super::audio::crc::crc8(bytes)
}

/// A FLAC file of `size` bytes whose audio is a valid-looking frame header every 8 bytes, none
/// continuing another (every frame number is 0) or closing a CRC-16.
fn flac_candidates(size: usize) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    let mut info = [0_u8; 34];
    info[0..2].copy_from_slice(&192_u16.to_be_bytes());
    info[2..4].copy_from_slice(&192_u16.to_be_bytes());
    // 44100 Hz, one channel, 16 bits, unknown length and frame sizes.
    let packed: u64 = (44_100_u64 << 44) | (15 << 36);
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&[0x80, 0, 0, 34]);
    out.extend_from_slice(&info);
    let mut header = vec![0xff, 0xf8, 0x19, 0x08, 0x00];
    header.push(flac_crc8(&header));
    header.push(0x00);
    let mut crc = 0_u16;
    while out.len() + 8 <= size {
        // A filler byte keeps the running CRC-16 at every header away from zero.
        let mut filler = 0x01_u8;
        loop {
            let mut unit = header.clone();
            unit.push(filler);
            let next = super::audio::crc::crc16(crc, &unit);
            if next != 0 {
                crc = next;
                out.extend_from_slice(&unit);
                break;
            }
            filler += 1;
        }
    }
    out
}

#[test]
fn a_flac_frame_end_is_found_in_time_linear_in_the_span() {
    alpha2_scaling_support::assert_linear_shared(
        "FLAC first packet",
        64 << 10,
        flac_candidates,
        |data| {
            let mut reader =
                crate::open_source(Box::new(Cursor::new(data.clone()))).expect("opens");
            let track = reader.tracks()[0].id;
            let _ = std::hint::black_box(reader.next_packet(track));
        },
    );
}

fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(payload);
    out
}

/// A Matroska file with one VP9 track and `clusters` clusters of one block each, none a keyframe.
fn keyframeless(clusters: u32) -> Vec<u8> {
    let mut out = el(&[0x1a, 0x45, 0xdf, 0xa3], &el(&[0x42, 0x82], b"matroska"));
    out.extend_from_slice(&[
        0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ]);
    let mut entry = el(&[0xd7], &[1]);
    entry.extend_from_slice(&el(&[0x83], &[1]));
    entry.extend_from_slice(&el(&[0x86], b"V_VP9"));
    let mut video = el(&[0xb0], &[64]);
    video.extend_from_slice(&el(&[0xba], &[48]));
    entry.extend_from_slice(&el(&[0xe0], &video));
    out.extend_from_slice(&el(&[0x16, 0x54, 0xae, 0x6b], &el(&[0xae], &entry)));
    for index in 0..clusters {
        let mut cluster = el(&[0xe7], &index.to_be_bytes());
        cluster.extend_from_slice(&el(&[0xa3], &[0x81, 0, 0, 0, 0x55]));
        out.extend_from_slice(&el(&[0x1f, 0x43, 0xb6, 0x75], &cluster));
    }
    out
}

#[test]
fn a_seek_without_keyframes_reads_the_file_at_most_once() {
    let opened = |clusters: usize| {
        let clusters = u32::try_from(clusters).expect("clusters");
        let mut reader = MatroskaReader::new(Cursor::new(keyframeless(clusters))).expect("opens");
        let track = reader.tracks()[0].id;
        while reader.next_packet(track).expect("packet").is_some() {}
        (reader, track, i64::from(clusters))
    };
    alpha2_scaling_support::assert_linear(
        "keyframeless seek",
        2_000,
        opened,
        |(reader, track, end)| {
            let _ = std::hint::black_box(reader.seek(*track, *end));
        },
    );
}
