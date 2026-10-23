//! Timeline projects shared by the evaluation benchmarks and the allocation
//! budget tests, so both measure the same work.

use std::collections::BTreeMap;

use alpha2_media::SourceId;
use alpha2_model::{
    Animated, Clip, ClipComposition, ClipEffect, ClipId, ClipTransition, Composition,
    CompositionId, CompositionNode, CubicBezier, Easing, EffectValue, Interpolation, Keyframe,
    MasterAudio, MediaKind, MediaSource, NodeId, NodeOp, Project, ProjectId, ProjectSettings,
    Timeline, Track, TrackId, TrackKind, Vec2,
};
use alpha2_time::{FrameRate, MediaTime, TimeRange};

/// Ticks every fixture's clips span: five seconds, 150 frames.
pub const SPAN_TICKS: i64 = 600_000;

/// What each visual clip carries besides its source.
#[derive(Copy, Clone, Debug)]
pub enum Load {
    /// A bare clip.
    Plain,
    /// A composition of two effects of three parameters each.
    Effects,
    /// The same effects with every parameter keyframed at eight points.
    AnimatedEffects,
    /// Position, scale, and opacity keyframed at eight points.
    Keyframes,
    /// Each track holds two overlapping clips joined by a transition.
    Transitions,
}

impl Load {
    /// Every load, in report order.
    pub const ALL: [Self; 5] = [
        Self::Plain,
        Self::Effects,
        Self::AnimatedEffects,
        Self::Keyframes,
        Self::Transitions,
    ];

    /// The name printed for this load.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Effects => "effects",
            Self::AnimatedEffects => "animated_effects",
            Self::Keyframes => "keyframes",
            Self::Transitions => "transitions",
        }
    }
}

/// `tracks` simultaneously active visual tracks at 1080p/29.97.
pub fn project(tracks: usize, load: Load) -> Project {
    let source_id = SourceId::new("video").expect("ID");
    let range = |start: i64, end: i64| {
        TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end)).expect("range")
    };
    let tracks = (0..tracks)
        .map(|index| {
            let mut track = Track::new(
                TrackId::new(format!("track-{index}")).expect("ID"),
                format!("Track {index}"),
                TrackKind::Video,
            );
            let clip = |name: &str, timeline: TimeRange| {
                let mut clip = Clip::new(
                    ClipId::new(format!("{name}-{index}")).expect("ID"),
                    source_id.clone(),
                    timeline,
                    range(0, timeline.end().ticks() - timeline.start().ticks()),
                );
                match load {
                    Load::Effects | Load::AnimatedEffects => {
                        clip.composition = Some(ClipComposition::new(
                            CompositionId::new(format!("look-{name}-{index}")).expect("ID"),
                        ));
                    }
                    Load::Keyframes => keyframes(&mut clip),
                    Load::Plain | Load::Transitions => {}
                }
                clip
            };
            if matches!(load, Load::Transitions) {
                let half = SPAN_TICKS / 2;
                let overlap = SPAN_TICKS / 4;
                track.clips.push(clip("from", range(0, half + overlap)));
                track
                    .clips
                    .push(clip("to", range(half - overlap, SPAN_TICKS)));
                track.transitions.push(ClipTransition {
                    from_clip: ClipId::new(format!("from-{index}")).expect("ID"),
                    to_clip: ClipId::new(format!("to-{index}")).expect("ID"),
                    timeline_range: range(half - overlap, half + overlap),
                    easing: Easing::CubicBezier(CubicBezier::new(0.42, 0.0, 0.58, 1.0)),
                    effect: ClipEffect {
                        id: "alpha2.transition.fade".into(),
                        version: 1,
                        params: BTreeMap::new(),
                        resources: BTreeMap::new(),
                    },
                });
            } else {
                track.clips.push(clip("clip", range(0, SPAN_TICKS)));
            }
            track
        })
        .collect::<Vec<_>>();
    // Each clip with effects shows its own chain of them.
    let compositions = tracks
        .iter()
        .flat_map(|track| &track.clips)
        .filter_map(|clip| clip.composition.as_ref())
        .map(|reference| {
            let effects = match load {
                Load::AnimatedEffects => animated_effects(),
                _ => effects(),
            };
            chain(reference.id.clone(), effects)
        })
        .collect();
    Project {
        schema_version: alpha2_model::CURRENT_SCHEMA_VERSION,
        id: ProjectId::new("benchmark").expect("ID"),
        name: "timeline benchmark".into(),
        settings: ProjectSettings::new(
            1_920,
            1_080,
            FrameRate::new(30_000, 1_001).expect("rate"),
            48_000,
        )
        .expect("settings"),
        sources: vec![MediaSource::new(
            source_id,
            "benchmark://video",
            MediaKind::Video,
        )],
        effect_resources: Vec::new(),
        compositions,
        timeline: Timeline { tracks },
        audio: MasterAudio::default(),
    }
}

