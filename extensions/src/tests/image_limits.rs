//! Embedded and declared image sizes held to the canvas budget before anything is decoded.

use super::*;

/// A PNG whose header declares a `width` by `height` RGBA8 image and whose
/// data is an empty zlib stream: what it declares is all a reader learns
/// before it allocates.
#[cfg(any(feature = "image-sequence", feature = "lottie", feature = "svg"))]
fn declared_png(width: u32, height: u32) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        png.extend_from_slice(&u32::try_from(data.len()).expect("small").to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
    chunk(b"IEND", &[]);
    png
}

#[cfg(feature = "image-sequence")]
#[test]
fn a_png_is_sized_from_its_header_before_it_is_decoded() {
    // 20000x20000 declares 1.6 GB of RGBA in a hundred bytes: refused for its size, not decoded.
    let large: Arc<[u8]> = declared_png(20_000, 20_000).into();
    let files = Files {
        files: [
            ("large.png".to_owned(), Arc::clone(&large)),
            ("small.png".to_owned(), bytes("sequence_0007.png")),
        ]
        .into(),
        fonts: Vec::new(),
    };
    let registry = Registry::with_defaults();
    let first = registry.open(
        &sequence(&[Some("large.png")], MissingFrames::Reject),
        None,
        Arc::new(files.clone()),
    );
    assert!(
        matches!(first, Err(ExtensionError::Budget(_))),
        "{:?}",
        first.err()
    );
    // A later frame is held to the same bound when it is reached.
    let mut later = registry
        .open(
            &sequence(
                &[Some("small.png"), Some("large.png")],
                MissingFrames::Reject,
            ),
            None,
            Arc::new(files.clone()),
        )
        .expect("the first frame opens");
    let frame = later.render(MediaTime::from_ticks(i64::MAX), (0, 0));
    assert!(
        matches!(frame, Err(ExtensionError::Budget(_))),
        "{:?}",
        frame.err()
    );
}

#[cfg(feature = "lottie")]
#[test]
fn a_lottie_image_asset_larger_than_a_canvas_is_reported_not_decoded() {
    use base64::Engine as _;
    // Allocating 16384 * (2^31 - 1) * 4 bytes aborts the process, so the header must refuse it.
    let png = declared_png(16_384, 0x7fff_ffff);
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes("moving.json")).expect("moving.json");
    json["assets"] = serde_json::json!([{
        "id": "large",
        "w": 16,
        "h": 16,
        "e": 1,
        "u": "",
        "p": format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        ),
    }]);
    let file: Arc<[u8]> = serde_json::to_vec(&json).expect("json").into();
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(file),
            Arc::new(NoResources),
        )
        .expect("open");
    assert!(
        source
            .report()
            .diagnostics
            .contains(&Diagnostic::MissingImage {
                href: "an embedded image".into()
            }),
        "{:?}",
        source.report().diagnostics
    );
    source.render(ms(0), (64, 48)).expect("frame");
}

