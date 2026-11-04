use std::io::Write as _;

use alpha2_model::Rgba;

use super::*;

const WHITE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

fn pdf_source(page: u32, background: Rgba) -> ProviderSource {
    ProviderSource::Pdf { page, background }
}

/// A one-page document of `content` with `resources` and `extra` objects numbered from 5.
fn one_page(resources: &str, content: &[u8], extra: &[Vec<u8>]) -> Arc<[u8]> {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 60 30] /Resources {resources} /Contents 4 0 R >>")
            .into_bytes(),
        stream("", content),
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
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .expect("write");
    out.into()
}

fn stream(entries: &str, data: &[u8]) -> Vec<u8> {
    let mut object = format!("<< {entries} /Length {} >>\nstream\n", data.len()).into_bytes();
    object.extend_from_slice(data);
    object.extend_from_slice(b"\nendstream");
    object
}

fn open(
    bytes: Arc<[u8]>,
    source: &ProviderSource,
) -> Result<Box<dyn crate::VisualSource>, ExtensionError> {
    Registry::with_defaults().open(source, Some(bytes), Arc::new(NoResources))
}

fn pixel(frame: &crate::VisualFrame, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * frame.width + x) * 4) as usize;
    <[u8; 4]>::try_from(&frame.pixels.rgba8().expect("rgba8")[at..at + 4]).expect("pixel")
}

#[test]
fn a_pdf_is_probed_as_its_pages_at_96_dpi_and_opens_the_page_it_names() {
    let registry = Registry::with_defaults();
    let file = bytes("pages.pdf");
    let (provider, report) = registry.probe(&file).expect("claimed").expect("probe");
    assert_eq!(provider, "pdf");
    assert_eq!(
        (report.format, report.width, report.height),
        (Format::Pdf, 793, 1123),
        "A4 at 96 dpi"
    );
    assert_eq!(report.timing, Timing::Static);
    assert_eq!(
        (report.items.clone(), report.item.clone()),
        (vec!["1".into(), "2".into()], Some("1".into()))
    );

    let mut first = open(Arc::clone(&file), &pdf_source(1, WHITE)).expect("page 1");
    assert_eq!(first.report().diagnostics, Vec::new());
    let frame = first.render(ms(0), (0, 0)).expect("frame");
    assert_eq!((frame.width, frame.height), (793, 1123));
    assert_eq!(
        pixel(&frame, 100, 100),
        [255, 0, 0, 255],
        "the red left half"
    );
    assert_eq!(pixel(&frame, 700, 100), [255, 255, 255, 255], "white paper");

    let second = open(Arc::clone(&file), &pdf_source(2, WHITE)).expect("page 2");
    assert_eq!(
        (
            second.report().width,
            second.report().height,
            second.report().item.clone()
        ),
        (1056, 816, Some("2".into())),
        "Letter turned 90 degrees"
    );
    assert!(matches!(
        open(file, &pdf_source(3, WHITE)),
        Err(ExtensionError::Malformed(reason)) if reason.contains("page 3 of 2")
    ));
}

#[test]
fn bytes_before_the_pdf_header_are_allowed_as_readers_allow_them() {
    let file = bytes("pages.pdf");
    let mut shifted = b"junk before the header\n".repeat(20);
    shifted.extend_from_slice(&file);
    let (provider, _) = Registry::with_defaults()
        .probe(&shifted)
        .expect("claimed")
        .expect("probe");
    assert_eq!(provider, "pdf");
    let mut late = vec![b' '; 1024];
    late.extend_from_slice(&file);
    assert!(Registry::with_defaults().probe(&late).is_none());
}

#[test]
fn the_background_is_painted_under_the_transparent_page() {
    let file = bytes("pages.pdf");
    let paper = |background: Rgba| {
        let mut page = open(Arc::clone(&file), &pdf_source(1, background)).expect("open");
        let frame = page.render(ms(0), (0, 0)).expect("frame");
        (pixel(&frame, 700, 100), pixel(&frame, 100, 100))
    };
    assert_eq!(
        paper(Rgba::new(0.0, 0.0, 0.0, 0.0)),
        ([0, 0, 0, 0], [255, 0, 0, 255])
    );
    assert_eq!(
        paper(Rgba::new(1.0, 1.0, 1.0, 0.5)),
        ([128, 128, 128, 128], [255, 0, 0, 255])
    );
}

