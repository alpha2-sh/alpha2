//! Caption tracks and their cues.

use super::*;

#[test]
fn a_caption_track_holds_as_many_cues_as_a_track_holds_clips() {
    use crate::{CaptionCue, Captions, TextStyle};

    let cue = |start: i64, end: i64| CaptionCue {
        timeline_range: TimeRange::new(MediaTime(start), MediaTime(end)).expect("range"),
        text: "Hello".into(),
    };
    let with_cues = |cues| {
        let mut project = project();
        let mut track = Track::new(
            id(TrackId::new, "track-captions"),
            "Captions",
            TrackKind::Caption,
        );
        track.captions = Some(Captions {
            style: TextStyle::default(),
            cues,
        });
        project.timeline.tracks.push(track);
        project
    };
    let sequential = |count: usize| {
        (0..i64::try_from(count).expect("count"))
            .map(|index| cue(index * 10, index * 10 + 10))
            .collect::<Vec<_>>()
    };
    with_cues(sequential(crate::MAX_CLIPS_PER_TRACK))
        .validate()
        .expect("a full caption track");
    let error = with_cues(sequential(crate::MAX_CLIPS_PER_TRACK + 1))
        .validate()
        .expect_err("one cue too many");
    assert_eq!(
        error.to_string(),
        "track track-captions has 4097 caption cues; the maximum is 4096"
    );
}

#[test]
fn caption_tracks_hold_ordered_cues_and_nothing_else() {
    use crate::{CaptionCue, Captions, TextStyle};

    let cue = |start: i64, end: i64| CaptionCue {
        timeline_range: TimeRange::new(MediaTime(start), MediaTime(end)).expect("range"),
        text: "Hello".into(),
    };
    let with_captions = |captions: Option<Captions>, kind: TrackKind| {
        let mut project = project();
        let mut track = Track::new(id(TrackId::new, "track-captions"), "Captions", kind);
        track.captions = captions;
        project.timeline.tracks.push(track);
        project
    };
    let invalid = |project: &crate::Project| {
        matches!(
            project.validate(),
            Err(crate::ModelError::InvalidCaptions { .. })
        )
    };
    let captions = |cues| {
        Some(Captions {
            style: TextStyle::default(),
            cues,
        })
    };
    with_captions(
        captions(vec![cue(0, 100), cue(100, 200)]),
        TrackKind::Caption,
    )
    .validate()
    .expect("cues that meet");
    // Overlapping, out of order, empty text, a caption track without captions, captions on a video
    // track.
    assert!(invalid(&with_captions(
        captions(vec![cue(0, 150), cue(100, 200)]),
        TrackKind::Caption
    )));
    assert!(invalid(&with_captions(
        captions(vec![cue(100, 200), cue(0, 50)]),
        TrackKind::Caption
    )));
    let mut empty = cue(0, 10);
    empty.text = crate::TextContent::default();
    assert!(invalid(&with_captions(
        captions(vec![empty]),
        TrackKind::Caption
    )));
    assert!(invalid(&with_captions(None, TrackKind::Caption)));
    assert!(invalid(&with_captions(
        captions(Vec::new()),
        TrackKind::Video
    )));
    // A style out of range.
    let mut style = TextStyle {
        font_size: 0.6,
        ..TextStyle::default()
    };
    let mut project = with_captions(captions(Vec::new()), TrackKind::Caption);
    project.timeline.tracks[1]
        .captions
        .as_mut()
        .expect("captions")
        .style = style.clone();
    assert!(invalid(&project));
    style.font_size = 0.05;
    // A font family that is empty or too long.
    for family in [String::new(), "a".repeat(crate::MAX_FONT_FAMILY_BYTES + 1)] {
        style.font_family = Some(family);
        project.timeline.tracks[1]
            .captions
            .as_mut()
            .expect("captions")
            .style = style.clone();
        assert!(invalid(&project));
    }
    style.font_family = Some("Noto Sans KR".into());
    project.timeline.tracks[1]
        .captions
        .as_mut()
        .expect("captions")
        .style = style;
    assert!(!invalid(&project));
}

#[test]
fn a_caption_look_is_held_to_its_limits() {
    use crate::{CaptionCue, Captions, TextStyle};

    fn stroke(width: f32) -> crate::TextStroke {
        crate::TextStroke {
            color: crate::Rgba::new(0.0, 0.0, 0.0, 1.0),
            width,
        }
    }
    fn shadow(offset: crate::Vec2, blur: f32) -> crate::TextShadow {
        crate::TextShadow {
            color: crate::Rgba::new(0.0, 0.0, 0.0, 1.0),
            offset,
            blur,
        }
    }

    let mut project = project();
    let mut track = Track::new(
        id(TrackId::new, "track-captions"),
        "Captions",
        TrackKind::Caption,
    );
    track.captions = Some(Captions {
        style: TextStyle::default(),
        cues: vec![CaptionCue {
            timeline_range: TimeRange::new(MediaTime(0), MediaTime(100)).expect("range"),
            text: "Hello".into(),
        }],
    });
    project.timeline.tracks.push(track);
    let caption_track = project.timeline.tracks.len() - 1;
    let invalid = |project: &crate::Project| {
        matches!(
            project.validate(),
            Err(crate::ModelError::InvalidCaptions { .. })
        )
    };
    let style = TextStyle::default();
    // A stroke of no width, more strokes than allowed, strokes wider together than the extent,
    // more shadows than allowed, a shadow too far off, and a blur out of range.
    let looks: [fn(&mut TextStyle); 6] = [
        |style| style.strokes = vec![stroke(0.0)],
        |style| style.strokes = vec![stroke(0.05); crate::MAX_TEXT_STROKES + 1],
        |style| style.strokes = vec![stroke(0.3), stroke(0.25)],
        |style| style.shadows = vec![shadow(crate::Vec2::default(), 0.1); 5],
        |style| style.shadows = vec![shadow(crate::Vec2::new(0.0, 1.5), 0.1)],
        |style| style.shadows = vec![shadow(crate::Vec2::default(), -0.1)],
    ];
    for look in looks {
        let mut invalid_style = style.clone();
        look(&mut invalid_style);
        project.timeline.tracks[caption_track]
            .captions
            .as_mut()
            .expect("captions")
            .style = invalid_style;
        assert!(invalid(&project));
    }
    // Four strokes reaching the whole extent and four shadows at the limits are valid.
    let mut look = style.clone();
    look.strokes = vec![stroke(0.125); crate::MAX_TEXT_STROKES];
    look.shadows = vec![shadow(crate::Vec2::new(-1.0, 1.0), 1.0); crate::MAX_TEXT_SHADOWS];
    project.timeline.tracks[caption_track]
        .captions
        .as_mut()
        .expect("captions")
        .style = look;
    assert!(!invalid(&project));
}
