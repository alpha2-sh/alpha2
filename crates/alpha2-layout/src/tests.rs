use super::*;

#[test]
fn a_picture_is_never_empty_nor_larger_than_a_frame() {
    assert_eq!(PictureSize::new(0, 1), Err(PictureSizeError::Empty));
    assert_eq!(PictureSize::new(1, 0), Err(PictureSizeError::Empty));
    assert_eq!(
        PictureSize::new(alpha2_media::MAX_FRAME_DIMENSION + 1, 1),
        Err(PictureSizeError::TooLarge)
    );
    let size = PictureSize::new(540, 960).expect("a picture");
    assert_eq!((size.width(), size.height()), (540, 960));
    assert_eq!(size.unit().to_bits(), 540.0_f32.to_bits());
}

#[test]
fn font_files_register_by_their_stem_and_name_the_file_they_fail_on() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fonts/Alpha2Fixture.ttf"
    );
    let FontFiles {
        faces: fonts,
        bytes: files,
    } = fonts_from_files(&[fixture]).expect("a font file");
    assert_eq!(fonts.len(), 1);
    assert_eq!(fonts[0].family, "Alpha2Fixture");
    assert_eq!(fonts[0].face_index, 0);
    assert!(Arc::ptr_eq(&fonts[0].bytes, &files[0]));
    let missing = Path::new(fixture).with_file_name("missing.ttf");
    let error = fonts_from_files(&[&missing]).expect_err("no such file");
    assert!(matches!(error, FontFileError::Read { ref path, .. } if *path == missing));
    let license = Path::new(fixture).with_file_name("MANIFEST.md");
    let error = fonts_from_files(&[&license]).expect_err("no font");
    assert!(matches!(
        error,
        FontFileError::Invalid { face_index: 0, .. }
    ));
}

#[test]
fn every_face_of_a_collection_is_registered_in_its_order() {
    let pair = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fonts/Alpha2FixturePair.ttc"
    );
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fonts/Alpha2Fixture.ttf"
    );
    let FontFiles {
        faces: fonts,
        bytes: files,
    } = fonts_from_files(&[fixture, pair]).expect("font files");
    assert_eq!(files.len(), 2);
    assert_eq!(
        fonts
            .iter()
            .map(|font| (font.family.as_str(), font.face_index))
            .collect::<Vec<_>>(),
        [
            ("Alpha2Fixture", 0),
            ("Alpha2FixturePair", 0),
            ("Alpha2FixturePair", 1)
        ]
    );
    assert!(Arc::ptr_eq(&fonts[1].bytes, &files[1]));
    assert!(Arc::ptr_eq(&fonts[2].bytes, &files[1]));
    // The weight picks the collection's Bold face, as the host would draw it.
    let rasterizer = alpha2_text::TextRasterizer::new(fonts).expect("fonts");
    let size = PictureSize::new(400, 200).expect("size");
    let style = |font_weight| alpha2_model::TextStyle {
        font_family: Some("Alpha2 Fixture Pair".into()),
        font_size: 0.25,
        font_weight,
        ..alpha2_model::TextStyle::default()
    };
    let ink = |font_weight| {
        let bounds = measure_text(&rasterizer, "HI", &style(font_weight), size).expect("measures");
        bounds.ink.expect("ink")
    };
    // The Bold face's "I" is wider about the same centre.
    assert!(ink(700).right > ink(400).right);
}
