//! The desktop host's audio windows: the shared windows of host-core over its `FFmpeg` decoders,
//! with failures in the host protocol's tagged form.

use std::collections::HashMap;
use std::path::PathBuf;

use alpha2_host_core::audio_windows::{AudioWindowError, OpenAudio};
pub use alpha2_host_core::audio_windows::{ByteBudget, audio_block_bytes};
#[cfg(test)]
pub use alpha2_host_core::audio_windows::{
    append_audio_block, merge_sample_ranges, pad_audio_window_to_declared_end,
};
use alpha2_model::Project;
use alpha2_native::NativeAudioDecoder;

/// Windows over the desktop's `FFmpeg` audio decoders.
pub type WindowedAudioSources =
    alpha2_host_core::audio_windows::WindowedAudioSources<NativeAudioDecoder>;

/// A window filled to a sample ends at most one block past it.
const AUDIO_DECODE_BLOCK_FRAMES: usize = 4_096;

/// Windows for `project`'s used audible sources at `sample_rate`; no decoder opens yet.
///
/// # Errors
/// Fails when a used source has no entry in `assets`.
pub fn windowed_audio_sources(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    sample_rate: u32,
    byte_budget: impl Into<ByteBudget>,
    decoding: std::sync::Arc<crate::recovery::ExportDecoding>,
) -> Result<WindowedAudioSources, String> {
    let mut paths = HashMap::new();
    for source in project
        .used_sources()
        .into_iter()
        .filter(|source| source.is_audible())
    {
        let path = assets.get(source.id.as_str()).ok_or_else(|| {
            crate::protocol::missing_input(format!("missing uploaded asset {}", source.id.as_str()))
        })?;
        paths.insert(source.id.clone(), path.clone());
    }
    Ok(WindowedAudioSources::new(
        sample_rate,
        byte_budget,
        native_opener(paths, sample_rate, decoding),
    ))
}

/// Opens a source's stream through the export's decode policy, in blocks of at most
/// [`AUDIO_DECODE_BLOCK_FRAMES`].
pub fn native_opener(
    paths: HashMap<alpha2_media::SourceId, PathBuf>,
    sample_rate: u32,
    decoding: std::sync::Arc<crate::recovery::ExportDecoding>,
) -> OpenAudio<NativeAudioDecoder> {
    Box::new(move |source, stream| {
        let path = paths.get(source).ok_or_else(|| {
            AudioWindowError::MissingInput(format!("missing audio source {}", source.as_str()))
        })?;
        let mut decoder = crate::recovery::audio(
            NativeAudioDecoder::open_stream(path, sample_rate, stream)
                .map_err(AudioWindowError::Decode)?,
            source,
            path,
            &decoding,
        );
        decoder
            .set_block_frames(AUDIO_DECODE_BLOCK_FRAMES)
            .map_err(AudioWindowError::Decode)?;
        Ok(decoder)
    })
}

/// `error` in the host protocol's form: tagged `MISSING_INPUT` or `RESOURCE_LIMIT` where it is
/// one, a decoder's failure as the host reports any.
pub fn window_error_message(error: AudioWindowError) -> String {
    match error {
        AudioWindowError::MissingInput(message) => crate::protocol::missing_input(message),
        AudioWindowError::ResourceLimit(message) => crate::memory::resource_limit(message),
        AudioWindowError::Decode(error) => crate::error_message(error),
        AudioWindowError::Broken(message) => message,
    }
}
