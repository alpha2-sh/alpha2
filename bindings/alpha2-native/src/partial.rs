//! The host-core transactional output, opened for an `FFmpeg` muxer.
pub(crate) use alpha2_host_core::partial::*;

use alpha2_ffmpeg_bridge::{CreateOutputError, OutputFile};
use alpha2_media::{Container, EncodeError};

/// Opens `partial`'s write path for the muxer of `container`. The output must drop before
/// `partial` so the file is closed before it is moved or removed.
///
/// # Errors
/// When the path is not UTF-8 or has a NUL byte, or the muxer cannot be created.
pub(crate) fn open_muxer(
    partial: &PartialOutput,
    container: Container,
) -> Result<OutputFile, EncodeError> {
    OutputFile::create(partial.write_path(), crate::convert::muxer_name(container)).map_err(
        |error| match error {
            CreateOutputError::Path(error) => EncodeError::Output(format!("output path {error}")),
            CreateOutputError::Ffmpeg(error) => EncodeError::Output(error.to_string()),
        },
    )
}
