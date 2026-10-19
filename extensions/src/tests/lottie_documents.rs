//! Lottie and dotLottie: evaluation, unsupported features, package limits, image assets, and precompositions.

use super::*;

#[cfg(feature = "lottie")]
#[test]
fn lottie_animations_are_evaluated_at_their_own_frames() {
    let registry = Registry::with_defaults();
    let file = bytes("moving.json");
    let (provider, report) = registry.probe(&file).expect("claimed").expect("probe");
    assert_eq!(provider, "lottie");
    assert_eq!(
        (report.format, report.width, report.height),
        (Format::Lottie, 64, 48)
    );
    assert_eq!(
        report.timing,
        Timing::Rate {
            rate: FrameRate::new(10, 1).expect("rate"),
            frames: 10
        }
    );
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    let mut source = registry
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(file),
            Arc::new(NoResources),
        )
        .expect("open");
    // Frame 0: the red box is centred at x = 16.
    let first = source.render(ms(0), (0, 0)).expect("frame");
    assert_eq!((first.width, first.height, first.index), (64, 48, 0));
    assert_eq!(pixel(&first, 16, 24), [255, 0, 0, 255]);
    assert_eq!(pixel(&first, 44, 24), [0, 0, 0, 0]);
    // 0.95 s is frame 9: the box has moved to x = 16 + 32 * 0.9 = 44.8.
    let late = source.render(ms(950), (0, 0)).expect("frame");
    assert_eq!((late.index, late.start, late.end), (9, ms(900), None));
    assert_eq!(pixel(&late, 44, 24), [255, 0, 0, 255]);
    assert_eq!(pixel(&late, 16, 24), [0, 0, 0, 0]);
    // Vector: rasterized to fit, keeping its 4:3 shape.
    let big = source.render(ms(0), (256, 256)).expect("frame");
    assert_eq!((big.width, big.height), (256, 192));
    assert_eq!(pixel(&big, 64, 96), [255, 0, 0, 255]);
}

