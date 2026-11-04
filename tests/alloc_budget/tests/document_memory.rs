//! Peak heap held while reading a Lottie document, dotLottie package, or PDF page, as a multiple
//! of its size or against a fixed bound.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]
#![cfg(any(feature = "lottie", feature = "pdf"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

#[cfg(feature = "lottie")]
use alpha2_extensions::LottieProvider;
use alpha2_extensions::{ExtensionError, NoResources, VisualProvider};
#[cfg(feature = "lottie")]
use alpha2_model::MAX_LOTTIE_DOCUMENT_BYTES;
use alpha2_model::ProviderSource;

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

#[cfg(feature = "lottie")]
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

#[cfg(feature = "lottie")]
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

#[cfg(feature = "lottie")]
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

#[cfg(feature = "lottie")]
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

#[cfg(feature = "lottie")]
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

/// A one-page PDF of `content` and `extra` objects numbered from 5, its page 60 by 30 points.
#[cfg(feature = "pdf")]
fn pdf(resources: &str, content: &[u8], extra: &[Vec<u8>]) -> Arc<[u8]> {
    use std::io::Write as _;
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 60 30] /Resources {resources} /Contents 4 0 R >>")
            .into_bytes(),
        pdf_stream("", content),
    ];
    objects.extend_from_slice(extra);
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        writeln!(out, "{} 0 obj", index + 1).expect("write");
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).expect("write");
    for offset in offsets {
        writeln!(out, "{offset:010} 00000 n ").expect("write");
    }
    let size = objects.len() + 1;
    write!(
        out,
        "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    )
    .expect("write");
    out.into()
}

#[cfg(feature = "pdf")]
fn pdf_stream(entries: &str, data: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {entries} /Length {} >>\nstream\n", data.len()).into_bytes();
    object.extend_from_slice(data);
    object.extend_from_slice(b"\nendstream");
    object
}

#[cfg(feature = "pdf")]
fn zlib(chunk: &[u8], times: usize) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    for _ in 0..times {
        encoder.write_all(chunk).expect("compress");
    }
    encoder.finish().expect("compressed")
}

#[cfg(feature = "pdf")]
fn open_pdf(bytes: &Arc<[u8]>) -> Result<(), ExtensionError> {
    alpha2_extensions::PdfProvider
        .open(
            Arc::clone(bytes),
            &ProviderSource::Pdf {
                page: 1,
                background: alpha2_model::Rgba::new(1.0, 1.0, 1.0, 1.0),
            },
            &NoResources,
        )
        .map(drop)
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_page_whose_streams_or_images_decode_past_the_limit_is_refused_holding_little() {
    // 300 MiB of spaces in a Form, and a 30000 by 30000 image of 100 Flate bytes.
    let form = [
        b"<< /Type /XObject /Subtype /Form /BBox [0 0 60 30] /Filter /FlateDecode ".as_slice(),
        &pdf_stream("", &zlib(&vec![b' '; 1 << 20], 300))[3..],
    ]
    .concat();
    let image = [
        b"<< /Type /XObject /Subtype /Image /Width 30000 /Height 30000 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode ".as_slice(),
        &pdf_stream("", &zlib(&[0; 4096], 1))[3..],
    ]
    .concat();
    for (what, bytes) in [
        (
            "form",
            pdf("<< /XObject << /X 5 0 R >> >>", b"/X Do", &[form]),
        ),
        (
            "image",
            pdf(
                "<< /XObject << /X 5 0 R >> >>",
                b"q 60 0 0 30 0 0 cm /X Do Q",
                &[image],
            ),
        ),
    ] {
        let (opened, held) = peak_ratio(&bytes, || open_pdf(&bytes));
        assert!(
            matches!(opened, Err(ExtensionError::Budget(_))),
            "{what}: {opened:?}"
        );
        #[allow(clippy::cast_precision_loss)]
        let held = held * bytes.len() as f64;
        eprintln!("{what}: held {held:.0} bytes");
        // The scan decodes in 64 KiB pieces and keeps only content streams; hayro reads the file.
        assert!(
            held < 16.0 * 1024.0 * 1024.0,
            "{what}: held {held:.0} bytes"
        );
    }
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_page_of_one_photo_opens_holding_a_few_times_its_pixels() {
    // 1200 by 900 RGB samples that compress poorly, as a photo's do.
    let mut samples = Vec::with_capacity(1200 * 900 * 3);
    let mut state = 0x2545_f491_u32;
    for _ in 0..1200 * 900 * 3 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        samples.push(u8::try_from(state >> 24).expect("byte"));
    }
    let image = [
        b"<< /Type /XObject /Subtype /Image /Width 1200 /Height 900 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode ".as_slice(),
        &pdf_stream("", &zlib(&samples, 1))[3..],
    ]
    .concat();
    let bytes = pdf(
        "<< /XObject << /X 5 0 R >> >>",
        b"q 60 0 0 30 0 0 cm /X Do Q",
        &[image],
    );
    let (opened, held) = peak_ratio(&bytes, || open_pdf(&bytes));
    opened.expect("opens");
    eprintln!("open held {held:.1} times the file");
    // The decoded samples, their PNG, its base64 in the SVG, and usvg's decoded copy.
    assert!(held < 12.0, "the open held {held:.1} times the file");
}
