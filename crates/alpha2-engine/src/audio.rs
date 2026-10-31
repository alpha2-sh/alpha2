//! Project-driven audio mixing.
//!
//! [`AudioRenderer`] turns a project snapshot and host-decoded source blocks into one mixed,
//! project-rate planar block. The host owns decode; this renderer owns gain, panning, fades,
//! effects, bus summing, and limiting.
//!
//! Signal order per clip is fixed:
//! source -> resample -> clip effects -> clip gain/fades -> pan -> track bus.
//! Gain and fades come after the clip effects so a fade reaches true silence even when an effect
//! has a tail.
//!
//! Track buses sum into the master bus. A ducked track reads the *pre-duck* output of its
//! sidechain track, so the mix never depends on track order. A bus nothing writes in a block is
//! neither cleared nor summed.

use std::collections::HashMap;

use alpha2_audio::{
    Biquad, BiquadKind, ChannelLayout, Compressor, Ducker, Interpolator, LoudnessMeter,
    LoudnessReport, PeakLimiter, ProcessCtx, Processor, ProcessorChain, SchroederReverb, Stretch,
    TransportState,
};
use alpha2_eval::{ActiveAudioClip, AudioSpanScratch, sample_scalar};
use alpha2_media::{AudioBlock, AudioBlockProvider, SourceId, SpeakerLayout};
use alpha2_model::{
    Animated, AudioEffect, ClipId, EqBandKind, LOOP_FADE_SECONDS, LoopMode, MAX_GAIN, MasterAudio,
    TimeMap, TrackAudio, TrackId,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND, TimeRange};

use crate::downmix::{ChannelMix, Downmix};
use crate::{EngineError, EngineProject};

mod stretch_read;

use stretch_read::{StretchSlot, stretched_request, stretches};

/// Cap on cached interpolation weights for a retimed read (256 KiB).
const MAX_CACHED_WEIGHTS: usize = 1 << 15;

/// Largest block a renderer will mix in one call.
pub const MAX_AUDIO_BLOCK: usize = 8_192;

// The model counts a clip's effects as built from a margin before its lookahead. That margin must
// hold one largest block at the lowest rate plus the rounding at each clip end and chain lookahead.
const _: () = {
    let rate = *alpha2_model::SAMPLE_RATE_RANGE.start() as usize;
    let samples = MAX_AUDIO_BLOCK + 2 + alpha2_model::MAX_CHAIN_EFFECTS;
    assert!(
        samples * 1_000 <= alpha2_model::AUDIO_EFFECT_STATE_MARGIN_MS as usize * rate,
        "AUDIO_EFFECT_STATE_MARGIN_MS must hold a largest block at the lowest rate"
    );
};

/// Longest a stage replays to settle before a seek target, in seconds.
///
/// A recursive effect's settling horizon is unbounded as it nears infinite memory, so each
/// stage's lead is capped here. A seek replays at most this much audio plus the graph's lookahead;
/// a stage with a longer horizon only approximates its settled state.
pub const MAX_SETTLING_SECONDS: u32 = 30;

/// What [`AudioRenderer::render`] does with a block that does not start
/// where the previous one ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AudioContinuity {
    /// Every such block resets the renderer and replays the settling history before it, so it
    /// mixes what a fresh renderer would. Use for exports, loudness measurement, and any output
    /// that must repeat.
    #[default]
    Exact,
    /// For live preview only. A block starting within [`AudioRenderer::max_block`] samples of
    /// where the previous one ended continues from that state without a reset or replay, so a
    /// small step costs one block instead of up to [`MAX_SETTLING_SECONDS`] of replay. The result
    /// differs from a fresh renderer's but is still determined by the blocks asked for, in order.
    /// Longer jumps, and the first block after a reset, are exact.
    ContinueSmallGaps,
}

/// Outcome of mixing one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRenderStatus {
    /// Sample index of the first mixed sample.
    pub start_sample: i64,
    /// Number of mixed samples per channel.
    pub frames: usize,
    /// Sources an audible clip needed but the provider could not supply.
    ///
    /// Those clips contributed silence and the block was not metered. Decode them and render the
    /// block again. With [`AudioRenderer::set_hold_incomplete_blocks`] the whole block was silent,
    /// nothing advanced, and the retry gives the uninterrupted mix; otherwise it gives the mix
    /// after a seek.
    pub missing_sources: Vec<SourceId>,
}

impl AudioRenderStatus {
    /// Returns whether every audible clip was mixed from real samples.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing_sources.is_empty()
    }
}

/// A source interval a host decodes before a render reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioNeed {
    /// Source to decode or retrieve.
    pub source_id: SourceId,
    /// Half-open source interval in ticks, including interpolation neighbours.
    pub source_range: TimeRange,
}

struct TrackBus {
    id: TrackId,
    audio: TrackAudio,
    chain: ProcessorChain,
    ducker: Option<BusDucker>,
    sidechain: Option<usize>,
    /// The bus's samples for the block, valid only while [`Self::live`].
    output: [Vec<f32>; 2],
    /// Whether `output` holds this block's samples. Otherwise it is stale and is cleared only when
    /// a stage reads or adds to it ([`Self::make_live`]).
    live: bool,
    /// Samples the bus chain delays its input by.
    latency: i64,
    /// One slot per clip on the track, in the track's order.
    clips: Vec<ClipSlot>,
    /// How long before a seek target the bus chain must run to settle (see [`Leads`]).
    chain_lead: i64,
    /// The same for the ducker, when one runs.
    ducker_lead: i64,
    /// Whether the bus is heard or keys the ducker of one that is (see [`read_buses`]).
    read: bool,
}

impl TrackBus {
    /// The bus driving this bus's ducker, if any and not itself; `index` is this bus's own.
    fn active_sidechain(&self, index: usize) -> Option<usize> {
        self.ducker.as_ref()?;
        self.sidechain.filter(|&sidechain| sidechain != index)
    }

    /// Clears the first `frames` samples of `output` unless something already wrote the block.
    fn make_live(&mut self, channels: usize, frames: usize) {
        if self.live {
            return;
        }
        for plane in self.output.iter_mut().take(channels) {
            plane[..frames].fill(0.0);
        }
        self.live = true;
        #[cfg(test)]
        BUS_PLANES_CLEARED.set(BUS_PLANES_CLEARED.get() + channels);
    }
}

/// A track's ducker and its output buffer, held only by buses that duck.
struct BusDucker {
    ducker: Ducker,
    output: [Vec<f32>; 2],
}

struct ClipSlot {
    id: ClipId,
    chain: Option<ClipChain>,
}

/// A clip's own effect chain.
///
/// Processors exist only while the clip sounds (see [`AudioRenderer::keep_mixed_chains`]), so
/// renderer state is bounded by the clips one block mixes
/// ([`alpha2_model::Project::audio_effect_load`]), not by the whole timeline.
struct ClipChain {
    effects: Vec<AudioEffect>,
    chain: Option<ProcessorChain>,
    /// The block that last ran the chain.
    ran_in: u64,
    /// Samples the chain delays its input by.
    latency: i64,
    /// Samples the chain's nodes need to settle, excluding their delay.
    history: u32,
    /// Input position the chain continues from; anything else resets it.
    next_input: Option<i64>,
    /// How long before a seek target the chain must run to settle.
    lead: i64,
}

/// Mixes a project's audio tracks into one project-rate block.
pub struct AudioRenderer {
    layout: ChannelLayout,
    sample_rate: u32,
    max_block: usize,
    /// One bus per track, in project order.
    buses: Vec<TrackBus>,
    /// Buses by track ID, for clips evaluated from a different snapshot than the renderer's.
    bus_of_track: HashMap<TrackId, usize>,
    /// Clip slots by clip ID, `(bus, position)`, for the same case.
    slot_of_clip: HashMap<ClipId, (usize, usize)>,
    span_scratch: AudioSpanScratch,
    master_chain: ProcessorChain,
    /// Samples the master chain delays its input by.
    master_latency: i64,
    /// The longest latency from any clip's source to the output.
    lookahead: i64,
    /// Settling history beyond latency compensation: the longest lead of any stage.
    history: i64,
    /// How long before a seek target the master chain must run to settle.
    master_lead: i64,
    /// The same for the limiter.
    limiter_lead: i64,
    /// Distinct stage leads, longest first; a replay cuts its blocks at these so each stage starts
    /// at its own sample whatever the block size (see [`Self::replay_frames`]).
    stage_leads: Vec<i64>,
    master: MasterAudio,
    limiter: Option<PeakLimiter>,
    /// Loudness normalization gain, between the master gain and the limiter. 1.0 unless set.
    normalization_gain: f32,
    hold_incomplete: bool,
    continuity: AudioContinuity,
    /// Measures what reaches the limiter, when metering is on.
    meter: Option<LoudnessMeter>,
    /// Measures the output after the limiter, when output metering is on.
    output_meter: Option<LoudnessMeter>,
    any_solo: bool,
    clip_a: [Vec<f32>; 2],
    clip_b: [Vec<f32>; 2],
    bus_in: [Vec<f32>; 2],
    mix: [Vec<f32>; 2],
    output: [Vec<f32>; 2],
    /// Per-sample gain and left/right pan weights (see [`fill_envelope`]).
    envelope: [Vec<f32>; 3],
    /// Where the last successful render ended; `None` before the first.
    next_sample: Option<i64>,
    /// Slots, `(bus, position)`, whose clip chain is built.
    live_chains: Vec<(usize, usize)>,
    blocks_mixed: u64,
    /// Interpolation weights for a retimed read, per position or per repeating fraction.
    weights: Vec<f64>,
    /// Scratch for one fraction's weights while `weights` holds them all.
    phase_weights: Vec<f64>,
    /// Pitch-preserving stretch state for clips a block reads.
    stretch: Stretch,
    stretch_slots: HashMap<ClipId, StretchSlot>,
    /// The most clip effects, and their delay in ticks, built at once.
    #[cfg(test)]
    most_held: (usize, u64),
    /// Project samples [`Self::read_source`] has read from sources.
    #[cfg(test)]
    samples_read: usize,
}

/// What [`AudioRenderer::fill_clip`] does with a clip's source samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fill {
    /// Reads and resamples them into `clip_a`.
    Samples,
    /// Only checks that `provider` supplies them, as a held render does before mixing.
    Presence,
}