#[cfg(feature = "lottie")]
#[test]
fn lottie_features_the_backend_leaves_out_are_reported_and_what_stops_it_refused() {
    let registry = Registry::with_defaults();
    let (_, report) = registry
        .probe(&bytes("unsupported.json"))
        .expect("claimed")
        .expect("probe");
    let features = report
        .diagnostics
        .iter()
        .map(|diagnostic| match diagnostic {
            Diagnostic::UnsupportedFeature {
                feature,
                path,
                count,
            } => (feature.as_str(), path.as_str(), *count),
            other => panic!("{other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        features,
        [
            ("layer effects", "/layers/1", 1),
            ("text layers", "/layers/0", 1)
        ]
    );
    // What is left out does not keep the rest from being drawn.
    let mut source = registry
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(bytes("unsupported.json")),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(source.report().diagnostics, report.diagnostics);
    assert_eq!(
        pixel(&source.render(ms(0), (0, 0)).expect("frame"), 32, 24),
        [0, 0, 255, 255]
    );
    // A shape group's split position would stop the backend: it is refused before velato reads it.
    assert!(matches!(
        registry.probe(&bytes("split.json")),
        Some(Err(ExtensionError::Unsupported(message))) if message.contains("split positions")
    ));
}

#[cfg(feature = "lottie")]
#[test]
fn lottie_velato_cannot_read_safely_is_rewritten_or_refused_before_it_reads_it() {
    // tests/corpus/generate/extensions-lottie.py: without normalization, each would panic velato,
    // recurse without end, expand without bound, or keep a whole document from opening.
    let cases: Vec<serde_json::Value> =
        serde_json::from_slice(&bytes("normalize-cases.json")).expect("cases");
    let registry = Registry::with_defaults();
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let file: Arc<[u8]> = serde_json::to_vec(&case["animation"])
            .expect("animation")
            .into();
        let probed = registry.probe(&file).expect("claimed");
        let opened = registry.open(
            &ProviderSource::Lottie { animation: None },
            Some(file),
            Arc::new(NoResources),
        );
        let message = case["message"].as_str().unwrap_or_default();
        let refused = |error: &ExtensionError| match (case["outcome"].as_str(), error) {
            (Some("unsupported"), ExtensionError::Unsupported(text))
            | (Some("malformed"), ExtensionError::Malformed(text))
            | (Some("budget"), ExtensionError::Budget(text)) => text.contains(message),
            _ => false,
        };
        match (case["outcome"].as_str(), probed, opened) {
            (Some("opens"), Ok((_, report)), Ok(mut source)) => {
                let frame = source.render(ms(0), (0, 0)).expect(name);
                assert_eq!(pixel(&frame, 32, 24), [255, 0, 0, 255], "{name}");
                if let Some(expected) = case.get("diagnostics") {
                    let found = report
                        .diagnostics
                        .iter()
                        .map(|diagnostic| match diagnostic {
                            Diagnostic::UnsupportedFeature {
                                feature,
                                path,
                                count,
                            } => serde_json::json!([feature, path, count]),
                            other => panic!("{name}: {other:?}"),
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(&serde_json::Value::from(found), expected, "{name}");
                }
            }
            (_, Err(probe), Err(open)) => assert!(
                refused(&probe) && refused(&open),
                "{name}: probe {probe}, open {open}"
            ),
            (_, probe, open) => panic!("{name}: probe {:?}, open {:?}", probe.err(), open.err()),
        }
    }
}

#[cfg(feature = "lottie")]
/// A dotLottie package whose animation is `megabytes` MB larger than
/// moving.json (JSON whitespace after it), with `others` empty files it
/// does not read, and with the animation's uncompressed size rewritten to
/// `declared`.
fn package_with_padding(megabytes: usize, others: usize, declared: Option<u32>) -> Vec<u8> {
    use std::io::Write as _;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(megabytes >= 4_096);
    writer.start_file("manifest.json", options).expect("entry");
    writer
        .write_all(br#"{"version":"2","animations":[{"id":"moving"}]}"#)
        .expect("write");
    writer.start_file("a/moving.json", options).expect("entry");
    writer.write_all(&bytes("moving.json")).expect("write");
    let spaces = vec![b' '; 1 << 20];
    for _ in 0..megabytes {
        writer.write_all(&spaces).expect("write");
    }
    for index in 0..others {
        writer
            .start_file(format!("i/other{index}.bin"), options)
            .expect("entry");
    }
    let mut package = writer.finish().expect("finish").into_inner();
    if let Some(declared) = declared {
        // Rewrite the animation's uncompressed size, in its local header
        // and its central directory entry; the checksum stays right.
        let named = |package: &[u8], at: usize, length_at: usize, name_at: usize| {
            let length = usize::from(u16::from_le_bytes([
                package[at + length_at],
                package[at + length_at + 1],
            ]));
            &package[at + name_at..at + name_at + length] == b"a/moving.json"
        };
        let mut at = 0;
        while at + 4 <= package.len() {
            let (size_at, length_at, name_at) = match &package[at..at + 4] {
                b"PK\x03\x04" => (22, 26, 30),
                b"PK\x01\x02" => (24, 28, 46),
                _ => {
                    at += 1;
                    continue;
                }
            };
            if named(&package, at, length_at, name_at) {
                package[at + size_at..at + size_at + 4].copy_from_slice(&declared.to_le_bytes());
            }
            at += 4;
        }
    }
    package
}

#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_is_held_to_the_bytes_it_unpacks_not_the_sizes_it_declares() {
    let registry = Registry::with_defaults();
    let lottie = ProviderSource::Lottie { animation: None };
    let small: Arc<[u8]> = package_with_padding(1, 0, None).into();
    assert!(registry.probe(&small).expect("claimed").is_ok());
    // An animation past what a Lottie document may be, 17 MB or 300 MB unpacked: refused before
    // unpacking when the package declares its size, and while unpacking when it does not.
    for megabytes in [17, 300] {
        let bomb: Arc<[u8]> = package_with_padding(megabytes, 0, None).into();
        assert!(bomb.len() < 1 << 20, "{}", bomb.len());
        assert!(
            matches!(
                registry.probe(&bomb),
                Some(Err(ExtensionError::Budget(message)))
                    if message.contains("a/moving.json")
            ),
            "{megabytes} MB"
        );
        assert!(matches!(
            registry.open(&lottie, Some(bomb), Arc::new(NoResources)),
            Err(ExtensionError::Budget(_))
        ));
        let lying: Arc<[u8]> = package_with_padding(megabytes, 0, Some(0)).into();
        assert!(
            matches!(
                registry.probe(&lying),
                Some(Err(ExtensionError::Malformed(message)))
                    if message.contains("declared uncompressed size")
            ),
            "{megabytes} MB, declared 0"
        );
        assert!(matches!(
            registry.open(&lottie, Some(lying), Arc::new(NoResources)),
            Err(ExtensionError::Malformed(_))
        ));
    }
    // One that declares a byte and unpacks past the whole budget is refused, not cut short.
    let one: Arc<[u8]> = package_with_padding(257, 0, Some(1)).into();
    assert!(matches!(
        registry.probe(&one),
        Some(Err(ExtensionError::Malformed(_)))
    ));
    // A package of more files than a dotLottie holds.
    let many: Arc<[u8]> = package_with_padding(0, 1_100, None).into();
    assert!(matches!(
        registry.probe(&many),
        Some(Err(ExtensionError::Budget(message))) if message.contains("files")
    ));
}

/// A package's file count is read from its end of central directory record, the last in the
/// file, before the directory: a package that ends with a comment still opens, and one that
/// lists more files than a package holds, or has no such record, is refused before its
/// directory is read.
#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_is_counted_from_its_end_record() {
    let registry = Registry::with_defaults();
    let mut package = package_of(&[("moving", "moving.json")]).to_vec();
    // The record ends with its comment's length, then the comment.
    let comment = b"written by a packager that leaves a note";
    let end = package.len() - 2;
    package[end..].copy_from_slice(&u16::try_from(comment.len()).expect("short").to_le_bytes());
    package.extend_from_slice(comment);
    let (_, report) = registry
        .probe(&package)
        .expect("claimed")
        .expect("a package with a comment opens");
    assert_eq!(report.item.as_deref(), Some("moving"));
    // The same record claiming 2,000 files.
    let at = package.len() - comment.len() - 22;
    package[at + 8..at + 12].copy_from_slice(&[0xd0, 0x07, 0xd0, 0x07]);
    assert!(matches!(
        registry.probe(&package),
        Some(Err(ExtensionError::Budget(message))) if message.contains("1024 files")
    ));
    // No record at all.
    package.truncate(at);
    assert!(matches!(
        registry.probe(&package),
        Some(Err(ExtensionError::Malformed(message))) if message.contains("end of central directory")
    ));
}

/// A package whose end records describe no directory is refused after reading about twice its
/// size: the ZIP reader tries each earlier record, and a reader that searched forward for its
/// directory each time would cost the square of the package's size. zip 9 refuses it itself;
/// the provider's bound stops a reader that searches more.
#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_of_end_records_that_describe_no_directory_is_refused_in_one_pass() {
    // A local file header, then 2,000 end records each saying one file starts at 0: there is no
    // directory there, so the ZIP reader searches for one, once for each record before it.
    let mut package = b"PK\x03\x04".to_vec();
    package.resize(30, 0);
    for _ in 0..2_000 {
        package.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0\0\0\0\0\0\0\0\0\0\0");
    }
    match crate::VisualProvider::probe(&crate::LottieProvider, &package) {
        Some(Err(ExtensionError::Malformed(message))) => {
            assert!(message.starts_with("dotLottie: "), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

/// The files a dotLottie package unpacks are within 256 MB together: an image the animation
/// shows that would unpack past it is refused before it is unpacked.
#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_is_held_to_what_it_unpacks_in_all() {
    use std::io::Write as _;
    let mut json: serde_json::Value =
        serde_json::from_slice(&image_animation(&halves_png(16, 16), 16, 16)).expect("json");
    json["assets"][0]["u"] = "/i/".into();
    json["assets"][0]["p"] = "halves.png".into();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    writer.start_file("manifest.json", options).expect("entry");
    writer
        .write_all(br#"{"version":"2","animations":[{"id":"one"}]}"#)
        .expect("write");
    writer.start_file("a/one.json", options).expect("entry");
    writer
        .write_all(&serde_json::to_vec(&json).expect("json"))
        .expect("write");
    writer.start_file("i/halves.png", options).expect("entry");
    let zeros = vec![0; 1 << 20];
    for _ in 0..257 {
        writer.write_all(&zeros).expect("write");
    }
    let package: Arc<[u8]> = writer.finish().expect("finish").into_inner().into();
    let registry = Registry::with_defaults();
    // The probe reads the manifest and the animation only.
    assert!(registry.probe(&package).expect("claimed").is_ok());
    assert!(matches!(
        registry.open(
            &ProviderSource::Lottie { animation: None },
            Some(package),
            Arc::new(NoResources)
        ),
        Err(ExtensionError::Budget(message)) if message.contains("256 MB")
    ));
}

/// Lottie JSON, dotLottie and SVG are parsed whole, to many times their size: a file past its
/// format's limit (`MAX_LOTTIE_DOCUMENT_BYTES`, `MAX_PROVIDER_DOCUMENT_BYTES` for a package,
/// `MAX_SVG_DOCUMENT_BYTES`) is refused as too large before it is parsed, by probe and open alike,
/// as a host refuses it from its size before reading it; and bytes no provider reads are still
/// not claimed.
#[cfg(all(feature = "lottie", feature = "svg"))]
#[test]
fn a_document_format_file_past_its_limit_is_refused_before_it_is_parsed() {
    let registry = Registry::with_defaults();
    for (head, source, limit) in [
        (
            &b"PK\x03\x04"[..],
            ProviderSource::Lottie { animation: None },
            alpha2_model::MAX_PROVIDER_DOCUMENT_BYTES,
        ),
        (
            &b"{\"w\":1"[..],
            ProviderSource::Lottie { animation: None },
            alpha2_model::MAX_LOTTIE_DOCUMENT_BYTES,
        ),
        (
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\">"[..],
            ProviderSource::Svg,
            alpha2_model::MAX_SVG_DOCUMENT_BYTES,
        ),
    ] {
        let what = String::from_utf8_lossy(head).into_owned();
        assert_eq!(
            crate::document_limit(&source, head).map(|(most, _)| most),
            Some(limit),
            "{what}"
        );
        let mut large = vec![b' '; limit + 1];
        large[..head.len()].copy_from_slice(head);
        assert!(
            matches!(
                registry.probe(&large),
                Some(Err(ExtensionError::Budget(message)))
                    if message.contains(&format!("more than the {limit}"))
            ),
            "{what}"
        );
        assert!(
            crate::check_document_size(&source, head, limit as u64 + 1).is_err()
                && crate::check_document_size(&source, head, limit as u64).is_ok(),
            "{what}"
        );
        let bytes: Arc<[u8]> = large.into();
        assert!(
            matches!(
                registry.open(&source, Some(bytes), Arc::new(NoResources)),
                Err(ExtensionError::Budget(_))
            ),
            "{what}"
        );
    }
    // Text that is none of them is still no provider's.
    let mut large = vec![b' '; alpha2_model::MAX_PROVIDER_DOCUMENT_BYTES + 1];
    large[..5].copy_from_slice(b"hello");
    assert!(registry.probe(&large).is_none());
}

#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_unpacks_only_the_files_it_reads() {
    let registry = Registry::with_defaults();
    let lottie = ProviderSource::Lottie { animation: None };
    let mut package = package_of(&[("moving", "moving.json")]).to_vec();
    // Another file, past the whole budget once unpacked, is never read.
    let large: Vec<u8> = package_with_padding(300, 0, None);
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&large[..])).expect("zip");
    let mut writer =
        zip::ZipWriter::new_append(std::io::Cursor::new(&mut package)).expect("append");
    writer
        .raw_copy_file_rename(
            archive.by_name("a/moving.json").expect("entry"),
            "i/unused.bin",
        )
        .expect("copy");
    writer.finish().expect("finish");
    let package: Arc<[u8]> = package.into();
    let (_, report) = registry
        .probe(&package)
        .expect("claimed")
        .expect("the probe reads the manifest and the animation only");
    assert_eq!(report.item.as_deref(), Some("moving"));
    let mut source = registry
        .open(&lottie, Some(package), Arc::new(NoResources))
        .expect("the open reads them only");
    assert!(source.render(ms(0), (0, 0)).is_ok());
}

#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_lists_its_animations_and_opens_the_one_chosen() {
    let registry = Registry::with_defaults();
    let package = bytes("pair.lottie");
    let (provider, report) = registry.probe(&package).expect("claimed").expect("probe");
    assert_eq!((provider, report.format), ("lottie", Format::DotLottie));
    assert_eq!(report.items, ["moving", "still"]);
    let mut still = registry
        .open(
            &ProviderSource::Lottie {
                animation: Some("still".into()),
            },
            Some(Arc::clone(&package)),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(
        pixel(&still.render(ms(500), (0, 0)).expect("frame"), 32, 24),
        [0, 0, 255, 255]
    );
    let mut first = registry
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(Arc::clone(&package)),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(
        pixel(&first.render(ms(0), (0, 0)).expect("frame"), 16, 24),
        [255, 0, 0, 255]
    );
    assert!(matches!(
        registry.open(
            &ProviderSource::Lottie {
                animation: Some("gone".into())
            },
            Some(package),
            Arc::new(NoResources)
        ),
        Err(ExtensionError::Mismatch(_))
    ));
}

#[cfg(feature = "lottie")]
#[test]
fn a_lottie_precomposition_layer_shows_its_asset_within_its_own_size() {
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes("moving.json")).expect("moving.json");
    let mut box_layer = json["layers"][0].clone();
    box_layer["ks"]["p"] = serde_json::json!({"a": 0, "k": [32, 24, 0]});
    json["assets"] = serde_json::json!([{"id": "inner", "layers": [box_layer]}]);
    // The box covers x 24 to 40; the precomposition is 30 wide.
    json["layers"] = serde_json::json!([{
        "ddd": 0, "ind": 1, "ty": 0, "nm": "inner", "refId": "inner", "sr": 1,
        "ks": {"o": {"a": 0, "k": 100}, "r": {"a": 0, "k": 0}, "p": {"a": 0, "k": [0, 0, 0]},
               "a": {"a": 0, "k": [0, 0, 0]}, "s": {"a": 0, "k": [100, 100, 100]}},
        "w": 30, "h": 48, "ip": 0, "op": 10, "st": 0
    }]);
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(serde_json::to_vec(&json).expect("json").into()),
            Arc::new(NoResources),
        )
        .expect("open");
    assert!(
        source.report().diagnostics.is_empty(),
        "{:?}",
        source.report().diagnostics
    );
    let frame = source.render(ms(0), (0, 0)).expect("frame");
    assert_eq!(pixel(&frame, 28, 24), [255, 0, 0, 255]);
    assert_eq!(pixel(&frame, 34, 24), [0, 0, 0, 0], "clipped at x = 30");
}

