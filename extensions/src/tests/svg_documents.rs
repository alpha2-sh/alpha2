//! SVG: rasterizing at the shown size, fonts, images, and the budgets that refuse a document before it is drawn.

use super::*;

#[cfg(feature = "svg")]
#[test]
fn svg_is_rasterized_at_the_size_it_is_shown() {
    let registry = Registry::with_defaults();
    let file = bytes("shapes.svg");
    let (provider, report) = registry.probe(&file).expect("claimed").expect("probe");
    assert_eq!(provider, "svg");
    assert_eq!(
        (report.format, report.width, report.height),
        (Format::Svg, 64, 32)
    );
    assert_eq!(report.timing, Timing::Static);
    let mut source = registry
        .open(&ProviderSource::Svg, Some(file), Arc::new(NoResources))
        .expect("open");
    let frame = source.render(ms(0), (256, 256)).expect("frame");
    assert_eq!((frame.width, frame.height), (256, 128));
    let pixel = |x: usize, y: usize| {
        let at = (y * 256 + x) * 4;
        <[u8; 4]>::try_from(&frame.pixels.rgba8().expect("rgba8")[at..at + 4]).expect("pixel")
    };
    assert_eq!(
        pixel(40, 64),
        [0x20, 0xa0, 0x40, 255],
        "the opaque left half"
    );
    let [red, _, blue, alpha] = pixel(130, 10);
    assert!(
        red > 200 && blue < 60 && alpha == 255,
        "the gradient starts red"
    );
    let [red, _, blue, _] = pixel(252, 10);
    assert!(red < 60 && blue > 200, "and ends blue");
    // The circle's centre: half-transparent white, premultiplied.
    assert_eq!(pixel(192, 80), [128, 128, 128, 128]);
    assert_eq!(pixel(160, 124), [0, 0, 0, 0], "outside every shape");
    // The same size again is the same frame; the time never matters.
    assert_eq!(source.render(ms(9_000), (256, 256)).expect("frame"), frame);
}

