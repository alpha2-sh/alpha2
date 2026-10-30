use super::*;
use alpha2_eval::SampledAnimator;
use alpha2_model::{RunTime, TextRun};
use alpha2_text::{ClusterInfo, LineInfo};

/// A layout of `count` one-byte clusters on one line, 10 pixels each, every
/// fourth a space between words.
fn row(count: usize) -> TextLayout {
    let clusters = (0..count)
        .map(|index| ClusterInfo {
            bytes: index..index + 1,
            line: 0,
            word: (index % 4 != 3).then(|| u32::try_from(index / 4).expect("small")),
            is_space: index % 4 == 3,
            #[allow(clippy::cast_precision_loss)]
            x: index as f32 * 10.0,
            advance: 10.0,
            baseline: 16.0,
        })
        .collect();
    TextLayout {
        clusters,
        lines: vec![LineInfo {
            clusters: 0..count,
            left: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: count as f32 * 10.0,
            baseline: 16.0,
        }],
        font_size: 20.0,
    }
}

fn range(shape: RangeShape, units: RangeUnits, start: f32, end: f32) -> SampledSelector {
    SampledSelector::Range {
        based_on: TextUnit::Characters,
        units,
        start,
        end,
        offset: 0.0,
        shape,
        amount: 1.0,
    }
}

fn close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (actual - expected).abs() < 1e-6,
            "unit {index}: {actual} != {expected} in {actual:?}"
        );
    }
}

#[test]
fn each_range_shape_matches_its_hand_values() {
    // Five units, from 0.5 to 3.5: unit centres 0.5 .. 4.5 lie at u = 0, 1/3, 2/3, 1, 4/3.
    let layout = row(5);
    let at = |shape| {
        selection(
            &range(shape, RangeUnits::Index, 0.5, 3.5),
            &layout,
            &[],
            MediaTime::ZERO,
        )
    };
    close(&at(RangeShape::Square), &[0.5, 1.0, 1.0, 0.5, 0.0]);
    let third = 1.0 / 3.0;
    close(
        &at(RangeShape::RampUp),
        &[0.0, third, 2.0 * third, 1.0, 1.0],
    );
    close(
        &at(RangeShape::RampDown),
        &[1.0, 2.0 * third, third, 0.0, 0.0],
    );
    close(
        &at(RangeShape::Triangle),
        &[0.0, 2.0 * third, 2.0 * third, 0.0, 0.0],
    );
    let round = (1.0_f32 - third * third).sqrt();
    close(&at(RangeShape::Round), &[0.0, round, round, 0.0, 0.0]);
    let t = 2.0 * third;
    let smooth = t * t * (3.0 - 2.0 * t);
    close(&at(RangeShape::Smooth), &[0.0, smooth, smooth, 0.0, 0.0]);
    // An empty range selects nothing; a reversed one is the same range.
    close(
        &selection(
            &range(RangeShape::Square, RangeUnits::Index, 2.0, 2.0),
            &layout,
            &[],
            MediaTime::ZERO,
        ),
        &[0.0; 5],
    );
    close(
        &selection(
            &range(RangeShape::Square, RangeUnits::Index, 3.5, 0.5),
            &layout,
            &[],
            MediaTime::ZERO,
        ),
        &at(RangeShape::Square),
    );
}

#[test]
fn percent_and_index_units_agree() {
    let layout = row(8);
    for shape in [RangeShape::Square, RangeShape::RampUp, RangeShape::Smooth] {
        let percent = selection(
            &range(shape, RangeUnits::Percent, 12.5, 62.5),
            &layout,
            &[],
            MediaTime::ZERO,
        );
        let index = selection(
            &range(shape, RangeUnits::Index, 1.0, 5.0),
            &layout,
            &[],
            MediaTime::ZERO,
        );
        close(&percent, &index);
    }
}

#[test]
fn words_and_spaces_follow_the_layout_units() {
    // "abc def gh": words 0, 1, 2; the spaces in none; lines all 0.
    let layout = row(10);
    let words = SampledSelector::Range {
        based_on: TextUnit::Words,
        units: RangeUnits::Index,
        start: 1.0,
        end: 2.0,
        offset: 0.0,
        shape: RangeShape::Square,
        amount: 1.0,
    };
    close(
        &selection(&words, &layout, &[], MediaTime::ZERO),
        &[0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0],
    );
    // Without spaces, units 0..2 are the first word's three letters only.
    let letters = SampledSelector::Range {
        based_on: TextUnit::CharactersExcludingSpaces,
        units: RangeUnits::Index,
        start: 2.0,
        end: 4.0,
        offset: 0.0,
        shape: RangeShape::Square,
        amount: 0.5,
    };
    close(
        &selection(&letters, &layout, &[], MediaTime::ZERO),
        &[0.0, 0.0, 0.5, 0.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0],
    );
}

fn timed_runs() -> Vec<TextRun> {
    let run = |text: &str, start: i64, end: i64, class: &[&str]| TextRun {
        text: text.into(),
        class: class.iter().map(|&name| name.into()).collect(),
        time: Some(RunTime {
            start: MediaTime(start),
            end: MediaTime(end),
        }),
    };
    vec![
        run("abc ", 0, 1_000, &["first"]),
        run("def ", 1_000, 2_000, &[]),
        run("gh", 2_000, 3_000, &["key", "first"]),
    ]
}

