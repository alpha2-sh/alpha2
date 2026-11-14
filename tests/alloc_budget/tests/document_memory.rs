//! Peak heap held while reading an SVG, Lottie document, dotLottie package, EXR image, or PDF
//! page, as a multiple of its size or against a fixed bound.

// The counting allocator is the only unsafe code, and it only forwards.
#![allow(unsafe_code)]
#![cfg(any(feature = "svg", feature = "lottie", feature = "exr", feature = "pdf"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

#[cfg(feature = "lottie")]
use alpha2_extensions::LottieProvider;
#[cfg(feature = "svg")]
use alpha2_extensions::SvgProvider;
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
fn a_lottie_animation_of_a_thousand_assets_naming_one_file_holds_one_decode() {
    struct Photo(Arc<[u8]>);
    impl alpha2_extensions::ResourceResolver for Photo {
        fn bytes(&self, _: &str) -> Result<Arc<[u8]>, ExtensionError> {
            Ok(Arc::clone(&self.0))
        }
    }
    // A 512 x 512 RGBA PNG of 1 MiB of samples, stored without compression.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&u32::try_from(data.len()).expect("small").to_be_bytes());
        let body = [kind, data].concat();
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut header = [512_u32.to_be_bytes(), 512_u32.to_be_bytes()].concat();
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &header);
    let mut rows = Vec::with_capacity(512 * (1 + 512 * 4));
    for _ in 0..512 {
        rows.push(0);
        rows.extend_from_slice(&[200; 512 * 4]);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::none());
    std::io::Write::write_all(&mut encoder, &rows).expect("compress");
    chunk(b"IDAT", &encoder.finish().expect("compressed"));
    chunk(b"IEND", &[]);
    let assets: Vec<String> = (0..1000)
        .map(|index| format!(r#"{{"id":"a{index}","w":512,"h":512,"e":0,"u":"","p":"photo.png"}}"#))
        .collect();
    let bytes: Arc<[u8]> = format!(
        r#"{{"v":"5.7.0","w":64,"h":64,"fr":30,"ip":0,"op":30,"assets":[{}],"layers":[]}}"#,
        assets.join(",")
    )
    .into_bytes()
    .into();
    let resolver = Photo(png.into());
    let (opened, held) = peak_ratio(&bytes, || {
        LottieProvider
            .open(
                Arc::clone(&bytes),
                &ProviderSource::Lottie { animation: None },
                &resolver,
            )
            .map(drop)
    });
    opened.expect("opens");
    #[allow(clippy::cast_precision_loss)]
    let held = held * bytes.len() as f64;
    eprintln!("held {held:.0} bytes");
    // The decoded picture, 1 MiB, and the parsed document; 1000 copies were 2 GB.
    assert!(held < 17.0 * 1024.0 * 1024.0, "held {held:.0} bytes");
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
        // Covers the file hayro parses, the scan's 64 KiB decode buffers, and the 32 MiB of
        // content a page's streams may decode to, which the scan keeps to read.
        assert!(
            held < 48.0 * 1024.0 * 1024.0,
            "{what}: held {held:.0} bytes"
        );
    }
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_image_whose_codestream_declares_billions_of_pixels_is_refused_holding_little() {
    // A 16 x 16 gray JPEG 2000 codestream whose SIZ marker says 60000 x 60000.
    let mut codestream = vec![
        0xff, 0x4f, 0xff, 0x51, 0x00, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
        0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x01,
        0xff, 0x52, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x04, 0x04, 0x00, 0x01, 0xff,
        0x5c, 0x00, 0x04, 0x40, 0x40, 0xff, 0x90, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0f,
        0x00, 0x01, 0xff, 0x93, 0x80, 0xff, 0xd9,
    ];
    for at in [8, 24] {
        codestream[at..at + 4].copy_from_slice(&60_000_u32.to_be_bytes());
        codestream[at + 4..at + 8].copy_from_slice(&60_000_u32.to_be_bytes());
    }
    let image = pdf_stream(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /JPXDecode",
        &codestream,
    );
    let bytes = pdf(
        "<< /XObject << /Im 5 0 R >> >>",
        b"q 60 0 0 30 0 0 cm /Im Do Q",
        &[image],
    );
    let (opened, held) = peak_ratio(&bytes, || open_pdf(&bytes));
    assert!(
        matches!(opened, Err(ExtensionError::Budget(_))),
        "{opened:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let held = held * bytes.len() as f64;
    eprintln!("held {held:.0} bytes");
    assert!(held < 16.0 * 1024.0 * 1024.0, "held {held:.0} bytes");
}

#[cfg(feature = "pdf")]
#[test]
fn a_pdf_page_of_a_billion_form_invocations_is_refused_holding_little() {
    // The page invokes a form 1000 times, it the next 1000 times, and that one a rectangle.
    let invoke = |form: usize| format!("/F{form} Do\n").repeat(1000).into_bytes();
    let form = |resources: &str, content: &[u8]| {
        pdf_stream(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [0 0 60 30] /Resources {resources} /Filter /FlateDecode"
            ),
            &zlib(content, 1),
        )
    };
    let bytes = pdf(
        "<< /XObject << /F1 5 0 R >> >>",
        &invoke(1),
        &[
            form("<< /XObject << /F2 6 0 R >> >>", &invoke(2)),
            form("<< /XObject << /F3 7 0 R >> >>", &invoke(3)),
            form("<< >>", b"0 0 1 1 re f"),
        ],
    );
    let (opened, held) = peak_ratio(&bytes, || open_pdf(&bytes));
    assert!(
        matches!(&opened, Err(ExtensionError::Budget(reason)) if reason.contains("draws more than")),
        "{opened:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let held = held * bytes.len() as f64;
    eprintln!("held {held:.0} bytes");
    assert!(held < 16.0 * 1024.0 * 1024.0, "held {held:.0} bytes");
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
    // Covers the decoded samples, their PNG, its base64 in the SVG, and usvg's decoded copy.
    assert!(held < 12.0, "the open held {held:.1} times the file");
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_whose_entities_expand_past_its_text_budget_is_refused_holding_little() {
    let document = |declarations: String, body: String| -> Arc<[u8]> {
        format!(
            concat!(
                "<?xml version=\"1.0\"?>\n<!DOCTYPE svg [\n{}]>\n",
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\">{}</svg>\n"
            ),
            declarations, body
        )
        .into_bytes()
        .into()
    };
    // A hundred million elements, and one attribute of 2.6 GB.
    for bytes in [
        document(
            format!("<!ENTITY b \"{}\">\n", "<g/>".repeat(1000)),
            "&b;".repeat(100_000),
        ),
        document(
            format!(
                "<!ENTITY t \"{}\">\n<!ENTITY m \"{}\">\n",
                "x".repeat(100_000),
                "&t;".repeat(255)
            ),
            format!("<desc id=\"{}\"/>", "&m;".repeat(100)),
        ),
    ] {
        let (opened, held) = peak_ratio(&bytes, || {
            SvgProvider
                .open(Arc::clone(&bytes), &ProviderSource::Svg, &NoResources)
                .map(drop)
        });
        assert!(
            matches!(opened, Err(ExtensionError::Budget(_))),
            "{opened:?}"
        );
        eprintln!("held {held:.2} times the file");
        // The entity scan keeps only its declarations; the parser never runs.
        assert!(held < 2.0, "held {held:.2} times the file");
    }
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_image_drawn_by_many_use_copies_is_refused_holding_its_byte_budget() {
    // A 4 x 4 PNG padded to 1 MiB: usvg decodes the `data:` URL again for every copy.
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x04\0\0\0\x04\x08\x06\0\0\0".to_vec();
    png.extend_from_slice(&[0xa9, 0xf1, 0x9e, 0x7e]);
    png.extend_from_slice(&u32::to_be_bytes(1 << 20));
    png.extend_from_slice(b"tEXt");
    png.resize(png.len() + (1 << 20), b'x');
    png.extend_from_slice(&[0; 4]);
    png.extend_from_slice(b"\0\0\0\0IEND\xaeB`\x82");
    let bytes: Arc<[u8]> = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><defs><image id="i" width="4" height="4" href="data:image/png;base64,{}"/></defs>{}</svg>"#,
        base64(&png),
        r##"<use href="#i"/>"##.repeat(300)
    )
    .into_bytes()
    .into();
    let (opened, held) = peak_ratio(&bytes, || {
        SvgProvider
            .open(Arc::clone(&bytes), &ProviderSource::Svg, &NoResources)
            .map(drop)
    });
    assert!(
        matches!(opened, Err(ExtensionError::Budget(_))),
        "{opened:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let held = held * bytes.len() as f64;
    eprintln!("held {held:.0} bytes");
    // The 256 MiB image byte budget and the copy being decoded when it runs out.
    assert!(held < 300.0 * 1024.0 * 1024.0, "held {held:.0} bytes");
}

#[cfg(feature = "svg")]
fn base64(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (at, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if at <= chunk.len() {
                char::from(DIGITS[(word >> shift & 63) as usize])
            } else {
                '='
            });
        }
    }
    out
}

/// A ZIP-compressed EXR file of one 3840 by 2160 tile with B, G, R and `extra` float channels, whose only
/// chunk holds a 9-byte zlib stream.
#[cfg(feature = "exr")]
fn exr_one_tile(extra: usize) -> Arc<[u8]> {
    fn attribute(out: &mut Vec<u8>, name: &str, kind: &str, value: &[u8]) {
        out.extend_from_slice(&[name.as_bytes(), b"\0", kind.as_bytes(), b"\0"].concat());
        out.extend_from_slice(&u32::try_from(value.len()).expect("size").to_le_bytes());
        out.extend_from_slice(value);
    }
    let channel = |name: &str, pixel_type: u32| {
        let mut bytes = [name.as_bytes(), b"\0"].concat();
        for word in [pixel_type, 0, 1, 1] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    };
    let mut list = ["B", "G", "R"].map(|name| channel(name, 1)).concat();
    for at in 0..extra {
        list.extend(channel(&format!("c{at:06}"), 2));
    }
    list.push(0);
    let window = [0_i32, 0, 3839, 2159].map(i32::to_le_bytes).concat();
    let mut tiles = [3840_u32, 2160].map(u32::to_le_bytes).concat();
    tiles.push(0);
    let mut out = [20_000_630_u32, 0x202].map(u32::to_le_bytes).concat();
    attribute(&mut out, "channels", "chlist", &list);
    attribute(&mut out, "compression", "compression", &[3]);
    attribute(&mut out, "dataWindow", "box2i", &window);
    attribute(&mut out, "displayWindow", "box2i", &window);
    attribute(&mut out, "lineOrder", "lineOrder", &[0]);
    attribute(
        &mut out,
        "pixelAspectRatio",
        "float",
        &1.0_f32.to_le_bytes(),
    );
    attribute(&mut out, "screenWindowCenter", "v2f", &[0; 8]);
    attribute(
        &mut out,
        "screenWindowWidth",
        "float",
        &1.0_f32.to_le_bytes(),
    );
    attribute(&mut out, "tiles", "tiledesc", &tiles);
    out.push(0);
    let chunk = u64::try_from(out.len() + 8).expect("offset");
    out.extend_from_slice(&chunk.to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&9_i32.to_le_bytes());
    out.extend_from_slice(&[0x78, 0x9c, 0x63, 0, 0, 0, 1, 0, 1]);
    out.into()
}

#[cfg(feature = "exr")]
#[test]
fn an_exr_header_of_sixty_thousand_channels_is_refused_holding_little() {
    let bytes = exr_one_tile(60_000);
    let (opened, held) = peak_ratio(&bytes, || {
        alpha2_extensions::ExrProvider
            .open(
                Arc::clone(&bytes),
                &ProviderSource::Exr { primaries: None },
                &NoResources,
            )
            .map(drop)
    });
    assert!(
        matches!(opened, Err(ExtensionError::Budget(_))),
        "{opened:?}"
    );
    #[allow(clippy::cast_precision_loss)]
    let held = held * bytes.len() as f64;
    eprintln!("held {held:.0} bytes");
    // The parsed channel list; the 2 TB block it describes is never allocated.
    assert!(held < 16.0 * 1024.0 * 1024.0, "held {held:.0} bytes");
}
