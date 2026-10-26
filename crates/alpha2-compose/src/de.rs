//! Parsing a spec so that an error names where in it the problem lies.
//!
//! `serde_path_to_error` follows a path through structs, maps and lists,
//! but not into the buffered content of a tagged or untagged enum, where a
//! spec's nodes are. Those enums parse by hand here: each reads its JSON
//! value, picks its variant by tag or JSON type, and parses that variant
//! with its own path, which it hands up inside the error message for the
//! level above to join to its own.

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::spec::{
    Canvas, FrameRateChoice, Inset, Loudness, MediaItem, Node, ThemeChoice, TransitionChoice,
};

/// Marks a message that carries the path below the value that failed.
const MARK: char = '\u{0}';

/// A problem and where it lies, below some value.
pub(crate) struct Located {
    /// `serde_path_to_error`'s form: `children[2].text`, or `.` for the
    /// value itself.
    pub path: String,
    pub message: String,
}

/// `inner` (a path below `outer`) joined to `outer`.
pub(crate) fn join(outer: &str, inner: &str) -> String {
    match (outer, inner) {
        (".", inner) => inner.to_owned(),
        (outer, ".") => outer.to_owned(),
        (outer, inner) if inner.starts_with('[') => format!("{outer}{inner}"),
        (outer, inner) => format!("{outer}.{inner}"),
    }
}

/// Parses `value` as `T`, the path through any enum included.
pub(crate) fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, Located> {
    serde_path_to_error::deserialize(value).map_err(|error| {
        let outer = error.path().to_string();
        let message = error.into_inner().to_string();
        match message
            .strip_prefix(MARK)
            .and_then(|rest| rest.split_once(MARK))
        {
            Some((inner, message)) => Located {
                path: join(&outer, inner),
                message: message.to_owned(),
            },
            None => Located {
                path: outer,
                message,
            },
        }
    })
}

/// A serde error carrying `located`'s path up.
#[allow(clippy::needless_pass_by_value)]
fn carry<E: serde::de::Error>(located: Located) -> E {
    E::custom(format!("{MARK}{}{MARK}{}", located.path, located.message))
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn node(mut value: Value) -> Result<Node, Located> {
    let here = |message: String| Located {
        path: ".".to_owned(),
        message,
    };
    let Value::Object(map) = &mut value else {
        return Err(here(format!(
            "a node is an object with a `type`, not {}",
            kind_of(&value)
        )));
    };
    let tag = match map.remove("type") {
        Some(Value::String(tag)) => tag,
        Some(other) => {
            return Err(Located {
                path: "type".to_owned(),
                message: format!("a node's type is a string, not {}", kind_of(&other)),
            });
        }
        None => return Err(here("a node has a `type`".to_owned())),
    };
    match tag.as_str() {
        "sequence" => from_value(value).map(Node::Sequence),
        "stack" => from_value(value).map(Node::Stack),
        "grid" => from_value(value).map(Node::Grid),
        "media" => from_value(value).map(Node::Media),
        "text" => from_value(value).map(Node::Text),
        "color" => from_value(value).map(Node::Color),
        "captions" => from_value(value).map(Node::Captions),
        "layout" => from_value(value).map(Node::Layout),
        "audio" => from_value(value).map(Node::Audio),
        "raw" => from_value(value).map(Node::Raw),
        "slideshow" => from_value(value).map(Node::Slideshow),
        "captioned_video" => from_value(value).map(Node::CaptionedVideo),
        "title" => from_value(value).map(Node::Title),
        "title_card" => from_value(value).map(Node::TitleCard),
        "lower_third" => from_value(value).map(Node::LowerThird),
        "end_card" => from_value(value).map(Node::EndCard),
        "comparison_grid" => from_value(value).map(Node::ComparisonGrid),
        "reframe" => from_value(value).map(Node::Reframe),
        "logo" => from_value(value).map(Node::Logo),
        "music_bed" => from_value(value).map(Node::MusicBed),
        other => Err(Located {
            path: "type".to_owned(),
            message: format!(
                "unknown node type \"{other}\"; the types are {}",
                NODE_TYPES.join(", ")
            ),
        }),
    }
}

/// Every node type, as specs write them.
pub(crate) const NODE_TYPES: [&str; 20] = [
    "sequence",
    "stack",
    "grid",
    "media",
    "text",
    "color",
    "captions",
    "layout",
    "audio",
    "raw",
    "slideshow",
    "captioned_video",
    "title",
    "title_card",
    "lower_third",
    "end_card",
    "comparison_grid",
    "reframe",
    "logo",
    "music_bed",
];

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        node(Value::deserialize(deserializer)?).map_err(carry)
    }
}

/// Parses `value` by its JSON type: a string as `string` reads it, anything
/// else as `other`.
fn by_type<T, E: serde::de::Error>(
    value: Value,
    string: impl FnOnce(String) -> T,
    other: impl FnOnce(Value) -> Result<T, Located>,
) -> Result<T, E> {
    match value {
        Value::String(text) => Ok(string(text)),
        value => other(value).map_err(carry),
    }
}

fn object<T: DeserializeOwned>(value: Value, expected: &str) -> Result<T, Located> {
    if value.is_object() {
        from_value(value)
    } else {
        Err(Located {
            path: ".".to_owned(),
            message: format!("expected {expected}, not {}", kind_of(&value)),
        })
    }
}

impl<'de> Deserialize<'de> for MediaItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        by_type(Value::deserialize(deserializer)?, Self::Path, |value| {
            node(value).map(|node| Self::Node(Box::new(node)))
        })
    }
}

impl<'de> Deserialize<'de> for Canvas {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        by_type(Value::deserialize(deserializer)?, Self::Preset, |value| {
            object(value, "a preset name or a canvas object").map(Self::Custom)
        })
    }
}

impl<'de> Deserialize<'de> for ThemeChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        by_type(Value::deserialize(deserializer)?, Self::Named, |value| {
            object(value, "a theme name or a theme object")
                .map(|theme| Self::Custom(Box::new(theme)))
        })
    }
}

impl<'de> Deserialize<'de> for TransitionChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        by_type(Value::deserialize(deserializer)?, Self::Named, |value| {
            object(value, "a transition name or a transition object").map(Self::Given)
        })
    }
}

impl<'de> Deserialize<'de> for Loudness {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Number(number) => number
                .as_f64()
                .map(Self::Lufs)
                .ok_or_else(|| D::Error::custom("the loudness is a number of LUFS")),
            value => object(
                value,
                "a number of LUFS or {target_lufs, true_peak_ceiling_dbtp}",
            )
            .map(Self::Target)
            .map_err(carry),
        }
    }
}

impl<'de> Deserialize<'de> for Inset {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match &value {
            #[allow(clippy::cast_possible_truncation)]
            Value::Number(number) => number
                .as_f64()
                .map(|pixels| Self::Uniform(pixels as f32))
                .ok_or_else(|| D::Error::custom("an inset is pixels")),
            Value::Array(_) => from_value(value).map(Self::Sides).map_err(carry),
            other => Err(D::Error::custom(format!(
                "an inset is pixels, or [top, right, bottom, left] pixels, not {}",
                kind_of(other)
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for FrameRateChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Number(number) => number
                .as_f64()
                .map(Self::Number)
                .ok_or_else(|| D::Error::custom("a frame rate is a number")),
            value => object(value, "frames per second, or {num, den}")
                .map(Self::Exact)
                .map_err(carry),
        }
    }
}
