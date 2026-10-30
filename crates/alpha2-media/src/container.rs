//! Export containers: the file formats an export is written in, and which
//! codecs each one offers.
//!
//! This is one policy for every host. The native sinks, the host protocol and
//! the browser exporter all validate a request against it, and the
//! capabilities a host reports are copied from it. It is narrower than what
//! the muxers accept: a container offers only the codecs its mainstream
//! players decode there. `QuickTime` does not play VP9, AV1 or Opus in a MOV,
//! and `WebM` is VP8/VP9/AV1 with Opus/Vorbis by definition. `ProRes` is a
//! `QuickTime` codec and is offered in MOV only. Whether a runtime
//! can encode an offered codec is a separate question each host answers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{AudioCodec, VideoCodec};

/// The file format an export is written in.
#[derive(Copy, Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Container {
    /// ISO base media file (MP4).
    Mp4,
    /// `QuickTime` movie.
    Mov,
    /// Matroska.
    Mkv,
    /// `WebM`, the Matroska subset browsers play.
    Webm,
}

impl Container {
    /// Every container, in the order the engine lists them.
    pub const ALL: [Self; 4] = [Self::Mp4, Self::Mov, Self::Mkv, Self::Webm];

    /// Stable identifier used by every protocol and binding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Mkv => "mkv",
            Self::Webm => "webm",
        }
    }

    /// Lowercase file extensions without the dot; the first is the one an
    /// exporter names a file with.
    #[must_use]
    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Mp4 => &["mp4", "m4v"],
            Self::Mov => &["mov"],
            Self::Mkv => &["mkv"],
            Self::Webm => &["webm"],
        }
    }

    /// The MIME type of a file in this container.
    #[must_use]
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Mov => "video/quicktime",
            Self::Mkv => "video/x-matroska",
            Self::Webm => "video/webm",
        }
    }

    /// Video codecs the container offers, the default first.
    #[must_use]
    pub const fn video_codecs(self) -> &'static [VideoCodec] {
        match self {
            Self::Mp4 | Self::Mkv => &[
                VideoCodec::H264,
                VideoCodec::Hevc,
                VideoCodec::Vp9,
                VideoCodec::Av1,
                VideoCodec::Mpeg4Part2,
            ],
            Self::Mov => &[
                VideoCodec::H264,
                VideoCodec::Hevc,
                VideoCodec::Mpeg4Part2,
                VideoCodec::ProRes,
            ],
            // VP9 first: it has a fast software encoder and every browser decodes it.
            Self::Webm => &[VideoCodec::Vp9, VideoCodec::Av1],
        }
    }

    /// Audio codecs the container offers, the default first. A browser that
    /// cannot encode the default tries the next one in this order.
    #[must_use]
    pub const fn audio_codecs(self) -> &'static [AudioCodec] {
        match self {
            Self::Mp4 | Self::Mkv => &[AudioCodec::Aac, AudioCodec::Opus],
            Self::Mov => &[AudioCodec::Aac],
            Self::Webm => &[AudioCodec::Opus],
        }
    }

    /// The video codec an export in this container gets when it names none.
    #[must_use]
    pub const fn default_video_codec(self) -> VideoCodec {
        self.video_codecs()[0]
    }

    /// The audio codec an export in this container gets when it names none.
    #[must_use]
    pub const fn default_audio_codec(self) -> AudioCodec {
        self.audio_codecs()[0]
    }

    /// Whether the container offers `codec`.
    #[must_use]
    pub fn offers_video(self, codec: VideoCodec) -> bool {
        self.video_codecs().contains(&codec)
    }

    /// Whether the container offers `codec`.
    #[must_use]
    pub fn offers_audio(self, codec: AudioCodec) -> bool {
        self.audio_codecs().contains(&codec)
    }

    /// The container an output path's extension (without the dot) names,
    /// case-insensitively.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|container| {
            container
                .extensions()
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
    }
}

impl fmt::Display for Container {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Container {
    type Err = ContainerError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|container| container.as_str() == value)
            .ok_or_else(|| ContainerError::Unknown(value.to_owned()))
    }
}

