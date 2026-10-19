//! Deterministic timeline primitives.

mod audio_grid;
mod error;
mod frame_rate;
mod media_time;
mod range;

pub use audio_grid::{AudioFrameGrid, audio_frame_grid};
pub use error::{FrameRateError, TimeRangeError};
pub use frame_rate::FrameRate;
pub use media_time::{
    MAX_SERIALIZED_TICKS, MediaTime, PRESENTATION_SLACK, ROUNDING_SCAN_BYTES, ROUNDING_SCAN_FRAMES,
    TICKS_PER_SECOND, presentation_slack, rounding_quantum,
};
pub use range::TimeRange;