/// The corpus's moving box with its fill replaced by `style`.
#[cfg(feature = "lottie")]
fn box_styled(style: serde_json::Value) -> Arc<[u8]> {
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes("moving.json")).expect("moving.json");
    json["layers"][0]["shapes"][0]["it"][1] = style;
    serde_json::to_vec(&json).expect("json").into()
}

#[cfg(feature = "lottie")]
#[test]
fn a_lottie_stroke_of_no_width_draws_nothing() {
    for width in [
        serde_json::json!({"a": 0, "k": 0}),
        serde_json::json!({"a": 1, "k": [{"t": 0, "s": [0], "i": {"x": [1], "y": [1]}, "o": {"x": [0], "y": [0]}}, {"t": 10, "s": [0]}]}),
        serde_json::json!({"a": 0, "k": -2}),
    ] {
        let file = box_styled(serde_json::json!({
            "ty": "st", "c": {"a": 0, "k": [1, 0, 0, 1]}, "o": {"a": 0, "k": 100},
            "w": width, "lc": 1, "lj": 1, "ml": 4
        }));
        let mut source = Registry::with_defaults()
            .open(
                &ProviderSource::Lottie { animation: None },
                Some(file),
                Arc::new(NoResources),
            )
            .expect("open");
        for fit in [(0, 0), (256, 256)] {
            let frame = source.render(ms(0), fit).expect("frame");
            let painted = frame
                .pixels
                .rgba8()
                .expect("rgba8")
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] != 0)
                .count();
            assert_eq!(painted, 0, "{width} at {fit:?}");
        }
    }
    // A stroke of some width still draws.
    let file = box_styled(serde_json::json!({
        "ty": "st", "c": {"a": 0, "k": [1, 0, 0, 1]}, "o": {"a": 0, "k": 100},
        "w": {"a": 0, "k": 2}, "lc": 1, "lj": 1, "ml": 4
    }));
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(file),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(
        pixel(&source.render(ms(0), (0, 0)).expect("frame"), 8, 24),
        [255, 0, 0, 255]
    );
}

