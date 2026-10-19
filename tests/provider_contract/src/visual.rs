//! The visual provider contract, checked against
//! `tests/cases/visual-provider-contract.json`.
//!
//! Every extension provider keeps one contract, natively and in the browser
//! module alike (`scripts/test-wasm-browser.mjs` checks the module against
//! the same fixture):
//! - bytes a probe does not claim, opening refuses as another format's;
//! - bytes a probe refuses, opening refuses for the same reason (`too large`,
//!   `malformed`, `unsupported`), typed, never a panic;
//! - bytes a probe describes open as that source, or are malformed past what
//!   the probe reads, and the opened source describes itself as the probe
//!   did;
//! - a frame is the size its source fits the size asked (a raster source its
//!   own), holds that many pixels, and covers the time it answers;
//! - each case of the fixture reaches the conclusion it lists.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_extensions::{
    ExtensionError, FramePixels, NoResources, ProbeReport, Timing, VisualProvider, VisualSource,
};
use alpha2_model::ProviderSource;
use alpha2_time::MediaTime;
use base64::Engine as _;
use serde::Deserialize;

/// The fixture: inputs and what a provider concludes about each.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisualContract {
    /// The cases.
    pub cases: Vec<VisualCase>,
}

/// One input and its conclusion.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VisualCase {
    /// What the case is about.
    pub name: String,
    /// A corpus file, relative to `tests/`.
    #[serde(default)]
    pub file: Option<String>,
    /// The input as text.
    #[serde(default)]
    pub text: Option<String>,
    /// The input as base64.
    #[serde(default)]
    pub base64: Option<String>,
    /// What probing concludes: `ok` (claimed and described), `none` (not
    /// claimed), or the refusal's kind (`too large`, `malformed`,
    /// `unsupported`).
    pub probe: String,
    /// The provider that claims it.
    #[serde(default)]
    pub provider: Option<String>,
    /// What opening concludes, when the probe describes it: `ok`, or a
    /// refusal's kind.
    #[serde(default)]
    pub open: Option<String>,
    /// The size the probe reports.
    #[serde(default)]
    pub size: Option<[u32; 2]>,
    /// How many frames.
    #[serde(default)]
    pub frames: Option<usize>,
    /// Renders of an opened source: the size asked to fit and what answers.
    #[serde(default)]
    pub renders: Vec<VisualRender>,
}

/// One render of a case.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VisualRender {
    /// Which frame: its index.
    pub frame: usize,
    /// The size asked to fit (`[0, 0]` for the source's own).
    pub fit: [u32; 2],
    /// The frame's size, or `None` when the render is refused.
    #[serde(default)]
    pub size: Option<[u32; 2]>,
    /// The refusal's kind.
    #[serde(default)]
    pub refused: Option<String>,
}

impl VisualContract {
    /// Reads `tests/cases/visual-provider-contract.json`.
    ///
    /// # Panics
    /// Panics when the fixture is missing or malformed.
    #[must_use]
    pub fn load() -> Self {
        let path = tests_dir()
            .join("cases")
            .join("visual-provider-contract.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }
}

impl VisualCase {
    /// The input's bytes.
    ///
    /// # Panics
    /// Panics when the case names none, or its file is missing.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        match (&self.file, &self.text, &self.base64) {
            (Some(file), None, None) => {
                let path = tests_dir().join(file);
                std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            }
            (None, Some(text), None) => text.as_bytes().to_vec(),
            (None, None, Some(encoded)) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_else(|error| panic!("{}: {error}", self.name)),
            _ => panic!("{}: one of file, text, base64", self.name),
        }
    }
}

/// A refusal's kind, as the browser module's message starts with it.
#[must_use]
pub fn kind(error: &ExtensionError) -> &'static str {
    match error {
        ExtensionError::Unsupported(_) => "unsupported",
        ExtensionError::Malformed(_) => "malformed",
        ExtensionError::Budget(_) => "too large",
        ExtensionError::MissingResource(_) => "missing resource",
        ExtensionError::Mismatch(_) => "mismatch",
        _ => "other",
    }
}