impl AudioRenderer {
    /// Builds every track and master processor for a project snapshot. Clip processors are built
    /// when the clip is first mixed.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidAudioConfiguration`] for an unsupported
    /// channel count, a zero sample rate, or an oversized block.
    pub fn new(
        snapshot: &EngineProject,
        channels: usize,
        max_block: usize,
    ) -> Result<Self, EngineError> {
        let layout =
            ChannelLayout::from_channels(channels).ok_or(EngineError::InvalidAudioConfiguration)?;
        let project = snapshot.project();
        let sample_rate = project.settings.sample_rate;
        if sample_rate == 0 || max_block == 0 || max_block > MAX_AUDIO_BLOCK {
            return Err(EngineError::InvalidAudioConfiguration);
        }

        let (mut buses, bus_of_track) = track_buses(project, layout, sample_rate, max_block);
        resolve_sidechains(&mut buses, &bus_of_track);
        let slot_of_clip = buses
            .iter()
            .enumerate()
            .flat_map(|(bus, track)| {
                track
                    .clips
                    .iter()
                    .enumerate()
                    .map(move |(position, slot)| (slot.id.clone(), (bus, position)))
            })
            .collect();

        let mut master_chain = ProcessorChain::new(layout);
        push_effects(&mut master_chain, &project.audio.effects);
        master_chain.prepare(sample_rate, max_block);
        let limiter = project.audio.limiter.map(|limiter| {
            let mut peak = PeakLimiter::new(limiter.threshold, limiter.release_ms);
            peak.prepare(sample_rate, max_block);
            peak
        });
        let any_solo = project.timeline.any_solo();
        read_buses(&mut buses, any_solo);
        let master_latency = i64::from(master_chain.latency_samples());
        let lookahead = master_latency + longest_path_latency(&buses);
        let settling = Settling(i64::from(sample_rate) * i64::from(MAX_SETTLING_SECONDS));
        let limiter_lead = settling.lead(0, limiter.as_ref().map_or(0, Processor::history_samples));
        let master_lead = settling.lead(limiter_lead, master_chain.history_samples());
        let history = assign_leads(&mut buses, master_lead, settling).max(master_lead);
        let stage_leads = stage_leads(&buses, master_lead, limiter_lead);

        Ok(Self {
            layout,
            sample_rate,
            max_block,
            buses,
            bus_of_track,
            slot_of_clip,
            span_scratch: AudioSpanScratch::default(),
            master_chain,
            master_latency,
            lookahead,
            history,
            master_lead,
            limiter_lead,
            stage_leads,
            master: project.audio.clone(),
            limiter,
            normalization_gain: 1.0,
            hold_incomplete: false,
            continuity: AudioContinuity::Exact,
            meter: None,
            output_meter: None,
            any_solo,
            clip_a: buffers(max_block),
            clip_b: buffers(max_block),
            bus_in: buffers(max_block),
            mix: buffers(max_block),
            output: buffers(max_block),
            envelope: [
                vec![0.0; max_block],
                vec![0.0; max_block],
                vec![0.0; max_block],
            ],
            next_sample: None,
            live_chains: Vec::new(),
            blocks_mixed: 0,
            weights: Vec::new(),
            phase_weights: Vec::new(),
            stretch: Stretch::new(sample_rate),
            stretch_slots: HashMap::new(),
            #[cfg(test)]
            most_held: (0, 0),
            #[cfg(test)]
            samples_read: 0,
        })
    }

    /// Returns the project sample rate every block uses.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the channel count of every mixed block.
    #[must_use]
    pub const fn channels(&self) -> usize {
        self.layout.channels()
    }

    /// Returns the audio tracks this renderer mixes, in bus order.
    #[must_use]
    pub fn track_ids(&self) -> Vec<&TrackId> {
        self.buses.iter().map(|bus| &bus.id).collect()
    }

    /// Returns the largest block this renderer accepts.
    #[must_use]
    pub const fn max_block(&self) -> usize {
        self.max_block
    }

    /// Returns one mixed output channel from the most recent render.
    #[must_use]
    pub fn channel(&self, index: usize) -> &[f32] {
        self.output.get(index).map_or(&[], Vec::as_slice)
    }

    /// How far ahead of a block, in samples, the renderer reads its sources.
    ///
    /// The renderer reads each path's input early by its latency to the output (for example a
    /// compressor's `lookahead_ms`), so the output lines up with the timeline. Use
    /// [`Self::audio_needs`] for source supply, since a cold seek also needs settling history.
    #[must_use]
    pub fn required_lookahead_samples(&self) -> u32 {
        u32::try_from(self.lookahead).unwrap_or(u32::MAX)
    }

    /// The source intervals rendering `frames` samples from `start_sample` reads, for a host to
    /// decode first.
    ///
    /// These are exactly what [`Self::render`] would read, including any settling replay, so a
    /// host that holds them needs no margin. Overlapping or touching intervals of one source are
    /// merged; disjoint ones stay separate.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidAudioConfiguration`] for a negative start, an empty or
    /// oversized block, or a span that does not fit in ticks, and an evaluation error when the
    /// snapshot cannot be evaluated over a block's span.
    pub fn audio_needs(
        &self,
        snapshot: &EngineProject,
        start_sample: i64,
        frames: usize,
    ) -> Result<Vec<AudioNeed>, EngineError> {
        self.check_block(start_sample, frames)?;
        let mut needs = Vec::new();
        let mut scratch = AudioSpanScratch::default();
        if self.continues_at(start_sample).is_none() {
            let mut position = self.preroll_start(start_sample)?;
            while position < start_sample {
                let count = self.replay_frames(position, start_sample);
                self.block_needs(
                    snapshot,
                    position,
                    count,
                    Some(start_sample),
                    &mut scratch,
                    &mut needs,
                )?;
                position += i64::try_from(count).unwrap_or(i64::MAX);
            }
        }
        self.block_needs(
            snapshot,
            start_sample,
            frames,
            None,
            &mut scratch,
            &mut needs,
        )?;
        Ok(needs)
    }

    /// Adds what one block of [`Self::mix_block`] reads to `needs`.
    fn block_needs(
        &self,
        snapshot: &EngineProject,
        start: i64,
        frames: usize,
        replay_target: Option<i64>,
        scratch: &mut AudioSpanScratch,
        needs: &mut Vec<AudioNeed>,
    ) -> Result<(), EngineError> {
        let frames_i64 = i64::try_from(frames).map_err(|_| EngineError::OutOfRange)?;
        let stages = Stages {
            target: replay_target,
            block_end: start + frames_i64,
            lookahead: self.lookahead,
        };
        let span = self.block_span(start, frames_i64)?;
        let described = snapshot
            .evaluate_audio_in(span, scratch)
            .map_err(EngineError::Evaluate)?;
        let master_input = start + self.master_latency;
        let rate = self.sample_rate;
        for clip in described.iter() {
            let Some(read) = self.clip_read(&clip, master_input, frames, stages) else {
                continue;
            };
            source_requests(
                &self.stretch,
                &clip,
                read.first,
                read.window.content,
                rate,
                |from, to| {
                    let (Some(start), Some(end)) =
                        (checked_ticks_at(from, rate), ticks_at_or_after(to, rate))
                    else {
                        return;
                    };
                    if let Ok(range) =
                        TimeRange::new(MediaTime::from_ticks(start), MediaTime::from_ticks(end))
                    {
                        merge_need(needs, clip.source_id, range);
                    }
                },
            );
        }
        Ok(())
    }

    fn check_block(&self, start_sample: i64, frames: usize) -> Result<(), EngineError> {
        if start_sample < 0 || frames == 0 || frames > self.max_block {
            return Err(EngineError::InvalidAudioConfiguration);
        }
        // Every later position lies inside this span, so nothing below can overflow.
        i64::try_from(frames)
            .ok()
            .and_then(|frames| start_sample.checked_add(frames))
            .and_then(|end| end.checked_add(self.lookahead))
            .and_then(|end| checked_ticks_at(end, self.sample_rate))
            .map(|_| ())
            .ok_or(EngineError::InvalidAudioConfiguration)
    }

    /// Settling history replayed on discontinuities, in samples, excluding latency. At most
    /// [`MAX_SETTLING_SECONDS`].
    #[must_use]
    pub const fn required_history_samples(&self) -> i64 {
        self.history
    }

    fn preroll_start(&self, start: i64) -> Result<i64, EngineError> {
        if start < 0 {
            return Err(EngineError::InvalidAudioConfiguration);
        }
        let history = if self.continues_at(start).is_some() {
            0
        } else {
            self.history
        };
        // Input before timeline zero is silent, so only the latency pre-roll reaches before it.
        Ok(start
            .saturating_sub(history)
            .max(0)
            .saturating_sub(self.lookahead))
    }

    /// Samples the replay before a seek to `target` mixes at `position`. Blocks are cut where a
    /// stage starts, so the replay is the same for every block size.
    fn replay_frames(&self, position: i64, target: i64) -> usize {
        let room = i64::try_from(self.max_block).unwrap_or(i64::MAX);
        let mut end = target.min(position.saturating_add(room));
        // Leads are sorted longest first, so stage starts ascend.
        let begun = self
            .stage_leads
            .partition_point(|&lead| Stages::start(target, lead, self.lookahead) <= position);
        if let Some(&lead) = self.stage_leads.get(begun) {
            end = end.min(Stages::start(target, lead, self.lookahead));
        }
        usize::try_from(end - position).unwrap_or(1)
    }

    /// The step from where the last block ended, when a block at `start` continues it. `None`
    /// when it must reset and replay.
    fn continues_at(&self, start: i64) -> Option<i64> {
        let step = start.checked_sub(self.next_sample?)?;
        let room = i64::try_from(self.max_block).unwrap_or(i64::MAX);
        let joins = self.continuity == AudioContinuity::ContinueSmallGaps && step.abs() <= room;
        (step == 0 || joins).then_some(step)
    }

    /// Shifts every built clip chain's next input by `step` so it carries its state across a
    /// small step.
    fn step_live_chains(&mut self, step: i64) {
        if step == 0 {
            return;
        }
        for &(bus, position) in &self.live_chains {
            if let Some(clip) = self.buses[bus].clips[position].chain.as_mut() {
                clip.next_input = clip.next_input.and_then(|next| next.checked_add(step));
            }
        }
    }

    /// Sets the [`AudioContinuity`] policy (default [`AudioContinuity::Exact`]). Changing it resets
    /// the renderer.
    pub fn set_continuity(&mut self, continuity: AudioContinuity) {
        if self.continuity != continuity {
            self.reset();
            self.continuity = continuity;
        }
    }

    /// The policy [`Self::set_continuity`] last chose.
    #[must_use]
    pub const fn continuity(&self) -> AudioContinuity {
        self.continuity
    }

    /// Clears every processor history without changing authored parameters.
    ///
    /// [`Self::render`] resets on its own at discontinuities. Call this only to start fresh exactly
    /// where the last block ended, such as an export beginning where preview stopped.
    pub fn reset(&mut self) {
        self.next_sample = None;
        // Built chains stay allocated; the next block drops the ones it does not mix.
        for &(bus, position) in &self.live_chains {
            if let Some(clip) = self.buses[bus].clips[position].chain.as_mut() {
                if let Some(chain) = clip.chain.as_mut() {
                    chain.reset();
                }
                clip.next_input = None;
            }
        }
        for bus in &mut self.buses {
            bus.chain.reset();
            if let Some(ducking) = bus.ducker.as_mut() {
                ducking.ducker.reset();
            }
        }
        self.master_chain.reset();
        if let Some(limiter) = self.limiter.as_mut() {
            limiter.reset();
        }
    }

    /// Mixes `frames` samples starting at an absolute project-rate sample index.
    ///
    /// A block that does not continue the previous one (see [`AudioContinuity`]) first resets
    /// every processor and replays the settling history [`Self::audio_needs`] asks for, so its
    /// output matches a fresh renderer's regardless of block size.
    ///
    /// Recursive effects settle over a finite horizon (32 exponential time constants plus
    /// filter/feedback margins, capped at [`MAX_SETTLING_SECONDS`]), not bit-exactly. Each stage
    /// joins the replay only at its own horizon, so a long reverb on one track replays only that
    /// track.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidAudioConfiguration`] for a negative start, an oversized block,
    /// or a span that does not fit in ticks, and an evaluation error when the snapshot cannot be
    /// evaluated over the block's span.
    pub fn render(
        &mut self,
        snapshot: &EngineProject,
        start_sample: i64,
        frames: usize,
        provider: &dyn AudioBlockProvider,
        transport: TransportState,
    ) -> Result<AudioRenderStatus, EngineError> {
        self.check_block(start_sample, frames)?;
        let end_sample = start_sample + i64::try_from(frames).unwrap_or(i64::MAX);
        if self.hold_incomplete {
            let missing_sources = self.unready_sources(snapshot, start_sample, frames, provider)?;
            if !missing_sources.is_empty() {
                for channel in 0..self.layout.channels() {
                    self.output[channel][..frames].fill(0.0);
                }
                return Ok(AudioRenderStatus {
                    start_sample,
                    frames,
                    missing_sources,
                });
            }
        }
        let mut missing_sources: Vec<SourceId> = Vec::new();
        if let Some(step) = self.continues_at(start_sample) {
            self.step_live_chains(step);
        } else {
            let mut position = self.preroll_start(start_sample)?;
            self.reset();
            while position < start_sample {
                let count = self.replay_frames(position, start_sample);
                self.mix_block(
                    snapshot,
                    position,
                    count,
                    provider,
                    transport,
                    Some(start_sample),
                    &mut missing_sources,
                )?;
                position += i64::try_from(count).unwrap_or(i64::MAX);
            }
        }
        self.mix_block(
            snapshot,
            start_sample,
            frames,
            provider,
            transport,
            None,
            &mut missing_sources,
        )?;
        self.next_sample = Some(end_sample);

        Ok(AudioRenderStatus {
            start_sample,
            frames,
            missing_sources,
        })
    }