/// Frames of a Lottie animation a test changes, rendered at its own size
/// and to fit two larger ones.
#[cfg(feature = "lottie")]
fn lottie_frames(json: &serde_json::Value) -> Vec<Vec<u8>> {
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(serde_json::to_vec(json).expect("json").into()),
            Arc::new(NoResources),
        )
        .expect("open");
    [0, 500, 950]
        .into_iter()
        .flat_map(|time| [(0, 0), (256, 256), (1920, 1080)].map(|fit| (time, fit)))
        .map(|(time, fit)| {
            let frame = source.render(ms(time), fit).expect("frame");
            frame.pixels.rgba8().expect("rgba8").to_vec()
        })
        .collect()
}

/// A shape, a stroke, and a picture scaled far past the canvas (tiny-skia
/// panics drawing coordinates past about a million pixels)
/// are drawn as the same scaled only as far as it takes to cover the canvas.
#[cfg(feature = "lottie")]
#[test]
fn a_lottie_shape_scaled_far_past_its_canvas_is_drawn_as_it_covers_it() {
    let moving: serde_json::Value =
        serde_json::from_slice(&bytes("moving.json")).expect("moving.json");
    let scaled = |style: Option<serde_json::Value>, size: f64, scale: [f64; 2]| {
        let mut json = moving.clone();
        let group = &mut json["layers"][0]["shapes"][0]["it"];
        group[0]["s"]["k"] = serde_json::json!([size, size]);
        if let Some(style) = style {
            group[1] = style;
        }
        group[2]["s"]["k"] = serde_json::json!(scale);
        json
    };
    let stroke = serde_json::json!({
        "ty": "st", "c": {"a": 0, "k": [0, 0, 1, 1]}, "o": {"a": 0, "k": 100},
        "w": {"a": 0, "k": 2}, "lc": 1, "lj": 1, "ml": 4
    });
    let picture = |scale: f64| {
        let mut json: serde_json::Value =
            serde_json::from_slice(&image_animation(&halves_png(16, 16), 16, 16)).expect("json");
        json["layers"][0]["ks"]["s"]["k"] = serde_json::json!([scale, scale, 100]);
        json
    };
    let cases = [
        // A 1000-unit square a hundred million percent tall.
        (
            "fill",
            scaled(None, 1_000.0, [100.0, 1e8]),
            scaled(None, 1_000.0, [100.0, 1_000.0]),
        ),
        // A 16-unit square's outline stretched wide: its top and bottom cross the canvas.
        (
            "stroke",
            scaled(Some(stroke.clone()), 16.0, [1e8, 100.0]),
            scaled(Some(stroke), 16.0, [10_000.0, 100.0]),
        ),
        ("picture", picture(1e9), picture(1e5)),
    ];
    for (what, far, near) in cases {
        let (far, near) = (lottie_frames(&far), lottie_frames(&near));
        for (index, (far, near)) in far.iter().zip(&near).enumerate() {
            let worst = far
                .iter()
                .zip(near)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                worst <= 2,
                "{what}, frame {index}: differs by up to {worst}"
            );
            assert!(
                far.iter().any(|value| *value != 0),
                "{what}: something is drawn"
            );
        }
    }
}

