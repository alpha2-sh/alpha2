//! Peak heap held while reading a Lottie document or dotLottie package, as a multiple of its size.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]
#![cfg(feature = "lottie")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use alpha2_extensions::{ExtensionError, LottieProvider, NoResources, VisualProvider};
use alpha2_model::{MAX_LOTTIE_DOCUMENT_BYTES, ProviderSource};

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

fn peak_ratio<T>(bytes: &[u8], call: impl FnOnce() -> T) -> (T, f64) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let result = call();
    let peak = PEAK.with(Cell::get) - start;
    #[allow(clippy::cast_precision_loss)]
    (result, peak as f64 / bytes.len() as f64)
}

/// About `bytes` of one path's points, close to the worst parsed size per byte.
fn points_animation(bytes: usize) -> Arc<[u8]> {
    use std::fmt::Write as _;
    let mut points = String::from("[");
    let mut count = 0_u32;
    while points.len() < bytes / 3 {
        if count > 0 {
            points.push(',');
        }
        write!(
            points,
            "[{},{}]",
            f64::from(count) % 997.25,
            f64::from(count) % 991.5
        )
        .expect("write");
        count += 1;
    }
    points.push(']');
    let zeros = format!("[{}]", vec!["[0,0]"; count as usize].join(","));
    format!(
        r#"{{"v":"5.7.0","w":1920,"h":1080,"fr":30,"ip":0,"op":30,"layers":[{{"ty":4,"ind":1,"ip":0,"op":30,"st":0,"ks":{{"a":{{"a":0,"k":[0,0]}},"p":{{"a":0,"k":[0,0]}},"s":{{"a":0,"k":[100,100]}},"r":{{"a":0,"k":0}},"o":{{"a":0,"k":100}}}},"shapes":[{{"ty":"sh","ks":{{"a":0,"k":{{"c":true,"v":{points},"i":{zeros},"o":{zeros}}}}}}},{{"ty":"fl","c":{{"a":0,"k":[1,0,0,1]}},"o":{{"a":0,"k":100}}}}]}}]}}"#
    )
    .into_bytes()
    .into()
}

#[test]
fn a_lottie_animation_opens_holding_one_parse_of_it_at_a_time() {
    let bytes = points_animation(2 << 20);
    let provider = LottieProvider;
    let (probed, probe) = peak_ratio(&bytes, || provider.probe(&bytes));
    assert!(matches!(probed, Some(Ok(_))), "{probed:?}");
    let (opened, open) = peak_ratio(&bytes, || {
        provider
            .open(
                Arc::clone(&bytes),
                &ProviderSource::Lottie { animation: None },
                &NoResources,
            )
            .map(drop)
    });
    opened.expect("opens");
    eprintln!("probe {probe:.1}, open {open:.1} times the file");
    // velato's parse is about 25 times the file, plus 1 for the text.
    assert!(probe < 26.0, "the probe held {probe:.1} times the file");
    assert!(open < 30.0, "the open held {open:.1} times the file");
}

#[test]
fn a_lottie_file_past_its_limit_is_refused_holding_nothing() {
    let mut text = br#"{"v":"5.7.0","w":64,"h":64,"fr":30,"ip":0,"op":30,"layers":[]}"#.to_vec();
    text.resize(MAX_LOTTIE_DOCUMENT_BYTES + 1, b' ');
    let bytes: Arc<[u8]> = text.into();
    let provider = LottieProvider;
    let (probed, probe) = peak_ratio(&bytes, || provider.probe(&bytes));
    assert!(
        matches!(probed, Some(Err(ExtensionError::Budget(_)))),
        "{probed:?}"
    );
    let (opened, open) = peak_ratio(&bytes, || {
        provider
            .open(
                Arc::clone(&bytes),
                &ProviderSource::Lottie { animation: None },
                &NoResources,
            )
            .map(drop)
    });
    assert!(
        matches!(opened, Err(ExtensionError::Budget(_))),
        "{opened:?}"
    );
    assert!(
        probe < 0.01 && open < 0.01,
        "held {probe} and {open} times the file"
    );
}

/// Uses a ZIP64 end record when `files` exceeds the 16-bit count.
fn empty_files(files: u32) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut directory = Vec::new();
    for index in 0..files {
        let name = format!("f{index}");
        let offset = u32::try_from(archive.len()).expect("small");
        let name_length = u16::try_from(name.len()).expect("short");
        // Local header: signature, version 20, no flags, stored, time, date, CRC 0, sizes 0.
        archive.extend_from_slice(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        archive.extend_from_slice(&name_length.to_le_bytes());
        archive.extend_from_slice(&[0, 0]);
        archive.extend_from_slice(name.as_bytes());
        // Central header: as the local one, then comment, disk, attributes, and the offset.
        directory
            .extend_from_slice(b"PK\x01\x02\x14\0\x14\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        directory.extend_from_slice(&name_length.to_le_bytes());
        directory.extend_from_slice(&[0; 12]);
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let start = archive.len() as u64;
    let size = directory.len() as u64;
    archive.extend_from_slice(&directory);
    let count = u16::try_from(files).unwrap_or(u16::MAX);
    if count == u16::MAX {
        let end64 = archive.len() as u64;
        archive.extend_from_slice(b"PK\x06\x06");
        archive.extend_from_slice(&44_u64.to_le_bytes());
        archive.extend_from_slice(&[45, 0, 45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        archive.extend_from_slice(&u64::from(files).to_le_bytes());
        archive.extend_from_slice(&u64::from(files).to_le_bytes());
        archive.extend_from_slice(&size.to_le_bytes());
        archive.extend_from_slice(&start.to_le_bytes());
        archive.extend_from_slice(b"PK\x06\x07\0\0\0\0");
        archive.extend_from_slice(&end64.to_le_bytes());
        archive.extend_from_slice(&1_u32.to_le_bytes());
    }
    archive.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    archive.extend_from_slice(&count.to_le_bytes());
    archive.extend_from_slice(&count.to_le_bytes());
    archive.extend_from_slice(&u32::try_from(size).unwrap_or(u32::MAX).to_le_bytes());
    let start = if count == u16::MAX {
        u32::MAX
    } else {
        u32::try_from(start).expect("small")
    };
    archive.extend_from_slice(&start.to_le_bytes());
    archive.extend_from_slice(&[0, 0]);
    archive
}

#[test]
fn a_dotlottie_package_of_too_many_files_is_refused_before_its_directory_is_read() {
    let provider = LottieProvider;
    for files in [2_000, 300_000] {
        let bytes: Arc<[u8]> = empty_files(files).into();
        let (probed, probe) = peak_ratio(&bytes, || provider.probe(&bytes));
        assert!(
            matches!(&probed, Some(Err(ExtensionError::Budget(message))) if message.contains("files")),
            "{files}: {probed:?}"
        );
        let (opened, open) = peak_ratio(&bytes, || {
            provider
                .open(
                    Arc::clone(&bytes),
                    &ProviderSource::Lottie { animation: None },
                    &NoResources,
                )
                .map(drop)
        });
        assert!(
            matches!(&opened, Err(ExtensionError::Budget(message)) if message.contains("files")),
            "{files}: {opened:?}"
        );
        assert!(
            probe < 0.01 && open < 0.01,
            "{files}: held {probe} and {open} times the package"
        );
    }
}