    /// Sources a render of `[start_sample, start_sample + frames)`, pre-roll included, would read
    /// that `provider` lacks. Reads no samples and changes no renderer state.
    fn unready_sources(
        &mut self,
        snapshot: &EngineProject,
        start_sample: i64,
        frames: usize,
        provider: &dyn AudioBlockProvider,
    ) -> Result<Vec<SourceId>, EngineError> {
        let mut missing = Vec::new();
        let mut position = if self.continues_at(start_sample).is_some() {
            start_sample
        } else {
            self.preroll_start(start_sample)?
        };
        while position < start_sample {
            let count = self.replay_frames(position, start_sample);
            self.read_block(
                snapshot,
                position,
                count,
                provider,
                Some(start_sample),
                &mut missing,
            )?;
            position += i64::try_from(count).unwrap_or(i64::MAX);
        }
        self.read_block(snapshot, start_sample, frames, provider, None, &mut missing)?;
        Ok(missing)
    }

    /// Adds the sources `provider` lacks for one block of [`Self::mix_block`] to `missing`.
    fn read_block(
        &mut self,
        snapshot: &EngineProject,
        start: i64,
        frames: usize,
        provider: &dyn AudioBlockProvider,
        replay_target: Option<i64>,
        missing: &mut Vec<SourceId>,
    ) -> Result<(), EngineError> {
        let frames_i64 = i64::try_from(frames).map_err(|_| EngineError::OutOfRange)?;
        let stages = Stages {
            target: replay_target,
            block_end: start + frames_i64,
            lookahead: self.lookahead,
        };
        let span = self.block_span(start, frames_i64)?;
        let mut scratch = std::mem::take(&mut self.span_scratch);
        let described = match snapshot.evaluate_audio_in(span, &mut scratch) {
            Ok(described) => described,
            Err(error) => {
                self.span_scratch = scratch;
                return Err(EngineError::Evaluate(error));
            }
        };
        let master_input = start + self.master_latency;
        for clip in described.iter() {
            let Some(read) = self.clip_read(&clip, master_input, frames, stages) else {
                continue;
            };
            let window = read.window;
            let complete = self.fill_clip(
                &clip,
                read.first,
                window.count,
                window.content,
                provider,
                Fill::Presence,
            );
            if !complete && !missing.contains(clip.source_id) {
                missing.push(clip.source_id.clone());
            }
        }
        self.span_scratch = scratch;
        Ok(())
    }

    /// Mixes output samples `[start, start + frames)` into `output`, carrying processor state
    /// forward.
    ///
    /// Each stage reads ahead by its latency to the output (master input at
    /// `start + master_latency`, each bus and clip chain adding its own), so stage outputs line
    /// up. A replay before a seek to `replay_target` runs only stages whose settling has begun.
    #[allow(clippy::too_many_arguments)]
    fn mix_block(
        &mut self,
        snapshot: &EngineProject,
        start: i64,
        frames: usize,
        provider: &dyn AudioBlockProvider,
        transport: TransportState,
        replay_target: Option<i64>,
        missing_sources: &mut Vec<SourceId>,
    ) -> Result<(), EngineError> {
        let channels = self.layout.channels();
        let frames_i64 = i64::try_from(frames).map_err(|_| EngineError::OutOfRange)?;
        let stages = Stages {
            target: replay_target,
            block_end: start + frames_i64,
            lookahead: self.lookahead,
        };
        let span = self.block_span(start, frames_i64)?;
        // Take the scratch out of `self` so the clip loop can still borrow `self` mutably.
        let mut scratch = std::mem::take(&mut self.span_scratch);
        let described = match snapshot.evaluate_audio_in(span, &mut scratch) {
            Ok(described) => described,
            Err(error) => {
                self.span_scratch = scratch;
                return Err(EngineError::Evaluate(error));
            }
        };

        for channel in 0..channels {
            self.mix[channel][..frames].fill(0.0);
        }
        for bus in &mut self.buses {
            bus.live = false;
        }

        let master_input = start + self.master_latency;
        self.keep_mixed_chains(&described, master_input, frames, stages);
        let mut complete_block = true;
        for clip in described.iter() {
            let clip = &clip;
            let Some(read) = self.clip_read(clip, master_input, frames, stages) else {
                continue;
            };
            let window = read.window;
            let complete = self.fill_clip(
                clip,
                read.first,
                window.count,
                window.content,
                provider,
                Fill::Samples,
            );
            complete_block &= complete;
            if !complete && !missing_sources.contains(clip.source_id) {
                missing_sources.push(clip.source_id.clone());
            }
            if let Some(slot) = read.slot {
                self.apply_clip_chain(slot, read.first, transport, window.count);
            }
            self.mix_clip_into_bus(
                clip,
                read.bus,
                read.bus_input,
                window.offset,
                window.count,
                frames,
            );
        }
        self.span_scratch = scratch;
        // Drop stretch readers this block did not use.
        let block = self.blocks_mixed;
        self.stretch_slots.retain(|_, slot| slot.used_in == block);

        self.process_buses(master_input, transport, frames, stages);
        self.apply_ducking(&self.ctx(master_input, transport), frames, stages);
        if stages.run(self.master_lead) {
            self.sum_buses(master_input, frames);
            self.finish_master(
                start,
                master_input,
                transport,
                frames,
                stages,
                complete_block,
            );
        }
        self.blocks_mixed += 1;
        Ok(())
    }

    /// Drops the chain of every clip the block does not mix, before the block builds any, so only
    /// one block's clips hold chains at once. A dropped clip mixed again would start from fresh
    /// state anyway.
    fn keep_mixed_chains(
        &mut self,
        described: &alpha2_eval::AudioSpan<'_, '_>,
        master_input: i64,
        frames: usize,
        stages: Stages,
    ) {
        let block = self.blocks_mixed;
        for clip in described.iter() {
            let Some((bus, position)) = self
                .clip_read(&clip, master_input, frames, stages)
                .and_then(|read| read.slot)
            else {
                continue;
            };
            if let Some(chain) = self.buses[bus].clips[position].chain.as_mut() {
                chain.ran_in = block;
            }
        }
        let buses = &mut self.buses;
        self.live_chains.retain(|&(bus, position)| {
            let Some(clip) = buses[bus].clips[position].chain.as_mut() else {
                return false;
            };
            if clip.ran_in == block {
                return true;
            }
            clip.chain = None;
            clip.next_input = None;
            false
        });
    }

    fn ctx(&self, sample: i64, transport: TransportState) -> ProcessCtx {
        ProcessCtx {
            position: MediaTime::from_ticks(ticks_at(sample, self.sample_rate)),
            sample_position: sample,
            sample_rate: self.sample_rate,
            transport,
        }
    }