/// A PNG of `width` by `height` pixels, red on the left half and blue on the right.
#[cfg(feature = "lottie")]
fn halves_png(width: u32, height: u32) -> Vec<u8> {
    let mut file = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut file, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        let pixels = (0..height)
            .flat_map(|_| {
                (0..width).flat_map(move |x| {
                    if x < width / 2 {
                        [255, 0, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    }
                })
            })
            .collect::<Vec<u8>>();
        writer.write_image_data(&pixels).expect("data");
    }
    file
}

/// A Lottie animation of one image layer at (16, 12) showing a `w` by `h`
/// asset whose picture is `png`.
#[cfg(feature = "lottie")]
fn image_animation(png: &[u8], w: u32, h: u32) -> Arc<[u8]> {
    use base64::Engine as _;
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes("moving.json")).expect("moving.json");
    json["assets"] = serde_json::json!([{
        "id": "picture", "w": w, "h": h, "e": 1, "u": "",
        "p": format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)),
    }]);
    json["layers"] = serde_json::json!([{
        "ddd": 0, "ind": 1, "ty": 2, "nm": "picture", "refId": "picture", "sr": 1,
        "ks": {"o": {"a": 0, "k": 100}, "r": {"a": 0, "k": 0}, "p": {"a": 0, "k": [16, 12, 0]},
               "a": {"a": 0, "k": [0, 0, 0]}, "s": {"a": 0, "k": [100, 100, 100]}},
        "ip": 0, "op": 10, "st": 0
    }]);
    serde_json::to_vec(&json).expect("json").into()
}

