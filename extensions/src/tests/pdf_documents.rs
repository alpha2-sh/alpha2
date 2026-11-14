use alpha2_model::Rgba;

use std::io::Write as _;

use super::hostile::{
    j2k_codestream, jbig2_page, pdf_coded_image, pdf_content_bomb, pdf_far_path, pdf_forms,
    pdf_image_draws, pdf_one_page, pdf_self_invoking_form, pdf_stream as stream, pdf_type3_glyphs,
};
use super::*;

const WHITE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

fn pdf_source(page: u32, background: Rgba) -> ProviderSource {
    ProviderSource::Pdf { page, background }
}

/// A one-page document of `content` with `resources` and `extra` objects numbered from 5.
fn one_page(resources: &str, content: &[u8], extra: &[Vec<u8>]) -> Arc<[u8]> {
    pdf_one_page(resources, content, extra).into()
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

fn refused_saying(file: Vec<u8>, says: &str) {
    match open(file.into(), &pdf_source(1, WHITE)) {
        Err(ExtensionError::Budget(reason)) if reason.contains(says) => {}
        other => panic!("expected `too large: ...{says}...`, got {:?}", other.err()),
    }
}

#[test]
fn a_page_is_held_to_what_its_forms_draw_each_time_they_are_invoked() {
    // 97,336 rectangles open; a million are refused before hayro converts the page.
    open(pdf_forms(46, 3).into(), &pdf_source(1, WHITE)).expect("46^3 draws open");
    refused_saying(pdf_forms(100, 3), "the page draws more than 262144");
    refused_saying(pdf_self_invoking_form(), "draws itself");
}

#[test]
fn a_page_of_a_billion_form_invocations_is_refused_before_it_is_converted() {
    refused_saying(pdf_forms(1000, 3), "the page draws more than 262144");
}

#[test]
fn a_page_is_held_to_the_pixels_and_data_its_image_draws_write() {
    // A flat 4 MP image drawn 10 times opens; 100 times it decodes 400 MP.
    open(
        pdf_image_draws(2000, true, false, 10).into(),
        &pdf_source(1, WHITE),
    )
    .expect("ten draws open");
    refused_saying(
        pdf_image_draws(2000, true, false, 100),
        "image draws decode more than",
    );
    refused_saying(
        pdf_image_draws(1000, false, true, 1000),
        "image draws decode more than",
    );
}

#[test]
fn a_page_is_held_to_what_its_type3_glyphs_draw() {
    open(pdf_type3_glyphs(10, 100).into(), &pdf_source(1, WHITE)).expect("small text opens");
    refused_saying(
        pdf_type3_glyphs(1000, 10_000),
        "the page draws more than 262144",
    );
}

#[test]
fn a_page_is_held_to_what_its_content_streams_decode_to_together() {
    refused_saying(
        pdf_content_bomb(250 << 20),
        "content streams decode to more than 33554432",
    );
}

#[test]
fn an_image_is_sized_from_its_own_coded_data() {
    // One pixel by the dictionary; 3.6 and 4.3 billion by their codecs.
    let jpx = |side, draws| {
        pdf_coded_image(
            "/Height 1 /BitsPerComponent 8 /Filter /JPXDecode",
            &j2k_codestream(side),
            draws,
        )
    };
    refused_saying(jpx(60_000, 1), "decode to more than");
    refused_saying(
        pdf_coded_image(
            "/Height 1 /BitsPerComponent 1 /Filter /JBIG2Decode",
            &jbig2_page(65_535),
            1,
        ),
        "decode to more than",
    );
    refused_saying(
        pdf_coded_image(
            "/Height 30000 /BitsPerComponent 1 /Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 1000000 >>",
            &[0, 0x10, 1, 0, 0x10, 1],
            1,
        ),
        "decode to more than",
    );
    refused_saying(
        pdf_coded_image(
            "/Height 0 /BitsPerComponent 1 /Filter /CCITTFaxDecode /DecodeParms << /K -1 >>",
            &[0, 0x10, 1, 0, 0x10, 1],
            1,
        ),
        "row count is not given",
    );
    // A 4000 x 4000 codestream counts as 16 MP: ten draws are past four canvases.
    open(jpx(4000, 1).into(), &pdf_source(1, WHITE)).expect("one draw opens");
    refused_saying(jpx(4000, 10), "image draws decode more than");
    let inline = pdf_one_page(
        "<< >>",
        b"q 60 0 0 30 0 0 cm BI /W 1 /H 30000 /CS /G /BPC 1 /F /CCF /DP << /K -1 /Columns 1000000 >> ID \x00\x10\x01 EI Q",
        &[],
    );
    refused_saying(inline, "decode to more than");
}

#[test]
fn an_image_whose_codestream_does_not_parse_is_reported_unreadable() {
    let truncated = j2k_codestream(16)[..20].to_vec();
    let source = open(
        pdf_coded_image(
            "/Height 1 /BitsPerComponent 8 /Filter /JPXDecode",
            &truncated,
            1,
        )
        .into(),
        &pdf_source(1, WHITE),
    )
    .expect("open");
    assert!(
        source
            .report()
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic, Diagnostic::UnreadableImage { .. })),
        "{:?}",
        source.report().diagnostics
    );
}

#[test]
fn a_probe_refuses_a_first_page_that_opening_refuses() {
    let file = pdf_far_path();
    let probed = Registry::with_defaults().probe(&file).expect("claimed");
    let opened = open(file.into(), &pdf_source(1, WHITE));
    match (probed, opened) {
        (Err(ExtensionError::Budget(probed)), Err(ExtensionError::Budget(opened))) => {
            assert_eq!(probed, opened);
        }
        (probed, opened) => panic!("probe {:?}, open {:?}", probed.err(), opened.err()),
    }
}

#[test]
fn a_page_reports_the_images_it_holds() {
    // An incompressible 256 x 256 RGB image stays about 196 kB as PNG.
    let mut state = 0x2545_f491_u32;
    let noise: Vec<u8> = (0..256 * 256 * 3)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect();
    let image = stream(
        "/Type /XObject /Subtype /Image /Width 256 /Height 256 /ColorSpace /DeviceRGB /BitsPerComponent 8",
        &noise,
    );
    let file = one_page(
        "<< /XObject << /Im1 5 0 R >> >>",
        b"q 60 0 0 30 0 0 cm /Im1 Do Q",
        &[image],
    );
    let source = open(file, &pdf_source(1, WHITE)).expect("open");
    assert!(
        source.resident_bytes() > 190_000,
        "{}",
        source.resident_bytes()
    );
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