#[cfg(feature = "svg")]
#[test]
fn svg_text_draws_with_registered_fonts_only() {
    let svg: Arc<[u8]> = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><text x="2" y="16" font-family="DejaVu Sans" font-size="16">Ag</text></svg>"#
        .as_slice()
        .into();
    let registry = Registry::with_defaults();
    let without = registry
        .open(
            &ProviderSource::Svg,
            Some(Arc::clone(&svg)),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(
        without.report().diagnostics,
        vec![Diagnostic::MissingFont {
            families: "DejaVu Sans".into()
        }]
    );
    let font: Arc<[u8]> = std::fs::read(corpus("../../fonts/Alpha2Fixture.ttf"))
        .expect("font")
        .into();
    let files = Files {
        files: HashMap::new(),
        fonts: vec![font],
    };
    let mut with = registry
        .open(&ProviderSource::Svg, Some(svg), Arc::new(files.clone()))
        .expect("open");
    assert_eq!(with.report().diagnostics, [] as [Diagnostic; 0]);
    let frame = with.render(ms(0), (0, 0)).expect("frame");
    assert!(
        frame
            .pixels
            .rgba8()
            .expect("rgba8")
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] > 0),
        "the text is drawn"
    );
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_draws_its_embedded_and_registered_images_and_reports_a_missing_one() {
    use base64::Engine as _;
    let png = {
        let mut file = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut file, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("header");
            writer
                .write_image_data(&[255, 0, 0, 255, 0, 0, 255, 255])
                .expect("data");
        }
        file
    };
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="48" height="16">
<image x="0" y="0" width="16" height="16" preserveAspectRatio="none" href="data:image/png;base64,{}"/>
<image x="16" y="0" width="16" height="16" preserveAspectRatio="none" xlink:href="red-blue.png"/>
<image x="32" y="0" width="16" height="16" href="gone.png"/></svg>"#,
        base64::engine::general_purpose::STANDARD.encode(&png)
    );
    let files = Files {
        files: [("red-blue.png".to_owned(), Arc::from(png))].into(),
        fonts: Vec::new(),
    };
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Svg,
            Some(svg.into_bytes().into()),
            Arc::new(files),
        )
        .expect("open");
    assert_eq!(
        source.report().diagnostics,
        [Diagnostic::MissingImage {
            href: "gone.png".into()
        }]
    );
    let frame = source.render(ms(0), (0, 0)).expect("frame");
    // Each picture stretched over its 16x16 box: red on the left, blue on the right.
    for left in [0, 16] {
        assert_eq!(pixel(&frame, left + 2, 8), [255, 0, 0, 255], "at {left}");
        assert_eq!(pixel(&frame, left + 13, 8), [0, 0, 255, 255], "at {left}");
    }
    assert_eq!(
        pixel(&frame, 40, 8),
        [0, 0, 0, 0],
        "the missing one is left out"
    );
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_is_known_whatever_comes_before_its_root() {
    // Adobe Illustrator's "preserve editing capabilities" export declares namespaces and styles
    // as entities in an internal subset; its first `>` is not the doctype's end.
    let illustrator = r##"<?xml version="1.0" encoding="utf-8"?>
<!-- Generator: Adobe Illustrator 24.0.0, SVG Export Plug-In . SVG Version: 6.00 Build 0)  -->
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd" [
	<!ENTITY ns_extend "http://ns.adobe.com/Extensibility/1.0/">
	<!ENTITY ns_ai "http://ns.adobe.com/AdobeIllustrator/10.0/">
	<!ENTITY ns_graphs "http://ns.adobe.com/Graphs/1.0/">
	<!ENTITY ns_vars "http://ns.adobe.com/Variables/1.0/">
	<!-- a comment that says <svg> and ] isn't the end -->
	<?pi a > inside?>
	<!ENTITY st0 "fill:#FF0000;">
]>
<svg version="1.1" xmlns:x="&ns_extend;" xmlns:i="&ns_ai;" xmlns:graph="&ns_graphs;" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" x="0px" y="0px" width="8px" height="8px" viewBox="0 0 8 8">
<switch>
	<foreignObject requiredExtensions="&ns_ai;" x="0" y="0" width="1" height="1"><i:aipgfRef xlink:href="#adobe_illustrator_pgf"></i:aipgfRef></foreignObject>
	<g i:extraneous="self"><rect style="&st0;" width="8" height="8"/></g>
</switch>
</svg>
"##;
    let long_comment = format!(
        "<?xml version=\"1.0\"?>\n<!-- {} -->\n<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"red\"/></svg>",
        "x".repeat(8_192)
    );
    let quoted = "\u{feff}<!DOCTYPE svg SYSTEM \"a>b.dtd\"><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"red\"/></svg>";
    let registry = Registry::with_defaults();
    for (what, file) in [
        ("an Illustrator export", illustrator.to_owned()),
        ("a prolog past 4 KB", long_comment),
        ("a doctype whose literal holds >", quoted.to_owned()),
    ] {
        let file: Arc<[u8]> = file.into_bytes().into();
        let (provider, report) = registry
            .probe(&file)
            .unwrap_or_else(|| panic!("{what}: not claimed"))
            .unwrap_or_else(|error| panic!("{what}: {error}"));
        assert_eq!(
            (provider, report.width, report.height),
            ("svg", 8, 8),
            "{what}"
        );
        let frame = registry
            .open(&ProviderSource::Svg, Some(file), Arc::new(NoResources))
            .unwrap_or_else(|error| panic!("{what}: {error}"))
            .render(MediaTime::ZERO, (0, 0))
            .expect("frame");
        assert_eq!(pixel(&frame, 4, 4), [255, 0, 0, 255], "{what}");
    }
    for (what, file) in [
        ("HTML", "<!DOCTYPE html><html><svg></svg></html>"),
        (
            "an unclosed doctype",
            "<!DOCTYPE svg [ <!ENTITY a \">\"> <svg>",
        ),
        ("an unclosed comment", "<!-- <svg>"),
        ("another root", "<?xml version=\"1.0\"?><svgx/>"),
    ] {
        assert!(registry.probe(file.as_bytes()).is_none(), "{what}");
    }
}

