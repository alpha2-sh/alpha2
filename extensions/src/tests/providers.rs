//! Choosing a provider for a file, the formats a build leaves out, opening, and a provider that panics.

use super::*;

#[cfg(all(
    feature = "svg",
    feature = "animated-image",
    feature = "lottie",
    feature = "exr"
))]
#[test]
fn providers_are_chosen_by_content_and_bytes_must_be_what_the_project_says() {
    let registry = Registry::with_defaults();
    assert_eq!(registry.names(), ["svg", "animated-image", "lottie", "exr"]);
    // A still PNG is the host's image decoder's, not an animation.
    assert!(registry.probe(&bytes("sequence_0007.png")).is_none());
    assert!(registry.probe(b"<html><body></body></html>").is_none());
    // A GIF named in a project as an SVG is refused, not rendered by another provider.
    assert!(matches!(
        registry.open(
            &ProviderSource::Svg,
            Some(bytes("animation.gif")),
            Arc::new(NoResources)
        ),
        Err(ExtensionError::Mismatch(_))
    ));
    assert!(matches!(
        registry.open(
            &ProviderSource::AnimatedImage,
            Some(bytes("shapes.svg")),
            Arc::new(NoResources)
        ),
        Err(ExtensionError::Mismatch(_))
    ));
}

#[test]
fn a_build_reports_the_formats_it_leaves_out_as_unsupported() {
    // Each format is a feature: a build without one opens everything else, and answers that
    // format's sources as unsupported.
    let registry = Registry::with_defaults();
    let sources = [
        (ProviderSource::Svg, cfg!(feature = "svg"), "shapes.svg"),
        (
            ProviderSource::AnimatedImage,
            cfg!(feature = "animated-image"),
            "animation.gif",
        ),
        (
            ProviderSource::Lottie { animation: None },
            cfg!(feature = "lottie"),
            "moving.json",
        ),
        (
            ProviderSource::Exr { primaries: None },
            cfg!(feature = "exr"),
            "half.exr",
        ),
    ];
    for (source, built, file) in sources {
        assert_eq!(registry.supports(&source), built, "{source:?}");
        let opened = registry.open(&source, Some(bytes(file)), Arc::new(NoResources));
        assert_eq!(
            matches!(opened, Err(ExtensionError::Unsupported(_))),
            !built,
            "{source:?}"
        );
    }
    let sequence = ProviderSource::ImageSequence {
        frames: vec![Some("sequence_0007.png".into())],
        frame_rate: FrameRate::new(10, 1).expect("rate"),
        start_number: 7,
        missing: MissingFrames::Reject,
        primaries: None,
    };
    assert_eq!(
        registry.supports(&sequence),
        cfg!(feature = "image-sequence")
    );
    assert_eq!(
        registry.names().len(),
        [
            cfg!(feature = "svg"),
            cfg!(feature = "animated-image"),
            cfg!(feature = "lottie"),
            cfg!(feature = "exr")
        ]
        .into_iter()
        .filter(|built| *built)
        .count()
    );
    let empty = Registry::default();
    assert!(!empty.supports(&ProviderSource::Svg));
    assert!(matches!(
        empty.open(
            &ProviderSource::Svg,
            Some(bytes("shapes.svg")),
            Arc::new(NoResources)
        ),
        Err(ExtensionError::Unsupported(_))
    ));
}

/// A provider that panics where its bytes say: in `probe`, `open`, or a
/// render, as a third-party parser does on a file it does not expect.
struct Panicking {
    renders: Arc<std::sync::atomic::AtomicUsize>,
}

struct PanickingSource {
    report: crate::ProbeReport,
    renders: Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::VisualSource for PanickingSource {
    fn report(&self) -> &crate::ProbeReport {
        &self.report
    }

    fn render(
        &mut self,
        _time: MediaTime,
        _fit: (u32, u32),
    ) -> Result<crate::VisualFrame, ExtensionError> {
        self.renders
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        panic!("not yet implemented: split rotation");
    }

    fn resident_bytes(&self) -> usize {
        0
    }
}

impl crate::VisualProvider for Panicking {
    fn name(&self) -> &'static str {
        "svg"
    }

