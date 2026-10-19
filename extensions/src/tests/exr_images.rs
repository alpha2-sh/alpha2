//! OpenEXR: channels, data windows, compressions, primaries, sequences, and the part a frame reads.

use super::*;

/// A frame's linear half-float pixels as `f32`.
#[cfg(feature = "exr")]
fn linear(frame: &crate::VisualFrame) -> Vec<f32> {
    match &frame.pixels {
        FramePixels::LinearF16(pixels) => pixels
            .iter()
            .map(|bits| half::f16::from_bits(*bits).to_f32())
            .collect(),
        FramePixels::Rgba8(_) => panic!("an OpenEXR frame is linear half floats"),
    }
}

#[cfg(feature = "exr")]
fn exr_source(
    file: Arc<[u8]>,
    primaries: Option<alpha2_model::LinearPrimaries>,
) -> crate::ProbeReport {
    Registry::with_defaults()
        .open(
            &ProviderSource::Exr { primaries },
            Some(file),
            Arc::new(NoResources),
        )
        .expect("open")
        .report()
        .clone()
}

#[cfg(feature = "exr")]
fn exr_frame(file: Arc<[u8]>, primaries: Option<alpha2_model::LinearPrimaries>) -> Vec<f32> {
    let mut source = Registry::with_defaults()
        .open(
            &ProviderSource::Exr { primaries },
            Some(file),
            Arc::new(NoResources),
        )
        .expect("open");
    linear(&source.render(ms(0), (0, 0)).expect("frame"))
}