/// `effects` applied in order to the clip's picture.
fn chain(id: CompositionId, effects: Vec<ClipEffect>) -> Composition {
    let node = |name: String| NodeId::new(name).expect("ID");
    let mut nodes = vec![CompositionNode::new(node("clip".into()), NodeOp::Clip {})];
    let mut previous = node("clip".into());
    for (index, effect) in effects.into_iter().enumerate() {
        let id = node(format!("effect-{index}"));
        nodes.push(CompositionNode::new(
            id.clone(),
            NodeOp::Effect {
                input: Some(previous),
                inputs: BTreeMap::new(),
                effect,
            },
        ));
        previous = id;
    }
    Composition {
        id,
        nodes,
        output: Some(previous),
        inputs: Vec::new(),
        controls: Vec::new(),
    }
}

fn effects() -> Vec<ClipEffect> {
    ["alpha2.effect.gaussian-blur", "alpha2.effect.chroma-key"]
        .into_iter()
        .map(|id| ClipEffect {
            id: id.into(),
            version: 1,
            params: [
                ("radius", EffectValue::Float(0.02)),
                ("offset", EffectValue::Vec2([0.1, 0.2])),
                ("tint", EffectValue::Color([0.2, 0.4, 0.6, 1.0])),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), Animated::Constant { value }))
            .collect(),
            resources: BTreeMap::new(),
        })
        .collect()
}

/// [`effects`] with every parameter keyframed at eight points.
fn animated_effects() -> Vec<ClipEffect> {
    let mut effects = effects();
    for effect in &mut effects {
        for animation in effect.params.values_mut() {
            let Animated::Constant { value } = *animation else {
                continue;
            };
            *animation = Animated::Keyframes {
                keyframes: (0..8)
                    .map(|step| Keyframe {
                        time: MediaTime::from_ticks(step * SPAN_TICKS / 8),
                        value,
                        interpolation: Interpolation::Linear,
                    })
                    .collect(),
            };
        }
    }
    effects
}

fn keyframes(clip: &mut Clip) {
    let frames = |value: &dyn Fn(i64) -> f32| {
        (0..8)
            .map(|step| Keyframe {
                time: MediaTime::from_ticks(step * SPAN_TICKS / 8),
                value: value(step),
                interpolation: Interpolation::Linear,
            })
            .collect::<Vec<_>>()
    };
    #[allow(clippy::cast_precision_loss)]
    let scalar = |step: i64| step as f32 / 8.0;
    clip.visual.opacity = Animated::Keyframes {
        keyframes: frames(&scalar),
    };
    clip.visual.position = alpha2_model::AnimatedPoint::from(Animated::Keyframes {
        keyframes: frames(&scalar)
            .into_iter()
            .map(|keyframe| Keyframe {
                time: keyframe.time,
                value: Vec2::new(keyframe.value, 1.0 - keyframe.value),
                interpolation: keyframe.interpolation,
            })
            .collect(),
    });
    clip.visual.scale = Animated::Keyframes {
        keyframes: frames(&|step| 1.0 + scalar(step))
            .into_iter()
            .map(|keyframe| Keyframe {
                time: keyframe.time,
                value: Vec2::new(keyframe.value, keyframe.value),
                interpolation: keyframe.interpolation,
            })
            .collect(),
    };
}