#[cfg(feature = "lottie")]
#[test]
fn a_lottie_image_asset_is_drawn_at_its_authored_size() {
    let render = |png: &[u8], w: u32, h: u32| {
        let mut source = Registry::with_defaults()
            .open(
                &ProviderSource::Lottie { animation: None },
                Some(image_animation(png, w, h)),
                Arc::new(NoResources),
            )
            .expect("open");
        assert_eq!(source.report().diagnostics, [] as [Diagnostic; 0]);
        source.render(ms(0), (0, 0)).expect("frame")
    };
    let (red, blue, clear) = ([255, 0, 0, 255], [0, 0, 255, 255], [0, 0, 0, 0]);
    // The asset is 16x16 at (16, 12): its left half red, its right half blue, whatever the
    // picture's own size.
    for size in [16, 32, 8] {
        let frame = render(&halves_png(size, size), 16, 16);
        assert_eq!(pixel(&frame, 18, 20), red, "{size}x{size}");
        assert_eq!(pixel(&frame, 29, 20), blue, "{size}x{size}");
        assert_eq!(pixel(&frame, 34, 20), clear, "{size}x{size}");
    }
    // A picture of another shape fits inside, centered: 32x16 in 16x16 is 16x8, at y 16 to 24.
    let frame = render(&halves_png(32, 16), 16, 16);
    assert_eq!(pixel(&frame, 29, 14), clear);
    assert_eq!(pixel(&frame, 29, 20), blue);
    assert_eq!(pixel(&frame, 29, 26), clear);
}