#[cfg(feature = "svg")]
#[test]
fn an_svg_image_larger_than_a_canvas_is_reported_and_left_out() {
    use base64::Engine as _;
    let png = declared_png(20_000, 20_000);
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><image width="16" height="16" href="data:image/png;base64,{}"/><image width="16" height="16" href="large.png"/></svg>"#,
        base64::engine::general_purpose::STANDARD.encode(&png)
    );
    let files = Files {
        files: [("large.png".to_owned(), Arc::from(png))].into(),
        fonts: Vec::new(),
    };
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Svg,
            Some(svg.into_bytes().into()),
            Arc::new(files.clone()),
        )
        .expect("open");
    assert_eq!(
        source.report().diagnostics,
        vec![
            Diagnostic::MissingImage {
                href: "data:image/png".into()
            },
            Diagnostic::MissingImage {
                href: "large.png".into()
            },
        ]
    );
    // Drawing does not decode either: resvg is never given them.
    let frame = source.render(ms(0), (16, 16)).expect("frame");
    assert!(
        frame
            .pixels
            .rgba8()
            .expect("rgba8")
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[cfg(feature = "svg")]
#[test]
fn the_images_an_svg_draws_are_held_to_four_canvases_together() {
    use base64::Engine as _;
    // resvg decodes every image it draws at its declared size on every render: count them too.
    let canvas = declared_png(7_680, 4_320);
    let href = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&canvas)
    );
    let image = |id: usize| format!(r#"<image id="i{id}" width="16" height="16" href="{href}"/>"#);
    let svg = |body: String| {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="16" height="16">{body}</svg>"#
        )
    };
    let files = Files {
        files: [("canvas.png".to_owned(), Arc::from(canvas.clone()))].into(),
        fonts: Vec::new(),
    };
    let open = |document: String| {
        Registry::with_defaults().open(
            &ProviderSource::Svg,
            Some(document.into_bytes().into()),
            Arc::new(files.clone()),
        )
    };
    // Four whole canvases, one of them registered, are the most one source draws.
    let four = svg((0..3).map(image).collect::<String>()
        + r#"<image width="16" height="16" href="canvas.png"/>"#);
    open(four.clone())
        .expect("four canvases open")
        .render(ms(0), (16, 16))
        .expect("four canvases draw");
    // A fifth image, a fifth registered one, or one image drawn five times by <use> is refused.
    for (what, document) in [
        ("a fifth image", svg((0..5).map(image).collect())),
        (
            "a fifth registered image",
            svg((0..4).map(image).collect::<String>()
                + r#"<image width="16" height="16" href="canvas.png"/>"#),
        ),
        (
            "one image drawn five times",
            svg(format!(
                "<defs>{}</defs>{}",
                image(0),
                r##"<use xlink:href="#i0"/>"##.repeat(5)
            )),
        ),
        (
            "images in a pattern five paths fill",
            svg(format!(
                r#"<defs><pattern id="p" width="16" height="16" patternUnits="userSpaceOnUse">{}</pattern></defs>{}"#,
                image(0),
                r#"<rect width="16" height="16" fill="url(#p)"/>"#.repeat(5)
            )),
        ),
    ] {
        let opened = open(document);
        assert!(
            matches!(&opened, Err(ExtensionError::Budget(message)) if message.contains("images decode to")),
            "{what}: {:?}",
            opened.err()
        );
    }
    let probed = Registry::with_defaults()
        .probe(svg((0..5).map(image).collect()).as_bytes())
        .expect("claimed");
    assert!(
        matches!(probed, Err(ExtensionError::Budget(_))),
        "{:?}",
        probed.err()
    );
}

#[cfg(feature = "lottie")]
#[test]
fn the_image_assets_a_lottie_animation_holds_are_held_to_four_canvases_together() {
    use base64::Engine as _;
    // Opening decodes every asset and the source holds them all, so their number is bounded too.
    let canvas = declared_png(7_680, 4_320);
    let animation = |count: usize| -> Arc<[u8]> {
        let mut json: serde_json::Value =
            serde_json::from_slice(&bytes("moving.json")).expect("moving.json");
        json["assets"] = (0..count)
            .map(|index| {
                serde_json::json!({
                    "id": format!("canvas{index}"), "w": 16, "h": 16, "e": 1, "u": "",
                    "p": format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&canvas)
                    ),
                })
            })
            .collect();
        serde_json::to_vec(&json).expect("json").into()
    };
    let open = |file: Arc<[u8]>| {
        Registry::with_defaults().open(
            &ProviderSource::Lottie { animation: None },
            Some(file),
            Arc::new(NoResources),
        )
    };
    // Four whole canvases open; these declare their size and hold no picture, so they are
    // reported missing once decoded.
    let mut four = open(animation(4)).expect("four canvases open");
    four.render(ms(0), (64, 48)).expect("frame");
    let five = open(animation(5));
    assert!(
        matches!(&five, Err(ExtensionError::Budget(message)) if message.contains("images decode to")),
        "{:?}",
        five.err()
    );
}