    /// Sets the loudness normalization gain in dB, applied between the master gain and the
    /// limiter. Zero removes it, as does [`Self::set_loudness_metering`].
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidNormalizationGain`] for a non-finite gain or one whose linear
    /// factor is not a normal `f32` (beyond about 760 dB either way).
    pub fn set_normalization_gain_db(&mut self, gain_db: f64) -> Result<(), EngineError> {
        #[allow(clippy::cast_possible_truncation)]
        let gain = libm::pow(10.0, gain_db / 20.0) as f32;
        if !gain_db.is_finite() || !gain.is_normal() {
            return Err(EngineError::InvalidNormalizationGain);
        }
        self.normalization_gain = gain;
        Ok(())
    }

    /// Whether a block missing a source is held: output as silence with nothing advanced, so
    /// rendering it again after the host supplies the source matches an uninterrupted render.
    /// Off by default (live playback); exports turn it on.
    pub const fn set_hold_incomplete_blocks(&mut self, hold: bool) {
        self.hold_incomplete = hold;
    }

    /// Starts (or restarts from empty) measuring the loudness of what reaches the limiter; `false`
    /// stops. Only complete requested blocks are measured, never replayed history.
    ///
    /// Starting removes the normalization gain ([`Self::set_normalization_gain_db`]) so the
    /// authored mix is measured; a gain set afterwards is included.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidAudioConfiguration`] for an unsupported layout or rate.
    pub fn set_loudness_metering(&mut self, enabled: bool) -> Result<(), EngineError> {
        if enabled {
            self.normalization_gain = 1.0;
        }
        self.meter = if enabled {
            Some(
                LoudnessMeter::new(self.sample_rate, self.layout.channels())
                    .map_err(|_| EngineError::InvalidAudioConfiguration)?,
            )
        } else {
            None
        };
        Ok(())
    }

    /// What the meter has measured, if metering is on.
    #[must_use]
    pub fn loudness_report(&self) -> Option<LoudnessReport> {
        self.meter.as_ref().map(LoudnessMeter::report)
    }

    /// The master limiter's linear threshold, if any. Pass it to
    /// [`alpha2_audio::LoudnessNormalization::plan`] so the planned gain stops short of the limiter.
    #[must_use]
    pub fn limiter_threshold(&self) -> Option<f64> {
        self.master
            .limiter
            .map(|limiter| f64::from(limiter.threshold))
    }

    /// Starts (or restarts from empty) measuring the loudness of the final output, after the
    /// limiter and normalization gain; `false` stops. Unlike [`Self::set_loudness_metering`] it
    /// leaves the gain unchanged.
    ///
    /// # Errors
    /// Returns [`EngineError::InvalidAudioConfiguration`] for an unsupported layout or rate.
    pub fn set_output_loudness_metering(&mut self, enabled: bool) -> Result<(), EngineError> {
        self.output_meter = if enabled {
            Some(
                LoudnessMeter::new(self.sample_rate, self.layout.channels())
                    .map_err(|_| EngineError::InvalidAudioConfiguration)?,
            )
        } else {
            None
        };
        Ok(())
    }

    /// What the output meter has measured, if output metering is on.
    #[must_use]
    pub fn output_loudness_report(&self) -> Option<LoudnessReport> {
        self.output_meter.as_ref().map(LoudnessMeter::report)
    }

    /// Copies the most recent render into an owned block for offline sinks.
    #[must_use]
    pub fn block(&self, status: &AudioRenderStatus) -> AudioBlock {
        AudioBlock {
            start_sample: status.start_sample,
            sample_rate: self.sample_rate,
            speakers: match self.layout {
                ChannelLayout::Mono => SpeakerLayout::MONO,
                ChannelLayout::Stereo => SpeakerLayout::STEREO,
            },
            channels: (0..self.layout.channels())
                .map(|channel| self.output[channel][..status.frames].to_vec())
                .collect(),
        }
    }

    /// The bus of a clip's track, by track order or, for another snapshot's clip, by track ID.
    fn bus_of(&self, clip: &ActiveAudioClip) -> Option<usize> {
        match self.buses.get(clip.track_order) {
            Some(bus) if bus.id == *clip.track_id => Some(clip.track_order),
            _ => self.bus_of_track.get(clip.track_id).copied(),
        }
    }

    /// A clip's slot, `(bus, position)`, found as [`Self::bus_of`] finds its bus.
    fn slot_of(&self, clip: &ActiveAudioClip) -> Option<(usize, usize)> {
        let slot = (clip.track_order, clip.clip_order);
        let matches = self
            .buses
            .get(slot.0)
            .and_then(|bus| bus.clips.get(slot.1))
            .is_some_and(|found| found.id == *clip.clip_id);
        if matches {
            Some(slot)
        } else {
            self.slot_of_clip.get(clip.clip_id).copied()
        }
    }

    fn clip_chain(&self, (bus, position): (usize, usize)) -> Option<&ClipChain> {
        self.buses[bus].clips[position].chain.as_ref()
    }

    /// Whether the bus is muted or outside the solo.
    fn is_silent_bus(&self, index: usize) -> bool {
        self.buses[index].audio.is_silenced(self.any_solo)
    }

    /// The timeline span evaluated for output samples `[start, start + frames)`, including the
    /// lookahead.
    fn block_span(&self, start: i64, frames: i64) -> Result<TimeRange, EngineError> {
        let invalid = EngineError::InvalidAudioConfiguration;
        let span_end = start
            .checked_add(frames)
            .and_then(|end| end.checked_add(self.lookahead))
            .ok_or(invalid.clone())?;
        let start_ticks = checked_ticks_at(start, self.sample_rate).ok_or(invalid.clone())?;
        let end_ticks = checked_ticks_at(span_end, self.sample_rate)
            .zip(start_ticks.checked_add(1))
            .map(|(end, after_start)| end.max(after_start))
            .ok_or(invalid.clone())?;
        TimeRange::new(
            MediaTime::from_ticks(start_ticks),
            MediaTime::from_ticks(end_ticks),
        )
        .map_err(|_| invalid)
    }

    /// Where a block reads a clip, or `None` if it skips it. The mix and [`Self::audio_needs`]
    /// share this so a host is asked for exactly what the mix reads.
    fn clip_read(
        &self,
        clip: &ActiveAudioClip,
        master_input: i64,
        frames: usize,
        stages: Stages,
    ) -> Option<ClipRead> {
        let bus = self.bus_of(clip)?;
        if !clip.audio.is_audible() || !self.buses[bus].read {
            return None;
        }
        let bus_input = master_input + self.buses[bus].latency;
        let slot = self.slot_of(clip);
        let chain = slot.and_then(|slot| self.clip_chain(slot));
        let lead = chain.map_or(self.buses[bus].chain_lead, |chain| chain.lead);
        if !stages.run(lead) {
            return None;
        }
        let clip_latency = chain.map_or(0, |chain| chain.latency);
        let chain_input = bus_input + clip_latency;
        let window = self.clip_window(clip, chain_input, frames, clip_latency)?;
        Some(ClipRead {
            bus,
            bus_input,
            slot,
            first: chain_input + i64::try_from(window.offset).ok()?,
            window,
        })
    }

    /// The part of a block of chain input that a clip covers, plus `latency` samples of silence to
    /// flush its chain's delay line.
    fn clip_window(
        &self,
        clip: &ActiveAudioClip,
        input: i64,
        frames: usize,
        latency: i64,
    ) -> Option<ClipWindow> {
        let first = sample_at_or_after(clip.timeline_range.start().ticks(), self.sample_rate);
        let last = sample_at_or_after(clip.timeline_range.end().ticks(), self.sample_rate);
        let block_end = input.checked_add(i64::try_from(frames).ok()?)?;
        let begin = first.max(input);
        let end = last.saturating_add(latency).min(block_end);
        if end <= begin {
            return None;
        }
        Some(ClipWindow {
            offset: usize::try_from(begin - input).ok()?,
            count: usize::try_from(end - begin).ok()?,
            content: usize::try_from((last.min(end) - begin).max(0)).ok()?,
        })
    }

    /// Reads one clip's source window into `clip_a` (or only checks it, with [`Fill::Presence`]).
    /// Returns whether every requested sample came from supplied media.
    fn fill_clip(
        &mut self,
        clip: &ActiveAudioClip,
        first_sample: i64,
        count: usize,
        content: usize,
        provider: &dyn AudioBlockProvider,
        fill: Fill,
    ) -> bool {
        if fill == Fill::Samples {
            for channel in 0..self.layout.channels() {
                self.clip_a[channel][..count].fill(0.0);
            }
        }
        // Past the clip's end is the silence that flushes its chain.
        let count = content;
        if count == 0 {
            return true;
        }
        let rate = self.sample_rate;
        match clip.time_map {
            // A frozen clip is silent.
            TimeMap::Freeze { .. } => true,
            TimeMap::Forward | TimeMap::Reverse if stretches(clip) => {
                self.read_stretched(clip, first_sample, count, provider, fill)
            }
            TimeMap::Forward | TimeMap::Reverse => {
                let reversed = clip.time_map == TimeMap::Reverse;
                SourceMap::stretched(clip, reversed, rate).is_some_and(|map| {
                    self.read_source(clip.source_id, &map, first_sample, 0, count, provider, fill)
                })
            }
            TimeMap::Loop { mode } => {
                self.fill_loop(clip, mode, first_sample, count, provider, fill)
            }
        }
    }

    /// Reads a looping clip one pass at a time. A repeating loop fades out and in around each wrap.
    fn fill_loop(
        &mut self,
        clip: &ActiveAudioClip,
        mode: LoopMode,
        first_sample: i64,
        count: usize,
        provider: &dyn AudioBlockProvider,
        fill: Fill,
    ) -> bool {
        let rate = self.sample_rate;
        let end = clip.timeline_range.end().ticks();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let fade = ((f64::from(rate) * LOOP_FADE_SECONDS).round() as i64).max(1);
        let mut complete = true;
        let mapped = loop_pieces(clip, mode, first_sample, count, rate, |piece| {
            complete &= self.read_source(
                clip.source_id,
                &piece.map,
                piece.sample,
                piece.index,
                piece.length,
                provider,
                fill,
            );
            if mode == LoopMode::Repeat && fill == Fill::Samples {
                // Fade around wraps inside the clip only; its own ends have authored fades.
                let fade_in = (piece.pass > 0).then(|| sample_at_or_after(piece.pass_start, rate));
                let fade_out = (piece.pass_end < end).then_some(piece.piece_end);
                self.fade_wraps(
                    piece.sample,
                    piece.index,
                    piece.length,
                    fade_in,
                    fade_out,
                    fade,
                );
            }
        });
        mapped && complete
    }

    /// Applies a linear `fade`-sample ramp after the wrap at `fade_in` and before the one at
    /// `fade_out`, centred between the samples either side of the wrap.
    fn fade_wraps(
        &mut self,
        first_sample: i64,
        offset: usize,
        count: usize,
        fade_in: Option<i64>,
        fade_out: Option<i64>,
        fade: i64,
    ) {
        #[allow(clippy::cast_precision_loss)]
        let fade = fade as f64;
        for index in 0..count {
            let sample = first_sample + i64::try_from(index).unwrap_or(i64::MAX);
            #[allow(clippy::cast_precision_loss)]
            let after = fade_in.map_or(f64::INFINITY, |wrap| (sample - wrap) as f64 + 0.5);
            #[allow(clippy::cast_precision_loss)]
            let before = fade_out.map_or(f64::INFINITY, |wrap| (wrap - sample) as f64 - 0.5);
            let distance = after.min(before);
            if distance >= fade {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)]
            let gain = (distance / fade).clamp(0.0, 1.0) as f32;
            for channel in 0..self.layout.channels() {
                self.clip_a[channel][offset + index] *= gain;
            }
        }
    }

    /// Reads `count` project samples from `first_sample` through `map` into `clip_a` at `offset`.
    /// Returns whether every requested sample came from supplied media.
    #[allow(clippy::too_many_arguments)]
    fn read_source(
        &mut self,
        source_id: &SourceId,
        map: &SourceMap,
        first_sample: i64,
        offset: usize,
        count: usize,
        provider: &dyn AudioBlockProvider,
        fill: Fill,
    ) -> bool {
        // Positions are exact rationals, so a source aligned with the project grid copies unchanged.
        let Some((request_start, request_end)) = map.request(first_sample, count) else {
            return false;
        };
        let request_frames =
            usize::try_from(request_end.saturating_sub(request_start)).unwrap_or(0);
        let Some(span) = provider.block(source_id, request_start, request_frames) else {
            return false;
        };
        // An uncovered part of the window is media that has not arrived, not silence.
        if !span.covers(request_start, request_end) {
            return false;
        }
        let source = span.block;
        if source.sample_rate != self.sample_rate {
            return false;
        }

        // The engine downmixes so the fold is the same on every platform.
        if source.speakers.channels() != source.channels.len() {
            return false;
        }
        let Some(downmix) = Downmix::new(source.speakers.speakers(), self.layout) else {
            return false;
        };
        if fill == Fill::Presence {
            return true;
        }
        #[cfg(test)]
        {
            self.samples_read += count;
        }
        let read = SourceRead {
            planes: source.channels.as_slice(),
            downmix: &downmix,
            start: source.start_sample,
            offset,
            count,
        };
        match (map.cursor(first_sample), map.interpolator()) {
            // The divisor does not fit the cursor, so divide per sample.
            (None, interpolator) => self.read_divided(&read, map, first_sample, interpolator),
            (Some(cursor), None) => self.read_copied(&read, cursor),
            (Some(cursor), Some(interpolator)) => {
                self.read_filtered(&read, map, cursor, interpolator);
            }
        }
        true
    }

    /// [`Self::read_source`] where every position lands on a whole source sample.
    fn read_copied(&mut self, read: &SourceRead<'_>, cursor: RationalCursor) {
        let Some(first) = cursor.whole.checked_sub(read.start) else {
            return;
        };
        for channel in 0..self.layout.channels() {
            let mix = read.downmix.channel(channel);
            let mut at = first;
            let outputs = &mut self.clip_a[channel][read.offset..read.offset + read.count];
            if let Some(plane) = mix.unity() {
                let plane = read.planes[plane].as_slice();
                for output in outputs {
                    *output = copied(plane, at);
                    at = at.wrapping_add(cursor.step_whole);
                }
            } else {
                for output in outputs {
                    *output = mixed_sample(mix, read.planes, at);
                    at = at.wrapping_add(cursor.step_whole);
                }
            }
        }
    }

    /// [`Self::read_source`] through `interpolator`. When the fractional positions repeat with a
    /// short period (small speed ratios), each phase's weights are computed once per read.
    fn read_filtered(
        &mut self,
        read: &SourceRead<'_>,
        map: &SourceMap,
        mut cursor: RationalCursor,
        interpolator: Interpolator,
    ) {
        let mut weights = std::mem::take(&mut self.weights);
        let reach = interpolator.half_taps() - 1;
        let taps = usize::try_from(2 * interpolator.half_taps()).unwrap_or(0);
        let phases = usize::try_from(cursor.period())
            .ok()
            .filter(|&period| period <= read.count / 2 && period * taps <= MAX_CACHED_WEIGHTS);
        if let Some(phases) = phases {
            weights.resize(phases * taps, 0.0);
            let mut scratch = std::mem::take(&mut self.phase_weights);
            let mut walk = cursor;
            for row in weights.chunks_exact_mut(taps) {
                interpolator.weights(map.fraction(&walk).1, &mut scratch);
                row.copy_from_slice(&scratch);
                walk.advance();
            }
            self.phase_weights = scratch;
        }
        let mut phase = 0;
        for index in 0..read.count {
            let (whole, fraction) = map.fraction(&cursor);
            cursor.advance();
            let row = if let Some(phases) = phases {
                let row = phase;
                phase = if phase + 1 == phases { 0 } else { phase + 1 };
                &weights[row * taps..(row + 1) * taps]
            } else {
                interpolator.weights(fraction, &mut weights);
                &weights[..]
            };
            let Some(first) = whole
                .checked_sub(read.start)
                .and_then(|at| at.checked_sub(reach))
            else {
                continue;
            };
            for channel in 0..self.layout.channels() {
                self.clip_a[channel][read.offset + index] =
                    filtered_sample(read.downmix.channel(channel), read.planes, first, row);
            }
        }
        self.weights = weights;
    }

    /// [`Self::read_source`] for a map whose divisor does not fit a cursor.
    fn read_divided(
        &mut self,
        read: &SourceRead<'_>,
        map: &SourceMap,
        first_sample: i64,
        interpolator: Option<Interpolator>,
    ) {
        let mut weights = std::mem::take(&mut self.weights);
        for index in 0..read.count {
            let sample = first_sample + i64::try_from(index).unwrap_or(0);
            let Some((whole, fraction)) = map.position(sample) else {
                continue;
            };
            let Some(at) = whole.checked_sub(read.start) else {
                continue;
            };
            if let Some(interpolator) = interpolator {
                interpolator.weights(fraction, &mut weights);
            }
            for channel in 0..self.layout.channels() {
                let mix = read.downmix.channel(channel);
                self.clip_a[channel][read.offset + index] = match interpolator {
                    None => mixed_sample(mix, read.planes, at),
                    Some(interpolator) => filtered_sample(
                        mix,
                        read.planes,
                        at - interpolator.half_taps() + 1,
                        &weights,
                    ),
                };
            }
        }
        self.weights = weights;
    }

    /// Runs a clip's chain over `count` samples starting at `input`, resetting it unless it
    /// continues from where it stopped.
    fn apply_clip_chain(
        &mut self,
        (bus, position): (usize, usize),
        input: i64,
        transport: TransportState,
        count: usize,
    ) {
        let ctx = self.ctx(input, transport);
        let (layout, sample_rate, max_block) = (self.layout, self.sample_rate, self.max_block);
        let Some(clip_chain) = self.buses[bus].clips[position].chain.as_mut() else {
            return;
        };
        if clip_chain.chain.is_none() {
            self.live_chains.push((bus, position));
        }
        let effects = &clip_chain.effects;
        let chain = clip_chain
            .chain
            .get_or_insert_with(|| clip_processors(effects, layout, sample_rate, max_block));
        clip_chain.ran_in = self.blocks_mixed;
        if clip_chain.next_input != Some(input) {
            chain.reset();
        }
        clip_chain.next_input = Some(input + i64::try_from(count).unwrap_or(0));
        chain.process_in_place(&ctx, &mut self.clip_a, &mut self.clip_b, count);
        #[cfg(test)]
        self.note_held();
    }

    /// Records the clip effects built now in [`Self::most_held`].
    #[cfg(test)]
    fn note_held(&mut self) {
        let held = self
            .live_chains
            .iter()
            .filter_map(|&(bus, position)| self.buses[bus].clips[position].chain.as_ref())
            .filter(|clip| clip.chain.is_some())
            .fold((0, 0), |(effects, delay), clip| {
                (
                    effects + clip.effects.len(),
                    delay
                        + clip
                            .effects
                            .iter()
                            .map(AudioEffect::delay_ticks)
                            .sum::<u64>(),
                )
            });
        self.most_held = (self.most_held.0.max(held.0), self.most_held.1.max(held.1));
    }

    /// Applies gain, fades, and panning, then sums into the bus. Constant gain and pan with no
    /// fade in the window are evaluated once; otherwise they follow [`fill_envelope`].
    fn mix_clip_into_bus(
        &mut self,
        clip: &ActiveAudioClip,
        bus_index: usize,
        start_sample: i64,
        offset: usize,
        count: usize,
        frames: usize,
    ) {
        let channels = self.layout.channels();
        let sample_rate = self.sample_rate;
        let bus = &mut self.buses[bus_index];
        let first = start_sample + i64::try_from(offset).unwrap_or(0);
        bus.make_live(channels, frames);
        let gain = constant_value(&clip.audio.gain).map(|gain| gain.clamp(0.0, MAX_GAIN));
        let weights =
            constant_value(&clip.audio.pan).map(|pan| pan_gains(pan.clamp(-1.0, 1.0), channels));
        if let (Some(gain), Some(weights), false) =
            (gain, weights, fades_reach(clip, first, count, sample_rate))
        {
            for (channel, weight) in weights.iter().enumerate().take(channels) {
                let input = &self.clip_a[channel][..count];
                let output = &mut bus.output[channel][offset..offset + count];
                for (output, sample) in output.iter_mut().zip(input) {
                    *output += sample * gain * weight;
                }
            }
            return;
        }
        let audio = clip.audio;
        fill_envelope(
            &mut self.envelope[..],
            first,
            count,
            sample_rate,
            |ticks| {
                earliest(
                    earliest(
                        next_keyframe(&audio.gain, ticks),
                        next_keyframe(&audio.pan, ticks),
                    ),
                    next_fade_edge(clip, ticks),
                )
            },
            |time| {
                let gain =
                    sample_scalar(&audio.gain, time).clamp(0.0, MAX_GAIN) * fade_gain(clip, time);
                let [left, right] = pan_gains(sample_scalar(&audio.pan, time), channels);
                [gain, left, right]
            },
        );
        let [gains, left, right] = &self.envelope;
        for (channel, weights) in [left, right].into_iter().enumerate().take(channels) {
            let input = &self.clip_a[channel][..count];
            let output = &mut bus.output[channel][offset..offset + count];
            for (((output, sample), gain), weight) in
                output.iter_mut().zip(input).zip(gains).zip(weights)
            {
                *output += sample * gain * weight;
            }
        }
    }

    fn process_buses(
        &mut self,
        master_input: i64,
        transport: TransportState,
        frames: usize,
        stages: Stages,
    ) {
        let channels = self.layout.channels();
        let sample_rate = self.sample_rate;
        for bus in &mut self.buses {
            if !stages.run(bus.chain_lead) || bus.chain.passes_through() {
                continue;
            }
            let input = master_input + bus.latency;
            let ctx = &ProcessCtx {
                position: MediaTime::from_ticks(ticks_at(input, sample_rate)),
                sample_position: input,
                sample_rate,
                transport,
            };
            bus.make_live(channels, frames);
            for channel in 0..channels {
                self.bus_in[channel][..frames].copy_from_slice(&bus.output[channel][..frames]);
            }
            match channels {
                1 => bus.chain.process(
                    ctx,
                    &[&self.bus_in[0][..frames]],
                    &mut [&mut bus.output[0][..frames]],
                ),
                2 => {
                    let (left, right) = bus.output.split_at_mut(1);
                    bus.chain.process(
                        ctx,
                        &[&self.bus_in[0][..frames], &self.bus_in[1][..frames]],
                        &mut [&mut left[0][..frames], &mut right[0][..frames]],
                    );
                }
                _ => {}
            }
        }
    }

    /// Attenuates ducked buses from the pre-duck output of their sidechain.
    ///
    /// Every ducker writes its own buffer before any bus `output` changes, so a ducked sidechain
    /// is read pre-duck regardless of track order.
    fn apply_ducking(&mut self, ctx: &ProcessCtx, frames: usize, stages: Stages) {
        let channels = self.layout.channels();
        let ducks = |bus: &TrackBus, index: usize| {
            bus.active_sidechain(index).is_some() && stages.run(bus.ducker_lead)
        };
        for index in 0..self.buses.len() {
            if !ducks(&self.buses[index], index) {
                continue;
            }
            let Some(sidechain) = self.buses[index].active_sidechain(index) else {
                continue;
            };
            // Either bus may be idle; the ducker still runs on silence.
            self.buses[sidechain].make_live(channels, frames);
            for channel in 0..channels {
                self.bus_in[channel][..frames]
                    .copy_from_slice(&self.buses[sidechain].output[channel][..frames]);
            }
            let bus = &mut self.buses[index];
            bus.make_live(channels, frames);
            let Some(BusDucker { ducker, output }) = bus.ducker.as_mut() else {
                continue;
            };
            match channels {
                1 => ducker.process(
                    ctx,
                    &[&bus.output[0][..frames], &self.bus_in[0][..frames]],
                    &mut [&mut output[0][..frames]],
                ),
                2 => {
                    let (left, right) = output.split_at_mut(1);
                    ducker.process(
                        ctx,
                        &[
                            &bus.output[0][..frames],
                            &bus.output[1][..frames],
                            &self.bus_in[0][..frames],
                            &self.bus_in[1][..frames],
                        ],
                        &mut [&mut left[0][..frames], &mut right[0][..frames]],
                    );
                }
                _ => {}
            }
        }
        for (index, bus) in self.buses.iter_mut().enumerate() {
            if ducks(bus, index)
                && let Some(ducking) = bus.ducker.as_mut()
            {
                std::mem::swap(&mut bus.output, &mut ducking.output);
            }
        }
    }

    fn sum_buses(&mut self, start_sample: i64, frames: usize) {
        let channels = self.layout.channels();
        for bus_index in 0..self.buses.len() {
            // Skipping an idle bus is exact: adding `+0.0` to a sum that is never `-0.0` is a no-op.
            if self.is_silent_bus(bus_index) || !self.buses[bus_index].live {
                continue;
            }
            let bus = &self.buses[bus_index];
            #[cfg(test)]
            BUS_PLANES_SUMMED.set(BUS_PLANES_SUMMED.get() + channels);
            let gain = constant_value(&bus.audio.gain).map(|gain| gain.clamp(0.0, MAX_GAIN));
            let weights =
                constant_value(&bus.audio.pan).map(|pan| pan_gains(pan.clamp(-1.0, 1.0), channels));
            if let (Some(gain), Some(weights)) = (gain, weights) {
                for (channel, weight) in weights.iter().enumerate().take(channels) {
                    let input = &bus.output[channel][..frames];
                    let output = &mut self.mix[channel][..frames];
                    for (output, sample) in output.iter_mut().zip(input) {
                        *output += sample * gain * weight;
                    }
                }
                continue;
            }
            let audio = &bus.audio;
            fill_envelope(
                &mut self.envelope[..],
                start_sample,
                frames,
                self.sample_rate,
                |ticks| {
                    earliest(
                        next_keyframe(&audio.gain, ticks),
                        next_keyframe(&audio.pan, ticks),
                    )
                },
                |time| {
                    let gain = sample_scalar(&audio.gain, time).clamp(0.0, MAX_GAIN);
                    let [left, right] = pan_gains(sample_scalar(&audio.pan, time), channels);
                    [gain, left, right]
                },
            );
            let [gains, left, right] = &self.envelope;
            for (channel, weights) in [left, right].into_iter().enumerate().take(channels) {
                let input = &bus.output[channel][..frames];
                let output = &mut self.mix[channel][..frames];
                for (((output, sample), gain), weight) in
                    output.iter_mut().zip(input).zip(gains).zip(weights)
                {
                    *output += sample * gain * weight;
                }
            }
        }
    }

    fn finish_master(
        &mut self,
        start_sample: i64,
        master_input: i64,
        transport: TransportState,
        frames: usize,
        stages: Stages,
        complete: bool,
    ) {
        let channels = self.layout.channels();
        let input_ctx = self.ctx(master_input, transport);
        let ctx = &self.ctx(start_sample, transport);
        match channels {
            1 => self.master_chain.process(
                &input_ctx,
                &[&self.mix[0][..frames]],
                &mut [&mut self.output[0][..frames]],
            ),
            2 => {
                let (left, right) = self.output.split_at_mut(1);
                self.master_chain.process(
                    &input_ctx,
                    &[&self.mix[0][..frames], &self.mix[1][..frames]],
                    &mut [&mut left[0][..frames], &mut right[0][..frames]],
                );
            }
            _ => {}
        }
        if let Some(gain) = constant_value(&self.master.gain) {
            let gain = gain.clamp(0.0, MAX_GAIN);
            for channel in 0..channels {
                for sample in &mut self.output[channel][..frames] {
                    *sample *= gain;
                }
            }
        } else {
            let gain = &self.master.gain;
            fill_envelope(
                &mut self.envelope[..],
                start_sample,
                frames,
                self.sample_rate,
                |ticks| next_keyframe(gain, ticks),
                |time| [sample_scalar(gain, time).clamp(0.0, MAX_GAIN)],
            );
            for channel in 0..channels {
                let envelope = &self.envelope[0][..frames];
                for (sample, gain) in self.output[channel][..frames].iter_mut().zip(envelope) {
                    *sample *= gain;
                }
            }
        }
        if self.normalization_gain.to_bits() != 1.0_f32.to_bits() {
            for channel in 0..channels {
                for sample in &mut self.output[channel][..frames] {
                    *sample *= self.normalization_gain;
                }
            }
        }
        // Meter only complete requested blocks, never replayed history or blocks missing a source.
        let measured = stages.target.is_none() && complete;
        if measured && let Some(meter) = self.meter.as_mut() {
            let planes = [&self.output[0][..frames], &self.output[1][..frames]];
            // The meter was made for this layout, so the block always fits.
            let _ = meter.push(&planes[..channels]);
        }
        if let Some(limiter) = self
            .limiter
            .as_mut()
            .filter(|_| stages.run(self.limiter_lead))
        {
            for channel in 0..channels {
                self.mix[channel][..frames].copy_from_slice(&self.output[channel][..frames]);
            }
            match channels {
                1 => limiter.process(
                    ctx,
                    &[&self.mix[0][..frames]],
                    &mut [&mut self.output[0][..frames]],
                ),
                2 => {
                    let (left, right) = self.output.split_at_mut(1);
                    limiter.process(
                        ctx,
                        &[&self.mix[0][..frames], &self.mix[1][..frames]],
                        &mut [&mut left[0][..frames], &mut right[0][..frames]],
                    );
                }
                _ => {}
            }
        }
        if measured && let Some(meter) = self.output_meter.as_mut() {
            let planes = [&self.output[0][..frames], &self.output[1][..frames]];
            let _ = meter.push(&planes[..channels]);
        }
    }
}

