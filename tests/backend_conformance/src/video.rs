//! Video seek exactness and frame metadata.

use super::*;

fn seek_pts(decoder: &mut dyn VideoDecoder, ticks: i64, generation: u64) -> Result<i64, String> {
    let cancel = CancelToken::default();
    match decoder
        .seek(request(MediaTime::from_ticks(ticks), generation), &cancel)
        .map_err(|error| format!("seek {ticks}: {error}"))?
        .event
    {
        DecodeEvent::Video(frame) => Ok(frame.pts.ticks()),
        event => Err(format!("seek {ticks}: {event:?}")),
    }
}

fn presented_ticks(backend: &dyn Backend, name: &str) -> Result<Option<Vec<i64>>, String> {
    let Some(mut decoder) = video_or_refusal(backend, name)? else {
        return Ok(None);
    };
    let cancel = CancelToken::default();
    let mut presented = Vec::new();
    loop {
        match decoder
            .decode_next(&cancel)
            .map_err(|error| format!("{name}: {error}"))?
        {
            DecodeEvent::Video(frame) => presented.push(frame.pts.ticks()),
            DecodeEvent::EndOfStream => break,
            DecodeEvent::Retry => {}
            event @ DecodeEvent::Audio(_) => return Err(format!("{name}: unexpected {event:?}")),
        }
    }
    Ok(Some(presented))
}

/// Between frames a seek answers with the earlier one without consuming the next.
///
/// # Errors
/// What differed.
pub fn video_seek_shows_the_presented_frame_and_keeps_the_next(backend: &dyn Backend) -> Outcome {
    // vfr.mp4 presents frames at 0, 4000, 12000, 24000 and 40000 ticks.
    let Some(mut decoder) = video_or_refusal(backend, "vfr.mp4")? else {
        return Ok(());
    };
    let slack = decoder.presentation_slack().ticks();
    let cancel = CancelToken::default();
    let mut seek = |ticks: i64| -> Result<(i64, i64), String> {
        let shown = seek_pts(&mut *decoder, ticks, 1)?;
        let DecodeEvent::Video(next) = decoder
            .decode_next(&cancel)
            .map_err(|error| error.to_string())?
        else {
            return Err("no frame after the seek".into());
        };
        Ok((shown, next.pts.ticks()))
    };
    let mut failures = Failures::default();
    for (ticks, expected) in [
        (18_000, (12_000, 24_000)),
        (12_000, (12_000, 24_000)),
        (12_000 - slack, (12_000, 24_000)),
        (12_000 - slack - 1, (4_000, 12_000)),
        (0, (0, 4_000)),
    ] {
        let answer = seek(ticks);
        failures.check(answer == Ok(expected), || {
            format!("vfr.mp4 seek {ticks}: {answer:?}, expected {expected:?}")
        });
    }
    failures.outcome()
}

