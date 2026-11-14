//! Peak heap held while a container file is opened, against its reader's per-file budget.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Cursor;

use alpha2_container::{ContainerReader, DemuxError, MatroskaReader, Mp4Reader};

struct Counting;

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn grow(by: usize) {
    let _ = LIVE.try_with(|live| {
        live.set(live.get() + by);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
    });
}

fn shrink(by: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(by)));
}

// SAFETY: every call forwards to `System` with the caller's layout; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `GlobalAlloc::alloc` holds for `System`.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grow(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `System` with this layout.
        unsafe { System.dealloc(pointer, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: `pointer` came from `System` with this layout, and the caller's contract for
        // `GlobalAlloc::realloc` holds for `System`.
        let moved = unsafe { System.realloc(pointer, layout, size) };
        if !moved.is_null() {
            grow(size.saturating_sub(layout.size()));
            shrink(layout.size().saturating_sub(size));
        }
        moved
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The result of `call` and the most heap it held at once beyond what was live before it.
fn peak<T>(call: impl FnOnce() -> T) -> (T, usize) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let result = call();
    (result, PEAK.with(Cell::get) - start)
}

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

/// A video `trak` of `samples` one-byte samples in one chunk at `offset`, with `edits` edits that
/// each show every sample.
fn trak(id: u32, samples: u32, offset: u32, edits: usize) -> Vec<u8> {
    let mut tkhd = be(&[0, 0, id, 0, samples, 0, 0, 0, 0]);
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
    let mut stbl = full(*b"stsd", &stsd);
    stbl.extend_from_slice(&full(*b"stts", &be(&[1, samples, 1])));
    stbl.extend_from_slice(&full(*b"stsc", &be(&[1, 1, samples, 1])));
    stbl.extend_from_slice(&full(*b"stsz", &be(&[1, samples])));
    stbl.extend_from_slice(&full(*b"stco", &be(&[1, offset])));
    let minf = bx(*b"minf", &bx(*b"stbl", &stbl));
    let mut mdia = full(*b"mdhd", &mdhd);
    mdia.extend_from_slice(&full(*b"hdlr", &hdlr));
    mdia.extend_from_slice(&minf);
    let mut trak = full(*b"tkhd", &tkhd);
    if edits > 0 {
        let mut elst = be(&[u32::try_from(edits).expect("edits")]);
        for _ in 0..edits {
            elst.extend_from_slice(&be(&[samples, 0, 0x1_0000]));
        }
        trak.extend_from_slice(&bx(*b"edts", &full(*b"elst", &elst)));
    }
    trak.extend_from_slice(&bx(*b"mdia", &mdia));
    bx(*b"trak", &trak)
}

/// An MP4 file whose tracks, `(samples, edits)` each, index the same bytes of one `mdat`.
fn mp4(tracks: &[(u32, usize)]) -> Vec<u8> {
    let samples = tracks.iter().map(|track| track.0).max().unwrap_or(0);
    let ftyp = bx(*b"ftyp", b"isom\0\0\0\0isom");
    let build = |offset: u32| {
        let mut mvhd = be(&[0, 0, 1000, samples, 0x1_0000, 0x0100_0000, 0, 0]);
        mvhd.extend_from_slice(&be(&IDENTITY));
        let next = u32::try_from(tracks.len() + 1).expect("tracks");
        mvhd.extend_from_slice(&be(&[0, 0, 0, 0, 0, 0, next]));
        let mut moov = full(*b"mvhd", &mvhd);
        for (id, &(count, edits)) in (1..).zip(tracks) {
            moov.extend_from_slice(&trak(id, count, offset, edits));
        }
        bx(*b"moov", &moov)
    };
    let moov_len = build(0).len();
    let offset = u32::try_from(ftyp.len() + moov_len + 8).expect("offset");
    let mut out = ftyp;
    out.extend_from_slice(&build(offset));
    out.extend_from_slice(&(samples + 8).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.resize(out.len() + samples as usize, 0);
    out
}

fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(payload);
    out
}

/// A Matroska file with one PCM track entry followed by `extra` empty ones and, when
/// `cue_points` is not zero, a Cues element of that many cue points.
fn mkv(extra: usize, cue_points: usize) -> Vec<u8> {
    let mut out = el(&[0x1a, 0x45, 0xdf, 0xa3], &el(&[0x42, 0x82], b"matroska"));
    out.extend_from_slice(&[
        0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ]);
    let mut audio = el(&[0xb5], &48_000.0_f64.to_be_bytes());
    audio.extend_from_slice(&el(&[0x9f], &[2]));
    audio.extend_from_slice(&el(&[0x62, 0x64], &[16]));
    let mut entry = el(&[0xd7], &[1]);
    entry.extend_from_slice(&el(&[0x83], &[2]));
    entry.extend_from_slice(&el(&[0x86], b"A_PCM/INT/LIT"));
    entry.extend_from_slice(&el(&[0xe1], &audio));
    let mut tracks = el(&[0xae], &entry);
    for _ in 0..extra {
        tracks.extend_from_slice(&[0xae, 0x80]);
    }
    out.extend_from_slice(&el(&[0x16, 0x54, 0xae, 0x6b], &tracks));
    if cue_points > 0 {
        let point = [0xbb, 0x85, 0xb7, 0x83, 0xf1, 0x81, 0x00];
        out.extend_from_slice(&el(&[0x1c, 0x53, 0xbb, 0x6b], &point.repeat(cue_points)));
    }
    out
}

fn refused(opened: Result<impl ContainerReader, DemuxError>) -> String {
    match opened {
        Ok(_) => panic!("the file opened"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn sixteen_full_mp4_tracks_are_refused_after_the_first() {
    let data = mp4(&[(1 << 23, 0); 16]);
    let (opened, held) = peak(|| Mp4Reader::new(Cursor::new(data.clone())).map(|_| ()));
    let error = opened.expect_err("over the file budget");
    assert!(
        error
            .to_string()
            .contains("more samples than this layer reads"),
        "{error}"
    );
    assert!(held < 1 << 30, "held {held} bytes");
    let one = mp4(&[(1 << 23, 0)]);
    let reader = Mp4Reader::new(Cursor::new(one)).expect("one full track opens");
    assert_eq!(reader.tracks().len(), 1);
}

#[test]
fn edits_that_multiply_an_index_stop_at_the_file_budget() {
    // Eight edits showing every sample would make 2^24 entries; the file holds 2^23.
    let data = mp4(&[(1 << 21, 8)]);
    Mp4Reader::new(Cursor::new(data)).expect("opens with the index the budget holds");
    let crowded = mp4(&[(1 << 21, 8), (1, 0)]);
    let error = refused(Mp4Reader::new(Cursor::new(crowded)));
    assert!(
        error.contains("more samples than this layer reads"),
        "{error}"
    );
}

#[test]
fn more_tracks_than_a_file_may_have_are_refused() {
    Mp4Reader::new(Cursor::new(mp4(&[(1, 0); 256]))).expect("256 tracks open");
    let error = refused(Mp4Reader::new(Cursor::new(mp4(&[(1, 0); 257]))));
    assert!(
        error.contains("more tracks than this layer reads"),
        "{error}"
    );
    MatroskaReader::new(Cursor::new(mkv(255, 0))).expect("256 track entries open");
    let error = refused(MatroskaReader::new(Cursor::new(mkv(256, 0))));
    assert!(
        error.contains("more tracks than this layer reads"),
        "{error}"
    );
}

#[test]
fn millions_of_empty_track_entries_are_refused_within_a_fixed_budget() {
    let data = mkv(1 << 22, 0);
    let (opened, held) = peak(|| MatroskaReader::new(Cursor::new(data.clone())).map(|_| ()));
    assert!(opened.is_err());
    assert!(held < 64 << 20, "held {held} bytes");
}

#[test]
fn millions_of_cue_points_are_read_within_a_fixed_budget() {
    let data = mkv(0, 1 << 22);
    let (opened, held) = peak(|| MatroskaReader::new(Cursor::new(data.clone())).map(|_| ()));
    opened.expect("opens");
    // 2^20 cue positions of 16 bytes, the element itself, and vector growth.
    assert!(held < 128 << 20, "held {held} bytes");
}