/// The source a host opens bytes a provider describes as.
#[must_use]
pub fn source_for(provider: &str, report: Option<&ProbeReport>) -> ProviderSource {
    match provider {
        "svg" => ProviderSource::Svg,
        "lottie" => ProviderSource::Lottie {
            animation: report.and_then(|report| report.item.clone()),
        },
        "exr" => ProviderSource::Exr { primaries: None },
        _ => ProviderSource::AnimatedImage,
    }
}

/// When frame `index` of `timing` starts.
#[must_use]
pub fn frame_time(timing: &Timing, index: usize) -> MediaTime {
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

/// What `providers`, in priority order, conclude about `bytes`, keeping the
/// contract (see the module) on the way; in the fixture's terms.
///
/// # Panics
/// Panics, naming `what`, where a provider breaks the contract.
#[must_use]
pub fn conclude(
    providers: &[&dyn VisualProvider],
    bytes: &[u8],
    renders: &[(usize, [u32; 2])],
    what: &str,
) -> Conclusion {
    let shared: Arc<[u8]> = Arc::from(bytes);
    let mut conclusion = Conclusion {
        probe: "none".into(),
        ..Conclusion::default()
    };
    let Some((provider, probed)) = providers
        .iter()
        .find_map(|provider| provider.probe(bytes).map(|probed| (*provider, probed)))
    else {
        // Every provider tells bytes that are not its own apart when it opens them.
        for provider in providers {
            let source = source_for(provider.name(), None);
            let opened = provider.open(Arc::clone(&shared), &source, &NoResources);
            assert!(
                matches!(opened, Err(ExtensionError::Mismatch(_))),
                "{what}: {} claims no bytes it opens: {:?}",
                provider.name(),
                opened.err()
            );
        }
        return conclusion;
    };
    conclusion.provider = Some(provider.name().to_owned());
    let report = match probed {
        Ok(report) => report,
        Err(refused) => {
            kind(&refused).clone_into(&mut conclusion.probe);
            let source = source_for(provider.name(), None);
            let opened = provider.open(Arc::clone(&shared), &source, &NoResources);
            let error = opened
                .err()
                .unwrap_or_else(|| panic!("{what}: probe refused ({refused}), open opened"));
            assert_eq!(
                kind(&error),
                kind(&refused),
                "{what}: probe {refused}, open {error}"
            );
            return conclusion;
        }
    };
    conclusion.probe = "ok".into();
    conclusion.size = Some([report.width, report.height]);
    conclusion.frames = Some(report.timing.frame_count());
    let source = source_for(provider.name(), Some(&report));
    let mut opened = match provider.open(Arc::clone(&shared), &source, &NoResources) {
        Ok(opened) => opened,
        Err(error) => {
            assert!(
                matches!(error, ExtensionError::Malformed(_)),
                "{what}: probe described it, open refused it as {error}"
            );
            conclusion.open = Some(kind(&error).to_owned());
            return conclusion;
        }
    };
    conclusion.open = Some("ok".into());
    let described = opened.report();
    assert_eq!(
        (
            described.format,
            described.width,
            described.height,
            &described.timing
        ),
        (report.format, report.width, report.height, &report.timing),
        "{what}: open describes another source than probe"
    );
    conclusion.renders = render_all(&mut *opened, &report, renders, what);
    conclusion
}

/// Renders each of `renders` of a source the probe described as `report`,
/// holding each frame to the contract.
fn render_all(
    opened: &mut dyn VisualSource,
    report: &ProbeReport,
    renders: &[(usize, [u32; 2])],
    what: &str,
) -> Vec<RenderAnswer> {
    let mut answers = Vec::new();
    let count = report.timing.frame_count();
    for &(asked, fit) in renders {
        // Past the last frame: the last.
        let frame = asked.min(count - 1);
        let time = frame_time(&report.timing, frame);
        match opened.render(time, (fit[0], fit[1])) {
            Ok(rendered) => {
                assert_eq!(rendered.index, frame, "{what}: frame {frame}");
                assert!(rendered.start <= time, "{what}: frame {frame} starts late");
                assert!(
                    rendered.end.is_none_or(|end| time < end),
                    "{what}: frame {frame} ends early"
                );
                let size = [rendered.width, rendered.height];
                assert!(
                    fits(report, fit, size),
                    "{what}: frame {frame} at {fit:?} is {size:?}"
                );
                let pixels = u64::from(rendered.width) * u64::from(rendered.height) * 4;
                let held = match &rendered.pixels {
                    FramePixels::Rgba8(bytes) => bytes.len() as u64,
                    FramePixels::LinearF16(halves) => halves.len() as u64,
                    _ => pixels,
                };
                assert_eq!(held, pixels, "{what}: frame {frame} at {fit:?} holds");
                answers.push((asked, fit, Ok(size)));
            }
            Err(error) => {
                assert!(
                    matches!(error, ExtensionError::Budget(_)),
                    "{what}: frame {frame} at {fit:?} refused as {error}"
                );
                answers.push((asked, fit, Err(kind(&error).to_owned())));
            }
        }
    }
    answers
}

/// Whether a frame of `size` is what a source the probe described renders
/// to fit `fit`: a raster source at its own size, a vector source (and any
/// at `[0, 0]`) at the largest whole-pixel size within `fit` that keeps its
/// shape, give or take the pixel rounding takes.
fn fits(report: &ProbeReport, fit: [u32; 2], size: [u32; 2]) -> bool {
    let own = [report.width, report.height];
    let vector = matches!(
        report.format,
        alpha2_extensions::Format::Svg
            | alpha2_extensions::Format::Lottie
            | alpha2_extensions::Format::DotLottie
    );
    if !vector || fit[0] == 0 || fit[1] == 0 {
        return size
            .iter()
            .zip(own)
            .all(|(side, own)| side.abs_diff(own) <= 1);
    }
    let [width, height] = size.map(u64::from);
    let [own_width, own_height] = own.map(u64::from);
    size[0] <= fit[0]
        && size[1] <= fit[1]
        && (fit[0] - size[0] <= 1 || fit[1] - size[1] <= 1)
        && (width * own_height).abs_diff(height * own_width) <= own_width.max(own_height)
}

/// A render's frame, the size asked, and its size or refusal's kind.
pub type RenderAnswer = (usize, [u32; 2], Result<[u32; 2], String>);

/// What a provider concluded, in the fixture's terms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conclusion {
    /// `ok`, `none`, or the refusal's kind.
    pub probe: String,
    /// The provider that claims the bytes.
    pub provider: Option<String>,
    /// `ok` or the refusal's kind.
    pub open: Option<String>,
    /// The size the probe reports.
    pub size: Option<[u32; 2]>,
    /// How many frames.
    pub frames: Option<usize>,
    /// Each render: its frame, the size asked, and its size or refusal.
    pub renders: Vec<RenderAnswer>,
}

impl VisualCase {
    /// What the case says a provider concludes.
    #[must_use]
    pub fn conclusion(&self) -> Conclusion {
        Conclusion {
            probe: self.probe.clone(),
            provider: self.provider.clone(),
            open: self.open.clone(),
            size: self.size,
            frames: self.frames,
            renders: self
                .renders
                .iter()
                .map(|render| {
                    (
                        render.frame,
                        render.fit,
                        render.size.ok_or_else(|| {
                            render.refused.clone().unwrap_or_else(|| "refused".into())
                        }),
                    )
                })
                .collect(),
        }
    }

    /// The renders the case asks for.
    #[must_use]
    pub fn render_requests(&self) -> Vec<(usize, [u32; 2])> {
        self.renders
            .iter()
            .map(|render| (render.frame, render.fit))
            .collect()
    }
}

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .to_owned()
}
