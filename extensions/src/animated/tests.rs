use super::*;

fn corpus(name: &str) -> Arc<[u8]> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/corpus/extensions")
            .join(name),
    )
    .expect("corpus file")
    .into()
}

#[test]
fn a_restarted_decoder_reads_the_sources_file_without_copying_it() {
    for name in ["animation.gif", "animation.png", "animation.webp"] {
        // Nothing kept: every request decodes.
        let mut source = AnimatedSource::open(corpus(name), 0).expect("open");
        source.frame(2).expect("frame 2");
        source.frame(1).expect("frame 1, a restart");
        assert_eq!(
            Arc::strong_count(&source.shared.bytes),
            2,
            "{name}: the decoder shares the source's file"
        );
        let canvas = 48 * 32 * 4;
        assert_eq!(
            source.resident_bytes(),
            source.shared.bytes.len() + canvas,
            "{name}"
        );
    }
}

/// A GIF of `frames` 4x4 frames, each one color of its own, 100 ms each.
fn numbered_gif(frames: u8) -> Arc<[u8]> {
    let mut file = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut file, 4, 4, &[]).expect("encoder");
        encoder.set_repeat(gif::Repeat::Infinite).expect("repeat");
        for index in 0..frames {
            let mut pixels = [index, 255 - index, index / 2, 255].repeat(16);
            let mut frame = gif::Frame::from_rgba_speed(4, 4, &mut pixels, 30);
            frame.delay = 10;
            encoder.write_frame(&frame).expect("frame");
        }
    }
    file.into()
}

#[test]
fn kept_frames_are_the_ones_used_last_and_counted_once() {
    let file = numbered_gif(40);
    let frame_bytes = 4 * 4 * 4;
    let expected = {
        let mut all = AnimatedSource::open(Arc::clone(&file), usize::MAX).expect("open");
        (0..40)
            .map(|index| all.frame(index).expect("frame"))
            .collect::<Vec<_>>()
    };
    let decodes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let counter = std::rc::Rc::clone(&decodes);
    let _watch = crate::progress::watch_progress(move || counter.set(counter.get() + 1));
    // Four frames kept.
    let mut source = AnimatedSource::open(Arc::clone(&file), 4 * frame_bytes).expect("open");
    let mut play = |name: &str, order: &mut dyn Iterator<Item = usize>| {
        decodes.set(0);
        for index in order {
            let frame = source.frame(index).expect("frame");
            assert_eq!(frame, expected[index], "{name}: frame {index}");
            let kept = source.shared.kept().expect("kept");
            assert_eq!(
                kept.by_use.len(),
                kept.frames.len(),
                "{name}: after frame {index}, every frame kept is ordered by use once"
            );
            assert!(kept.bytes() <= 4 * frame_bytes, "{name}");
        }
        decodes.get()
    };
    assert!(play("forward", &mut (0..40)) <= 40);
    // Backward, a restart decodes forward to the frame and keeps the
    // frames before it: one restart in four requests, not one each.
    let backward = play("backward", &mut (0..40).rev());
    assert!(backward <= 40 * 40 / 4 + 40, "backward decoded {backward}");
    // Two clips ten frames apart: each request is near one kept.
    let interleaved = play(
        "two clips",
        &mut (0..30).flat_map(|index| [index + 10, index]),
    );
    assert!(
        interleaved <= 2 * 40 * 40 / 4 + 80,
        "two clips decoded {interleaved}"
    );
}

#[test]
fn readers_share_what_opening_decoded_and_keep_their_own_decoders() {
    let file = numbered_gif(40);
    let frame_bytes = 4 * 4 * 4;
    let expected = {
        let mut all = AnimatedSource::open(Arc::clone(&file), usize::MAX).expect("open");
        (0..40)
            .map(|index| all.frame(index).expect("frame"))
            .collect::<Vec<_>>()
    };
    let decodes = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let counter = std::rc::Rc::clone(&decodes);
    let _watch = crate::progress::watch_progress(move || counter.set(counter.get() + 1));
    // Eight frames kept, for every reader together.
    let first = AnimatedSource::open(Arc::clone(&file), 8 * frame_bytes).expect("open");
    assert_eq!(decodes.get(), 40, "opening checks every frame once");
    decodes.set(0);
    let mut readers = (0..4)
        .map(|_| first.reader().expect("a reader"))
        .collect::<Vec<_>>();
    assert_eq!(decodes.get(), 0, "a reader decodes nothing to open");
    assert_eq!(Arc::strong_count(&first.shared), 5, "one animation shared");
    assert_eq!(
        Arc::strong_count(&first.shared.bytes),
        2,
        "the test's file and the one copy"
    );
    let time = |index: usize| {
        MediaTime::from_ticks(i64::try_from(index).expect("index") * TICKS_PER_SECOND / 10)
    };
    // Four readers ten frames apart, each playing forward: each decodes up to its first frame
    // once and then only the frames it shows, never restarted by another.
    for step in 0..10 {
        for (reader, source) in readers.iter_mut().enumerate() {
            let index = reader * 10 + step;
            let frame = source.render(time(index), (4, 4)).expect("frame");
            let FramePixels::Rgba8(pixels) = frame.pixels else {
                panic!("RGBA8 frames");
            };
            assert_eq!(pixels, expected[index], "reader {reader} at frame {index}");
        }
    }
    assert!(
        decodes.get() <= 10 + 20 + 30 + 40,
        "four readers decoded {}",
        decodes.get()
    );
    let shared = first.shared_bytes();
    assert_eq!(
        shared,
        file.len() + 8 * frame_bytes,
        "one budget of kept frames"
    );
    for source in &readers {
        assert_eq!(source.shared_bytes(), shared);
        assert_eq!(
            source.resident_bytes(),
            shared + frame_bytes,
            "and its own decoder"
        );
    }
    assert_eq!(
        first.resident_bytes(),
        shared,
        "the first never decoded again"
    );
}

/// One reader decoding never holds what the readers share: another
/// reader is answered a kept frame meanwhile.
#[test]
fn a_reader_decoding_does_not_hold_up_another() {
    let file = numbered_gif(40);
    let first = AnimatedSource::open(file, 8 * 4 * 4 * 4).expect("open");
    let mut decoding = first.reader().expect("a reader");
    let mut other = first.reader().expect("a reader");
    let (entered, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let hold = std::cell::RefCell::new(Some((entered, released)));
        let _watch = crate::progress::watch_progress(move || {
            if let Some((entered, released)) = hold.borrow_mut().take() {
                entered.send(()).ok();
                released.recv().ok();
            }
        });
        // Not kept: decoded from the file's start.
        decoding
            .render(MediaTime::from_ticks(30 * TICKS_PER_SECOND / 10), (4, 4))
            .map(|frame| frame.index)
    });
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the reader started decoding");
    let (done, done_rx) = std::sync::mpsc::channel();
    let kept = std::thread::spawn(move || {
        done.send(
            other
                .render(MediaTime::ZERO, (4, 4))
                .map(|frame| frame.index),
        )
        .ok();
    });
    let answered = done_rx.recv_timeout(std::time::Duration::from_secs(10));
    release.send(()).ok();
    assert_eq!(thread.join().expect("decoding").expect("frame 30"), 30);
    kept.join().expect("kept");
    assert_eq!(
        answered
            .expect("a kept frame while another reader decodes")
            .expect("frame 0"),
        0
    );
}
