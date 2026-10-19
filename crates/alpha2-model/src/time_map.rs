//! How a clip's timeline time maps onto its source range.

use alpha2_time::MediaTime;
use serde::{Deserialize, Serialize};

/// How a clip plays its source range over its timeline range.
///
/// `forward` and `reverse` stretch the source range over the timeline range,
/// so their speed is the ratio of the two. A loop plays the source range at
/// its own speed, again and again, for as long as the timeline range lasts.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", from = "TimeMapWire")]
pub enum TimeMap {
    /// From the start of the source range to its end.
    #[default]
    Forward,
    /// From the end of the source range to its start: pictures in reverse
    /// order and sound played backwards.
    Reverse,
    /// One source instant for the whole clip, and silence.
    Freeze {
        /// The instant shown; inside the source range.
        source_time: MediaTime,
    },
    /// The source range again and again, at its own speed.
    Loop {
        /// How each pass follows the one before.
        mode: LoopMode,
    },
}

/// The wire form of [`TimeMap`]: empty struct variants reject extra fields
/// where serde's unit variants would ignore them.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "TimeMap"))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TimeMapWire {
    /// From the start of the source range to its end.
    Forward {},
    /// From the end of the source range to its start: pictures in reverse
    /// order and sound played backwards.
    Reverse {},
    /// One source instant for the whole clip, and silence.
    Freeze {
        /// The instant shown; inside the source range.
        source_time: MediaTime,
    },
    /// The source range again and again, at its own speed.
    Loop {
        /// How each pass follows the one before.
        mode: LoopMode,
    },
}

impl From<TimeMapWire> for TimeMap {
    fn from(wire: TimeMapWire) -> Self {
        match wire {
            TimeMapWire::Forward {} => Self::Forward,
            TimeMapWire::Reverse {} => Self::Reverse,
            TimeMapWire::Freeze { source_time } => Self::Freeze { source_time },
            TimeMapWire::Loop { mode } => Self::Loop { mode },
        }
    }
}

/// How each pass of a [`TimeMap::Loop`] follows the one before.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    /// Every pass plays forward, from the start again. Sound fades out and
    /// in over [`LOOP_FADE_SECONDS`] at each wrap, so the jump does not click.
    Repeat,
    /// Passes alternate forward and backward.
    PingPong,
}

/// How long sound fades out before, and in after, each wrap of a repeating
/// loop.
pub const LOOP_FADE_SECONDS: f64 = 0.005;