    fn probe(&self, bytes: &[u8]) -> Option<Result<crate::ProbeReport, ExtensionError>> {
        assert!(
            bytes != b"probe",
            "index out of bounds: the len is 0 but the index is 0"
        );
        Some(Ok(crate::ProbeReport {
            format: Format::Svg,
            width: 2,
            height: 2,
            timing: Timing::Static,
            loop_count: None,
            items: Vec::new(),
            item: None,
            color: None,
            diagnostics: Vec::new(),
        }))
    }

    fn open(
        &self,
        bytes: Arc<[u8]>,
        _source: &ProviderSource,
        _resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn crate::VisualSource>, ExtensionError> {
        assert!(&*bytes != b"open", "assertion failed: arr.len() == 1");
        let report = self.probe(&bytes).expect("claimed")?;
        Ok(Box::new(PanickingSource {
            report,
            renders: Arc::clone(&self.renders),
        }))
    }
}

#[test]
fn a_provider_that_panics_fails_that_file_and_nothing_else() {
    // A panic must not unwind through the host's decode shard or its serve thread, which would
    // stall the export for 40 s or end the process.
    let renders = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(Box::new(Panicking {
        renders: Arc::clone(&renders),
    }));
    let malformed = |error: ExtensionError, panic: &str| match error {
        ExtensionError::Malformed(message) => assert!(
            message.contains("svg provider") && message.contains(panic),
            "{message}"
        ),
        other => panic!("expected a malformed file, got {other:?}"),
    };
    match registry.probe(b"probe") {
        Some(Err(error)) => malformed(error, "index out of bounds"),
        other => panic!("expected the probe to fail, got {other:?}"),
    }
    let open = |bytes: &[u8]| {
        registry.open(
            &ProviderSource::Svg,
            Some(Arc::from(bytes)),
            Arc::new(NoResources),
        )
    };
    malformed(
        open(b"open").err().expect("the open fails"),
        "arr.len() == 1",
    );
    malformed(
        open(b"probe").err().expect("the open's probe fails"),
        "index out of bounds",
    );
    let mut source = open(b"render").expect("the file opens");
    assert_eq!(source.report().width, 2);
    malformed(
        source
            .render(MediaTime::ZERO, (2, 2))
            .expect_err("the render fails"),
        "split rotation",
    );
    // State the panic left behind is not run again: the source answers the same failure.
    malformed(
        source
            .render(MediaTime::ZERO, (2, 2))
            .expect_err("the source stays failed"),
        "split rotation",
    );
    assert_eq!(renders.load(std::sync::atomic::Ordering::Relaxed), 1);
    // The registry still reads other files.
    assert!(registry.probe(b"fine").is_some_and(|found| found.is_ok()));
    assert!(open(b"fine").is_ok());
}

#[cfg(all(feature = "image-sequence", not(feature = "exr")))]
#[test]
fn a_build_without_openexr_refuses_an_openexr_sequence_frame() {
    let files = Files {
        files: [("sequence_0001.exr".to_owned(), bytes("sequence_0001.exr"))]
            .into_iter()
            .collect(),
        fonts: Vec::new(),
    };
    let opened = Registry::with_defaults().open(
        &ProviderSource::ImageSequence {
            frames: vec![Some("sequence_0001.exr".into())],
            frame_rate: FrameRate::new(10, 1).expect("rate"),
            start_number: 1,
            missing: MissingFrames::Reject,
            primaries: None,
        },
        None,
        Arc::new(files),
    );
    assert!(
        matches!(&opened, Err(ExtensionError::Unsupported(message)) if message.contains("OpenEXR")),
        "{:?}",
        opened.err()
    );
}

/// A provider that counts the times it is asked to probe and to open.
struct Counting {
    probes: Arc<std::sync::atomic::AtomicUsize>,
    opens: Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::VisualProvider for Counting {
    fn name(&self) -> &'static str {
        "svg"
    }

    fn probe(&self, _bytes: &[u8]) -> Option<Result<crate::ProbeReport, ExtensionError>> {
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }

    fn open(
        &self,
        _bytes: Arc<[u8]>,
        _source: &ProviderSource,
        _resolver: &dyn ResourceResolver,
    ) -> Result<Box<dyn crate::VisualSource>, ExtensionError> {
        self.opens
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(ExtensionError::Mismatch("counted".into()))
    }
}

#[test]
fn opening_a_source_reads_its_file_once_without_probing_it() {
    // A probe is a whole read (an SVG parse, an OpenEXR decode, a package's files), so opening
    // must not run one only to drop it and read the file again.
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(Box::new(Counting {
        probes: Arc::clone(&probes),
        opens: Arc::clone(&opens),
    }));
    let opened = registry.open(
        &ProviderSource::Svg,
        Some(Arc::from(&b"<svg/>"[..])),
        Arc::new(NoResources),
    );
    assert!(matches!(opened, Err(ExtensionError::Mismatch(_))));
    assert_eq!(opens.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(probes.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[cfg(all(
    feature = "svg",
    feature = "animated-image",
    feature = "lottie",
    feature = "exr"
))]
#[test]
fn a_provider_opens_as_mismatched_exactly_the_bytes_its_probe_does_not_claim() {
    // Opening does not probe first, so each provider's open must tell another format apart as
    // its probe does.
    let registry = Registry::with_defaults();
    let mut files = std::fs::read_dir(corpus(""))
        .expect("corpus")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                ["gif", "png", "webp", "svg", "json", "lottie", "exr"]
                    .contains(&extension.to_str().unwrap_or_default())
            })
        })
        .map(|path| {
            (
                path.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
                Arc::<[u8]>::from(std::fs::read(&path).expect("file")),
            )
        })
        .collect::<Vec<_>>();
    files.push(("still.gif".into(), declared_gif(4, 4, 1, (4, 4)).into()));
    files.push((
        "page.html".into(),
        Arc::from(&b"<html><body></body></html>"[..]),
    ));
    files.push(("empty".into(), Arc::from(&b""[..])));
    let sources = [
        ("svg", ProviderSource::Svg),
        ("animated-image", ProviderSource::AnimatedImage),
        ("lottie", ProviderSource::Lottie { animation: None }),
        ("exr", ProviderSource::Exr { primaries: None }),
    ];
    let mut checked = 0;
    for (name, file) in &files {
        for (provider, source) in &sources {
            let mut only = Registry::default();
            only.register(match *provider {
                "svg" => Box::new(crate::SvgProvider),
                "animated-image" => Box::new(crate::AnimatedImageProvider),
                "lottie" => Box::new(crate::LottieProvider),
                _ => Box::new(crate::ExrProvider),
            });
            let claimed = only.probe(file).is_some();
            let opened = registry.open(source, Some(Arc::clone(file)), Arc::new(NoResources));
            assert_eq!(
                matches!(opened, Err(ExtensionError::Mismatch(_))),
                !claimed,
                "{provider} on {name}: {:?}",
                opened.err()
            );
            checked += 1;
        }
    }
    assert!(checked > 60, "{checked}");
}

#[cfg(all(
    feature = "svg",
    feature = "animated-image",
    feature = "image-sequence",
    feature = "lottie",
    feature = "exr"
))]
#[test]
fn a_frame_a_source_answers_again_shares_its_pixels() {
    // A render shares what the source keeps (a still, a frame it holds) instead of copying it on
    // every preview frame.
    let registry = Registry::with_defaults();
    let opened = |source: ProviderSource, file: Option<&str>, resolver: crate::SharedResolver| {
        registry
            .open(&source, file.map(bytes), resolver)
            .expect("open")
    };
    for (what, mut source) in [
        (
            "svg",
            opened(
                ProviderSource::Svg,
                Some("shapes.svg"),
                Arc::new(NoResources),
            ),
        ),
        (
            "exr",
            opened(
                ProviderSource::Exr { primaries: None },
                Some("half.exr"),
                Arc::new(NoResources),
            ),
        ),
        (
            "lottie",
            opened(
                ProviderSource::Lottie { animation: None },
                Some("moving.json"),
                Arc::new(NoResources),
            ),
        ),
        (
            "animated image",
            opened(
                ProviderSource::AnimatedImage,
                Some("animation.gif"),
                Arc::new(NoResources),
            ),
        ),
        (
            "image sequence",
            opened(
                sequence(
                    &[Some("sequence_0007.png"), Some("sequence_0008.png")],
                    MissingFrames::Reject,
                ),
                None,
                Arc::new(sequence_files()),
            ),
        ),
    ] {
        let first = source.render(ms(0), (64, 64)).expect("frame");
        let again = source.render(ms(0), (64, 64)).expect("frame");
        assert_eq!(first, again, "{what}");
        assert_eq!(buffer(&first), buffer(&again), "{what}: the same pixels");
    }
}