/// A seek to a project frame's start at the source's own rate returns that frame though its
/// timestamp is a few ticks later (1/90000 at 90 fps, or Matroska milliseconds at 29.97 fps).
///
/// # Errors
/// What differed.
pub fn video_seek_to_a_frame_start_at_the_source_rate(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for (name, num, den) in [
        ("frame-index-90fps.mov", 90, 1),
        ("frame-index-2997.mkv", 30_000, 1_001),
    ] {
        let rate = FrameRate::new(num, den).expect("rate");
        let Some(presented) = presented_ticks(backend, name)? else {
            continue;
        };
        failures.check(presented.len() == 30, || {
            format!("{name}: {} frames, not 30", presented.len())
        });
        let Some(mut decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        let mut answered = Vec::new();
        for index in (0..30_i64).rev() {
            let time = rate.frame_start(index).expect("frame start");
            let pts = seek_pts(&mut *decoder, time.ticks(), index.unsigned_abs())?;
            answered.push(presented.iter().position(|&shown| shown == pts));
        }
        let expected = (0..30).rev().map(Some).collect::<Vec<_>>();
        failures.check(answered == expected, || {
            format!("{name}: answered {answered:?}")
        });
    }
    failures.outcome()
}

/// Every seek answers with the frame a linear decode presents at the time plus the slack, across
/// MPEG-TS keyframe flags, open GOPs, millisecond rounding, AVI indexes and variable rates.
///
/// # Errors
/// What differed.
pub fn video_seek_matches_a_linear_decode(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for name in [
        "h264-bt709-copy.mp4",
        "h264-bt709-copy.ts",
        "h264-bt709-copy.mkv",
        "long-gop.mp4",
        "odd-dimensions.avi",
        "vfr.mp4",
    ] {
        let Some(presented) = presented_ticks(backend, name)? else {
            continue;
        };
        if presented.is_empty() || !presented.windows(2).all(|pair| pair[0] < pair[1]) {
            failures.check(false, || format!("{name}: presented {presented:?}"));
            continue;
        }
        let first = presented[0];
        let last = presented[presented.len() - 1];
        let Some(mut decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        let slack = decoder.presentation_slack().ticks();
        let mut targets = vec![0, first - 1];
        targets.extend(presented.iter().flat_map(|&pts| {
            [
                pts - slack - 1,
                pts - slack,
                pts - 1,
                pts,
                pts + 1,
                pts + 2_000,
            ]
        }));
        targets.push(last + 120_000);
        let reversed = targets.iter().rev().copied().collect::<Vec<_>>();
        targets.extend(reversed);
        let mut wrong = Vec::new();
        for (generation, &target) in (0_u64..).zip(&targets) {
            let deadline = target + slack;
            let expected = presented
                .iter()
                .rev()
                .find(|&&pts| pts <= deadline)
                .copied()
                .unwrap_or(first);
            let answer = seek_pts(&mut *decoder, target, generation);
            if answer != Ok(expected) {
                wrong.push(format!("{target}: {answer:?} not {expected}"));
            }
        }
        failures.check(wrong.is_empty(), || {
            format!(
                "{name}: {} of {} seeks answered another frame: {wrong:?}",
                wrong.len(),
                targets.len()
            )
        });
    }
    failures.outcome()
}

/// The checked-in corpus opens and decodes, with the rotation, size and color its streams declare.
///
/// # Errors
/// What differed.
pub fn video_corpus_decodes_with_its_metadata(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for name in [
        "cfr-bt709-limited.mp4",
        "vfr.mp4",
        "long-gop.mp4",
        "edit-list.mp4",
        "rotation.mp4",
        "video-only.mp4",
        "odd-dimensions.avi",
        "bt601-full.mp4",
        "missing-color-metadata.mp4",
    ] {
        let Some(mut decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        failures.check(video_stream(&*decoder).is_some(), || {
            format!("{name}: no video stream")
        });
        let frame = first_frame(&mut *decoder);
        failures.check(frame.is_ok(), || format!("{name}: {frame:?}"));
        let stream = video_stream(&*decoder).cloned();
        match name {
            // `-display_rotation 90` is counter-clockwise: shown turned a quarter clockwise.
            "rotation.mp4" => failures.check(
                stream
                    .as_ref()
                    .is_some_and(|stream| stream.orientation.rotation == Rotation::Deg270),
                || format!("{name}: {stream:?}"),
            ),
            "odd-dimensions.avi" => failures.check(
                stream
                    .as_ref()
                    .is_some_and(|stream| stream.dimensions == Some((65, 49))),
                || format!("{name}: {stream:?}"),
            ),
            "bt601-full.mp4" => failures.check(
                stream
                    .as_ref()
                    .and_then(|stream| stream.color)
                    .is_some_and(|color| {
                        color.matrix == ColorMatrix::Bt601 && color.range == ColorRange::Full
                    }),
                || format!("{name}: {stream:?}"),
            ),
            "missing-color-metadata.mp4" => failures.check(
                stream.as_ref().is_some_and(|stream| stream.color.is_none()),
                || format!("{name}: {stream:?}"),
            ),
            _ => {}
        }
    }
    failures.outcome()
}

/// A still is shown as its EXIF orientation says, its stream giving the stored size.
///
/// # Errors
/// What differed.
pub fn video_still_shows_its_exif_orientation(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for value in 1..=8_u16 {
        let name = format!("exif-orientation-{value}.jpg");
        let expected = Orientation::from_exif(value).expect("Exif orientation");
        let Some(mut decoder) = video_or_refusal(backend, &name)? else {
            continue;
        };
        let stored = if expected.rotation.swaps_axes() {
            (48, 64)
        } else {
            (64, 48)
        };
        let stream = video_stream(&*decoder).cloned();
        failures.check(
            stream.as_ref().is_some_and(|stream| {
                stream.orientation == expected && stream.dimensions == Some(stored)
            }),
            || format!("{name}: stream {stream:?}, expected {expected:?} at {stored:?}"),
        );
        let frame = first_frame(&mut *decoder);
        failures.check(
            frame
                .as_ref()
                .is_ok_and(|frame| frame.orientation == expected),
            || {
                format!(
                    "{name}: frame orientation {:?}",
                    frame.map(|f| f.orientation)
                )
            },
        );
    }
    failures.outcome()
}

/// A mirroring display matrix is a mirror plus a rotation; Matroska's projection pose too.
///
/// # Errors
/// What differed.
pub fn video_mirroring_display_matrix(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for (name, rotation) in [
        ("h264-bt709-hflip.mp4", Rotation::Deg0),
        ("h264-bt709-vflip.mp4", Rotation::Deg180),
        ("h264-bt709-transverse.mp4", Rotation::Deg90),
        ("h264-bt709-transverse.mkv", Rotation::Deg90),
    ] {
        let Some(decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        let expected = Orientation {
            rotation,
            mirror: true,
        };
        let found = video_stream(&*decoder).map(|stream| stream.orientation);
        failures.check(found == Some(expected), || {
            format!("{name}: {found:?}, expected {expected:?}")
        });
    }
    failures.outcome()
}

/// A frame carries the color its stream tags, and none when nothing is tagged.
///
/// # Errors
/// What differed.
pub fn video_frame_color_is_the_streams(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for (name, expected) in [
        ("missing-color-metadata.mp4", None),
        ("cfr-bt709-limited.mp4", Some(ColorMetadata::EXPORT)),
    ] {
        let Some(mut decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        let found = first_frame(&mut *decoder).map(|frame| frame.color);
        failures.check(found == Ok(expected), || {
            format!("{name}: {found:?}, expected {expected:?}")
        });
    }
    failures.outcome()
}

/// Each frame carries the chroma siting its stream signals; H.264 without VUI infers left.
///
/// # Errors
/// What differed.
pub fn video_chroma_siting_is_the_streams(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for (name, siting) in [
        ("hdr-pq.mp4", ChromaLocation::Left),
        ("h264-bt709-no-vui.mp4", ChromaLocation::Left),
        ("hdr-pq-topleft.mp4", ChromaLocation::TopLeft),
        ("hevc-topleft.mp4", ChromaLocation::TopLeft),
        ("av1-colocated.mp4", ChromaLocation::TopLeft),
        ("av1.mp4", ChromaLocation::Unspecified),
    ] {
        let Some(mut decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        let found = first_frame(&mut *decoder).map(|frame| frame.chroma_location);
        failures.check(found == Ok(siting), || {
            format!("{name}: {found:?}, expected {siting:?}")
        });
    }
    failures.outcome()
}

/// A stream whose SPS crops 8 columns off the left shows exactly the 120 columns after them.
///
/// # Errors
/// What differed.
pub fn video_crop_shows_exactly_the_cropped_columns(backend: &dyn Backend) -> Outcome {
    let (Some(mut cropped), Some(mut whole)) = (
        video_or_refusal(backend, "h264-bt709-crop-left.mp4")?,
        video_or_refusal(backend, "h264-bt709-copy.mp4")?,
    ) else {
        return Ok(());
    };
    let mut failures = Failures::default();
    let stream = video_stream(&*cropped).and_then(|stream| stream.dimensions);
    failures.check(stream == Some((120, 96)), || {
        format!("the stream is {stream:?}, not 120x96")
    });
    for index in 0..3 {
        let shown = first_frame(&mut *cropped)?;
        let coded = first_frame(&mut *whole)?;
        let rect = shown.visible_rect;
        if (rect.width, rect.height) != (120, 96) {
            failures.check(false, || format!("frame {index}: {rect:?}"));
            continue;
        }
        failures.check(shown.chroma_location == coded.chroma_location, || {
            format!("frame {index}: chroma siting changed")
        });
        let mut differ = None;
        'planes: for (plane, step) in [(0_usize, 1_u32), (1, 2), (2, 2)] {
            let (Some(shown_plane), Some(coded_plane)) =
                (shown.planes.get(plane), coded.planes.get(plane))
            else {
                differ = Some(format!("plane {plane} missing"));
                break;
            };
            let at = |source: &alpha2_media::Plane, row: u32, x: u32| {
                usize::try_from(row * source.stride + x)
                    .ok()
                    .and_then(|offset| source.bytes.get(offset).copied())
            };
            for row in 0..coded_plane.height {
                for column in 0..120 / step {
                    if at(shown_plane, rect.y / step + row, rect.x / step + column)
                        != at(coded_plane, row, 8 / step + column)
                    {
                        differ = Some(format!("plane {plane}, row {row}, column {column}"));
                        break 'planes;
                    }
                }
            }
        }
        failures.check(differ.is_none(), || {
            format!("frame {index}: {}", differ.unwrap_or_default())
        });
    }
    failures.outcome()
}

/// The pixel aspect ratio of each picture is the one its container or bitstream declares, the
/// container winning; a ratio that changes mid-stream changes with its pictures.
///
/// # Errors
/// What differed.
pub fn video_pixel_aspect_is_the_streams(backend: &dyn Backend) -> Outcome {
    let mut failures = Failures::default();
    for (name, early, late) in [
        ("h264-bt709-copy.mp4", sar(1, 1), sar(1, 1)),
        ("h264-bt709-vui-sar.mp4", sar(4, 3), sar(4, 3)),
        ("h264-bt709-vui-sar.ts", sar(4, 3), sar(4, 3)),
        ("h264-bt709-pasp.mp4", sar(4, 3), sar(4, 3)),
        ("h264-bt709-sar-change.ts", sar(4, 3), sar(1, 1)),
    ] {
        let Some(mut decoder) = video_or_refusal(backend, name)? else {
            continue;
        };
        let cancel = CancelToken::default();
        let mut index = 0;
        loop {
            match decoder
                .decode_next(&cancel)
                .map_err(|error| format!("{name}: {error}"))?
            {
                DecodeEvent::Video(frame) => {
                    let expected = if index < 30 { early } else { late };
                    if frame.sample_aspect_ratio != expected && (index == 0 || index == 30) {
                        failures.check(false, || {
                            format!(
                                "{name} from frame {index}: {:?}, expected {expected:?}",
                                frame.sample_aspect_ratio
                            )
                        });
                    }
                    index += 1;
                }
                DecodeEvent::EndOfStream => break,
                _ => {}
            }
        }
        failures.check(index == 60, || format!("{name}: {index} frames, not 60"));
    }
    failures.outcome()
}