/// Why a container request cannot be resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContainerError {
    /// A container name outside [`Container::ALL`].
    Unknown(String),
    /// No container was named and the output's extension names none; `None`
    /// when the output has no extension.
    UnknownExtension(Option<String>),
    /// The output's extension is not one of the named container's.
    ExtensionMismatch {
        /// The container the request named.
        container: Container,
        /// The output's extension, empty when it has none.
        extension: String,
    },
    /// The container does not offer the requested video codec.
    VideoCodecNotOffered {
        /// The resolved container.
        container: Container,
        /// The requested codec.
        codec: VideoCodec,
    },
    /// The container does not offer the requested audio codec.
    AudioCodecNotOffered {
        /// The resolved container.
        container: Container,
        /// The requested codec.
        codec: AudioCodec,
    },
}

fn dotted(extensions: &[&str]) -> String {
    let dotted: Vec<String> = extensions.iter().map(|ext| format!(".{ext}")).collect();
    dotted.join(", ")
}

fn listed<T>(items: &[T], name: impl Fn(&T) -> &'static str) -> String {
    items.iter().map(name).collect::<Vec<_>>().join(", ")
}

impl fmt::Display for ContainerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(
                formatter,
                "unknown container \"{name}\"; expected one of {}",
                listed(&Container::ALL, |container| container.as_str())
            ),
            Self::UnknownExtension(extension) => {
                let every: Vec<&str> = Container::ALL
                    .iter()
                    .flat_map(|container| container.extensions().iter().copied())
                    .collect();
                match extension {
                    Some(extension) => write!(
                        formatter,
                        "the output extension \".{extension}\" names no container; use one of {}",
                        dotted(&every)
                    ),
                    None => write!(
                        formatter,
                        "the output has no extension to name its container; use one of {}",
                        dotted(&every)
                    ),
                }
            }
            Self::ExtensionMismatch {
                container,
                extension,
            } => {
                let found = if extension.is_empty() {
                    "no extension".to_owned()
                } else {
                    format!("\".{extension}\"")
                };
                write!(
                    formatter,
                    "a {container} output must end in {}, not {found}",
                    dotted(container.extensions())
                )
            }
            Self::VideoCodecNotOffered { container, codec } => write!(
                formatter,
                "{container} does not offer video codec {}; it offers {}",
                codec.as_str(),
                listed(container.video_codecs(), |codec| codec.as_str())
            ),
            Self::AudioCodecNotOffered { container, codec } => write!(
                formatter,
                "{container} does not offer audio codec {}; it offers {}",
                codec.as_str(),
                listed(container.audio_codecs(), |codec| codec.as_str())
            ),
        }
    }
}

impl std::error::Error for ContainerError {}

/// The export's container and codecs, resolved once at a host's edge.
///
/// - `container` names the container. When it is absent, `output_extension`
///   names it, and with neither (the browser, which writes no path) it is MP4.
/// - A named container and an output extension must agree.
/// - An absent codec is the container's default, and a named one must be one
///   the container offers.
///
/// `output_extension` is the extension of the path being written, without the
/// dot and empty when the path has none; `None` means there is no path.
///
/// # Errors
/// Returns the first [`ContainerError`] the request runs into, in the order
/// above.
pub fn resolve_export_format(
    container: Option<Container>,
    output_extension: Option<&str>,
    video: Option<VideoCodec>,
    audio: Option<AudioCodec>,
) -> Result<(Container, VideoCodec, AudioCodec), ContainerError> {
    let container = match (container, output_extension) {
        (Some(container), Some(extension)) => {
            if !container
                .extensions()
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
            {
                return Err(ContainerError::ExtensionMismatch {
                    container,
                    extension: extension.to_owned(),
                });
            }
            container
        }
        (Some(container), None) => container,
        (None, Some(extension)) => Container::from_extension(extension).ok_or_else(|| {
            ContainerError::UnknownExtension((!extension.is_empty()).then(|| extension.to_owned()))
        })?,
        (None, None) => Container::Mp4,
    };
    let video = video.unwrap_or(container.default_video_codec());
    if !container.offers_video(video) {
        return Err(ContainerError::VideoCodecNotOffered {
            container,
            codec: video,
        });
    }
    let audio = audio.unwrap_or(container.default_audio_codec());
    if !container.offers_audio(audio) {
        return Err(ContainerError::AudioCodecNotOffered {
            container,
            codec: audio,
        });
    }
    Ok((container, video, audio))
}

#[cfg(test)]
#[path = "container_tests.rs"]
mod tests;