#[cfg(feature = "svg")]
fn fixture_font() -> Arc<[u8]> {
    std::fs::read(corpus("../../fonts/Alpha2Fixture.ttf"))
        .expect("font")
        .into()
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_reports_the_fonts_its_drawn_text_misses_and_only_those() {
    // Missing fonts come from the text the tree draws, not `<text` in the bytes: clipped, masked,
    // unused, hidden or commented-out text is not reported, and `<use>` copies hide nothing.
    let files = Files {
        files: HashMap::new(),
        fonts: vec![fixture_font()],
    };
    let registry = Registry::with_defaults();
    let report = |svg: &str| {
        registry
            .open(
                &ProviderSource::Svg,
                Some(Arc::from(svg.as_bytes())),
                Arc::new(files.clone()),
            )
            .expect("open")
            .report()
            .diagnostics
            .clone()
    };
    let wrap = |body: &str| {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="40" height="20">{body}</svg>"#
        )
    };
    let text = |family: &str, body: &str| {
        format!(r#"<text x="2" y="16" font-family="{family}" font-size="16">{body}</text>"#)
    };
    for (what, body) in [
        (
            "a clip path's text",
            format!(
                r#"<clipPath id="c">{}</clipPath><rect width="40" height="20" clip-path="url(#c)"/>"#,
                text("DejaVu Sans", "Ag")
            ),
        ),
        (
            "a mask's text",
            format!(
                r#"<mask id="m">{}</mask><rect width="40" height="20" mask="url(#m)"/>"#,
                text("DejaVu Sans", "Ag")
            ),
        ),
        (
            "unused text",
            format!("<defs>{}</defs>", text("Nope", "Ag")),
        ),
        (
            "hidden text",
            format!(r#"<g display="none">{}</g>"#, text("Nope", "Ag")),
        ),
        ("a comment", format!("<!-- {} -->", text("Nope", "Ag"))),
    ] {
        assert_eq!(report(&wrap(&body)), Vec::new(), "{what}");
    }
    let used_twice = format!(
        r##"<defs>{}</defs><use xlink:href="#t"/><use href="#t"/>{}"##,
        text("DejaVu Sans", "A").replace("<text", r#"<text id="t""#),
        text("Nope", "B")
    );
    assert_eq!(
        report(&wrap(&used_twice)),
        vec![Diagnostic::MissingFont {
            families: "Nope".into()
        }],
        "the text no font draws, beside one drawn twice"
    );
    let mixed = format!("{}{}", text("DejaVu Sans", "A"), text("Nope, Neither", "B"));
    assert_eq!(
        report(&wrap(&mixed)),
        vec![Diagnostic::MissingFont {
            families: "Nope, Neither".into()
        }]
    );
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_shares_its_fonts_and_loads_them_only_for_text() {
    // An open must not copy every registered font into a database of its own for its source to
    // hold, text or not.
    let font = fixture_font();
    let files = Files {
        files: HashMap::new(),
        fonts: vec![Arc::clone(&font)],
    };
    let registry = Registry::with_defaults();
    let open = |svg: &[u8]| {
        registry
            .open(
                &ProviderSource::Svg,
                Some(Arc::from(svg)),
                Arc::new(files.clone()),
            )
            .expect("open")
    };
    let held = || Arc::strong_count(&font);
    let before = held();
    let shapes = open(br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>"#);
    assert_eq!(held(), before, "a document without text loads no font");
    let titled = open(br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><text x="2" y="16" font-family="DejaVu Sans" font-size="16">Ag</text></svg>"#);
    assert_eq!(
        held(),
        before + 1,
        "the source's tree holds the registered font itself"
    );
    drop((shapes, titled));
    assert_eq!(held(), before);
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_draws_a_registered_image_however_its_href_is_written_and_reports_one_it_cannot_draw() {
    // Registered images resolve from the href the parser reads (entity references work); a
    // registered SVG is drawn, and an image that cannot be drawn is reported.
    let mut red = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut red, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer
            .write_image_data(&[255, 0, 0, 255].repeat(4))
            .expect("data");
    }
    let red: Arc<[u8]> = red.into();
    let green: Arc<[u8]> = br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="#00ff00"/></svg>"##
        .as_slice()
        .into();
    let files = Files {
        files: [
            ("red.png".to_owned(), Arc::clone(&red)),
            ("a&b.png".to_owned(), red),
            ("badge.svg".to_owned(), green),
            ("picture.bmp".to_owned(), Arc::from(&b"BM\0\0\0\0"[..])),
        ]
        .into_iter()
        .collect(),
        fonts: Vec::new(),
    };
    let registry = Registry::with_defaults();
    let draw = |image: &str| {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="8" height="8">{image}</svg>"#
        );
        let mut source = registry
            .open(
                &ProviderSource::Svg,
                Some(Arc::from(svg.as_bytes())),
                Arc::new(files.clone()),
            )
            .expect("open");
        let frame = source.render(MediaTime::ZERO, (0, 0)).expect("frame");
        (pixel(&frame, 4, 4), source.report().diagnostics.clone())
    };
    let red_pixel = [255, 0, 0, 255];
    for (what, image) in [
        ("plain", r#"<image href="red.png" width="8" height="8"/>"#),
        (
            "xlink",
            r#"<image xlink:href="red.png" width="8" height="8"/>"#,
        ),
        (
            "spaced",
            r#"<image href = "red.png" width="8" height="8"/>"#,
        ),
        (
            "an entity",
            r#"<image href="a&amp;b.png" width="8" height="8"/>"#,
        ),
    ] {
        assert_eq!(draw(image), (red_pixel, Vec::new()), "{what}");
    }
    assert_eq!(
        draw(r#"<image href="badge.svg" width="8" height="8"/>"#),
        ([0, 255, 0, 255], Vec::new()),
        "a registered SVG"
    );
    assert_eq!(
        draw(r#"<image href="picture.bmp" width="8" height="8"/>"#),
        (
            [0, 0, 0, 0],
            vec![Diagnostic::UnreadableImage {
                href: "picture.bmp".into(),
                reason: "not a PNG, JPEG, GIF, WebP, or SVG image".into()
            }]
        ),
        "a format an SVG image is not drawn from"
    );
    assert_eq!(
        draw(r#"<image href="absent.png" width="8" height="8"/>"#),
        (
            [0, 0, 0, 0],
            vec![Diagnostic::MissingImage {
                href: "absent.png".into()
            }]
        )
    );
}

/// Opens an SVG document with nothing registered and renders it at `fit`, in a thread with the
/// 1 MiB stack a browser gives the extensions module (an unoptimized build's frames take several
/// times more).
#[cfg(feature = "svg")]
fn svg_drawn(document: &str, fit: (u32, u32)) -> Result<(u32, u32), ExtensionError> {
    let document: Arc<[u8]> = Arc::from(document.as_bytes());
    let stack = if cfg!(debug_assertions) {
        16 << 20
    } else {
        1 << 20
    };
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            let registry = Registry::with_defaults();
            let probed = registry.probe(&document).expect("claimed").map(drop);
            let opened = registry.open(&ProviderSource::Svg, Some(document), Arc::new(NoResources));
            assert_eq!(
                probed.as_ref().err().map(ToString::to_string),
                opened.as_ref().err().map(ToString::to_string),
                "probe and open agree"
            );
            let frame = opened?.render(ms(0), fit)?;
            Ok((frame.width, frame.height))
        })
        .expect("thread")
        .join()
        .expect("no panic")
}

#[cfg(feature = "svg")]
fn svg_document(width: u32, height: u32, body: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{width}" height="{height}">{body}</svg>"#
    )
}

#[cfg(feature = "svg")]
fn refused_as(result: Result<(u32, u32), ExtensionError>, says: &str) {
    match result {
        Err(ExtensionError::Budget(message)) if message.contains(says) => {}
        other => panic!("expected `too large: ...{says}...`, got {other:?}"),
    }
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_filter_that_costs_more_than_a_render_may_is_refused_before_it_is_drawn() {
    // resvg runs a filter over its region unbounded, and morphology, convolution and turbulence
    // multiply each pixel's cost: each case took seconds to hours from a few hundred bytes.
    let filtered = |primitives: &str, region: &str| {
        svg_document(
            1920,
            1080,
            &format!(
                r#"<filter id="f" {region}>{primitives}</filter><rect width="1920" height="1080" fill="red" filter="url(#f)"/>"#
            ),
        )
    };
    let whole = r#"x="0" y="0" width="1" height="1""#;
    for (what, document, says) in [
        (
            "a morphology of radius 400 (hours)",
            filtered(r#"<feMorphology operator="dilate" radius="400"/>"#, whole),
            r#"256 times its canvas, counted as at least 1920x1080 (the most at feMorphology in filter "f")"#,
        ),
        (
            "a morphology of radius 30 (9 s)",
            filtered(r#"<feMorphology radius="30"/>"#, whole),
            "feMorphology",
        ),
        (
            "a 50x50 convolution (16 s)",
            filtered(
                &format!(
                    r#"<feConvolveMatrix order="50" kernelMatrix="{}"/>"#,
                    vec!["1"; 2500].join(" ")
                ),
                whole,
            ),
            "feConvolveMatrix",
        ),
        (
            "a turbulence of 1,000 octaves (75 s)",
            filtered(
                r#"<feTurbulence baseFrequency="0.05" numOctaves="1000"/>"#,
                whole,
            ),
            "feTurbulence",
        ),
        (
            "200 floods (1.6 GB held)",
            filtered(&r#"<feFlood flood-color="blue"/>"#.repeat(200), whole),
            "feFlood",
        ),
        (
            "one flood over 21 canvases a side (3.7 GB)",
            filtered(
                r#"<feFlood flood-color="blue"/>"#,
                r#"x="-10" y="-10" width="21" height="21""#,
            ),
            "pixels of layers and filter results at once (the most at feFlood",
        ),
    ] {
        let started = std::time::Instant::now();
        let drawn = svg_drawn(&document, (1920, 1080));
        assert!(
            started.elapsed().as_secs_f64() < 5.0,
            "{what}: refused before it is drawn"
        );
        refused_as(drawn, says);
    }
    // Filters a document means are drawn: a drop shadow, a small morphology, a blur.
    for primitives in [
        r#"<feDropShadow dx="4" dy="4" stdDeviation="3"/>"#,
        r#"<feMorphology radius="2"/><feGaussianBlur stdDeviation="5"/>"#,
    ] {
        assert_eq!(
            svg_drawn(&filtered(primitives, ""), (1920, 1080)).expect("drawn"),
            (1920, 1080)
        );
    }
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_drawn_larger_than_its_own_size_is_checked_again_at_that_size() {
    // A filter's radius grows with the drawn size: a morphology cheap at 192x108 costs ten thousand
    // times as much at 1920x1080.
    let document = svg_document(
        192,
        108,
        r#"<filter id="f" x="0" y="0" width="1" height="1"><feMorphology radius="5"/></filter><rect width="192" height="108" filter="url(#f)"/>"#,
    );
    assert_eq!(svg_drawn(&document, (192, 108)).expect("drawn"), (192, 108));
    refused_as(
        svg_drawn(&document, (1920, 1080)),
        "drawing the SVG at 1920x1080 costs more than 256 times its canvas",
    );
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_that_paints_more_than_a_render_may_is_refused() {
    // Every fill, layer, clip, mask, and nested document costs the area it covers: 100,000
    // translucent canvas-sized rectangles (6 MB) took minutes.
    let rect = r#"<rect width="1920" height="1080" fill="red" fill-opacity=".5"/>"#;
    assert_eq!(
        svg_drawn(&svg_document(1920, 1080, &rect.repeat(200)), (0, 0)).expect("drawn"),
        (1920, 1080)
    );
    for (what, body) in [
        ("300 fills", rect.repeat(300)),
        (
            "100 layers",
            format!(r#"<g opacity=".9">{rect}</g>"#).repeat(100),
        ),
        (
            "100 masks",
            format!(
                r#"<mask id="m"><rect width="1920" height="1080" fill="white"/></mask>{}"#,
                format!(r#"<g mask="url(#m)">{rect}</g>"#).repeat(100)
            ),
        ),
        (
            "a pattern filling 200 rectangles",
            format!(
                r#"<pattern id="p" width="1920" height="1080" patternUnits="userSpaceOnUse">{rect}</pattern>{}"#,
                r#"<rect width="1920" height="1080" fill="url(#p)"/>"#.repeat(200)
            ),
        ),
    ] {
        refused_as(
            svg_drawn(&svg_document(1920, 1080, &body), (0, 0)),
            "costs more than 256 times its canvas",
        );
        let _ = what;
    }
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_that_reaches_farther_past_its_canvas_than_tiny_skia_draws_is_refused() {
    // tiny-skia panics on coordinates a million pixels out (natively an abort while unwinding):
    // paths, strokes and images past 262,144 pixels, and layers past 2^30 pixels, are refused.
    refused_as(
        svg_drawn(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M 10 6 L 6 13 L 14 14 L 1 -1e9 z"/></svg>"#,
            (0, 0),
        ),
        "places a path more than 262144 pixels past",
    );
    let curves = "M 202114 57 C 283 1000000 199 0 -1000000 -1000000 C 1895 -170805 220 197 285 -1000000 Z \
                  M 764816 49 Q 1000000 1000000 -709189 0 C 207 -760503 172 282 342759 -1000000";
    for (what, body, says) in [
        (
            "curves",
            format!(r#"<path fill-rule="evenodd" d="{curves}"/>"#),
            "places a path more than 262144 pixels past",
        ),
        (
            "a stroke",
            r#"<path d="M 10 10 L 20 20" stroke="red" stroke-width="2000000"/>"#.to_owned(),
            "places a path",
        ),
        (
            "a filter region",
            r#"<filter id="f" filterUnits="userSpaceOnUse" x="-2e9" y="0" width="4e9" height="10"><feFlood/></filter><rect width="10" height="10" filter="url(#f)"/>"#.to_owned(),
            "1073741824 pixels past",
        ),
        (
            "a layer",
            r#"<g opacity=".5"><rect x="-2e9" width="4e9" height="10"/></g>"#.to_owned(),
            "places a group more than 1073741824 pixels past",
        ),
        (
            // resvg moves the image by whole pixels in `i32`: this panicked.
            "an offset",
            r#"<filter id="f"><feOffset dx="1e10"/></filter><rect width="10" height="10" filter="url(#f)"/>"#.to_owned(),
            "places feOffset more than 1073741824 pixels past",
        ),
        (
            "a mask's region, filled as a path",
            r#"<mask id="m" maskUnits="userSpaceOnUse" x="-3e6" y="0" width="5e6" height="9"><rect width="9" height="9" fill="white"/></mask><rect width="9" height="9" mask="url(#m)"/>"#.to_owned(),
            "places mask \"m\" more than 262144 pixels past",
        ),
    ] {
        refused_as(svg_drawn(&svg_document(326, 298, &body), (0, 0)), says);
        let _ = what;
    }
    let near = svg_document(
        64,
        64,
        r#"<path fill-rule="evenodd" d="M 200000 10 C 283 -200000 199 0 -200000 64 Z"/>"#,
    );
    assert_eq!(svg_drawn(&near, (0, 0)).expect("drawn"), (64, 64));
    // A path that reaches far only beside the canvas, or in horizontal and vertical lines (a strip
    // of clipped terminal frames hundreds of canvases wide), is drawn.
    let strip = svg_document(
        64,
        64,
        r#"<path d="M 1 1 h 9 v 9 h -9 Z M 2000000 1 h 9 v 9 h -9 Z M -9000000 50 h 9 v 9 h -9 Z"/><rect x="-5000000" width="10000000" height="9"/>"#,
    );
    assert_eq!(svg_drawn(&strip, (0, 0)).expect("drawn"), (64, 64));
    // Reach is counted at the size asked: what is near at its own size may be far drawn larger.
    let small = svg_document(16, 16, r#"<path d="M 0 0 L 20000 8 L 0 16 Z"/>"#);
    assert_eq!(svg_drawn(&small, (0, 0)).expect("drawn"), (16, 16));
    refused_as(svg_drawn(&small, (256, 256)), "262144 pixels past");
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_whose_arcs_or_markers_convert_past_a_budget_is_refused_before_it_is_read() {
    // usvg splits an arc into cubics by the sixth root of its radius, taking each from a list's
    // front: one icon endpoint 1.8e31 away took 2.7 s to probe and as long to open.
    let icon = svg_document(
        22,
        22,
        r#"<path d="M 16.5 14 A 2.4999914 2.4999914 0 0 0 14 16.5 A 2.4999914 2.4999914 0 0 0 14.416016 17.81e30 L 15.148438 17.144531 Z"/>"#,
    );
    refused_as(svg_drawn(&icon, (0, 0)), "paths' arcs");
    // A marker's content is copied at every vertex of every shape naming it, past usvg's node
    // limit: 100 rectangles at 20,000 vertices are two million elements.
    let markers = |vertices: usize| {
        svg_document(
            64,
            64,
            &format!(
                r#"<marker id="m" markerWidth="5" markerHeight="5">{}</marker><path d="M 0 0 {}" stroke="red" marker-mid="url(#m)"/>"#,
                r#"<rect width="1" height="1"/>"#.repeat(100),
                "L 9 9 L 0 0 ".repeat(vertices / 2)
            ),
        )
    };
    refused_as(
        svg_drawn(&markers(20_000), (0, 0)),
        "more than 1000000 elements",
    );
    assert_eq!(svg_drawn(&markers(10), (0, 0)).expect("drawn"), (64, 64));
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_nested_deeper_than_its_parsers_stack_is_refused_before_it_is_parsed() {
    // The XML parser, usvg and resvg recurse once a level and overflow the stack within a few
    // thousand levels, so depth is refused from the text first, `use` copies and entities included.
    let nested = |levels: usize| {
        svg_document(
            64,
            64,
            &format!(
                r#"{}<rect width="8" height="8"/>{}"#,
                r#"<g opacity=".99">"#.repeat(levels),
                "</g>".repeat(levels)
            ),
        )
    };
    // The root and 254 groups put the rectangle 256 deep.
    assert_eq!(svg_drawn(&nested(254), (0, 0)).expect("drawn"), (64, 64));
    refused_as(svg_drawn(&nested(255), (0, 0)), "nest more than 256 deep");
    refused_as(
        svg_drawn(&nested(100_000), (0, 0)),
        "nest more than 256 deep",
    );
    let mut entities = String::from(r#"<!ENTITY d0 "<rect width='8' height='8'/>">"#);
    entities.extend((1..=4).map(|level| {
        format!(
            r#"<!ENTITY d{level} "{}&d{};{}">"#,
            "<g>".repeat(90),
            level - 1,
            "</g>".repeat(90)
        )
    }));
    refused_as(
        svg_drawn(
            &format!(
                r"<!DOCTYPE svg [{entities}]>{}",
                svg_document(64, 64, "&d4;")
            ),
            (0, 0),
        ),
        "nest more than 256 deep",
    );
    let mut copies = String::from(r#"<defs><g id="u0"><rect width="8" height="8"/></g>"#);
    copies.extend((1..=4).map(|level| {
        format!(
            r##"<g id="u{level}">{}<use href="#u{}"/>{}</g>"##,
            "<g>".repeat(90),
            level - 1,
            "</g>".repeat(90)
        )
    }));
    copies.push_str(r##"</defs><use href="#u4"/>"##);
    refused_as(
        svg_drawn(&svg_document(64, 64, &copies), (0, 0)),
        "nest more than 256 deep with its `use` copies in place",
    );
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_that_takes_too_long_to_read_is_refused_before_usvg_converts_it() {
    // usvg backtracks through ancestors for properties and style rules: 300,000 elements 1,000
    // deep took 5.5 s, and five descendant combinators over 100 levels took over a minute.
    let deep = |rule: &str, levels: usize, elements: usize| {
        svg_document(
            64,
            64,
            &format!(
                "<style>{rule}</style>{}{}{}",
                "<g>".repeat(levels),
                r#"<rect width="1" height="1"/>"#.repeat(elements),
                "</g>".repeat(levels)
            ),
        )
    };
    for (what, document) in [
        (
            "backtracking selectors",
            deep("x g g g g g rect{fill:red}", 100, 10),
        ),
        ("many elements deep down", deep("", 250, 180_000)),
    ] {
        let started = std::time::Instant::now();
        refused_as(
            svg_drawn(&document, (0, 0)),
            "reading the SVG takes more than",
        );
        // About a second natively, optimized.
        assert!(
            cfg!(debug_assertions) || started.elapsed().as_secs_f64() < 5.0,
            "{what}"
        );
    }
    assert_eq!(
        svg_drawn(&deep("g rect{fill:red} .a{fill:blue}", 20, 1_000), (0, 0)).expect("drawn"),
        (64, 64)
    );
}

#[cfg(feature = "svg")]
#[test]
fn the_svg_images_an_svg_draws_are_read_within_its_text_budget() {
    use base64::Engine as _;
    use std::io::Write as _;
    // usvg unpacks a gzipped SVG image whole and parses a nested SVG each time it is drawn: 2.7 MB
    // unpacked to 2 GB. All the SVG text one source parses is held to the document limit.
    let mut packed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    packed
        .write_all(svg_document(8, 8, &" ".repeat(40 << 20)).as_bytes())
        .expect("packed");
    let packed = packed.finish().expect("packed");
    let image = |data: &[u8], kind: &str| {
        format!(
            r#"<image width="8" height="8" href="data:{kind};base64,{}"/>"#,
            base64::engine::general_purpose::STANDARD.encode(data)
        )
    };
    refused_as(
        svg_drawn(
            &svg_document(8, 8, &image(&packed, "image/svg+xml")),
            (0, 0),
        ),
        "bytes of text together",
    );
    let inner = svg_document(8, 8, &format!("<!-- {} -->", "x".repeat(1 << 20)));
    let drawn = |times: usize| {
        svg_drawn(
            &svg_document(
                8,
                8,
                &format!(
                    r"<defs>{}</defs>{}",
                    image(inner.as_bytes(), "image/svg+xml").replace("<image", r#"<image id="i""#),
                    r##"<use href="#i"/>"##.repeat(times)
                ),
            ),
            (0, 0),
        )
    };
    assert_eq!(drawn(4).expect("drawn"), (8, 8));
    refused_as(drawn(40), "bytes of text together");
}