/// A header holding a channel list besides `channels` behind an attribute whose type starts with a
/// null byte is refused as malformed, every time: the OpenEXR reader reads that type as the null
/// and the text after it, so the walk before it must too, or it ends the header there and leaves
/// the channel lists to the reader's hash order. Without that walk, this file (from the fuzz
/// target) is refused as malformed or as unsupported by chance.
#[cfg(feature = "exr")]
#[test]
fn an_exr_channel_list_behind_a_type_that_starts_with_a_null_is_refused_every_time() {
    use base64::Engine as _;
    let file = base64::engine::general_purpose::STANDARD
        .decode([
        "di8xAQIAAABjaGFubmVscwBjaGxpc3QANwAAAEIAAgAAAAAAAAABAAAAAQAAAEcAAgAAAAAAAAABAAAAAQAAAFIAAg",
        "AAAAAAAAABAAAAAQAAAABjb21pcmVzc2lvAAAAAAAAAGRpaGFufmV3AGJveDJpABAAAAAAAAAAAAAAAAEAMS92AAAA",
        "ZGlzcGxheVdpbmRvdwBib3gyaQAQAAAAAAAAAAAAAAABbgABAAAAAGRhdGFXaW5kb3cAYm94MmkAEAAAAAAAAAAAAA",
        "AAAQAAAAAAAABkaXNwbGF5V2luZG93AGJveDJpABAAAAAAAAAAAP////4AAAAAAAAAbGluZU9yZGVyAGxpbmVPcmRl",
        "cgABAAAAAHBpeGVsxEFzcGVjdFJhdGlvAGZsb2F0AAQAAAAAAAAAV2lubm5lbHMAY2hsaXN0ADcAAABCAAIAAAAAAA",
        "AAAgAAAAEAAABH9f///wAAAAAAAQAAAAEAAABSAAIAAAAAAAAAAQAAAAEAAAAAY29tcHJlc3Npb24AY29tcHJlc3Np",
        "b24AAQAAAABkYXRhV2luZG93AGJveDJpABAAAAAAAAAAAAAAAAEAAAAAAAAAZGloYW5uZWxzAGNobGlzdAA3AAAAQg",
        "AAAAAAAAAAAAABbgABAAcAAGRhdGFXaW5kb3cAYm94PwBBAQAAAAAAPwAAgH8AAAA/AADAfwAAAD8AAAAAAAAAZGEA",
        "AGNoYW4AAAAAAAAAAEEAAQAAAAAAAGxpbmVPcmRlcgBsaW5lT29uAGNvbXI=",
        ]
        .concat())
        .expect("base64");
    let refusals = (0..64)
        .map(|_| crate::VisualProvider::probe(&crate::ExrProvider, &file))
        .map(|probed| match probed {
            Some(Err(error)) => error.to_string(),
            other => panic!("{other:?}"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        refusals.into_iter().collect::<Vec<_>>(),
        ["malformed: a header holds a channel list besides `channels`: `Winnnels`"]
    );
}

/// A header holding a channel list besides `channels` is refused as malformed, every time: the
/// OpenEXR reader checks such attributes in hash order, so without the check this file (from the
/// fuzz target) is refused as malformed or as unsupported by chance, its probe and open
/// disagreeing.
#[cfg(feature = "exr")]
#[test]
fn an_exr_header_with_a_second_channel_list_is_refused_the_same_way_every_time() {
    use base64::Engine as _;
    let file = base64::engine::general_purpose::STANDARD
        .decode([
        "di8xAQIAAABjaGFubmVscwBjaGxpc3QASQAAAEEAAgAAAAAAAAABAAAAAQAAAEIAAgAAAAAAAAABAAAAAQAAAEcAAg",
        "AAAAAAAAABAAAAAQAAAFIAAgAAAAAAAAABAAAAAQAAAABjb21wcmFubmVsc2xzAGNobGlzdABJAAAAQQACAAAAAAAA",
        "AAEAAAACAAAAQgACAAAAAAAAAAEAAAABAAAARwACAAAAAAAAAAEAAAABAAAAUgACAAAAAAAAAAEAAAABAAAAAGNvbX",
        "ByYW5uZWxzAGNobGlzdABJAAAAQQACAAAAAAAAAAEAAABCAAEAAAACAAAAAAAAAAEAAAABAAAARwACAAAAAAAAAAEA",
        "AAABAAAAUgACAAAAAAAAAAEAAAABAAAAAGNvbXByZXNzaW9uAGNvbXByZXNzaW9uAAEAAAAAZGF0YVdpbmRvdwBib3",
        "gyaQAQAAAAAAAAAAAAAAADAAAAAQAAAGRpc3BsYXlXaW4AY2hsaXN0AEkAAABBAAIAAAAAAAAAAQAAAAEAAABCAAIA",
        "AAAAAAAAAQAAAAEAAABHAAIAAAAAAAAAAQAAAAEAAABSAAIAAAAAAAAAAQAAAAEAAAAAY29tcHJlc3Npb24AY29tcH",
        "Jlc3Npb24AAQAAAABkYXRhV2luZG93AGJveDJpABAAAAAAAAAAAAAAAAMAAAABAAAAZGlzcGxheVdpbmRvdwBib3gy",
        "aQAQAAAAAAAAAAAAAAADAAAAAQAAAAAAAQAAAAAAAABsaW4AAEEAAgAAAAAAAAA/AABZ////ZGF0YVc=",
        ]
        .concat())
        .expect("base64");
    let refusals = (0..64)
        .map(|_| crate::VisualProvider::probe(&crate::ExrProvider, &file))
        .map(|probed| match probed {
            Some(Err(error)) => error.to_string(),
            other => panic!("{other:?}"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert!(
        refusals
            .iter()
            .all(|refusal| refusal.starts_with("malformed")),
        "{refusals:?}"
    );
}

#[cfg(feature = "exr")]
#[test]
fn exr_images_keep_values_beyond_one_negative_ones_and_alpha() {
    // tests/corpus/generate/extensions-exr.py: one premultiplied image as 32-bit and half floats.
    let pixels = [
        [2.0, 0.5, -0.25, 1.0],
        [0.0, 0.0, 0.0, 0.0],
        [0.5, 0.5, 0.5, 0.5],
        [4.0, 0.25, 0.125, 1.0],
    ];
    let expected = (0..2)
        .flat_map(|y| (0..4).flat_map(move |x| pixels[(x + y) % 4]))
        .collect::<Vec<f32>>();
    for (name, precision) in [("float.exr", true), ("half.exr", false)] {
        let (_, report) = Registry::with_defaults()
            .probe(&bytes(name))
            .expect("claimed")
            .expect("probe");
        assert_eq!(report.format, Format::Exr, "{name}");
        assert_eq!((report.width, report.height), (4, 2), "{name}");
        assert_eq!(report.timing, Timing::Static, "{name}");
        let color = report.color.expect("an OpenEXR image is linear");
        assert_eq!(color.primaries, Some(alpha2_model::LinearPrimaries::Bt709));
        assert_eq!(color.origin, crate::ColorOrigin::FormatDefault);
        assert_eq!(
            report.diagnostics,
            if precision {
                vec![Diagnostic::ReducedPrecision]
            } else {
                Vec::new()
            },
            "{name}"
        );
        assert_eq!(exr_frame(bytes(name), None), expected, "{name}");
    }
}

#[cfg(feature = "exr")]
#[test]
fn exr_data_windows_sit_in_their_display_window() {
    let report = exr_source(bytes("window.exr"), None);
    assert_eq!((report.width, report.height), (4, 3));
    let frame = exr_frame(bytes("window.exr"), None);
    let pixel =
        |x: usize, y: usize| <[f32; 4]>::try_from(&frame[(y * 4 + x) * 4..][..4]).expect("pixel");
    assert_eq!(pixel(1, 1), [1.0, 0.25, 0.0, 1.0]);
    assert_eq!(pixel(2, 1), [0.5, 0.5, 2.0, 1.0]);
    assert_eq!(pixel(1, 2), [0.0, 1.0, 0.5, 1.0]);
    assert_eq!(pixel(2, 2), [1.0, 0.0, 0.5, 1.0]);
    for (x, y) in [
        (0, 0),
        (3, 0),
        (0, 1),
        (3, 1),
        (0, 2),
        (3, 2),
        (1, 0),
        (2, 0),
    ] {
        assert_eq!(pixel(x, y), [0.0; 4], "({x}, {y})");
    }
}

#[cfg(feature = "exr")]
#[test]
fn exr_non_finite_samples_are_zeroed_and_reported() {
    let report = exr_source(bytes("nonfinite.exr"), None);
    assert!(
        report
            .diagnostics
            .contains(&Diagnostic::NonFiniteSamples { count: 3 })
    );
    assert_eq!(
        exr_frame(bytes("nonfinite.exr"), None),
        [0.0, 0.0, 0.0, 1.0, 0.5, 0.5, 0.5, 1.0]
    );
}

/// An OpenEXR file the exr crate writes: RGB, and A when given, `width`
/// wide, as half floats unless `float`.
#[cfg(feature = "exr")]
fn written_exr(
    width: usize,
    channels: &[(&str, Vec<f32>)],
    float: bool,
    setup: impl FnOnce(
        &mut exr::prelude::Image<
            exr::prelude::Layer<exr::prelude::AnyChannels<exr::prelude::FlatSamples>>,
        >,
    ),
) -> Arc<[u8]> {
    use exr::prelude::{AnyChannel, AnyChannels, FlatSamples, Image, WritableImage as _};
    let list = channels
        .iter()
        .map(|(name, values)| {
            let samples = if float {
                FlatSamples::F32(values.clone())
            } else {
                FlatSamples::F16(
                    values
                        .iter()
                        .map(|value| half::f16::from_f32(*value))
                        .collect(),
                )
            };
            AnyChannel::new(*name, samples)
        })
        .collect();
    let height = channels[0].1.len() / width;
    let mut image = Image::from_channels((width, height), AnyChannels::sort(list));
    setup(&mut image);
    let mut buffer = std::io::Cursor::new(Vec::new());
    image
        .write()
        .non_parallel()
        .to_buffered(&mut buffer)
        .expect("write");
    buffer.into_inner().into()
}

#[cfg(feature = "exr")]
#[test]
fn exr_compressions_and_tiles_read_the_same_pixels() {
    use exr::prelude::{Blocks, Compression, LineOrder, ReadChannels as _, ReadLayers as _, Vec2};
    // Sixteen distinct half floats per channel, over and under the SDR range.
    let ramp = |offset: f32| {
        (0..64)
            .map(|at| (at as f32).mul_add(0.125, offset))
            .collect::<Vec<_>>()
    };
    let channels = [
        ("R", ramp(-1.0)),
        ("G", ramp(0.0)),
        ("B", ramp(2.0)),
        ("A", vec![1.0; 64]),
    ];
    let reference = exr_frame(written_exr(8, &channels, false, |_| {}), None);
    assert_eq!(reference[..4], [-1.0, 0.0, 2.0, 1.0]);
    for compression in [
        Compression::RLE,
        Compression::ZIP1,
        Compression::ZIP16,
        Compression::PIZ,
        Compression::PXR24,
    ] {
        for blocks in [Blocks::ScanLines, Blocks::Tiles(Vec2(4, 4))] {
            let file = written_exr(8, &channels, false, |image| {
                image.layer_data.encoding.compression = compression;
                image.layer_data.encoding.blocks = blocks;
                if blocks == Blocks::ScanLines {
                    image.layer_data.encoding.line_order = LineOrder::Increasing;
                }
            });
            assert_eq!(
                exr_frame(file, None),
                reference,
                "{compression:?} {blocks:?}"
            );
        }
    }
    // The lossy ones lose what they lose; the provider must show exactly what they decode to.
    for compression in [
        Compression::B44,
        Compression::B44A,
        Compression::DWAA(None),
        Compression::DWAB(None),
    ] {
        let file = written_exr(8, &channels, false, |image| {
            image.layer_data.encoding.compression = compression;
            image.layer_data.encoding.line_order = LineOrder::Increasing;
        });
        let decoded = exr::prelude::read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .first_valid_layer()
            .all_attributes()
            .from_buffered(std::io::Cursor::new(file.as_ref()))
            .expect("the exr crate reads it");
        let frame = exr_frame(file, None);
        for (index, channel) in decoded.layer_data.channel_data.list.iter().enumerate() {
            let slot = ["R", "G", "B", "A"]
                .iter()
                .position(|name| channel.name.to_string() == *name)
                .expect("an RGBA channel");
            let values = channel.sample_data.values_as_f32().collect::<Vec<_>>();
            for (at, value) in values.iter().enumerate() {
                assert_eq!(
                    frame[at * 4 + slot],
                    *value,
                    "{compression:?} channel {index} sample {at}"
                );
            }
        }
    }
}

#[cfg(feature = "exr")]
#[test]
fn exr_primaries_come_from_the_file_or_the_project_and_become_bt709() {
    use alpha2_model::LinearPrimaries;
    use exr::meta::attribute::Chromaticities;
    use exr::prelude::Vec2;
    // ACEScg to linear BT.709 with Bradford adaptation, as published by the ACES project (to 4
    // places).
    let ap1 = crate::exr::matrix_to_bt709(LinearPrimaries::AcesAp1).expect("not BT.709");
    let published = [
        [1.7051, -0.6218, -0.0833],
        [-0.1303, 1.1408, -0.0105],
        [-0.0240, -0.1290, 1.1530],
    ];
    for (row, expected) in ap1.iter().zip(published) {
        for (value, expected) in row.iter().zip(expected) {
            assert!((value - expected).abs() < 2e-4, "{ap1:?}");
        }
    }
    assert!(crate::exr::matrix_to_bt709(LinearPrimaries::Bt709).is_none());
    // Each matrix keeps white white.
    for primaries in [
        LinearPrimaries::Bt2020,
        LinearPrimaries::P3D65,
        LinearPrimaries::AcesAp0,
        LinearPrimaries::AcesAp1,
    ] {
        let matrix = crate::exr::matrix_to_bt709(primaries).expect("matrix");
        for row in matrix {
            assert!(
                (row.iter().sum::<f32>() - 1.0).abs() < 1e-4,
                "{primaries:?}"
            );
        }
    }

    let channels = [("R", vec![1.0]), ("G", vec![0.0]), ("B", vec![0.0])];
    let declared = |image: &mut exr::prelude::Image<_>| {
        image.attributes.chromaticities = Some(Chromaticities {
            red: Vec2(0.713, 0.293),
            green: Vec2(0.165, 0.830),
            blue: Vec2(0.128, 0.044),
            white: Vec2(0.32168, 0.33767),
        });
    };
    let file = written_exr(1, &channels, true, declared);
    let color = exr_source(Arc::clone(&file), None).color.expect("color");
    assert_eq!(color.primaries, Some(LinearPrimaries::AcesAp1));
    assert_eq!(color.origin, crate::ColorOrigin::File);
    let red = exr_frame(Arc::clone(&file), None);
    for (value, expected) in red.iter().zip([1.7051, -0.1303, -0.0240, 1.0]) {
        assert!((value - expected).abs() < 2e-3, "{red:?}");
    }
    // The project's primaries override the file's.
    let color = exr_source(Arc::clone(&file), Some(LinearPrimaries::Bt709))
        .color
        .expect("color");
    assert_eq!(color.origin, crate::ColorOrigin::Project);
    assert_eq!(
        exr_frame(file, Some(LinearPrimaries::Bt709)),
        [1.0, 0.0, 0.0, 1.0]
    );
}

#[cfg(feature = "exr")]
#[test]
fn exr_float_samples_beyond_half_range_are_clamped_and_reported() {
    let channels = [
        ("R", vec![1.0e6, 1.0]),
        ("G", vec![-1.0e6, 1.0]),
        ("B", vec![0.5, 1.0]),
    ];
    let file = written_exr(2, &channels, true, |_| {});
    let report = exr_source(Arc::clone(&file), None);
    assert!(
        report
            .diagnostics
            .contains(&Diagnostic::OutOfHalfRange { count: 2 })
    );
    assert!(report.diagnostics.contains(&Diagnostic::ReducedPrecision));
    assert_eq!(exr_frame(file, None)[..4], [65504.0, -65504.0, 0.5, 1.0]);
}

#[cfg(feature = "exr")]
#[test]
fn exr_luminance_images_are_gray_and_other_channels_are_reported() {
    let channels = [("Y", vec![0.25, 3.0]), ("Z", vec![10.0, 20.0])];
    let file = written_exr(2, &channels, false, |_| {});
    let report = exr_source(Arc::clone(&file), None);
    assert!(report.diagnostics.iter().any(|diagnostic| matches!(
        diagnostic,
        Diagnostic::UnsupportedFeature { path, count: 1, .. } if path == "/channels/Z"
    )));
    assert_eq!(
        exr_frame(file, None),
        [0.25, 0.25, 0.25, 1.0, 3.0, 3.0, 3.0, 1.0]
    );
    // Neither RGB nor Y: nothing to show.
    let depth = written_exr(1, &[("Z", vec![1.0])], true, |_| {});
    assert!(matches!(
        Registry::with_defaults().probe(&depth),
        Some(Err(ExtensionError::Unsupported(_)))
    ));
}

#[cfg(all(feature = "exr", feature = "image-sequence"))]
#[test]
fn exr_sequences_play_their_frames_and_refuse_mixed_formats() {
    let files = Files {
        files: [
            "sequence_0001.exr",
            "sequence_0002.exr",
            "sequence_0007.png",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), bytes(name)))
        .collect(),
        fonts: Vec::new(),
    };
    let registry = Registry::with_defaults();
    let exr = |frames: &[Option<&str>], missing, primaries| ProviderSource::ImageSequence {
        frames: frames
            .iter()
            .map(|frame| frame.map(str::to_owned))
            .collect(),
        frame_rate: FrameRate::new(10, 1).expect("rate"),
        start_number: 1,
        missing,
        primaries,
    };
    let mut source = registry
        .open(
            &exr(
                &[Some("sequence_0001.exr"), None, Some("sequence_0002.exr")],
                MissingFrames::Blank,
                None,
            ),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert_eq!(source.report().format, Format::ExrSequence);
    assert!(source.report().color.is_some());
    let value = |source: &mut Box<dyn crate::VisualSource>, time| {
        linear(&source.render(ms(time), (0, 0)).expect("frame"))[..4].to_vec()
    };
    assert_eq!(value(&mut source, 0), [0.25, 0.25, 0.25, 1.0]);
    assert_eq!(value(&mut source, 100), [0.0; 4]);
    assert_eq!(value(&mut source, 200), [0.75, 0.75, 0.75, 1.0]);
    let mut mixed = registry
        .open(
            &exr(
                &[Some("sequence_0001.exr"), Some("sequence_0007.png")],
                MissingFrames::Reject,
                None,
            ),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert!(matches!(
        mixed.render(ms(100), (0, 0)),
        Err(ExtensionError::Mismatch(_))
    ));
    // Primaries are for OpenEXR frames.
    assert!(matches!(
        registry.open(
            &exr(
                &[Some("sequence_0007.png")],
                MissingFrames::Reject,
                Some(alpha2_model::LinearPrimaries::Bt2020)
            ),
            None,
            Arc::new(files.clone())
        ),
        Err(ExtensionError::Mismatch(_))
    ));
}

#[cfg(feature = "exr")]
#[test]
fn exr_reads_only_the_part_it_shows() {
    use exr::prelude::{
        AnyChannel, AnyChannels, FlatSamples, Image, Layer, LayerAttributes, Vec2,
        WritableImage as _,
    };
    let layer = |name: &str, value: f32| {
        let samples = || FlatSamples::F16(vec![half::f16::from_f32(value); 64]);
        Layer::new(
            Vec2(8, 8),
            LayerAttributes::named(name),
            exr::prelude::Encoding::FAST_LOSSLESS,
            AnyChannels::sort(
                ["B", "G", "R"]
                    .into_iter()
                    .map(|channel| AnyChannel::new(channel, samples()))
                    .collect(),
            ),
        )
    };
    let image = Image::from_layers(
        exr::prelude::ImageAttributes::new(exr::prelude::IntegerBounds::from_dimensions((8, 8))),
        vec![layer("beauty", 0.5), layer("other", 2.0)],
    );
    let mut buffer = std::io::Cursor::new(Vec::new());
    image
        .write()
        .non_parallel()
        .to_buffered(&mut buffer)
        .expect("write");
    let mut file = buffer.into_inner();
    // Point every chunk of the second part past the end of the file: only its reader notices.
    let meta =
        exr::meta::MetaData::read_from_buffered(std::io::Cursor::new(&file), false).expect("meta");
    let chunks = meta
        .headers
        .iter()
        .map(|header| header.chunk_count)
        .collect::<Vec<_>>();
    let total = chunks.iter().sum::<usize>();
    let offsets_at = {
        // The offset tables end where the lowest offset they hold begins.
        let read = |at: usize| u64::from_le_bytes(file[at..at + 8].try_into().expect("8 bytes"));
        let mut start = None;
        for at in 0..file.len() - total * 8 {
            let lowest = (0..total)
                .map(|index| read(at + index * 8))
                .min()
                .unwrap_or(0);
            if usize::try_from(lowest).ok() == Some(at + total * 8) {
                start = Some(at);
                break;
            }
        }
        start.expect("offset tables")
    };
    for index in chunks[0]..total {
        let at = offsets_at + index * 8;
        file[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    }
    let (_, report) = Registry::with_defaults()
        .probe(&file)
        .expect("claimed")
        .expect("the first part reads");
    assert!(
        report.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            Diagnostic::UnsupportedFeature { count: 1, .. }
        ))
    );
    let shown = exr_frame(file.into(), None);
    assert_eq!(shown[..4], [0.5, 0.5, 0.5, 1.0]);
}

#[cfg(feature = "exr")]
#[test]
fn exr_chromaticities_that_define_no_color_space_are_left_out_and_reported() {
    use alpha2_model::LinearPrimaries;
    use exr::meta::attribute::Chromaticities;
    use exr::prelude::Vec2;
    let channels = [
        ("R", vec![0.0, 1.0]),
        ("G", vec![0.0, 1.0]),
        ("B", vec![0.0, 1.0]),
    ];
    let d65 = Vec2(0.3127, 0.3290);
    for (what, red, green, blue, white) in [
        (
            "one point",
            Vec2(0.3, 0.3),
            Vec2(0.3, 0.3),
            Vec2(0.3, 0.3),
            Vec2(0.3, 0.3),
        ),
        (
            "one line",
            Vec2(0.2, 0.2),
            Vec2(0.4, 0.4),
            Vec2(0.3, 0.3),
            d65,
        ),
        (
            "all zero",
            Vec2(0.0, 0.0),
            Vec2(0.0, 0.0),
            Vec2(0.0, 0.0),
            Vec2(0.0, 0.0),
        ),
    ] {
        let file = written_exr(2, &channels, false, |image| {
            image.attributes.chromaticities = Some(Chromaticities {
                red,
                green,
                blue,
                white,
            });
        });
        let report = exr_source(Arc::clone(&file), None);
        // OpenEXR's default, as for a file that declares none.
        let color = report.color.expect("color");
        assert_eq!(color.primaries, Some(LinearPrimaries::Bt709), "{what}");
        assert_eq!(color.origin, crate::ColorOrigin::FormatDefault, "{what}");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| matches!(diagnostic, Diagnostic::UnsupportedFeature { path, .. } if path == "/chromaticities")),
            "{what}: {:?}",
            report.diagnostics
        );
        assert_eq!(
            exr_frame(file, None),
            [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            "{what}"
        );
    }
}
