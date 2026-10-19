//! Which provider reads what: chosen by probing bytes, in the order the host
//! registered providers, and never by retrying another after a failure.
//!
//! Every provider call goes through [`contained`]: a provider parses
//! untrusted files with third-party code (velato, resvg, the image crates),
//! and a panic in it is that file's failure, never its caller's. Where panics
//! unwind (native hosts) that is the whole defense; in the browser module a
//! panic aborts, and `alpha2/extensions` restarts the module instead.

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
    /// A registry of every provider this build includes, in the default
    /// priority: SVG, animated images, Lottie, then OpenEXR.
    #[must_use]
    pub fn with_defaults() -> Self {
        // Without any provider feature nothing is registered.
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

    /// What the first provider that claims `bytes` finds, or `None` when none
    /// does. A provider that claims them and fails answers with its error:
    /// another provider is not tried in its place.
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

    /// Whether this build reads image sequences. They have no provider to
    /// register (each frame is a PNG or an OpenEXR image), so
    /// [`Self::names`] cannot say.
    #[must_use]
    pub const fn reads_image_sequences() -> bool {
        cfg!(feature = "image-sequence")
    }

    /// The resources opening `source` reads, by locator: an image
    /// sequence's first frame. A host that registers resources only while
    /// they are used registers these before [`Self::open`].
    #[must_use]
    pub fn open_needs(&self, source: &ProviderSource) -> Vec<String> {
        match source {
            ProviderSource::ImageSequence { frames, .. } => {
                frames.first().cloned().flatten().into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// Opens a project's provider source: the bytes at its locator (none for
    /// an image sequence, whose frames `resolver` supplies). The bytes must
    /// be what the project says they are. An image sequence keeps
    /// `resolver` and reads each frame from it as it shows it; other
    /// providers read what they need while they open.
    ///
    /// The source renders through the same panic boundary: a render that
    /// panics fails, and so does every later one, without running the
    /// provider's code again on state the panic left behind.
    ///
    /// # Errors
    /// [`ExtensionError::Unsupported`] when this build has no provider for
    /// it, [`ExtensionError::Mismatch`] when the bytes are another format,
    /// or why the provider cannot open them ([`ExtensionError::Malformed`]
    /// when it panicked).
    // An image sequence keeps the resolver; without that provider it is only lent.
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
                // The provider's open tells another format apart itself (see
                // `VisualProvider::open`): a probe here would read the whole file once more.
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
    /// Why a render panicked: every later render answers it.
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

    /// A reader behind the same panic boundary. A source whose render
    /// panicked makes none: what it shares may be what the panic left.
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

/// `call`, with a panic in it answered as the file's failure.
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

/// The provider a project's source names.
fn provider_name(source: &ProviderSource) -> &'static str {
    match source {
        ProviderSource::Svg => "svg",
        ProviderSource::AnimatedImage => "animated-image",
        ProviderSource::Lottie { .. } => "lottie",
        ProviderSource::Exr { .. } => "exr",
        _ => "image-sequence",
    }
}