/// A dotLottie package (version 2) of the corpus animations named.
#[cfg(feature = "lottie")]
fn package_of(animations: &[(&str, &str)]) -> Arc<[u8]> {
    use std::io::Write as _;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    let ids = animations
        .iter()
        .map(|(id, _)| serde_json::json!({ "id": id }))
        .collect::<Vec<_>>();
    writer.start_file("manifest.json", options).expect("entry");
    writer
        .write_all(
            &serde_json::to_vec(&serde_json::json!({"version": "2", "animations": ids}))
                .expect("json"),
        )
        .expect("write");
    for (id, file) in animations {
        writer
            .start_file(format!("a/{id}.json"), options)
            .expect("entry");
        writer.write_all(&bytes(file)).expect("write");
    }
    writer.finish().expect("finish").into_inner().into()
}

#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_is_described_by_its_first_animation_that_opens() {
    let registry = Registry::with_defaults();
    // Its first animation is refused; its second opens.
    let package = package_of(&[("split", "split.json"), ("moving", "moving.json")]);
    let (_, report) = registry
        .probe(&package)
        .expect("claimed")
        .expect("a package with an animation that opens");
    assert_eq!(report.items, ["split", "moving"]);
    assert_eq!(report.item.as_deref(), Some("moving"));
    assert_eq!((report.width, report.height), (64, 48));
    assert!(
        matches!(&report.diagnostics[..], [Diagnostic::UnreadableItem { item, reason }] if item == "split" && reason.contains("split positions")),
        "{:?}",
        report.diagnostics
    );
    let mut source = registry
        .open(
            &ProviderSource::Lottie {
                animation: report.item.clone(),
            },
            Some(Arc::clone(&package)),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(source.report().item.as_deref(), Some("moving"));
    assert_eq!(
        pixel(&source.render(ms(0), (0, 0)).expect("frame"), 16, 24),
        [255, 0, 0, 255]
    );
    // The source that names none shows the first, and says why it cannot.
    assert!(matches!(
        registry.open(
            &ProviderSource::Lottie { animation: None },
            Some(package),
            Arc::new(NoResources)
        ),
        Err(ExtensionError::Unsupported(_))
    ));
    // A package none of whose animations open is refused, for its first.
    let refused = package_of(&[("split", "split.json"), ("again", "split.json")]);
    assert!(matches!(
        registry.probe(&refused),
        Some(Err(ExtensionError::Unsupported(message))) if message.contains("split positions")
    ));
}

