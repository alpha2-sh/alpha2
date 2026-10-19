//! The YUV transcode: decoded pictures handed to the encoder unchanged.
//!
//! When a copy is impossible (another codec, another rate, a cut off the
//! source's keyframes) yet every output frame is one source picture shown as
//! it is, the render graph and its YUV-to-RGB-to-YUV round trip change nothing but
//! the rounding. The transcode skips both on the GPU-resident path: each
//! decoded picture is copied plane for plane into the encoder's NV12 input
//! ([`alpha2_render::RepackSource`]).
//!
//! The project side is the one a copy needs ([`alpha2_engine::passthrough_plan`]:
//! untouched clips tiling the range). The file side is looser than a copy's
//! ([`alpha2_native::check_yuv_transcode`]): any container and codec, and any
//! frame grid, because the decoder picks each frame's picture exactly as it
//! does for a render. What it keeps is what the render would have changed:
//! size, orientation, pixel shape, color, 8-bit 4:2:0 sampling, and where
//! the chroma samples sit, which the output's codec implies. Each
//! decoded picture is checked again as it arrives, since a stream may change
//! mid-way.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::decode::DecodedFrameSet;
use alpha2_engine::PassthroughSegment;
use alpha2_media::YuvEncoding;
use alpha2_model::{MediaKind, Project};
use alpha2_native::NativeVideoFrame;
use alpha2_time::TimeRange;

/// The clips an export shows unchanged, one after another.
pub(crate) struct TranscodePlan {
    segments: Vec<PassthroughSegment>,
}

impl TranscodePlan {
    /// The decoded picture that is the frame at `decoded.time`, taken out of
    /// the set, which ends here.
    ///
    /// The picture is all of the set the frame keeps, so once it is dropped
    /// the host holds nothing the set's decode-budget bytes paid for, and
    /// releasing them then is exact.
    ///
    /// # Errors
    /// When no clip covers that time or the set lacks its picture, which the
    /// plan's tiling rules out unless the decode and the plan disagree.
    pub(crate) fn picture(
        &self,
        decoded: DecodedFrameSet,
    ) -> Result<Arc<NativeVideoFrame>, String> {
        let time = decoded.time;
        let segment = self
            .segments
            .iter()
            .find(|segment| {
                segment.timeline_range.start() <= time && time < segment.timeline_range.end()
            })
            .ok_or_else(|| format!("no transcoded clip covers {time:?}"))?;
        let source_time = segment.source_time(time);
        decoded
            .frames
            .into_iter()
            .find(|frame| {
                frame.kind == MediaKind::Video
                    && frame.source == segment.source_id
                    && frame.requested == source_time
            })
            .map(|frame| frame.frame)
            .ok_or_else(|| {
                format!(
                    "the decoded frame set for {time:?} lacks clip {}'s picture at {source_time:?}",
                    segment.clip_id
                )
            })
    }
}

/// Whether an export of `range`, encoded as `encoding`, can hand its decoded
/// pictures to the encoder unchanged, and the clips it would take them from.
///
/// # Errors
/// The first reason it cannot, naming the clip when a file refuses.
pub(crate) fn yuv_transcode_plan(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    range: TimeRange,
    encoding: YuvEncoding,
) -> Result<TranscodePlan, String> {
    plan_with(project, assets, range, |source| {
        alpha2_native::check_yuv_transcode(
            source,
            project.settings.width,
            project.settings.height,
            encoding,
        )
    })
}

/// [`yuv_transcode_plan`], asking `check` whether each file qualifies.
///
/// Everything the check reads belongs to the file, not to a clip, so a file
/// that many clips are cut from is opened and probed once.
fn plan_with(
    project: &Project,
    assets: &HashMap<String, PathBuf>,
    range: TimeRange,
    mut check: impl FnMut(&Path) -> Result<(), String>,
) -> Result<TranscodePlan, String> {
    crate::config::configured_yuv_transcode()?;
    // The mix renders as always; only the pictures are in question.
    let plan = alpha2_engine::passthrough_plan(project, range, false)?;
    let mut qualified = HashSet::new();
    for segment in &plan.segments {
        let source = assets
            .get(segment.source_id.as_str())
            .ok_or_else(|| format!("source {} has no file", segment.source_id))?;
        if qualified.contains(source) {
            continue;
        }
        check(source).map_err(|reason| format!("clip {}: {reason}", segment.clip_id))?;
        qualified.insert(source.clone());
    }
    Ok(TranscodePlan {
        segments: plan.segments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Three untouched clips cut back to back from one file, then one from
    /// another.
    fn rough_cut() -> (Project, HashMap<String, PathBuf>) {
        let second = alpha2_time::TICKS_PER_SECOND;
        let clip = |id: &str, source: &str, start: i64, from: i64| {
            json!({
                "id": id, "source_id": source,
                "timeline_range": {"start": start, "end": start + second},
                "source_range": {"start": from, "end": from + second}
            })
        };
        let project = json!({
            "schema_version": 1, "id": "cut", "name": "Cut",
            "settings": {"width": 128, "height": 96, "frame_rate": {"num": 30, "den": 1}, "sample_rate": 48000},
            "sources": [
                {"id": "camera", "locator": "fixture://camera", "kind": "video"},
                {"id": "phone", "locator": "fixture://phone", "kind": "video"}
            ],
            "timeline": {"tracks": [{"id": "v", "name": "V", "kind": "video", "clips": [
                clip("a", "camera", 0, 0),
                clip("b", "camera", second, 3 * second),
                clip("c", "camera", 2 * second, second),
                clip("d", "phone", 3 * second, 0)
            ]}]}
        });
        let project = Project::from_json(&project.to_string()).expect("project");
        let assets = HashMap::from([
            ("camera".to_owned(), PathBuf::from("camera.mp4")),
            ("phone".to_owned(), PathBuf::from("phone.mp4")),
        ]);
        (project, assets)
    }

    #[test]
    fn a_file_many_clips_cut_from_is_checked_once() {
        let (project, assets) = rough_cut();
        let range = TimeRange::new(
            alpha2_time::MediaTime::ZERO,
            alpha2_time::MediaTime::from_ticks(4 * alpha2_time::TICKS_PER_SECOND),
        )
        .expect("range");
        let mut checked = Vec::new();
        let plan = plan_with(&project, &assets, range, |source| {
            checked.push(source.to_owned());
            Ok(())
        })
        .expect("the cut qualifies");
        assert_eq!(plan.segments.len(), 4);
        assert_eq!(
            checked,
            [PathBuf::from("camera.mp4"), PathBuf::from("phone.mp4")]
        );
    }

    #[test]
    fn a_file_that_refuses_names_the_first_clip_cut_from_it() {
        let (project, assets) = rough_cut();
        let range = TimeRange::new(
            alpha2_time::MediaTime::ZERO,
            alpha2_time::MediaTime::from_ticks(4 * alpha2_time::TICKS_PER_SECOND),
        )
        .expect("range");
        let error = plan_with(&project, &assets, range, |source| {
            if source == Path::new("phone.mp4") {
                Err("the source's chroma sits Center".to_owned())
            } else {
                Ok(())
            }
        })
        .err()
        .expect("the phone clip refuses");
        assert_eq!(error, "clip d: the source's chroma sits Center");
    }
}
