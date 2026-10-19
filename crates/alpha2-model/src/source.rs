use crate::{Generator, MediaFingerprint, ProviderSource};
use alpha2_media::{ColorMetadata, SourceId};
use serde::{Deserialize, Serialize};

/// A host-resolved media resource.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct MediaSource {
    /// Stable source identity.
    pub id: SourceId,
    /// Opaque URI or host locator. The engine does not open it directly.
    /// Required, except that a [`MediaKind::Generated`] or
    /// [`MediaKind::Composition`] source has none.
    #[serde(default)]
    pub locator: String,
    /// Media payload domain.
    pub kind: MediaKind,
    /// The color a [`MediaKind::Video`] source's pictures are in, which wins
    /// over what its frames and its stream are tagged with on every host: a
    /// host records it at import, or a document declares it to correct
    /// missing or wrong tags. `None` leaves the tags in charge. A still is
    /// read as sRGB code values whatever it declares.
    #[serde(default)]
    pub color_metadata: Option<ColorMetadata>,
    /// Whether this source's media has an audio stream: a fact of the file,
    /// so every host that imports it records the same, whether or not that
    /// host can decode the stream. A host that cannot reports so at run time
    /// and plays the source silent; the document keeps the stream for the
    /// hosts that can.
    ///
    /// [`MediaKind::Audio`] always does. A [`MediaKind::Video`] source declares
    /// it so that clips on a visual track contribute to the mix; leaving it
    /// unset keeps a video clip silent.
    #[serde(default)]
    pub has_audio: bool,
    /// What a [`MediaKind::Generated`] source draws; present exactly for
    /// that kind.
    #[serde(default)]
    pub generator: Option<Generator>,
    /// The composition a [`MediaKind::Composition`] source shows; present
    /// exactly for that kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<crate::CompositionId>,
    /// What the host found at the locator when the project recorded it, so
    /// another host can tell whether media it finds is the same. A generated
    /// source has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<MediaFingerprint>,
    /// The extension provider that supplies this source's media, and its
    /// settings, when the host's own decoders do not read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderSource>,
}

impl MediaSource {
    /// Creates a silent-by-default source without explicit color metadata.
    #[must_use]
    pub fn new(id: SourceId, locator: impl Into<String>, kind: MediaKind) -> Self {
        Self {
            id,
            locator: locator.into(),
            kind,
            color_metadata: None,
            has_audio: false,
            generator: None,
            composition: None,
            fingerprint: None,
            provider: None,
        }
    }

    /// Creates a source the engine draws itself.
    #[must_use]
    pub const fn generated(id: SourceId, generator: Generator) -> Self {
        Self {
            id,
            locator: String::new(),
            kind: MediaKind::Generated,
            color_metadata: None,
            has_audio: false,
            generator: Some(generator),
            composition: None,
            fingerprint: None,
            provider: None,
        }
    }

    /// Creates a source that shows composition `composition`.
    #[must_use]
    pub const fn composition(id: SourceId, composition: crate::CompositionId) -> Self {
        Self {
            id,
            locator: String::new(),
            kind: MediaKind::Composition,
            color_metadata: None,
            has_audio: false,
            generator: None,
            composition: Some(composition),
            fingerprint: None,
            provider: None,
        }
    }

    /// Returns whether a host supplies this source's media; the engine draws
    /// a generated or composition source itself.
    #[must_use]
    pub const fn is_host_supplied(&self) -> bool {
        !matches!(self.kind, MediaKind::Generated | MediaKind::Composition)
    }

    /// Returns whether this source can contribute samples to the audio mix.
    #[must_use]
    pub const fn is_audible(&self) -> bool {
        matches!(self.kind, MediaKind::Audio) || self.has_audio
    }

    /// Returns declared color metadata or the SDR BT.709 limited-range fallback.
    #[must_use]
    pub fn effective_color_metadata(&self) -> ColorMetadata {
        self.color_metadata.unwrap_or_default()
    }
}

/// The payload domain of a source.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A video stream, potentially with an audio stream.
    Video,
    /// An audio stream.
    Audio,
    /// A still visual image.
    Image,
    /// A picture the engine draws from the source's [`Generator`]. The host
    /// supplies nothing for it.
    Generated,
    /// A composition of the project, drawn at the source time as its own
    /// time. The host supplies nothing for it; the media its nodes read, it
    /// supplies as for any composition.
    Composition,
}
