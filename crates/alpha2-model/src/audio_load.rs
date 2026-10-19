//! How much audio effect state a project holds at once.
//!
//! [`crate::MAX_AUDIO_EFFECTS`] and [`crate::MAX_AUDIO_DELAY_MS`] bound the
//! state a mixer holds at one time, not the effects of the whole timeline: a
//! mixer holds the master's and every track's effects throughout, and a
//! clip's only while it mixes the clip. The TypeScript validator measures the
//! same way and gets the same numbers.

use crate::audio::TICKS_PER_MS;
use crate::{AUDIO_EFFECT_STATE_MARGIN_MS, AudioEffect, Project};

/// The most audio effect state a project holds at once.
///
/// The master's and every track's effects hold state throughout. A clip's
/// hold state from [`crate::AUDIO_EFFECT_STATE_MARGIN_MS`] and the lookahead
/// of its track's and its own effects before its start (never before timeline
/// zero) to its end, both ends included: a clip ending where another starts
/// holds state with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioEffectLoad {
    /// The most effects holding state at once.
    pub effects: usize,
    /// The earliest timeline position, in ticks, at which `effects` hold
    /// state.
    pub effects_at_ticks: i64,
    /// The most delay held at once, in ticks (120 a millisecond), each
    /// effect's [`AudioEffect::delay_ticks`].
    pub delay_ticks: u64,
    /// The earliest timeline position, in ticks, at which `delay_ticks` is
    /// held.
    pub delay_at_ticks: i64,
}

impl AudioEffectLoad {
    /// [`Self::delay_ticks`] in milliseconds, rounded up.
    #[must_use]
    pub fn delay_ms(&self) -> u64 {
        self.delay_ticks.div_ceil(u64::from(TICKS_PER_MS))
    }
}

/// One end of the interval in which a clip's effects hold state.
struct Edge {
    at: i64,
    /// Whether the interval ends here; at one position starts come first.
    ends: bool,
    effects: usize,
    delay: u64,
}

impl Project {
    /// Measures the most audio effect state this project holds at once, which
    /// validation bounds by [`crate::MAX_AUDIO_EFFECTS`] and
    /// [`crate::MAX_AUDIO_DELAY_MS`]. It sorts the clips' intervals once:
    /// O(n log n) in the clips with effects.
    #[must_use]
    pub fn audio_effect_load(&self) -> AudioEffectLoad {
        let chain = |effects: &[AudioEffect]| {
            (
                effects.len(),
                effects.iter().map(AudioEffect::delay_ticks).sum::<u64>(),
            )
        };
        let lookahead = |effects: &[AudioEffect]| {
            effects
                .iter()
                .map(AudioEffect::lookahead_ticks)
                .sum::<u64>()
        };
        let margin = u64::from(AUDIO_EFFECT_STATE_MARGIN_MS) * u64::from(TICKS_PER_MS);

        let (mut effects, mut delay) = chain(&self.audio.effects);
        let mut edges = Vec::new();
        for track in &self.timeline.tracks {
            let (count, held) = chain(&track.audio.effects);
            effects += count;
            delay += held;
            let track_lookahead = lookahead(&track.audio.effects);
            for clip in &track.clips {
                if clip.audio.effects.is_empty() {
                    continue;
                }
                let (count, held) = chain(&clip.audio.effects);
                let lead = track_lookahead + lookahead(&clip.audio.effects) + margin;
                let lead = i64::try_from(lead).unwrap_or(i64::MAX);
                let range = clip.timeline_range;
                edges.push(Edge {
                    at: range.start().ticks().saturating_sub(lead).max(0),
                    ends: false,
                    effects: count,
                    delay: held,
                });
                edges.push(Edge {
                    at: range.end().ticks(),
                    ends: true,
                    effects: count,
                    delay: held,
                });
            }
        }
        edges.sort_unstable_by_key(|edge| (edge.at, edge.ends));

        let mut load = AudioEffectLoad {
            effects,
            effects_at_ticks: 0,
            delay_ticks: delay,
            delay_at_ticks: 0,
        };
        for edge in edges {
            if edge.ends {
                effects -= edge.effects;
                delay -= edge.delay;
                continue;
            }
            effects += edge.effects;
            delay += edge.delay;
            if effects > load.effects {
                load.effects = effects;
                load.effects_at_ticks = edge.at;
            }
            if delay > load.delay_ticks {
                load.delay_ticks = delay;
                load.delay_at_ticks = edge.at;
            }
        }
        load
    }
}