fn track_buses(
    project: &alpha2_model::Project,
    layout: ChannelLayout,
    sample_rate: u32,
    max_block: usize,
) -> (Vec<TrackBus>, HashMap<TrackId, usize>) {
    let mut buses = Vec::new();
    let mut bus_of_track = HashMap::new();
    // Every track gets a bus, since a clip on a visual track can carry audio.
    for track in &project.timeline.tracks {
        let mut chain = ProcessorChain::new(layout);
        push_effects(&mut chain, &track.audio.effects);
        chain.prepare(sample_rate, max_block);
        let chain_latency = chain.latency_samples();
        let ducker = track.audio.ducking.as_ref().map(|ducking| {
            let mut ducker = Ducker::new(
                ducking.threshold,
                ducking.depth_db,
                ducking.attack_ms,
                ducking.release_ms,
            );
            ducker.prepare(sample_rate, max_block);
            BusDucker {
                ducker,
                output: buffers(max_block),
            }
        });
        bus_of_track.insert(track.id.clone(), buses.len());
        buses.push(TrackBus {
            id: track.id.clone(),
            audio: track.audio.clone(),
            chain,
            ducker,
            sidechain: None,
            output: buffers(max_block),
            live: false,
            latency: i64::from(chain_latency),
            clips: clip_slots(&track.clips, layout, sample_rate, max_block),
            chain_lead: 0,
            ducker_lead: 0,
            read: false,
        });
    }
    (buses, bus_of_track)
}

