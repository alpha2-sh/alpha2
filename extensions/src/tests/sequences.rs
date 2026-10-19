//! Image sequences: their rate, gaps, frame sizes, and when a frame is read.

use super::*;

#[cfg(feature = "image-sequence")]
#[test]
fn image_sequences_play_at_their_rate_and_show_gaps_as_they_say() {
    let registry = Registry::with_defaults();
    let files = sequence_files();
    let reference = std::fs::read(corpus("animation.png.ref.rgba")).expect("reference");
    let frame_bytes = 48 * 32 * 4;
    let expected = |index: usize| {
        // The sequence's files are the animation's source frames; frame 0 composites to itself.
        let (_, _, mut pixels) =
            crate::raster::decode_png(&bytes(&format!("sequence_{:04}.png", index + 7)))
                .expect("png");
        premultiply(&mut pixels);
        pixels
    };
    let mut first = reference[..frame_bytes].to_vec();
    premultiply(&mut first);
    assert_eq!(
        expected(0),
        first,
        "the fixture's first frame is the animation's"
    );

    let mut whole = registry
        .open(
            &sequence(
                &[
                    Some("sequence_0007.png"),
                    Some("sequence_0008.png"),
                    Some("sequence_0009.png"),
                    Some("sequence_0010.png"),
                ],
                MissingFrames::Reject,
            ),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert_eq!(whole.report().format, Format::PngSequence);
    // 10 fps: frame 2 covers [0.2 s, 0.3 s).
    let frame = whole.render(ms(250), (0, 0)).expect("frame");
    assert_eq!(
        (frame.index, frame.start, frame.end),
        (2, ms(200), Some(ms(300)))
    );
    assert_eq!(frame.pixels, FramePixels::Rgba8(expected(2).into()));

    let gap = [Some("sequence_0007.png"), None, Some("sequence_0009.png")];
    let mut held = registry
        .open(
            &sequence(&gap, MissingFrames::Hold),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert_eq!(
        held.render(ms(150), (0, 0)).expect("frame").pixels,
        FramePixels::Rgba8(expected(0).into())
    );
    let mut blank = registry
        .open(
            &sequence(&gap, MissingFrames::Blank),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert!(
        blank
            .render(ms(150), (0, 0))
            .expect("frame")
            .pixels
            .rgba8()
            .expect("rgba8")
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(matches!(
        registry.open(
            &sequence(&gap, MissingFrames::Reject),
            None,
            Arc::new(files.clone())
        ),
        Err(ExtensionError::Mismatch(_))
    ));
    // A frame that is not there to read fails where it shows: frames are read as they are
    // shown.
    let mut gone = registry
        .open(
            &sequence(
                &[Some("sequence_0007.png"), Some("gone.png")],
                MissingFrames::Reject,
            ),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert!(gone.render(ms(0), (0, 0)).is_ok());
    assert!(matches!(
        gone.render(ms(100), (0, 0)),
        Err(ExtensionError::MissingResource(locator)) if locator == "gone.png"
    ));
}

#[cfg(feature = "image-sequence")]
#[test]
fn a_sequence_frame_of_another_size_is_refused() {
    let mut files = sequence_files();
    // The SVG rasterized is not a PNG; a PNG of another size is: a 1x1 one.
    let mut tiny = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut tiny, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(&[1, 2, 3, 255]).expect("data");
    }
    files.files.insert("tiny.png".into(), tiny.into());
    let mut source = Registry::with_defaults()
        .open(
            &sequence(
                &[Some("sequence_0007.png"), Some("tiny.png")],
                MissingFrames::Reject,
            ),
            None,
            Arc::new(files.clone()),
        )
        .expect("open");
    assert!(source.render(ms(0), (0, 0)).is_ok());
    assert!(matches!(
        source.render(ms(100), (0, 0)),
        Err(ExtensionError::Mismatch(_))
    ));
}

/// Sequence frames counted as they are read.
#[cfg(feature = "image-sequence")]
struct Counted {
    files: Files,
    reads: std::sync::Mutex<Vec<String>>,
}

#[cfg(feature = "image-sequence")]
impl Counted {
    fn taken(&self) -> Vec<String> {
        std::mem::take(&mut *self.reads.lock().expect("reads"))
    }
}

#[cfg(feature = "image-sequence")]
impl ResourceResolver for Counted {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        self.reads.lock().expect("reads").push(locator.to_owned());
        self.files.bytes(locator)
    }
}

#[test]
#[cfg(feature = "image-sequence")]
fn a_sequence_reads_a_frame_when_it_shows_it() {
    // Opening a sequence must not read every frame's bytes and keep them as long as the source,
    // or its memory would be the sum of its files.
    let counted = Arc::new(Counted {
        files: sequence_files(),
        reads: std::sync::Mutex::new(Vec::new()),
    });
    let gap = sequence(
        &[
            Some("sequence_0007.png"),
            None,
            Some("sequence_0009.png"),
            Some("sequence_0010.png"),
        ],
        MissingFrames::Hold,
    );
    let registry = Registry::with_defaults();
    assert_eq!(registry.open_needs(&gap), ["sequence_0007.png"]);
    let mut source = registry.open(&gap, None, counted.clone()).expect("open");
    assert_eq!(
        counted.taken(),
        ["sequence_0007.png"],
        "opening reads the first frame, once"
    );
    // The held gap shows frame 0, which the source holds: nothing to read.
    assert_eq!(source.needs(ms(150)), [] as [String; 0]);
    source.render(ms(150), (0, 0)).expect("frame");
    assert_eq!(source.needs(ms(250)), ["sequence_0009.png"]);
    source.render(ms(250), (0, 0)).expect("frame");
    assert_eq!(counted.taken(), ["sequence_0009.png"]);
    assert!(source.needs(ms(250)).is_empty(), "a frame decoded is kept");

    // Decoded frames are kept within a byte budget: one 48x32 RGBA8 frame here.
    let mut tight = crate::sequence::open_within(
        &[
            Some("sequence_0007.png".into()),
            Some("sequence_0008.png".into()),
        ],
        FrameRate::new(10, 1).expect("rate"),
        MissingFrames::Reject,
        None,
        counted.clone(),
        48 * 32 * 4,
    )
    .expect("open");
    counted.taken();
    tight.render(ms(100), (0, 0)).expect("frame 1");
    assert_eq!(tight.needs(ms(0)), ["sequence_0007.png"], "frame 0 left");
    tight.render(ms(0), (0, 0)).expect("frame 0");
    assert_eq!(counted.taken(), ["sequence_0008.png", "sequence_0007.png"]);
}
