use alpha2_time::{FrameRate, MediaTime};

use crate::EngineError;

/// The index and start time of up to `count` frames from `first_frame`,
/// stopping before `end_frame`.
///
/// Returns [`EngineError::OutOfRange`] when a frame index or its start time
/// does not fit.
pub(crate) fn frame_times(
    rate: FrameRate,
    first_frame: i64,
    count: u32,
    end_frame: Option<i64>,
) -> Result<Vec<(i64, MediaTime)>, EngineError> {
    let mut frames = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for offset in 0..count {
        let frame = first_frame
            .checked_add(i64::from(offset))
            .ok_or(EngineError::OutOfRange)?;
        if end_frame.is_some_and(|end| frame >= end) {
            break;
        }
        let time = rate.frame_start(frame).ok_or(EngineError::OutOfRange)?;
        frames.push((frame, time));
    }
    Ok(frames)
}