/// One slot per clip. Chains are measured here and built when their clip is mixed.
fn clip_slots(
    clips: &[alpha2_model::Clip],
    layout: ChannelLayout,
    sample_rate: u32,
    max_block: usize,
) -> Vec<ClipSlot> {
    clips
        .iter()
        .map(|clip| ClipSlot {
            id: clip.id.clone(),
            chain: (!clip.audio.effects.is_empty()).then(|| {
                // Built once to measure latency and history, then dropped.
                let chain = clip_processors(&clip.audio.effects, layout, sample_rate, max_block);
                ClipChain {
                    effects: clip.audio.effects.clone(),
                    chain: None,
                    ran_in: 0,
                    latency: i64::from(chain.latency_samples()),
                    history: chain.history_samples(),
                    next_input: None,
                    lead: 0,
                }
            }),
        })
        .collect()
}

fn clip_processors(
    effects: &[AudioEffect],
    layout: ChannelLayout,
    sample_rate: u32,
    max_block: usize,
) -> ProcessorChain {
    let mut chain = ProcessorChain::new(layout);
    push_effects(&mut chain, effects);
    chain.prepare_nodes(sample_rate, max_block);
    chain
}

/// The longest latency from any clip's input to its bus's output.
fn longest_path_latency(buses: &[TrackBus]) -> i64 {
    buses
        .iter()
        .map(|bus| {
            bus.latency
                + bus
                    .clips
                    .iter()
                    .filter_map(|slot| slot.chain.as_ref())
                    .map(|chain| chain.latency)
                    .max()
                    .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
}

/// Gives every bus, ducker, and clip chain its lead, and returns the longest.
///
/// A stage's lead is its own history plus the lead of the stage consuming its output. A bus that
/// feeds both the master sum and other buses' duckers takes the longest of their leads.
fn assign_leads(buses: &mut [TrackBus], master_lead: i64, settling: Settling) -> i64 {
    let ducker_leads: Vec<Option<i64>> = buses
        .iter()
        .enumerate()
        .map(|(index, bus)| {
            let ducking = bus.ducker.as_ref()?;
            bus.active_sidechain(index)?;
            Some(settling.lead(master_lead, ducking.ducker.history_samples()))
        })
        .collect();
    let mut longest = master_lead;
    for index in 0..buses.len() {
        let sidechained = buses
            .iter()
            .enumerate()
            .filter(|&(user, bus)| bus.active_sidechain(user) == Some(index))
            .filter_map(|(user, _)| ducker_leads[user]);
        let settled_from = sidechained
            .chain(ducker_leads[index])
            .fold(master_lead, i64::max);
        let bus = &mut buses[index];
        bus.ducker_lead = ducker_leads[index].unwrap_or(0);
        bus.chain_lead = settling.lead(settled_from, bus.chain.history_samples());
        longest = longest.max(bus.chain_lead);
        for clip in bus.clips.iter_mut().filter_map(|slot| slot.chain.as_mut()) {
            clip.lead = settling.lead(bus.chain_lead, clip.history);
            longest = longest.max(clip.lead);
        }
    }
    longest
}

/// The lead cap in samples ([`MAX_SETTLING_SECONDS`]).
#[derive(Clone, Copy)]
struct Settling(i64);

impl Settling {
    /// The lead of a stage with `history` feeding one with lead `downstream`. Never shorter than
    /// `downstream`, otherwise capped.
    fn lead(self, downstream: i64, history: u32) -> i64 {
        downstream
            .saturating_add(i64::from(history))
            .min(self.0.max(downstream))
    }
}

/// Which stages a block runs. In a replay before a seek to `target`, a stage runs once its lead
/// has begun; otherwise all run.
#[derive(Clone, Copy)]
struct Stages {
    target: Option<i64>,
    block_end: i64,
    lookahead: i64,
}

impl Stages {
    fn run(self, lead: i64) -> bool {
        self.target
            .is_none_or(|target| self.block_end > Self::start(target, lead, self.lookahead))
    }

    /// The output sample from which a stage with `lead` runs in the replay. As in
    /// `preroll_start`, history stops at timeline zero but the lookahead reaches before it.
    fn start(target: i64, lead: i64, lookahead: i64) -> i64 {
        (target - lead).max(0).saturating_sub(lookahead)
    }
}

/// Every distinct stage lead, longest first.
fn stage_leads(buses: &[TrackBus], master_lead: i64, limiter_lead: i64) -> Vec<i64> {
    let mut leads = vec![master_lead, limiter_lead];
    for (index, bus) in buses.iter().enumerate() {
        leads.push(bus.chain_lead);
        if bus.active_sidechain(index).is_some() {
            leads.push(bus.ducker_lead);
        }
        leads.extend(
            bus.clips
                .iter()
                .filter_map(|slot| slot.chain.as_ref())
                .map(|chain| chain.lead),
        );
    }
    leads.sort_unstable_by(|a, b| b.cmp(a));
    leads.dedup();
    leads
}

/// Marks the buses whose clips the mix reads: every heard bus and every unmuted bus keying the
/// ducker of a heard one. Solo changes what is heard, not how a ducked track sounds, so a soloed
/// track still reads its sidechain.
fn read_buses(buses: &mut [TrackBus], any_solo: bool) {
    let heard: Vec<bool> = buses
        .iter()
        .map(|bus| !bus.audio.is_silenced(any_solo))
        .collect();
    let keys: Vec<Option<usize>> = buses
        .iter()
        .enumerate()
        .map(|(index, bus)| heard[index].then(|| bus.active_sidechain(index)).flatten())
        .collect();
    for (index, bus) in buses.iter_mut().enumerate() {
        bus.read = heard[index] || (!bus.audio.muted && keys.contains(&Some(index)));
    }
}

fn resolve_sidechains(buses: &mut [TrackBus], bus_of_track: &HashMap<TrackId, usize>) {
    for bus in buses {
        bus.sidechain = bus
            .audio
            .ducking
            .as_ref()
            .and_then(|ducking| bus_of_track.get(&ducking.sidechain_track).copied());
    }
}

fn buffers(max_block: usize) -> [Vec<f32>; 2] {
    [vec![0.0; max_block], vec![0.0; max_block]]
}

fn push_effects(chain: &mut ProcessorChain, effects: &[AudioEffect]) {
    for effect in effects {
        match effect {
            AudioEffect::Eq { bands } => {
                for band in bands {
                    push_effect(
                        chain,
                        Biquad::new(
                            match band.kind {
                                EqBandKind::Peaking => BiquadKind::Peaking,
                                EqBandKind::LowPass => BiquadKind::LowPass,
                                EqBandKind::HighPass => BiquadKind::HighPass,
                            },
                            band.frequency_hz,
                            band.q,
                            band.gain_db,
                        ),
                    );
                }
            }
            AudioEffect::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                lookahead_ms,
            } => push_effect(
                chain,
                Compressor::new(
                    *threshold_db,
                    *ratio,
                    *attack_ms,
                    *release_ms,
                    *lookahead_ms,
                ),
            ),
            AudioEffect::Reverb {
                room_size,
                damping,
                mix,
            } => push_effect(chain, SchroederReverb::new(*room_size, *damping, *mix)),
        }
    }
}

fn push_effect(chain: &mut ProcessorChain, effect: impl Processor + 'static) {
    chain
        .push(effect)
        .expect("a model effect reads one input per output");
}

fn ticks_at(sample: i64, sample_rate: u32) -> i64 {
    checked_ticks_at(sample, sample_rate).unwrap_or(i64::MAX)
}

/// The tick position of a sample, or `None` when it does not fit in ticks.
fn checked_ticks_at(sample: i64, sample_rate: u32) -> Option<i64> {
    let rate = i128::from(sample_rate.max(1));
    i64::try_from((i128::from(sample) * i128::from(TICKS_PER_SECOND)).div_euclid(rate)).ok()
}

/// A source block and where its samples go in `clip_a`.
struct SourceRead<'a> {
    planes: &'a [Vec<f32>],
    downmix: &'a Downmix,
    /// The source sample the planes start at.
    start: i64,
    offset: usize,
    count: usize,
}

/// How one block reads one clip: see [`AudioRenderer::clip_read`].
struct ClipRead {
    bus: usize,
    bus_input: i64,
    slot: Option<(usize, usize)>,
    /// The project sample of the clip's first read sample.
    first: i64,
    window: ClipWindow,
}

/// Where a clip lands in one block of its chain's input.
#[derive(Clone, Copy)]
struct ClipWindow {
    /// First block sample the clip covers.
    offset: usize,
    /// Samples the chain runs for, including the flush.
    count: usize,
    /// Of those, samples read from the source; the rest are silence.
    content: usize,
}