#[cfg(feature = "animated-image")]
#[test]
fn an_animation_is_sized_and_budgeted_from_its_headers_before_anything_is_decoded() {
    let registry = Registry::with_defaults();
    let probe = |file: &[u8]| {
        registry
            .probe(file)
            .expect("claimed")
            .map(|(_, report)| report)
    };
    // A WebP canvas of 2^24 x 2^24 asks for petabytes; 65535x65535 GIF and 16384x32767 APNG
    // canvases for gigabytes.
    let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0\xff\xff\xff\xff\xff\xffANIM\x06\0\0\0\0\0\0\0\0\0".to_vec();
    let size = u32::try_from(webp.len() - 8).expect("small");
    webp[4..8].copy_from_slice(&size.to_le_bytes());
    assert!(matches!(probe(&webp), Err(ExtensionError::Budget(_))));
    assert!(matches!(
        probe(&declared_gif(65_535, 65_535, 2, (1, 1))),
        Err(ExtensionError::Budget(_))
    ));
    // 129 whole frames of a 4096x4096 canvas are more than one pass may decode.
    assert!(matches!(
        probe(&declared_gif(4_096, 4_096, 129, (4_096, 4_096))),
        Err(ExtensionError::Budget(_))
    ));
    assert_eq!(
        probe(&declared_gif(4_096, 4_096, 128, (4_096, 4_096)))
            .expect("exactly 2^31 pixels")
            .timing
            .frame_count(),
        128
    );
    assert!(matches!(
        probe(&declared_gif(
            16,
            16,
            crate::MAX_ANIMATION_FRAMES + 1,
            (1, 1)
        )),
        Err(ExtensionError::Budget(_))
    ));

    // A canvas with up to ten thousand 1x1 frames: each frame costs its pixel, not the canvas, so
    // eight times the frames is little more work.
    let canvas = |width, height| {
        move |frames: usize| -> Arc<[u8]> { declared_gif(width, height, frames, (1, 1)).into() }
    };
    let report = probe(&canvas(7_680, 4_320)(crate::MAX_ANIMATION_FRAMES)).expect("probe");
    assert_eq!(report.timing.frame_count(), crate::MAX_ANIMATION_FRAMES);
    let animation = canvas(1_920, 1_080);
    alpha2_scaling_support::assert_growth_below_shared(
        "an animation of 1x1 frames",
        crate::MAX_ANIMATION_FRAMES / alpha2_scaling_support::GROWTH,
        // A canvas a frame would be eight times the work.
        4,
        animation,
        |file| {
            probe(file).expect("probe");
            let mut source = registry
                .open(
                    &ProviderSource::AnimatedImage,
                    Some(Arc::clone(file)),
                    Arc::new(NoResources),
                )
                .expect("open");
            for at in [i64::MAX / 2, 0, 1_000] {
                source
                    .render(MediaTime::from_ticks(at), (0, 0))
                    .expect("frame");
            }
        },
    );
}

#[cfg(feature = "animated-image")]
#[test]
fn a_webp_frame_is_the_rectangle_its_header_declares_within_the_canvas() {
    let registry = Registry::with_defaults();
    // animation.webp's second frame: a 28x20 lossless picture at (2, 2).
    let with_frame = |patch: &dyn Fn(&mut [u8])| -> Arc<[u8]> {
        let mut file = bytes("animation.webp").to_vec();
        assert_eq!(&file[110..114], b"ANMF");
        patch(&mut file[118..134]);
        file.into()
    };
    let size = |width: u8, height: u8| {
        move |header: &mut [u8]| {
            header[6] = width - 1;
            header[9] = height - 1;
        }
    };
    for (width, height) in [(29, 20), (27, 20), (28, 21)] {
        let file = with_frame(&size(width, height));
        assert!(registry.probe(&file).expect("claimed").is_ok());
        let opened = registry.open(
            &ProviderSource::AnimatedImage,
            Some(file),
            Arc::new(NoResources),
        );
        assert!(
            matches!(opened, Err(ExtensionError::Malformed(_))),
            "{width}x{height}: {:?}",
            opened.err()
        );
    }
    // At x = 22 the frame ends at 50, past the 48-pixel canvas.
    let outside = with_frame(&|header: &mut [u8]| header[0] = 11);
    assert!(matches!(
        registry.probe(&outside).expect("claimed"),
        Err(ExtensionError::Malformed(_))
    ));
}
