//! What compiling needs to know about the files a spec names, and which
//! files those are.
//!
//! The compiler reads no files. A binding lists what a spec needs
//! ([`requirements`]), finds it out (a host's `conform` describes media
//! without a GPU; a caption file is read as text), and passes it in as
//! [`Facts`].

use std::collections::BTreeMap;

use alpha2_model::MediaFingerprint;
use alpha2_time::FrameRate;
use serde::{Deserialize, Serialize};

use crate::spec::{FileKind, MediaItem, Node, VideoSpec};

/// What the compiler knows of the files a spec names, by the path the spec
/// gives them.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Facts {
    /// Media files' shapes and lengths.
    #[serde(default)]
    pub media: BTreeMap<String, MediaFacts>,
    /// Caption files' text.
    #[serde(default)]
    pub texts: BTreeMap<String, String>,
}

/// One media file's shape and length.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct MediaFacts {
    /// Displayed width of its pictures, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Displayed height of its pictures, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Its video's frame rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_rate: Option<FrameRate>,
    /// Where it ends on its own timeline, in ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ticks: Option<i64>,
    /// Whether it has an audio stream.
    #[serde(default)]
    pub has_audio: bool,
}

impl MediaFacts {
    /// The facts a host's fingerprint of the file records, as `conform`
    /// reports them.
    #[must_use]
    pub fn from_fingerprint(fingerprint: &MediaFingerprint) -> Self {
        let streams = fingerprint.streams.as_ref();
        let video = streams.and_then(|streams| streams.video.as_ref());
        Self {
            width: video.map(|video| video.width),
            height: video.map(|video| video.height),
            frame_rate: video.and_then(|video| video.frame_rate),
            duration_ticks: fingerprint.duration_ticks,
            has_audio: streams.is_some_and(|streams| streams.audio.is_some()),
        }
    }
}

/// The files a spec names.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    /// Media files, each once, in the order the spec first names them.
    pub media: Vec<RequiredMedia>,
    /// Caption files, each once, in that order.
    pub texts: Vec<String>,
}

/// A media file a spec names.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RequiredMedia {
    /// The path as the spec gives it.
    pub path: String,
    /// What the compiler takes it to hold, and a host should read it as.
    pub kind: FileKind,
}

/// What a file holds by its extension: a still for the image formats the
/// host decodes itself, sound for the audio-only ones, video otherwise.
/// Formats an extension provider reads (an animated GIF, SVG, OpenEXR) are
/// not media nodes' to guess.
#[must_use]
pub fn kind_of_path(path: &str) -> FileKind {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" | "avif" => FileKind::Image,
        "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "oga" | "opus" | "aif" | "aiff"
        | "caf" => FileKind::Audio,
        _ => FileKind::Video,
    }
}

/// The files `spec` names, for a binding to describe before compiling.
#[must_use]
pub fn requirements(spec: &VideoSpec) -> Requirements {
    let mut found = Requirements::default();
    let nodes = std::iter::once(&spec.body)
        .chain(&spec.overlays)
        .chain(&spec.audio);
    for node in nodes {
        walk(node, &mut found);
    }
    found
}

fn add_media(found: &mut Requirements, path: &str, kind: Option<FileKind>) {
    if !found.media.iter().any(|media| media.path == path) {
        found.media.push(RequiredMedia {
            path: path.to_owned(),
            kind: kind.unwrap_or_else(|| kind_of_path(path)),
        });
    }
}

fn add_item(found: &mut Requirements, item: &MediaItem) {
    match item {
        MediaItem::Path(path) => add_media(found, path, None),
        MediaItem::Node(node) => walk(node, found),
    }
}

fn walk(node: &Node, found: &mut Requirements) {
    match node {
        Node::Sequence(sequence) => sequence
            .children
            .iter()
            .for_each(|child| walk(child, found)),
        Node::Stack(stack) => stack.children.iter().for_each(|child| walk(child, found)),
        Node::Grid(grid) => grid.children.iter().for_each(|child| walk(child, found)),
        Node::Media(media) => add_media(found, &media.path, media.kind),
        Node::Audio(audio) => add_media(found, &audio.path, Some(FileKind::Audio)),
        Node::MusicBed(music) => add_media(found, &music.path, Some(FileKind::Audio)),
        Node::Logo(logo) => add_media(found, &logo.path, None),
        Node::Captions(captions) => {
            if let Some(file) = &captions.file
                && !found.texts.contains(file)
            {
                found.texts.push(file.clone());
            }
        }
        Node::Slideshow(slideshow) => {
            slideshow
                .items
                .iter()
                .for_each(|item| add_item(found, item));
            if let Some(path) = &slideshow.fit_to {
                add_media(found, path, Some(FileKind::Audio));
            }
        }
        Node::CaptionedVideo(captioned) => {
            add_item(found, &captioned.video);
            walk(&Node::Captions((*captioned.captions).clone()), found);
        }
        Node::ComparisonGrid(grid) => grid.items.iter().for_each(|item| add_item(found, item)),
        Node::Reframe(reframe) => add_item(found, &reframe.video),
        Node::EndCard(card) => {
            if let Some(logo) = &card.logo {
                add_media(found, logo, None);
            }
        }
        Node::Text(_)
        | Node::Color(_)
        | Node::Layout(_)
        | Node::Raw(_)
        | Node::Title(_)
        | Node::TitleCard(_)
        | Node::LowerThird(_) => {}
    }
}
