//! The `still` request.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use super::RequestError;
use super::effect::EffectRequest;
use super::media::{ExportMedia, ProjectJson, ProxyRequest, output_key};

/// A `still` request.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StillRequest {
    /// Always the protocol version.
    pub protocol_version: u32,
    /// The caller's id for the request.
    pub request_id: String,
    /// Always `still`.
    pub operation: String,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
    /// The project to draw.
    pub project: ProjectJson,
    /// Originals by source ID.
    pub assets: HashMap<String, super::MediaLocation>,
    /// Proxies by source ID.
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    /// Image sequence frames by source ID, in project order; `null` where the sequence has a gap.
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// Files an SVG or a Lottie file names, by source ID and then by locator.
    #[serde(default)]
    pub resources: HashMap<String, HashMap<String, super::MediaLocation>>,
    /// Whether a remote original whose project records a content hash is read whole and hashed.
    #[serde(rename = "remoteContentHash", default)]
    pub remote_content_hash: super::RemoteContentHash,
    /// Which media the still reads.
    #[serde(default)]
    pub media: ExportMedia,
    /// Lookup tables of runtime effects, by name.
    #[serde(default)]
    pub effect_assets: HashMap<String, PathBuf>,
    /// Font files, in fallback order.
    #[serde(rename = "fonts", default)]
    pub font_files: Vec<PathBuf>,
    /// Runtime effects the project's clips name.
    #[serde(default)]
    pub effects: Vec<EffectRequest>,
    /// The still size; the project's canvas when absent.
    #[serde(default)]
    pub size: Option<StillSize>,
    /// 1 to [`MAX_STILLS`], each to its own file.
    pub stills: Vec<StillEntry>,
    /// Draw without the project background, over transparent pixels.
    #[serde(default)]
    pub alpha: bool,
    /// Replace an existing regular file at each output.
    #[serde(default)]
    pub overwrite: bool,
}

/// The most stills one request draws.
pub const MAX_STILLS: usize = 256;

/// The widest and tallest still.
pub const MAX_STILL_DIMENSION: u32 = alpha2_model::MAX_CANVAS_DIMENSION;

/// A still's size in pixels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StillSize {
    /// Pixels across.
    pub width: u32,
    /// Pixels down.
    pub height: u32,
}

/// One still: a time and where its PNG goes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StillEntry {
    /// Ticks; the frame drawn is the grid frame that holds it.
    pub time: i64,
    /// `None` returns the PNG as an attachment of the response.
    #[serde(default)]
    pub output: Option<PathBuf>,
}

impl StillRequest {
    /// # Errors
    /// `INVALID_INPUT` for no stills or more than [`MAX_STILLS`], a negative time, two stills to
    /// one output, or a size out of `1..=MAX_STILL_DIMENSION`.
    pub fn check(&self) -> Result<(), RequestError> {
        if self.operation != "still" {
            return Err(RequestError::invalid_request(
                "invalid request: the operation is not `still`",
            ));
        }
        super::refuse_urls(
            &self.proxies,
            &self.frames,
            &self.effect_assets,
            &self.font_files,
        )?;
        for output in self.stills.iter().filter_map(|still| still.output.as_ref()) {
            super::refuse_url("a still's output", output)?;
        }
        if self.stills.is_empty() || self.stills.len() > MAX_STILLS {
            return Err(RequestError::invalid_input(format!(
                "stills has {} entries; a still request renders 1 to {MAX_STILLS}",
                self.stills.len()
            )));
        }
        // Keyed as a case- and normalization-insensitive file system sees the name.
        let mut outputs = std::collections::HashMap::new();
        for still in &self.stills {
            if still.time < 0 {
                return Err(RequestError::invalid_input(format!(
                    "a still's time is a timeline time from 0, not {}",
                    still.time
                )));
            }
            let Some(output) = &still.output else {
                continue;
            };
            if output.as_os_str().is_empty() {
                return Err(RequestError::invalid_input(
                    "a still's output must not be empty",
                ));
            }
            if let Some(earlier) = outputs.insert(output_key(output), output) {
                return Err(RequestError::invalid_input(format!(
                    "two stills write one file: {} and {}",
                    earlier.display(),
                    output.display()
                )));
            }
        }
        let fits = |pixels: u32| (1..=MAX_STILL_DIMENSION).contains(&pixels);
        if self
            .size
            .is_some_and(|size| !fits(size.width) || !fits(size.height))
        {
            return Err(RequestError::invalid_input(format!(
                "a still's size is 1 to {MAX_STILL_DIMENSION} pixels each way"
            )));
        }
        Ok(())
    }
}
