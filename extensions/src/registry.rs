//! Provider selection by probing bytes in registration order, with no fallback after a failure.
//!
//! Every provider call goes through [`contained`], so a panic in third-party parsing code fails
//! only that file. In the browser module panics abort, and `alpha2/extensions` restarts the module.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use alpha2_model::ProviderSource;
use alpha2_time::MediaTime;

use crate::api::{
    ExtensionError, ProbeReport, SharedResolver, VisualFrame, VisualProvider, VisualSource,
};

/// The providers a host's build includes, highest priority first.
#[derive(Default)]
pub struct Registry {
    providers: Vec<Box<dyn VisualProvider>>,
}

impl Registry {
    /// Every provider this build includes, in order: SVG, animated images, Lottie, OpenEXR.
    #[must_use]
    pub fn with_defaults() -> Self {
        #[allow(unused_mut)]
        let mut registry = Self::default();
        #[cfg(feature = "svg")]
        registry.register(Box::new(crate::SvgProvider));
        #[cfg(feature = "animated-image")]
        registry.register(Box::new(crate::AnimatedImageProvider));
        #[cfg(feature = "lottie")]
        registry.register(Box::new(crate::LottieProvider));
        #[cfg(feature = "exr")]
        registry.register(Box::new(crate::ExrProvider));
        registry
    }

    /// Adds `provider` after those already registered.
    pub fn register(&mut self, provider: Box<dyn VisualProvider>) {
        self.providers.push(provider);
    }

    /// The registered providers' names, in priority order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .map(|provider| provider.name())
            .collect()
    }

    /// The first provider that claims `bytes` and its report, or `None`. A claiming provider's
    /// error is returned as is; no other provider is tried.
    #[must_use]
    pub fn probe(
        &self,
        bytes: &[u8],
    ) -> Option<Result<(&'static str, ProbeReport), ExtensionError>> {
        self.providers.iter().find_map(|provider| {
            let name = provider.name();
            contained(name, || Ok(provider.probe(bytes)))
                .unwrap_or_else(|error| Some(Err(error)))
                .map(|report| report.map(|report| (name, report)))
        })
    }

    /// Whether this build opens `source`.
    #[must_use]
    pub fn supports(&self, source: &ProviderSource) -> bool {
        match source {
            ProviderSource::ImageSequence { .. } => Self::reads_image_sequences(),
            other => self.named(provider_name(other)).is_some(),
        }
    }

    /// Whether this build reads image sequences, which have no registered provider.
    #[must_use]
    pub const fn reads_image_sequences() -> bool {
        cfg!(feature = "image-sequence")
    }

    /// Locators [`Self::open`] reads for `source` (an image sequence's first frame).
    #[must_use]
    pub fn open_needs(&self, source: &ProviderSource) -> Vec<String> {
        match source {
            ProviderSource::ImageSequence { frames, .. } => {
                frames.first().cloned().flatten().into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Opens a project's provider source from its bytes (`None` for an image sequence, whose
    /// frames come from `resolver`).
    ///
    /// Renders go through the same panic boundary. After one render panics, every later render
    /// fails without running provider code on the state the panic left.
    ///
    /// # Errors
    /// [`ExtensionError::Unsupported`] without a provider, [`ExtensionError::Mismatch`] for
    /// another format, or the provider's error ([`ExtensionError::Malformed`] if it panicked).
    #[cfg_attr(not(feature = "image-sequence"), allow(clippy::needless_pass_by_value))]
    pub fn open(
        &self,
        source: &ProviderSource,
        bytes: Option<Arc<[u8]>>,
        resolver: SharedResolver,
    ) -> Result<Box<dyn VisualSource>, ExtensionError> {
        let name = provider_name(source);
        let opened = match source {
            #[cfg(feature = "image-sequence")]
            ProviderSource::ImageSequence {
                frames,
                frame_rate,
                missing,
                primaries,
                ..
            } => contained(name, || {
                crate::open_sequence(frames, *frame_rate, *missing, *primaries, resolver)
            })?,
            other => {
                let provider = self.named(name).ok_or_else(|| {
                    ExtensionError::Unsupported(format!("this build has no {name} provider"))
                })?;
                let bytes = bytes.ok_or_else(|| {
                    ExtensionError::MissingResource("the source's own bytes".into())
                })?;
                // Open detects a format mismatch itself, so no extra probe pass is needed.
                contained(name, || provider.open(bytes, other, &*resolver))?
            }
        };
        Ok(Box::new(Contained {
            name,
            source: opened,
            failed: None,
        }))
    }

    fn named(&self, name: &str) -> Option<&dyn VisualProvider> {
        self.providers
            .iter()
            .find(|provider| provider.name() == name)
            .map(AsRef::as_ref)
    }
}

/// A source whose renders go through the panic boundary.
struct Contained {
    name: &'static str,
    source: Box<dyn VisualSource>,
    /// Set once a render panics, and returned by every later render.
    failed: Option<ExtensionError>,
}

impl VisualSource for Contained {
    fn report(&self) -> &ProbeReport {
        self.source.report()
    }

    fn render(&mut self, time: MediaTime, fit: (u32, u32)) -> Result<VisualFrame, ExtensionError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let source = &mut self.source;
        catch_unwind(AssertUnwindSafe(|| source.render(time, fit))).unwrap_or_else(|payload| {
            let error = panicked(self.name, payload.as_ref());
            self.failed = Some(error.clone());
            Err(error)
        })
    }

    fn needs(&self, time: MediaTime) -> Vec<String> {
        if self.failed.is_some() {
            return Vec::new();
        }
        catch_unwind(AssertUnwindSafe(|| self.source.needs(time))).unwrap_or_default()
    }

    fn resident_bytes(&self) -> usize {
        catch_unwind(AssertUnwindSafe(|| self.source.resident_bytes())).unwrap_or_default()
    }

    /// None after a panic, since shared state may be what the panic left.
    fn reader(&self) -> Option<Box<dyn VisualSource>> {
        if self.failed.is_some() {
            return None;
        }
        let source = catch_unwind(AssertUnwindSafe(|| self.source.reader())).ok()??;
        Some(Box::new(Self {
            name: self.name,
            source,
            failed: None,
        }))
    }

    fn shared_bytes(&self) -> usize {
        catch_unwind(AssertUnwindSafe(|| self.source.shared_bytes())).unwrap_or_default()
    }
}

/// Runs `call`, turning a panic into the file's failure.
fn contained<T>(
    provider: &str,
    call: impl FnOnce() -> Result<T, ExtensionError>,
) -> Result<T, ExtensionError> {
    catch_unwind(AssertUnwindSafe(call))
        .unwrap_or_else(|payload| Err(panicked(provider, payload.as_ref())))
}

fn panicked(provider: &str, payload: &(dyn std::any::Any + Send)) -> ExtensionError {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message");
    ExtensionError::Malformed(format!(
        "the {provider} provider failed on this file (it panicked: {message})"
    ))
}

fn provider_name(source: &ProviderSource) -> &'static str {
    match source {
        ProviderSource::Svg => "svg",
        ProviderSource::AnimatedImage => "animated-image",
        ProviderSource::Lottie { .. } => "lottie",
        ProviderSource::Exr { .. } => "exr",
        _ => "image-sequence",
    }
}