#[test]
fn an_owner_password_opens_and_a_user_password_is_refused() {
    let mut owner = open(bytes("owner-password.pdf"), &pdf_source(1, WHITE)).expect("open");
    let frame = owner.render(ms(0), (0, 0)).expect("frame");
    assert_eq!(pixel(&frame, 42, 42), [255, 0, 0, 255]);
    assert!(matches!(
        open(bytes("user-password.pdf"), &pdf_source(1, WHITE)),
        Err(ExtensionError::Unsupported(reason)) if reason.contains("password")
    ));
}

#[test]
fn text_in_a_font_the_pdf_does_not_embed_is_drawn_with_a_standard_font_and_reported() {
    let font = |name: &str| {
        format!(
            "<< /Font << /F1 << /Type /Font /Subtype /TrueType /BaseFont /{name} /FirstChar 65 /LastChar 65 /Widths [600] >> >> >>"
        )
    };
    let drawn = |name: &str| {
        let file = one_page(&font(name), b"BT /F1 20 Tf 5 5 Td (AAA) Tj ET", &[]);
        let mut page = open(file, &pdf_source(1, Rgba::new(0.0, 0.0, 0.0, 0.0))).expect("open");
        let frame = page.render(ms(0), (0, 0)).expect("frame");
        assert!(
            frame
                .pixels
                .rgba8()
                .expect("rgba8")
                .iter()
                .any(|&sample| sample > 0),
            "{name}"
        );
        page.report().diagnostics.clone()
    };
    assert_eq!(
        drawn("ABCDEF+FancySans"),
        vec![Diagnostic::SubstitutedFont {
            families: "FancySans".into()
        }]
    );
    assert_eq!(drawn("Courier"), Vec::new());
}

#[test]
fn a_page_whose_images_or_streams_decode_too_large_is_refused_before_it_is_drawn() {
    let image = stream(
        "/Type /XObject /Subtype /Image /Width 30000 /Height 30000 /ColorSpace /DeviceGray /BitsPerComponent 8",
        b"\x00",
    );
    let file = one_page(
        "<< /XObject << /Im1 5 0 R >> >>",
        b"q 60 0 0 30 0 0 cm /Im1 Do Q",
        &[image],
    );
    assert!(matches!(
        open(file, &pdf_source(1, WHITE)),
        Err(ExtensionError::Budget(reason)) if reason.contains("pixels")
    ));

    let inline = one_page(
        "<< >>",
        b"q 60 0 0 30 0 0 cm BI /W 30000 /H 30000 /CS /G /BPC 8 ID \x00 EI Q",
        &[],
    );
    assert!(matches!(
        open(inline, &pdf_source(1, WHITE)),
        Err(ExtensionError::Budget(reason)) if reason.contains("pixels")
    ));

    // 260 MiB of spaces, a valid content stream, in about 260 KB of Flate.
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let spaces = vec![b' '; 1 << 20];
    for _ in 0..260 {
        encoder.write_all(&spaces).expect("compress");
    }
    let bomb = stream(
        "/Filter /FlateDecode",
        &encoder.finish().expect("compressed"),
    );
    let file = one_page(
        "<< /XObject << /Fm1 5 0 R >> >>",
        b"/Fm1 Do",
        &[[
            b"<< /Type /XObject /Subtype /Form /BBox [0 0 60 30] /Filter /FlateDecode ".as_slice(),
            &bomb[3..],
        ]
        .concat()],
    );
    assert!(matches!(
        open(file, &pdf_source(1, WHITE)),
        Err(ExtensionError::Budget(reason)) if reason.contains("decode to more than")
    ));
}

#[test]
fn malformed_and_empty_documents_are_refused() {
    assert!(matches!(
        open(
            Arc::from(&b"%PDF-1.4\nnot a document"[..]),
            &pdf_source(1, WHITE)
        ),
        Err(ExtensionError::Malformed(_))
    ));
    assert!(matches!(
        open(Arc::from(&b"<svg/>"[..]), &pdf_source(1, WHITE)),
        Err(ExtensionError::Mismatch(_))
    ));
}
