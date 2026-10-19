//! What every extension provider target does with its input, as a host does: probe the bytes, open
//! them as the source the probe describes, and render a few of its frames at a few sizes, then
//! again through another reader of it when it makes one.
//!
//! A provider answers or refuses, never panics: a panic in the browser module aborts it and takes
//! every source it holds with it. Bytes a probe does not claim, opening tells apart; bytes a probe
//! refuses, opening refuses for the same reason; bytes a probe describes open as that source, or
//! are malformed past what the probe reads (a probe reads what describes a file, opening the rest),
//! and a frame is the size and the time it says. `scripts/fuzz.sh` holds each run to a time and a
//! memory limit with libFuzzer's options: the budgets a provider keeps (canvases, pixels, document
//! sizes, painted area, filter cost, reading steps) are what keep a run within them.

use std::sync::Arc;

use alpha2_extensions::{
    ExtensionError, FramePixels, ProbeReport, ResourceResolver, Timing, VisualProvider,
};
use alpha2_model::ProviderSource;
use alpha2_time::MediaTime;

/// The sizes a vector source is drawn to fit: a small odd one, and one past its own for most
/// inputs.
const FITS: [(u32, u32); 2] = [(37, 23), (256, 256)];

/// A resolver with one font registered and nothing else, so text is laid out and drawn.
pub struct OneFont;

impl ResourceResolver for OneFont {
    fn bytes(&self, locator: &str) -> Result<Arc<[u8]>, ExtensionError> {
        Err(ExtensionError::MissingResource(locator.to_owned()))
    }

    fn fonts(&self) -> Vec<Arc<[u8]>> {
        vec![Arc::from(
            &include_bytes!("../../tests/fonts/Alpha2FixtureHangul.ttf")[..],
        )]
    }
}

fn kind(error: &ExtensionError) -> std::mem::Discriminant<ExtensionError> {
    std::mem::discriminant(error)
}

/// Probes, opens, and renders `bytes` with `provider`; `source` is the project source the report
/// describes.
///
/// # Panics
/// Where the provider breaks its contract: probe and open disagree, a frame is not the size or the
/// time it says, or the provider itself panics.
pub fn exercise(
    provider: &dyn VisualProvider,
    bytes: &[u8],
    source: impl Fn(&ProbeReport) -> ProviderSource,
) {
    let resolver = OneFont;
    let Some(probed) = provider.probe(bytes) else {
        // Not this provider's: opening tells them apart the same way.
        let opened = provider.open(Arc::from(bytes), &source_for_none(provider), &resolver);
        assert!(
            matches!(opened, Err(ExtensionError::Mismatch(_))),
            "probe said not mine, open said {:?}",
            opened.err()
        );
        return;
    };
    let report = match probed {
        Ok(report) => report,
        Err(refused) => {
            // A provider that refuses a file at probe refuses to open it, for the same reason.
            let guess = source_for_none(provider);
            let opened = provider.open(Arc::from(bytes), &guess, &resolver);
            let error = opened
                .err()
                .unwrap_or_else(|| panic!("probe refused ({refused}), open opened"));
            assert_eq!(
                kind(&error),
                kind(&refused),
                "probe: {refused}; open: {error}"
            );
            return;
        }
    };
    let mut opened = match provider.open(Arc::from(bytes), &source(&report), &resolver) {
        Ok(opened) => opened,
        // A probe reads what describes a file and opening reads the rest, which may be malformed;
        // any other refusal is one the probe should have made.
        Err(ExtensionError::Malformed(_)) => return,
        Err(error) => panic!("probe described {:?}, open refused: {error}", report.format),
    };
    let described = opened.report().clone();
    assert_eq!(
        (
            described.format,
            described.width,
            described.height,
            &described.timing
        ),
        (report.format, report.width, report.height, &report.timing),
        "open describes another source than probe"
    );
    let count = report.timing.frame_count();
    let picks = [0, count / 2, count.saturating_sub(1)];
    let mut drawn = Vec::new();
    for (pick, fit) in picks.iter().zip(FITS.iter().cycle()) {
        let time = start_of(&report.timing, *pick);
        let Ok(frame) = opened.render(time, *fit) else {
            // A source may refuse a frame (drawn past a budget at that size): typed, not a panic.
            continue;
        };
        assert!(frame.index < count, "frame {} of {count}", frame.index);
        assert!(
            frame.start <= time,
            "frame starts after the time it answers"
        );
        assert!(
            frame.end.is_none_or(|end| time < end),
            "frame ends before the time it answers"
        );
        let pixels = u64::from(frame.width) * u64::from(frame.height) * 4;
        let held = match &frame.pixels {
            FramePixels::Rgba8(bytes) => bytes.len() as u64,
            FramePixels::LinearF16(halves) => halves.len() as u64,
            _ => pixels,
        };
        assert_eq!(held, pixels, "a {}x{} frame", frame.width, frame.height);
        drawn.push((time, *fit, frame));
    }
    // Another reader of the open source, one a host gives each clip of an animation, draws the
    // same frames from what opening decoded, asked for them in another order.
    if let Some(mut reader) = opened.reader() {
        for (time, fit, frame) in drawn.iter().rev() {
            let again = reader.render(*time, *fit).unwrap_or_else(|error| {
                panic!("a reader refused a frame the source drew: {error}")
            });
            assert!(
                again == *frame,
                "a reader drew frame {} where the source drew frame {}",
                again.index,
                frame.index
            );
        }
    }
}

/// The source a host would open bytes no probe described as.
fn source_for_none(provider: &dyn VisualProvider) -> ProviderSource {
    match provider.name() {
        "svg" => ProviderSource::Svg,
        "lottie" => ProviderSource::Lottie { animation: None },
        "exr" => ProviderSource::Exr { primaries: None },
        _ => ProviderSource::AnimatedImage,
    }
}

/// When frame `index` of `timing` is first shown.
fn start_of(timing: &Timing, index: usize) -> MediaTime {
    match timing {
        Timing::Frames { durations } => {
            MediaTime::from_ticks(durations.iter().take(index).sum::<i64>())
        }
        Timing::Rate { rate, .. } => i64::try_from(index)
            .ok()
            .and_then(|index| rate.frame_start(index))
            .unwrap_or(MediaTime::ZERO),
        _ => MediaTime::ZERO,
    }
}