#[test]
fn timing_states_partition_the_run() {
    let (layout, runs) = (row(10), timed_runs());
    for tick in (0..3_500).step_by(125) {
        let time = MediaTime(tick);
        let state = |state| {
            selection(
                &SampledSelector::Timing {
                    state,
                    ramp: MediaTime::ZERO,
                    sweep: false,
                },
                &layout,
                &runs,
                time,
            )
        };
        let (upcoming, active, spoken) = (
            state(RunState::Upcoming),
            state(RunState::Active),
            state(RunState::Spoken),
        );
        for cluster in 0..10 {
            let sum = upcoming[cluster] + active[cluster] + spoken[cluster];
            assert!((sum - 1.0).abs() < 1e-6, "tick {tick}, cluster {cluster}");
        }
    }
    // At 1000 the first run is spoken and the second active; with a ramp they cross over.
    let at = |state, ramp, tick| {
        selection(
            &SampledSelector::Timing {
                state,
                ramp: MediaTime(ramp),
                sweep: false,
            },
            &layout,
            &runs,
            MediaTime(tick),
        )
    };
    assert_eq!(at(RunState::Spoken, 0, 1_000)[0], 1.0);
    assert_eq!(at(RunState::Active, 0, 1_000)[4], 1.0);
    assert!((at(RunState::Active, 200, 1_100)[4] - 0.5).abs() < 1e-6);
    assert!((at(RunState::Spoken, 200, 1_100)[0] - 0.5).abs() < 1e-6);
}

#[test]
fn sweep_splits_a_run_over_its_clusters() {
    let (layout, runs) = (row(10), timed_runs());
    // The second run's four clusters take a quarter of its second each.
    let active = |tick| {
        selection(
            &SampledSelector::Timing {
                state: RunState::Active,
                ramp: MediaTime::ZERO,
                sweep: true,
            },
            &layout,
            &runs,
            MediaTime(tick),
        )
    };
    assert_eq!(&active(1_100)[4..8], [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(&active(1_600)[4..8], [0.0, 0.0, 1.0, 0.0]);
}

#[test]
fn class_selects_runs_by_name() {
    let (layout, runs) = (row(10), timed_runs());
    let class = |name: &str| {
        selection(
            &SampledSelector::Class { class: name.into() },
            &layout,
            &runs,
            MediaTime::ZERO,
        )
    };
    assert_eq!(
        class("first"),
        [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0]
    );
    assert_eq!(class("key")[8..], [1.0, 1.0]);
    assert_eq!(class("none"), [0.0; 10]);
}

fn animator(selector: SampledSelector) -> SampledAnimator {
    SampledAnimator {
        selector,
        offset: None,
        scale: None,
        rotation_degrees: None,
        opacity: None,
        fill: None,
        stroke: None,
        tracking: None,
    }
}

#[test]
fn animators_apply_in_order() {
    let layout = row(4);
    let all = range(RangeShape::Square, RangeUnits::Percent, 0.0, 100.0);
    let half = SampledSelector::Range {
        based_on: TextUnit::Characters,
        units: RangeUnits::Percent,
        start: 0.0,
        end: 100.0,
        offset: 0.0,
        shape: RangeShape::Square,
        amount: 0.5,
    };
    let animators = [
        SampledAnimator {
            scale: Some([2.0, 3.0]),
            offset: Some([0.5, -0.25]),
            fill: Some([1.0, 0.0, 0.0, 1.0]),
            ..animator(all.clone())
        },
        SampledAnimator {
            scale: Some([3.0, 1.0]),
            rotation_degrees: Some(90.0),
            opacity: Some(0.0),
            fill: Some([0.0, 0.0, 1.0, 1.0]),
            tracking: Some(0.1),
            ..animator(half)
        },
    ];
    let (states, hash) = cluster_states(&layout, &[], &animators, MediaTime::ZERO, 20.0);
    assert_ne!(hash, 0);
    let state = &states[0];
    // Scales multiply: 2 then 1 + 0.5 * (3 - 1) = 2 on x; 3 then 1 on y.
    assert_eq!(state.scale, [4.0, 3.0]);
    assert_eq!(state.offset, [10.0, -5.0]);
    assert!((state.rotation - 45.0_f32.to_radians()).abs() < 1e-6);
    assert!((state.opacity - 0.5).abs() < 1e-6);
    assert!((state.tracking - 1.0).abs() < 1e-6);
    // Red, then half way to blue: half of red kept, half of blue added.
    let fill = state.fill.expect("a fill mix");
    assert!((fill.keep - 0.0).abs() < 1e-6);
    assert!((fill.add[0] - 0.5).abs() < 1e-6 && (fill.add[2] - 0.5).abs() < 1e-6);
    // Nothing selected draws as nothing animated.
    let none = [SampledAnimator {
        scale: Some([2.0, 2.0]),
        ..animator(range(RangeShape::Square, RangeUnits::Index, 0.0, 0.0))
    }];
    let (identity, hash) = cluster_states(&layout, &[], &none, MediaTime::ZERO, 20.0);
    assert_eq!(hash, 0);
    assert!(identity.iter().all(ClusterState::is_identity));
}