/// The first tick at or after a sample, or `None` when it does not fit.
fn ticks_at_or_after(sample: i64, sample_rate: u32) -> Option<i64> {
    let rate = i128::from(sample_rate.max(1));
    let numerator = i128::from(sample) * i128::from(TICKS_PER_SECOND);
    i64::try_from(numerator.div_euclid(rate) + i128::from(numerator.rem_euclid(rate) != 0)).ok()
}

/// Adds `source_range` to `needs`, merged with every need of that source it overlaps or touches.
fn merge_need(needs: &mut Vec<AudioNeed>, source_id: &SourceId, mut source_range: TimeRange) {
    // A new range can bridge two existing needs, so keep absorbing until none touch.
    while let Some(index) = needs.iter().position(|need| {
        need.source_id == *source_id
            && need.source_range.start() <= source_range.end()
            && source_range.start() <= need.source_range.end()
    }) {
        let existing = needs.remove(index).source_range;
        let merged_start = existing.start().min(source_range.start());
        let merged_end = existing.end().max(source_range.end());
        if let Ok(merged) = TimeRange::new(merged_start, merged_end) {
            source_range = merged;
        }
    }
    needs.push(AudioNeed {
        source_id: source_id.clone(),
        source_range,
    });
}

/// Calls `request` with each source window `[from, to)` that [`AudioRenderer::fill_clip`] would
/// read for the same arguments.
fn source_requests(
    stretch: &Stretch,
    clip: &ActiveAudioClip,
    first_sample: i64,
    count: usize,
    rate: u32,
    mut request: impl FnMut(i64, i64),
) {
    if count == 0 {
        return;
    }
    match clip.time_map {
        TimeMap::Freeze { .. } => {}
        TimeMap::Forward | TimeMap::Reverse if stretches(clip) => {
            if let Some((from, to)) = stretched_request(stretch, clip, first_sample, count, rate) {
                request(from, to);
            }
        }
        TimeMap::Forward | TimeMap::Reverse => {
            let reversed = clip.time_map == TimeMap::Reverse;
            if let Some((from, to)) = SourceMap::stretched(clip, reversed, rate)
                .and_then(|map| map.request(first_sample, count))
            {
                request(from, to);
            }
        }
        TimeMap::Loop { mode } => {
            loop_pieces(clip, mode, first_sample, count, rate, |piece| {
                if let Some((from, to)) = piece.map.request(piece.sample, piece.length) {
                    request(from, to);
                }
            });
        }
    }
}

/// One pass's share of a looping clip's read.
struct LoopPiece {
    /// The project sample the piece starts at; `index` is its offset into the read.
    sample: i64,
    index: usize,
    length: usize,
    /// The pass number; `pass_start` and `pass_end` are in timeline ticks.
    pass: i64,
    pass_start: i64,
    pass_end: i64,
    /// The first project sample of the next pass.
    piece_end: i64,
    map: SourceMap,
}

/// Cuts a looping clip's read at its wraps and calls `piece` for each pass. Returns false if the
/// clip cannot be mapped.
fn loop_pieces(
    clip: &ActiveAudioClip,
    mode: LoopMode,
    first_sample: i64,
    count: usize,
    rate: u32,
    mut piece: impl FnMut(LoopPiece),
) -> bool {
    let Some(period) = clip.source_range.duration().map(MediaTime::ticks) else {
        return false;
    };
    if period <= 0 {
        return false;
    }
    let start = clip.timeline_range.start().ticks();
    let mut index = 0;
    while index < count {
        let sample = first_sample + i64::try_from(index).unwrap_or(i64::MAX);
        let pass = (ticks_at(sample, rate) - start).div_euclid(period);
        let Some(pass_start) = pass
            .checked_mul(period)
            .and_then(|ticks| ticks.checked_add(start))
        else {
            return false;
        };
        let pass_end = pass_start.saturating_add(period);
        let piece_end = sample_at_or_after(pass_end, rate);
        let length = usize::try_from(piece_end.saturating_sub(sample))
            .unwrap_or(usize::MAX)
            .clamp(1, count - index);
        let reversed = mode == LoopMode::PingPong && pass.rem_euclid(2) == 1;
        let Some(map) = SourceMap::pass(clip, pass_start, reversed, rate) else {
            return false;
        };
        piece(LoopPiece {
            sample,
            index,
            length,
            pass,
            pass_start,
            pass_end,
            piece_end,
            map,
        });
        index += length;
    }
    true
}

fn sample_at_or_after(ticks: i64, sample_rate: u32) -> i64 {
    let rate = i128::from(sample_rate.max(1));
    let numerator = i128::from(ticks) * rate;
    let denominator = i128::from(TICKS_PER_SECOND);
    let value =
        numerator.div_euclid(denominator) + i128::from(numerator.rem_euclid(denominator) != 0);
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// One project channel's sample at source sample `index`, folded by `mix`. Terms are summed in
/// plane order so the result is the same on every platform.
fn mixed_sample(mix: &ChannelMix, planes: &[Vec<f32>], index: i64) -> f32 {
    if let Some(plane) = mix.unity() {
        return copied(&planes[plane], index);
    }
    let mut sum = 0.0;
    for &(plane, weight) in mix.terms() {
        sum += weight * copied(&planes[plane], index);
    }
    sum
}

/// Like [`mixed_sample`], filtering source samples `first..` by `weights`.
fn filtered_sample(mix: &ChannelMix, planes: &[Vec<f32>], first: i64, weights: &[f64]) -> f32 {
    if let Some(plane) = mix.unity() {
        return filtered(&planes[plane], first, weights);
    }
    let mut sum = 0.0;
    for &(plane, weight) in mix.terms() {
        sum += weight * filtered(&planes[plane], first, weights);
    }
    sum
}

/// Source sample `index` of `plane`. Samples outside the plane or non-finite are silence; reads
/// happen only where the block covers the request, so this matches explicit zeros.
fn copied(plane: &[f32], index: i64) -> f32 {
    let value = usize::try_from(index)
        .ok()
        .and_then(|index| plane.get(index).copied())
        .unwrap_or(0.0);
    if value.is_finite() { value } else { 0.0 }
}

/// The weighted sum of `plane[first..first + weights.len()]`, treating samples as [`copied`] does.
fn filtered(plane: &[f32], first: i64, weights: &[f64]) -> f32 {
    let mut sum = weighed(plane, first, weights, false);
    if !sum.is_finite() {
        sum = weighed(plane, first, weights, true);
    }
    #[allow(clippy::cast_possible_truncation)]
    let value = sum as f32;
    if value.is_finite() { value } else { 0.0 }
}

/// The sum behind [`filtered`]. Four interleaved running sums added pairwise give a fixed order
/// (identical on every platform) without one long dependency chain.
fn weighed(plane: &[f32], first: i64, weights: &[f64], finite_only: bool) -> f64 {
    let inside = usize::try_from(first).ok().filter(|&from| {
        from.checked_add(weights.len())
            .is_some_and(|end| end <= plane.len())
    });
    let mut sums = [0.0_f64; 4];
    match inside {
        Some(from) if !finite_only => {
            let (weight_quads, weight_rest) = weights.as_chunks::<4>();
            let (sample_quads, sample_rest) = plane[from..from + weights.len()].as_chunks::<4>();
            for (weight, value) in weight_quads.iter().zip(sample_quads) {
                for lane in 0..4 {
                    sums[lane] += weight[lane] * f64::from(value[lane]);
                }
            }
            for (lane, (weight, value)) in weight_rest.iter().zip(sample_rest).enumerate() {
                sums[lane] += weight * f64::from(*value);
            }
        }
        _ => {
            for (index, (at, weight)) in (first..).zip(weights).enumerate() {
                let value = usize::try_from(at)
                    .ok()
                    .and_then(|at| plane.get(at).copied())
                    .unwrap_or(0.0);
                if value.is_finite() {
                    sums[index % 4] += weight * f64::from(value);
                }
            }
        }
    }
    (sums[0] + sums[1]) + (sums[2] + sums[3])
}

/// Where each project sample of a clip reads its source, in source samples.
///
/// `p(n) = (a + n * b) / d` with integer terms, so positions are exact and independent of the
/// tick grid.
struct SourceMap {
    a: i128,
    b: i128,
    d: i128,
}

impl SourceMap {
    /// A forward or reverse clip's timeline range stretched over its source range. Reversed reads
    /// the forward positions in the opposite order.
    fn stretched(clip: &ActiveAudioClip, reversed: bool, rate: u32) -> Option<Self> {
        let timeline = clip.timeline_range;
        let forward = Self::line(
            i128::from(timeline.start().ticks()),
            i128::from(clip.source_range.start().ticks()),
            i128::from(timeline.duration()?.ticks()),
            i128::from(clip.source_range.duration()?.ticks()),
            rate,
        )?;
        if !reversed {
            return Some(forward);
        }
        forward.mirrored(
            sample_at_or_after(timeline.start().ticks(), rate),
            sample_at_or_after(timeline.end().ticks(), rate),
        )
    }

    /// One loop pass starting at timeline tick `pass_start`, at source speed.
    fn pass(clip: &ActiveAudioClip, pass_start: i64, reversed: bool, rate: u32) -> Option<Self> {
        let period = clip.source_range.duration()?.ticks();
        let forward = Self::line(
            i128::from(pass_start),
            i128::from(clip.source_range.start().ticks()),
            i128::from(period),
            i128::from(period),
            rate,
        )?;
        if !reversed {
            return Some(forward);
        }
        forward.mirrored(
            sample_at_or_after(pass_start, rate),
            sample_at_or_after(pass_start.checked_add(period)?, rate),
        )
    }

    /// This map mirrored over `first..last`: sample `n` reads what `first + last - 1 - n` reads.
    fn mirrored(&self, first: i64, last: i64) -> Option<Self> {
        let pivot = i128::from(first)
            .checked_add(i128::from(last))?
            .checked_sub(1)?;
        Some(Self {
            a: self.a.checked_add(pivot.checked_mul(self.b)?)?,
            b: self.b.checked_neg()?,
            d: self.d,
        })
    }

    /// The line through source tick `s0` at timeline tick `t0`, advancing
    /// `source` ticks per `timeline` ticks.
    fn line(t0: i128, s0: i128, timeline: i128, source: i128, rate: u32) -> Option<Self> {
        if timeline <= 0 || source <= 0 {
            return None;
        }
        let divisor = gcd(timeline, source);
        let (timeline, source) = (timeline / divisor, source / divisor);
        let rate = i128::from(rate);
        let ticks = i128::from(TICKS_PER_SECOND);
        // p(n) = (s0 + (n * TPS / rate - t0) * source / timeline) * rate / TPS
        let a = rate.checked_mul(
            s0.checked_mul(timeline)?
                .checked_sub(t0.checked_mul(source)?)?,
        )?;
        let b = ticks.checked_mul(source)?;
        let d = ticks.checked_mul(timeline)?;
        Some(Self { a, b, d })
    }

    /// The source sample at or before `sample`, and the fraction toward the next.
    fn position(&self, sample: i64) -> Option<(i64, f64)> {
        let numerator = self
            .a
            .checked_add(i128::from(sample).checked_mul(self.b)?)?;
        let whole = i64::try_from(numerator.div_euclid(self.d)).ok()?;
        let remainder = numerator.rem_euclid(self.d);
        #[allow(clippy::cast_precision_loss)]
        let fraction = remainder as f64 / self.d as f64;
        Some((whole, fraction))
    }

    /// The interpolation filter, or `None` at unity speed with every position on a source sample.
    fn interpolator(&self) -> Option<Interpolator> {
        if self.b.unsigned_abs() == self.d.unsigned_abs() && self.a.rem_euclid(self.d) == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let speed = self.b.unsigned_abs() as f64 / self.d.unsigned_abs() as f64;
        Some(Interpolator::new(speed))
    }

    /// The source window `[from, to)` that reading `count` samples from `first_sample` touches,
    /// including [`Interpolator`] taps. The map is linear, so the endpoints bound it.
    fn request(&self, first_sample: i64, count: usize) -> Option<(i64, i64)> {
        let last_sample = first_sample.checked_add(i64::try_from(count).ok()?.checked_sub(1)?)?;
        let (first, _) = self.position(first_sample)?;
        let (last, _) = self.position(last_sample)?;
        let (low, high) = (first.min(last), first.max(last));
        let Some(interpolator) = self.interpolator() else {
            return Some((low.max(0), high.saturating_add(1)));
        };
        let half = interpolator.half_taps();
        Some((
            low.saturating_sub(half - 1).max(0),
            high.saturating_add(half + 1),
        ))
    }

    /// A cursor stepping one project sample at a time from `sample`, or `None` if it would not fit.
    fn cursor(&self, sample: i64) -> Option<RationalCursor> {
        let numerator = self
            .a
            .checked_add(i128::from(sample).checked_mul(self.b)?)?;
        RationalCursor::new(numerator, self.b, self.d)
    }

    /// What [`Self::position`] returns at the cursor's sample, bit for bit.
    fn fraction(&self, cursor: &RationalCursor) -> (i64, f64) {
        // The u64 remainder converts to f64 exactly as the i128 one would.
        #[allow(clippy::cast_precision_loss)]
        let fraction = cursor.remainder as f64 / self.d as f64;
        (cursor.whole, fraction)
    }
}

/// Walks `(numerator + n * step) / divisor` over consecutive `n` with additions instead of 128-bit
/// divisions. The caller bounds the walk so every floor fits in `i64`.
#[derive(Clone, Copy, Debug)]
struct RationalCursor {
    whole: i64,
    remainder: u64,
    step_whole: i64,
    step_remainder: u64,
    divisor: u64,
}

impl RationalCursor {
    fn new(numerator: i128, step: i128, divisor: i128) -> Option<Self> {
        // A divisor below 2^63 keeps `remainder + step_remainder` in a u64.
        if divisor <= 0 || divisor > i128::from(i64::MAX) {
            return None;
        }
        Some(Self {
            whole: i64::try_from(numerator.div_euclid(divisor)).ok()?,
            remainder: u64::try_from(numerator.rem_euclid(divisor)).ok()?,
            step_whole: i64::try_from(step.div_euclid(divisor)).ok()?,
            step_remainder: u64::try_from(step.rem_euclid(divisor)).ok()?,
            divisor: u64::try_from(divisor).ok()?,
        })
    }

    /// Steps until the remainder repeats.
    const fn period(&self) -> u64 {
        let (mut a, mut b) = (self.step_remainder, self.divisor);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        self.divisor / a
    }

    fn advance(&mut self) {
        self.whole = self.whole.wrapping_add(self.step_whole);
        self.remainder += self.step_remainder;
        if self.remainder >= self.divisor {
            self.remainder -= self.divisor;
            self.whole = self.whole.wrapping_add(1);
        }
    }
}

/// [`ticks_at`] for consecutive samples, without a division per sample.
fn tick_cursor(sample: i64, sample_rate: u32) -> RationalCursor {
    let ticks = i128::from(TICKS_PER_SECOND);
    RationalCursor::new(
        i128::from(sample) * ticks,
        ticks,
        i128::from(sample_rate.max(1)),
    )
    // Unreachable for blocks `render` accepts; saturates like `ticks_at`.
    .unwrap_or(RationalCursor {
        whole: i64::MAX,
        remainder: 0,
        step_whole: 0,
        step_remainder: 0,
        divisor: 1,
    })
}

fn gcd(mut a: i128, mut b: i128) -> i128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.abs().max(1)
}