/// A ZIP of `entries`, as a dotLottie package is.
#[cfg(feature = "lottie")]
fn zip_of(entries: &[(&str, Vec<u8>)]) -> Arc<[u8]> {
    use std::io::Write as _;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, contents) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .expect("entry");
        writer.write_all(contents).expect("write");
    }
    writer.finish().expect("finish").into_inner().into()
}

#[cfg(feature = "lottie")]
#[test]
fn a_dotlottie_package_of_either_version_draws_its_images() {
    // The image layer of image_animation, its picture a file of the package.
    let animation = |directory: &str| {
        let mut json: serde_json::Value =
            serde_json::from_slice(&image_animation(&halves_png(16, 16), 16, 16)).expect("json");
        json["assets"][0]["u"] = directory.into();
        json["assets"][0]["p"] = "halves.png".into();
        serde_json::to_vec(&json).expect("json")
    };
    let png = halves_png(16, 16);
    for (version, entries) in [
        (
            "1",
            vec![
                (
                    "manifest.json",
                    br#"{"version":"1","animations":[{"id":"one"}]}"#.to_vec(),
                ),
                ("animations/one.json", animation("/images/")),
                ("images/halves.png", png.clone()),
            ],
        ),
        (
            "2",
            vec![
                (
                    "manifest.json",
                    br#"{"version":"2","animations":[{"id":"one"}]}"#.to_vec(),
                ),
                ("a/one.json", animation("/i/")),
                ("i/halves.png", png.clone()),
            ],
        ),
        // An asset that names no directory is found in either.
        (
            "2, no directory",
            vec![
                (
                    "manifest.json",
                    br#"{"version":"2","animations":[{"id":"one"}]}"#.to_vec(),
                ),
                ("a/one.json", animation("")),
                ("i/halves.png", png.clone()),
            ],
        ),
    ] {
        let package = zip_of(&entries);
        let registry = Registry::with_defaults();
        let (_, report) = registry.probe(&package).expect("claimed").expect("probe");
        assert_eq!(report.items, ["one"], "{version}");
        let mut source = registry
            .open(
                &ProviderSource::Lottie { animation: None },
                Some(package),
                Arc::new(NoResources),
            )
            .expect("open");
        assert!(
            source.report().diagnostics.is_empty(),
            "{version}: {:?}",
            source.report().diagnostics
        );
        let frame = source.render(ms(0), (0, 0)).expect("frame");
        assert_eq!(pixel(&frame, 18, 20), [255, 0, 0, 255], "{version}");
        assert_eq!(pixel(&frame, 29, 20), [0, 0, 255, 255], "{version}");
    }
    // A picture the package lacks is reported and left out.
    let package = zip_of(&[
        (
            "manifest.json",
            br#"{"version":"2","animations":[{"id":"one"}]}"#.to_vec(),
        ),
        ("a/one.json", animation("/i/")),
    ]);
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Lottie { animation: None },
            Some(package),
            Arc::new(NoResources),
        )
        .expect("open");
    assert_eq!(
        source.report().diagnostics,
        [Diagnostic::MissingImage {
            href: "/i/halves.png".into()
        }]
    );
    assert_eq!(
        pixel(&source.render(ms(0), (0, 0)).expect("frame"), 18, 20),
        [0, 0, 0, 0]
    );
}