/// Project samples between the points where automation is evaluated.
///
/// About 0.7 ms at 48 kHz: finer than any authored move, coarse enough that keyframe evaluation
/// (a cubic Bezier solve) stays cheap.
const ENVELOPE_INTERVAL: i64 = 32;

/// Writes the `N` multipliers from `at` for `count` samples from `first` into `envelope`.
///
/// `at` is evaluated at multiples of [`ENVELOPE_INTERVAL`] and interpolated linearly between them.
/// An interval containing a knot from `next_knot` (keyframe or fade edge) is evaluated per sample
/// instead. Values depend only on the project sample, so block size and seeks never change them.
fn fill_envelope<const N: usize>(
    envelope: &mut [Vec<f32>],
    first: i64,
    count: usize,
    sample_rate: u32,
    next_knot: impl Fn(i64) -> Option<i64>,
    at: impl Fn(MediaTime) -> [f32; N],
) {
    #[allow(clippy::cast_precision_loss)]
    let fraction = 1.0 / ENVELOPE_INTERVAL as f32;
    // Outer `None` means not yet looked up; inner `None` means no further knot.
    let mut knot: Option<Option<i64>> = None;
    let mut point: Option<(i64, [f32; N])> = None;
    let mut written = 0;
    while written < count {
        let sample = first.saturating_add(i64::try_from(written).unwrap_or(i64::MAX));
        let start = sample.div_euclid(ENVELOPE_INTERVAL) * ENVELOPE_INTERVAL;
        let stop = start.saturating_add(ENVELOPE_INTERVAL);
        let run = usize::try_from(stop - sample)
            .unwrap_or(count)
            .clamp(1, count - written);
        let (from, to) = (ticks_at(start, sample_rate), ticks_at(stop, sample_rate));
        let next = match knot {
            Some(next) if next.is_none_or(|next| next > from) => next,
            _ => next_knot(from),
        };
        knot = Some(next);
        let outputs = written..written + run;
        if next.is_some_and(|next| next <= to) {
            let mut ticks = tick_cursor(sample, sample_rate);
            for index in outputs {
                let value = at(MediaTime::from_ticks(ticks.whole));
                ticks.advance();
                for (plane, value) in envelope.iter_mut().zip(value) {
                    plane[index] = value;
                }
            }
            #[cfg(test)]
            ENVELOPE_POINTS.set(ENVELOPE_POINTS.get() + run);
        } else {
            let left = match point {
                Some((at_sample, value)) if at_sample == start => value,
                _ => {
                    #[cfg(test)]
                    ENVELOPE_POINTS.set(ENVELOPE_POINTS.get() + 1);
                    at(MediaTime::from_ticks(from))
                }
            };
            let right = at(MediaTime::from_ticks(to));
            #[cfg(test)]
            ENVELOPE_POINTS.set(ENVELOPE_POINTS.get() + 1);
            point = Some((stop, right));
            #[allow(clippy::cast_precision_loss)]
            let along = (sample - start) as f32;
            for (plane, (left, right)) in envelope.iter_mut().zip(left.into_iter().zip(right)) {
                let slope = right - left;
                for (position, output) in plane[outputs.clone()].iter_mut().enumerate() {
                    #[allow(clippy::cast_precision_loss)]
                    let position = along + position as f32;
                    *output = left + slope * (position * fraction);
                }
            }
        }
        written += run;
    }
}

#[cfg(test)]
thread_local! {
    /// How many times [`fill_envelope`] evaluated an envelope on this thread.
    static ENVELOPE_POINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Bus channels cleared and summed on this thread.
    static BUS_PLANES_CLEARED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BUS_PLANES_SUMMED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The earlier of two optional ticks, where `None` means never.
fn earliest(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

/// The first keyframe of `animation` after `ticks`; none for a constant or single keyframe.
fn next_keyframe(animation: &Animated<f32>, ticks: i64) -> Option<i64> {
    match animation {
        Animated::Keyframes { keyframes } if keyframes.len() > 1 => {
            let next = keyframes.partition_point(|keyframe| keyframe.time.ticks() <= ticks);
            keyframes.get(next).map(|keyframe| keyframe.time.ticks())
        }
        _ => None,
    }
}

/// The first fade start or end after `ticks`.
fn next_fade_edge(clip: &ActiveAudioClip, ticks: i64) -> Option<i64> {
    let start = clip.timeline_range.start().ticks();
    let end = clip.timeline_range.end().ticks();
    let fade_in = clip.audio.fade_in.ticks();
    let fade_out = clip.audio.fade_out.ticks();
    let mut next = None;
    if fade_in > 0 {
        for edge in [start, start.saturating_add(fade_in)] {
            if edge > ticks {
                next = earliest(next, Some(edge));
            }
        }
    }
    if fade_out > 0 {
        for edge in [end.saturating_sub(fade_out), end] {
            if edge > ticks {
                next = earliest(next, Some(edge));
            }
        }
    }
    next
}

/// The value of a scalar that does not change over time, including a single keyframe.
fn constant_value(animation: &Animated<f32>) -> Option<f32> {
    match animation {
        Animated::Constant { value } => Some(*value),
        Animated::Keyframes { keyframes } => match keyframes.as_slice() {
            [only] => Some(only.value),
            _ => None,
        },
    }
}

/// Whether either fade changes the gain in the `count` samples from `first`. Where it does not,
/// [`fade_gain`] is exactly one.
fn fades_reach(clip: &ActiveAudioClip, first: i64, count: usize, sample_rate: u32) -> bool {
    let fade_in = clip.audio.fade_in.ticks();
    let fade_out = clip.audio.fade_out.ticks();
    if fade_in <= 0 && fade_out <= 0 {
        return false;
    }
    let last = first.saturating_add(i64::try_from(count).unwrap_or(i64::MAX).saturating_sub(1));
    // Saturate: pre-roll can read before the clip, and clips may end near the largest tick.
    let elapsed = ticks_at(first, sample_rate).saturating_sub(clip.timeline_range.start().ticks());
    let remaining = clip
        .timeline_range
        .end()
        .ticks()
        .saturating_sub(ticks_at(last, sample_rate));
    (fade_in > 0 && elapsed < fade_in) || (fade_out > 0 && remaining < fade_out)
}

/// The clip's fade multiplier at `time`, using its [`alpha2_model::FadeShape`].
fn fade_gain(clip: &ActiveAudioClip, time: MediaTime) -> f32 {
    let start = clip.timeline_range.start().ticks();
    let end = clip.timeline_range.end().ticks();
    let shape = clip.audio.fade_shape;
    let mut gain = 1.0_f32;
    let fade_in = clip.audio.fade_in.ticks();
    if fade_in > 0 {
        let elapsed = time.ticks().saturating_sub(start);
        if elapsed < fade_in {
            gain *= shape.amplitude(ratio(elapsed, fade_in));
        }
    }
    let fade_out = clip.audio.fade_out.ticks();
    if fade_out > 0 {
        let remaining = end.saturating_sub(time.ticks());
        if remaining < fade_out {
            gain *= shape.amplitude(ratio(remaining, fade_out));
        }
    }
    gain.clamp(0.0, 1.0)
}

#[allow(clippy::cast_precision_loss)]
fn ratio(value: i64, span: i64) -> f32 {
    if span <= 0 {
        return 1.0;
    }
    (value.max(0) as f32 / span as f32).clamp(0.0, 1.0)
}

/// Constant-power pan weights normalized to unity at the centre, so clip and track pans compose
/// without attenuating a default project. A hard pan raises the remaining channel by 3 dB. Mono
/// output ignores pan.
fn pan_gains(pan: f32, channels: usize) -> [f32; 2] {
    if channels < 2 {
        return [1.0, 1.0];
    }
    // Computed in f64 so the centre rounds to exactly 1.0 in f32 (f32 math gives 0.99999994).
    let angle = (f64::from(pan.clamp(-1.0, 1.0)) + 1.0) * core::f64::consts::FRAC_PI_4;
    let normalize = core::f64::consts::SQRT_2;
    #[allow(clippy::cast_possible_truncation)]
    let weights = [
        (libm::cos(angle) * normalize) as f32,
        (libm::sin(angle) * normalize) as f32,
    ];
    weights
}

#[cfg(test)]
mod tests;
