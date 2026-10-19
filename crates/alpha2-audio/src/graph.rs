#![allow(clippy::needless_range_loop)]

use crate::{ChannelLayout, ProcessCtx, Processor, StateBlob, StateError};

/// Why a [`ProcessorChain`] refuses a processor.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ChainError {
    /// The processor reads a second bus after its own channels
    /// ([`Processor::inputs_per_output`]): a sidechain, or the other side of
    /// a crossfade. A chain gives each node only its own channels.
    MultipleInputs,
}

impl core::fmt::Display for ChainError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::MultipleInputs => {
                "a processor chain takes only processors that read one input per output"
            }
        })
    }
}

impl std::error::Error for ChainError {}

/// Where a [`ProcessorChain`] refused a saved state, and why. The chain is
/// left as it was.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ChainStateError {
    /// The node that refused its part, counted from the first; `None` for
    /// the chain's own: its node count, framing, or bypass record.
    pub node: Option<usize>,
    /// Why.
    pub error: StateError,
}

impl ChainStateError {
    const fn chain(error: StateError) -> Self {
        Self { node: None, error }
    }
}

impl core::fmt::Display for ChainStateError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.node {
            Some(node) => write!(formatter, "node {node}: {}", self.error),
            None => write!(formatter, "the chain: {}", self.error),
        }
    }
}

impl std::error::Error for ChainStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Where an [`AudioGraph`] refused a saved state, and why. The graph is
/// left as it was.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GraphStateError {
    /// The state was saved by a graph with another number of buses.
    Topology,
    /// A bus's clip chain refused its part.
    ClipChain {
        /// The bus.
        bus: usize,
        /// Where in the chain, and why.
        error: ChainStateError,
    },
    /// A track bus refused its part.
    TrackBus {
        /// The bus.
        bus: usize,
        /// Where in the chain, and why.
        error: ChainStateError,
    },
    /// The master chain refused its part.
    Master(ChainStateError),
    /// The audio a bus's alignment holds does not fit it: the buses'
    /// latencies differ from the saving graph's.
    Alignment {
        /// The bus.
        bus: usize,
        /// Why.
        error: StateError,
    },
}

impl core::fmt::Display for GraphStateError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Topology => {
                formatter.write_str("the saved state was made by a graph with other buses")
            }
            Self::ClipChain { bus, error } => write!(formatter, "clip chain {bus}, {error}"),
            Self::TrackBus { bus, error } => write!(formatter, "track bus {bus}, {error}"),
            Self::Master(error) => write!(formatter, "master, {error}"),
            Self::Alignment { bus, error } => write!(formatter, "bus {bus} alignment: {error}"),
        }
    }
}

impl std::error::Error for GraphStateError {}

/// How far a chain has been prepared since its processors last changed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Prepared {
    /// Never, or a processor was pushed since: [`ProcessorChain::process`]
    /// and [`ProcessorChain::process_in_place`] pass their input through.
    No,
    /// By [`ProcessorChain::prepare_nodes`]: only
    /// [`ProcessorChain::process_in_place`] runs the nodes.
    Nodes,
    /// By [`ProcessorChain::prepare`].
    Full,
}

/// Ordered processor chain with all realtime scratch allocated by `prepare`.
///
/// A chain runs its nodes only once prepared: before its first `prepare`,
/// and after a `push` until the next one, it passes its input through.
///
/// Each node gets the context of the audio it receives: the chain input's,
/// moved back by the latency of the nodes before it, so a sample-addressed
/// node (a fade, automation) after a lookahead acts on the audio it scales.
pub struct ProcessorChain {
    processors: Vec<Box<dyn Processor>>,
    layout: ChannelLayout,
    scratch_a: [Vec<f32>; 2],
    scratch_b: [Vec<f32>; 2],
    bypass: bool,
    prepared: Prepared,
    max_block: usize,
    /// Whether the chain keeps [`Self::dry`]: from its first
    /// [`Self::set_bypass`] on.
    bypassable: bool,
    /// The chain's input over its last [`Self::latency_samples`], which a
    /// bypassed chain plays.
    dry: Delay,
}

impl ProcessorChain {
    /// Creates an empty chain for a validated layout.
    #[must_use]
    pub fn new(layout: ChannelLayout) -> Self {
        Self {
            processors: Vec::new(),
            layout,
            scratch_a: [Vec::new(), Vec::new()],
            scratch_b: [Vec::new(), Vec::new()],
            bypass: false,
            prepared: Prepared::No,
            max_block: 0,
            bypassable: false,
            dry: Delay::default(),
        }
    }
    /// Adds a processor outside the realtime path. The chain passes its
    /// input through until it is prepared again.
    ///
    /// # Errors
    /// Returns [`ChainError::MultipleInputs`], and leaves the chain as it
    /// was, for a processor that reads more than one input per output.
    pub fn push(&mut self, processor: impl Processor + 'static) -> Result<(), ChainError> {
        if processor.inputs_per_output() != 1 {
            return Err(ChainError::MultipleInputs);
        }
        self.processors.push(Box::new(processor));
        self.prepared = Prepared::No;
        Ok(())
    }
    /// Bypasses the whole chain, or ends the bypass, outside the realtime
    /// path.
    ///
    /// A bypassed chain plays its input delayed by its
    /// [`Self::latency_samples`], which it keeps reporting, so a host that
    /// compensates the latency stays aligned; and its nodes keep running on
    /// the input, so ending the bypass plays what they make of the current
    /// audio, never audio from before the bypass. The delayed input comes
    /// from a record the chain keeps from its first call here on (either
    /// way; it allocates it then, or at the next prepare): bypassed sooner
    /// after that than its latency, it plays silence for the part it has no
    /// record of. A chain never bypassed keeps no record and costs nothing.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
        if !self.bypassable {
            self.bypassable = true;
            if self.prepared != Prepared::No {
                self.dry
                    .resize(self.delay_samples(), self.layout.channels());
            }
        }
        // `process_in_place` runs bypassed nodes into scratch of the chain's own.
        if bypass && self.prepared == Prepared::Nodes {
            self.allocate_scratch(false);
        }
    }
    /// Whether [`Self::process`] copies its input to its output unchanged
    /// and touches nothing else, so a host may skip it: the chain is empty,
    /// or not prepared since its last push. (A bypassed chain still runs its
    /// nodes.)
    #[must_use]
    pub fn passes_through(&self) -> bool {
        self.processors.is_empty() || self.prepared == Prepared::No
    }
    /// Whether the chain has been prepared since its processors last
    /// changed, so it runs them.
    #[must_use]
    pub fn is_prepared(&self) -> bool {
        self.prepared != Prepared::No
    }
    /// Allocates scratch and prepares every node.
    pub fn prepare(&mut self, sample_rate: u32, max_block: usize) {
        for processor in &mut self.processors {
            processor.prepare(sample_rate, max_block);
        }
        // At least one sample, so a block of any length goes through in pieces.
        self.max_block = max_block.max(1);
        self.prepared = Prepared::Full;
        if self.processors.is_empty() {
            self.scratch_a.iter_mut().for_each(Vec::clear);
            self.scratch_b.iter_mut().for_each(Vec::clear);
            self.dry.resize(0, 0);
            return;
        }
        self.allocate_scratch(true);
        self.dry
            .resize(self.delay_samples(), self.layout.channels());
    }
    /// Prepares every node but leaves the chain without scratch of its own:
    /// a chain run only through [`Self::process_in_place`] works in its
    /// caller's buffers, so many such chains cost only their nodes' state
    /// (a bypassed one also a block of scratch). [`Self::process`] passes
    /// its input through until [`Self::prepare`].
    pub fn prepare_nodes(&mut self, sample_rate: u32, max_block: usize) {
        for processor in &mut self.processors {
            processor.prepare(sample_rate, max_block);
        }
        self.max_block = max_block.max(1);
        for channel in self.scratch_a.iter_mut().chain(&mut self.scratch_b) {
            *channel = Vec::new();
        }
        self.prepared = Prepared::Nodes;
        if self.bypass {
            self.allocate_scratch(false);
        }
        self.dry
            .resize(self.delay_samples(), self.layout.channels());
    }
    /// A block of scratch per channel: `scratch_a`, and with `both`
    /// `scratch_b` too.
    fn allocate_scratch(&mut self, both: bool) {
        for channel in 0..self.layout.channels() {
            self.scratch_a[channel].resize(self.max_block, 0.0);
            if both {
                self.scratch_b[channel].resize(self.max_block, 0.0);
            }
        }
    }
    /// How long a record of its input the chain keeps for bypass.
    fn delay_samples(&self) -> usize {
        if self.bypassable {
            usize::try_from(self.latency_samples()).unwrap_or(usize::MAX)
        } else {
            0
        }
    }
    /// Processes the first `frames` samples of each `buffer` channel in
    /// place without allocating, the way [`Self::process`] would: `spare`
    /// holds each node's output before it becomes the next node's input, so
    /// the two may trade allocations. Both need `frames` samples per channel
    /// of the layout.
    pub fn process_in_place(
        &mut self,
        ctx: &ProcessCtx,
        buffer: &mut [Vec<f32>; 2],
        spare: &mut [Vec<f32>; 2],
        frames: usize,
    ) {
        let channels = self.layout.channels();
        if self.processors.is_empty() || self.prepared == Prepared::No {
            return;
        }
        let frames = buffer
            .iter()
            .take(channels)
            .chain(spare.iter().take(channels))
            .map(Vec::len)
            .fold(frames, usize::min);
        if !self.bypass {
            for channel in 0..channels {
                self.dry.remember(channel, &buffer[channel][..frames]);
            }
            self.dry.advance(frames);
            let mut upstream = 0_u32;
            for processor in &mut self.processors {
                let node_ctx = ctx.offset_by(-i64::from(upstream));
                process_processor(
                    processor.as_mut(),
                    &node_ctx,
                    channels,
                    (buffer, 0..frames),
                    (spare, 0..frames),
                );
                core::mem::swap(buffer, spare);
                upstream = upstream.saturating_add(processor.latency_samples());
            }
            return;
        }
        // Bypassed: the nodes run from the buffer through `spare` and the chain's own scratch, by
        // turns, and the buffer becomes the delayed input. In pieces of the scratch, which holds a
        // prepared block.
        let piece = self.scratch_a[0].len().max(1);
        let mut start = 0;
        while start < frames {
            let end = (start + piece).min(frames);
            let count = end - start;
            let piece_ctx = ctx.offset_by(i64::try_from(start).unwrap_or(i64::MAX));
            let mut upstream = 0_u32;
            for (index, processor) in self.processors.iter_mut().enumerate() {
                let node_ctx = piece_ctx.offset_by(-i64::from(upstream));
                if index == 0 {
                    process_processor(
                        processor.as_mut(),
                        &node_ctx,
                        channels,
                        (buffer, start..end),
                        (spare, start..end),
                    );
                } else if index % 2 == 1 {
                    process_processor(
                        processor.as_mut(),
                        &node_ctx,
                        channels,
                        (spare, start..end),
                        (&mut self.scratch_a, 0..count),
                    );
                } else {
                    process_processor(
                        processor.as_mut(),
                        &node_ctx,
                        channels,
                        (&self.scratch_a, 0..count),
                        (spare, start..end),
                    );
                }
                upstream = upstream.saturating_add(processor.latency_samples());
            }
            for channel in 0..channels {
                self.dry
                    .delay_in_place(channel, &mut buffer[channel][start..end]);
            }
            self.dry.advance(count);
            start = end;
        }
    }
    /// Sum of serial node latencies; none while the chain is not prepared
    /// and so passes its input through.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        if self.prepared == Prepared::No {
            return 0;
        }
        self.processors.iter().fold(0_u32, |total, processor| {
            total.saturating_add(processor.latency_samples())
        })
    }
    /// Sum of serial processor settling horizons, excluding their delays.
    #[must_use]
    pub fn history_samples(&self) -> u32 {
        self.processors.iter().fold(0_u32, |total, processor| {
            total.saturating_add(processor.history_samples())
        })
    }
    /// Clears all histories.
    pub fn reset(&mut self) {
        for processor in &mut self.processors {
            processor.reset();
        }
        for channel in &mut self.scratch_a {
            channel.fill(0.0);
        }
        for channel in &mut self.scratch_b {
            channel.fill(0.0);
        }
        self.dry.reset();
    }
    /// Processes one block without allocating: the first `n` samples of each
    /// output channel of the layout, for `n` the shortest input or output
    /// channel. A block longer than the prepared `max_block` runs in pieces,
    /// as consecutive blocks would. Without an input or output channel for
    /// every channel of the layout there is nothing to process, and the
    /// outputs given are silenced.
    pub fn process(&mut self, ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let channels = self.layout.channels();
        if inputs.len() < channels || outputs.len() < channels {
            for output in outputs.iter_mut() {
                output.fill(0.0);
            }
            return;
        }
        let frames = inputs
            .iter()
            .take(channels)
            .map(|v| v.len())
            .chain(outputs.iter().take(channels).map(|v| v.len()))
            .min()
            .unwrap_or(0);
        if self.processors.is_empty() || self.prepared != Prepared::Full {
            for channel in 0..channels {
                outputs[channel][..frames].copy_from_slice(&inputs[channel][..frames]);
            }
            return;
        }
        let piece = self.scratch_a[0].len();
        let mut start = 0;
        while start < frames {
            let end = (start + piece).min(frames);
            let count = end - start;
            for channel in 0..channels {
                self.scratch_a[channel][..count].copy_from_slice(&inputs[channel][start..end]);
            }
            let piece_ctx = ctx.offset_by(i64::try_from(start).unwrap_or(i64::MAX));
            let mut upstream = 0_u32;
            for processor in &mut self.processors {
                let node_ctx = piece_ctx.offset_by(-i64::from(upstream));
                process_processor(
                    processor.as_mut(),
                    &node_ctx,
                    channels,
                    (&self.scratch_a, 0..count),
                    (&mut self.scratch_b, 0..count),
                );
                core::mem::swap(&mut self.scratch_a, &mut self.scratch_b);
                upstream = upstream.saturating_add(processor.latency_samples());
            }
            for channel in 0..channels {
                if self.bypass {
                    self.dry.delay(
                        channel,
                        &inputs[channel][start..end],
                        &mut outputs[channel][start..end],
                    );
                } else {
                    self.dry.remember(channel, &inputs[channel][start..end]);
                    outputs[channel][start..end].copy_from_slice(&self.scratch_a[channel][..count]);
                }
            }
            self.dry.advance(count);
            start = end;
        }
    }
    /// Serializes each processor state with length framing, then the record
    /// of the input a bypass plays.
    #[must_use]
    pub fn save_state(&self) -> StateBlob {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            &(u32::try_from(self.processors.len()).unwrap_or(u32::MAX)).to_le_bytes(),
        );
        for state in self
            .processors
            .iter()
            .map(|processor| processor.save_state())
            .chain(core::iter::once(self.dry.save_state()))
        {
            bytes.extend_from_slice(
                &(u64::try_from(state.as_bytes().len()).unwrap_or(u64::MAX)).to_le_bytes(),
            );
            bytes.extend_from_slice(state.as_bytes());
        }
        StateBlob::new(bytes)
    }
    /// Restores a state saved by a chain of the same nodes, prepared alike,
    /// whole or not at all: when a node refuses its part, the nodes before
    /// it get their own state back.
    ///
    /// A bypass record the saving chain did not keep (never bypassed) leaves
    /// this one's empty, as a first bypass finds it; one this chain does not
    /// keep is dropped.
    ///
    /// # Errors
    /// Returns where and why the state was refused: the node count or
    /// framing differ, or the record has another length
    /// ([`ChainStateError::node`] `None`), or a node refused its part.
    pub fn load_state(&mut self, state: &StateBlob) -> Result<(), ChainStateError> {
        let (nodes, record) = Self::split_state(state.as_bytes(), self.processors.len())
            .ok_or(ChainStateError::chain(StateError::Mismatch))?;
        let record = self
            .dry
            .check_record(&record)
            .map_err(ChainStateError::chain)?;
        let snapshot = self
            .processors
            .iter()
            .map(|processor| processor.save_state())
            .collect::<Vec<_>>();
        for (index, part) in nodes.iter().enumerate() {
            if let Err(error) = self.processors[index].load_state(part) {
                undo(&mut self.processors[..index], &snapshot);
                return Err(ChainStateError {
                    node: Some(index),
                    error,
                });
            }
        }
        self.dry.load_record(record);
        Ok(())
    }
    /// The history of the node at `index` alone, as its
    /// [`Processor::save_state`] makes it; `None` past the last node.
    ///
    /// With [`Self::load_node_state`] a host that rebuilds a chain with an
    /// edited node hands each node it kept, or replaced with one of the same
    /// kind, the history of the one before it.
    #[must_use]
    pub fn node_state(&self, index: usize) -> Option<StateBlob> {
        self.processors
            .get(index)
            .map(|processor| processor.save_state())
    }
    /// Restores the node at `index` from a state [`Self::node_state`] took,
    /// whole or not at all.
    ///
    /// # Errors
    /// Returns [`StateError::Mismatch`] past the last node, and otherwise
    /// what the node's [`Processor::load_state`] refuses, leaving it as it
    /// was.
    pub fn load_node_state(&mut self, index: usize, state: &StateBlob) -> Result<(), StateError> {
        self.processors
            .get_mut(index)
            .ok_or(StateError::Mismatch)?
            .load_state(state)
    }
    /// A chain state's parts: `count` node states, and the bypass record.
    /// `None` when the count or framing differ.
    fn split_state(bytes: &[u8], count: usize) -> Option<(Vec<StateBlob>, StateBlob)> {
        let saved = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
        if usize::try_from(saved).ok()? != count {
            return None;
        }
        let mut offset = 4;
        let mut parts = Vec::with_capacity(count + 1);
        for _ in 0..=count {
            let length = usize::try_from(u64::from_le_bytes(
                bytes.get(offset..offset + 8)?.try_into().ok()?,
            ))
            .ok()?;
            let end = (offset + 8).checked_add(length)?;
            parts.push(StateBlob::new(bytes.get(offset + 8..end)?.to_vec()));
            offset = end;
        }
        let record = parts.pop()?;
        (offset == bytes.len()).then_some((parts, record))
    }
}

/// Gives `processors` back their `snapshot`, states each saved itself, after
/// a later node refused its part. Such a state always loads back
/// ([`Processor::load_state`]); a processor that breaks that is reset.
fn undo(processors: &mut [Box<dyn Processor>], snapshot: &[StateBlob]) {
    for (processor, state) in processors.iter_mut().zip(snapshot) {
        if processor.load_state(state).is_err() {
            processor.reset();
        }
    }
}

/// Runs `processor` from `input`'s `range` into `output`'s, both the same
/// length, on the layout's first `channels`.
fn process_processor(
    processor: &mut dyn Processor,
    ctx: &ProcessCtx,
    channels: usize,
    (input, from): (&[Vec<f32>; 2], core::ops::Range<usize>),
    (output, to): (&mut [Vec<f32>; 2], core::ops::Range<usize>),
) {
    match channels {
        1 => processor.process(ctx, &[&input[0][from]], &mut [&mut output[0][to]]),
        2 => {
            let (left, right) = output.split_at_mut(1);
            processor.process(
                ctx,
                &[&input[0][from.clone()], &input[1][from]],
                &mut [&mut left[0][to.clone()], &mut right[0][to]],
            );
        }
        _ => {}
    }
}

/// A delay of a fixed number of samples on each channel of a layout, kept as
/// a ring. Every channel shares the cursor: run each channel's piece, then
/// [`Self::advance`] once.
#[derive(Clone, Debug, Default)]
struct Delay {
    lines: [Vec<f32>; 2],
    cursor: usize,
}

impl Delay {
    /// Empties the delay and makes it `samples` long on `channels` channels.
    fn resize(&mut self, samples: usize, channels: usize) {
        for (channel, line) in self.lines.iter_mut().enumerate() {
            line.clear();
            if channel < channels {
                line.resize(samples, 0.0);
            } else {
                line.shrink_to_fit();
            }
        }
        self.cursor = 0;
    }
    fn reset(&mut self) {
        for line in &mut self.lines {
            line.fill(0.0);
        }
        self.cursor = 0;
    }
    /// Writes `input` delayed into `output`, the same length.
    fn delay(&mut self, channel: usize, input: &[f32], output: &mut [f32]) {
        let line = &mut self.lines[channel];
        if line.is_empty() {
            output.copy_from_slice(input);
            return;
        }
        let mut cursor = self.cursor;
        for (input, output) in input.iter().zip(output.iter_mut()) {
            *output = line[cursor];
            line[cursor] = *input;
            cursor += 1;
            if cursor == line.len() {
                cursor = 0;
            }
        }
    }
    /// Replaces `samples` with themselves delayed.
    fn delay_in_place(&mut self, channel: usize, samples: &mut [f32]) {
        let line = &mut self.lines[channel];
        if line.is_empty() {
            return;
        }
        let mut cursor = self.cursor;
        for sample in samples {
            core::mem::swap(&mut line[cursor], sample);
            cursor += 1;
            if cursor == line.len() {
                cursor = 0;
            }
        }
    }
    /// Records `input` as [`Self::delay`] would, without writing it out.
    fn remember(&mut self, channel: usize, input: &[f32]) {
        let line = &mut self.lines[channel];
        let length = line.len();
        if length == 0 {
            return;
        }
        let skip = input.len().saturating_sub(length);
        let mut cursor = (self.cursor + skip % length) % length;
        for &sample in &input[skip..] {
            line[cursor] = sample;
            cursor += 1;
            if cursor == length {
                cursor = 0;
            }
        }
    }
    /// Moves every channel on by `frames` samples.
    fn advance(&mut self, frames: usize) {
        let length = self.lines[0].len();
        if length != 0 {
            self.cursor = (self.cursor + frames % length) % length;
        }
    }
    fn save_state(&self) -> StateBlob {
        let mut bytes = Vec::with_capacity(8 + self.lines.iter().map(Vec::len).sum::<usize>() * 4);
        bytes.extend_from_slice(&(u64::try_from(self.cursor).unwrap_or(0)).to_le_bytes());
        for value in self.lines.iter().flatten() {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        StateBlob::new(bytes)
    }
    /// The cursor of a state saved by a delay of the same length and
    /// channels. Any sample fits: a delay plays each once, keeping nothing
    /// of it, as it plays whatever audio it is given.
    fn check(&self, state: &StateBlob) -> Result<usize, StateError> {
        let bytes = state.as_bytes();
        let samples = self.lines.iter().map(Vec::len).sum::<usize>();
        if bytes.len() != 8 + samples * 4 {
            return Err(StateError::Mismatch);
        }
        let cursor = usize::try_from(u64::from_le_bytes(
            bytes[..8].try_into().expect("fixed range"),
        ))
        .map_err(|_| StateError::Invalid)?;
        if cursor >= self.lines[0].len().max(1) {
            return Err(StateError::Invalid);
        }
        Ok(cursor)
    }
    /// Restores a state [`Self::check`] took, at the cursor it returned.
    fn load(&mut self, state: &StateBlob, cursor: usize) {
        let values = state.as_bytes()[8..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| f32::from_le_bytes(*word));
        for (target, value) in self.lines.iter_mut().flatten().zip(values) {
            *target = value;
        }
        self.cursor = cursor;
    }
    /// How a chain's bypass record takes a saved one ([`Self::load_record`]):
    /// whole when both are kept, emptied when only this one is, not at all
    /// when this one is not kept.
    fn check_record<'a>(&self, state: &'a StateBlob) -> Result<Record<'a>, StateError> {
        if self.lines[0].is_empty() {
            Ok(Record::Ignore)
        } else if state.as_bytes().len() == 8 {
            Ok(Record::Empty)
        } else {
            Ok(Record::Load(state, self.check(state)?))
        }
    }
    fn load_record(&mut self, record: Record<'_>) {
        match record {
            Record::Ignore => {}
            Record::Empty => self.reset(),
            Record::Load(state, cursor) => self.load(state, cursor),
        }
    }
}

/// What a chain's bypass record does with a saved one.
#[derive(Copy, Clone)]
enum Record<'a> {
    Ignore,
    Empty,
    Load(&'a StateBlob, usize),
}

/// Clip chain -> track bus -> master bus graph with deterministic latency aggregation.
///
/// Buses whose chains differ in latency mix aligned: each bus is delayed to
/// the longest clip-and-track latency before the mix, so the graph's output
/// is its input delayed by exactly [`Self::latency_samples`], whichever
/// chain a lookahead sits in. Each chain gets the context of the audio it
/// receives.
pub struct AudioGraph {
    layout: ChannelLayout,
    clip_chains: Vec<ProcessorChain>,
    track_buses: Vec<ProcessorChain>,
    master: ProcessorChain,
    /// Per bus, the delay that brings it to the longest bus's latency.
    alignment: Vec<Delay>,
    mix: [Vec<f32>; 2],
    bus_output: [Vec<f32>; 2],
    track_output: [Vec<f32>; 2],
    max_block: usize,
}

/// Complete graph history captured between offline chunks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioGraphState {
    clip_chains: Vec<StateBlob>,
    track_buses: Vec<StateBlob>,
    master: StateBlob,
    alignment: Vec<StateBlob>,
}

impl AudioGraph {
    /// Creates one clip chain and one track bus per input bus.
    #[must_use]
    pub fn new(layout: ChannelLayout, buses: usize) -> Self {
        Self {
            layout,
            clip_chains: (0..buses).map(|_| ProcessorChain::new(layout)).collect(),
            track_buses: (0..buses).map(|_| ProcessorChain::new(layout)).collect(),
            master: ProcessorChain::new(layout),
            alignment: (0..buses).map(|_| Delay::default()).collect(),
            mix: [Vec::new(), Vec::new()],
            bus_output: [Vec::new(), Vec::new()],
            track_output: [Vec::new(), Vec::new()],
            max_block: 0,
        }
    }
    /// Channel layout every bus and the master run in.
    #[must_use]
    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }
    /// Number of input buses, each with its own clip chain and track bus.
    #[must_use]
    pub fn buses(&self) -> usize {
        self.clip_chains.len()
    }
    /// Mutable clip effect chain of `bus`; `None` past the last bus.
    pub fn clip_chain_mut(&mut self, bus: usize) -> Option<&mut ProcessorChain> {
        self.clip_chains.get_mut(bus)
    }
    /// Mutable track effect chain of `bus`; `None` past the last bus.
    pub fn track_bus_mut(&mut self, bus: usize) -> Option<&mut ProcessorChain> {
        self.track_buses.get_mut(bus)
    }
    /// Mutable master effect chain.
    pub fn master_mut(&mut self) -> &mut ProcessorChain {
        &mut self.master
    }
    /// Prepares every allocation before realtime processing.
    pub fn prepare(&mut self, sample_rate: u32, max_block: usize) {
        // At least one sample, so a block of any length goes through in pieces.
        self.max_block = max_block.max(1);
        for chain in self
            .clip_chains
            .iter_mut()
            .chain(&mut self.track_buses)
            .chain(core::iter::once(&mut self.master))
        {
            chain.prepare(sample_rate, self.max_block);
        }
        for channel in 0..2 {
            self.mix[channel].resize(self.max_block, 0.0);
            self.bus_output[channel].resize(self.max_block, 0.0);
            self.track_output[channel].resize(self.max_block, 0.0);
        }
        let longest = self.bus_latency();
        for bus in 0..self.alignment.len() {
            let pad = longest - self.path_latency(bus);
            self.alignment[bus].resize(
                usize::try_from(pad).unwrap_or(usize::MAX),
                self.layout.channels(),
            );
        }
    }
    /// The latency of bus `bus` through its clip chain and track bus.
    fn path_latency(&self, bus: usize) -> u32 {
        self.clip_chains[bus]
            .latency_samples()
            .saturating_add(self.track_buses[bus].latency_samples())
    }
    /// The longest latency of any bus through its clip chain and track bus.
    fn bus_latency(&self) -> u32 {
        (0..self.clip_chains.len())
            .map(|bus| self.path_latency(bus))
            .max()
            .unwrap_or(0)
    }
    /// Maximum serial latency among tracks plus master latency: how late
    /// the graph's output is, every bus aligned to it.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        self.bus_latency()
            .saturating_add(self.master.latency_samples())
    }
    /// Processes flattened planar bus inputs (`bus0-L, bus0-R, bus1-L, ...`)
    /// into the first `n` samples of each output channel, for `n` the
    /// shortest output channel; a shorter bus is padded with silence, and a
    /// block longer than the prepared `max_block` runs in pieces. Without an
    /// input channel for every channel of every bus, an output channel for
    /// every channel of the layout, or a [`Self::prepare`] first, there is
    /// nothing to mix, and the outputs given are silenced.
    pub fn process(&mut self, ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        let channels = self.layout.channels();
        if inputs.len() < self.clip_chains.len() * channels
            || outputs.len() < channels
            || self.max_block == 0
        {
            for output in outputs.iter_mut() {
                output.fill(0.0);
            }
            return;
        }
        let frames = outputs
            .iter()
            .take(channels)
            .map(|v| v.len())
            .min()
            .unwrap_or(0);
        let mut start = 0;
        while start < frames {
            let end = (start + self.max_block).min(frames);
            let ctx = ctx.offset_by(i64::try_from(start).unwrap_or(i64::MAX));
            self.process_piece(&ctx, inputs, outputs, start, end);
            start = end;
        }
    }

    /// Mixes samples `start..end` of the block, at most `max_block` of them.
    fn process_piece(
        &mut self,
        ctx: &ProcessCtx,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        start: usize,
        end: usize,
    ) {
        let channels = self.layout.channels();
        let frames = end - start;
        for channel in 0..channels {
            self.mix[channel][..frames].fill(0.0);
        }
        for bus in 0..self.clip_chains.len() {
            let base = bus * channels;
            let piece = |channel: usize| {
                let input = inputs[base + channel];
                &input[start.min(input.len())..end.min(input.len())]
            };
            let bus_inputs: [&[f32]; 2] = [piece(0), piece(channels - 1)];
            process_chain_to_internal(
                &mut self.clip_chains[bus],
                ctx,
                channels,
                frames,
                &bus_inputs[..channels],
                &mut self.bus_output,
            );
            let bus_refs: [&[f32]; 2] =
                [&self.bus_output[0][..frames], &self.bus_output[1][..frames]];
            // The track bus gets audio as late as the clip chain made it.
            let track_ctx = ctx.offset_by(-i64::from(self.clip_chains[bus].latency_samples()));
            process_chain_to_internal(
                &mut self.track_buses[bus],
                &track_ctx,
                channels,
                frames,
                &bus_refs[..channels],
                &mut self.track_output,
            );
            let alignment = &mut self.alignment[bus];
            for channel in 0..channels {
                alignment.delay_in_place(channel, &mut self.track_output[channel][..frames]);
            }
            alignment.advance(frames);
            for channel in 0..channels {
                for frame in 0..frames {
                    self.mix[channel][frame] += self.track_output[channel][frame];
                }
            }
        }
        let mix_refs: [&[f32]; 2] = [&self.mix[0][..frames], &self.mix[1][..frames]];
        // Every bus reaches the master as late as the longest.
        let ctx = ctx.offset_by(-i64::from(self.bus_latency()));
        let (left, right) = outputs.split_at_mut(1);
        match channels {
            1 => self
                .master
                .process(&ctx, &mix_refs[..1], &mut [&mut left[0][start..end]]),
            _ => self.master.process(
                &ctx,
                &mix_refs[..2],
                &mut [&mut left[0][start..end], &mut right[0][start..end]],
            ),
        }
    }
    /// Clears every graph node.
    pub fn reset(&mut self) {
        for chain in self
            .clip_chains
            .iter_mut()
            .chain(&mut self.track_buses)
            .chain(core::iter::once(&mut self.master))
        {
            chain.reset();
        }
        for alignment in &mut self.alignment {
            alignment.reset();
        }
    }

    /// Captures every clip, track, and master processor history, and the
    /// audio the buses' alignment holds.
    #[must_use]
    pub fn save_state(&self) -> AudioGraphState {
        AudioGraphState {
            clip_chains: self
                .clip_chains
                .iter()
                .map(ProcessorChain::save_state)
                .collect(),
            track_buses: self
                .track_buses
                .iter()
                .map(ProcessorChain::save_state)
                .collect(),
            master: self.master.save_state(),
            alignment: self.alignment.iter().map(Delay::save_state).collect(),
        }
    }

    /// Restores a state saved by a graph of the same buses and chains,
    /// prepared alike, whole or not at all: when a chain refuses its part,
    /// the chains before it get their own state back.
    ///
    /// # Errors
    /// Returns where and why the state was refused: the bus counts differ,
    /// a chain refused its part, or a bus's alignment does not fit (the
    /// buses' latencies differ from the saving graph's).
    pub fn load_state(&mut self, state: &AudioGraphState) -> Result<(), GraphStateError> {
        let buses = self.clip_chains.len();
        if state.clip_chains.len() != buses
            || state.track_buses.len() != buses
            || state.alignment.len() != buses
        {
            return Err(GraphStateError::Topology);
        }
        let mut cursors = Vec::with_capacity(buses);
        for (bus, (alignment, saved)) in self.alignment.iter().zip(&state.alignment).enumerate() {
            cursors.push(
                alignment
                    .check(saved)
                    .map_err(|error| GraphStateError::Alignment { bus, error })?,
            );
        }
        let snapshot = self
            .chains()
            .map(ProcessorChain::save_state)
            .collect::<Vec<_>>();
        let saved = state
            .clip_chains
            .iter()
            .chain(&state.track_buses)
            .chain(core::iter::once(&state.master));
        let refused =
            self.chains_mut()
                .zip(saved)
                .enumerate()
                .find_map(|(index, (chain, saved))| {
                    chain.load_state(saved).err().map(|error| (index, error))
                });
        if let Some((index, error)) = refused {
            for (chain, state) in self.chains_mut().zip(&snapshot).take(index) {
                if chain.load_state(state).is_err() {
                    chain.reset();
                }
            }
            return Err(if index < buses {
                GraphStateError::ClipChain { bus: index, error }
            } else if index < 2 * buses {
                GraphStateError::TrackBus {
                    bus: index - buses,
                    error,
                }
            } else {
                GraphStateError::Master(error)
            });
        }
        for ((alignment, saved), cursor) in
            self.alignment.iter_mut().zip(&state.alignment).zip(cursors)
        {
            alignment.load(saved, cursor);
        }
        Ok(())
    }

    /// Every chain: the clip chains, the track buses, then the master.
    fn chains(&self) -> impl Iterator<Item = &ProcessorChain> {
        self.clip_chains
            .iter()
            .chain(&self.track_buses)
            .chain(core::iter::once(&self.master))
    }

    fn chains_mut(&mut self) -> impl Iterator<Item = &mut ProcessorChain> {
        self.clip_chains
            .iter_mut()
            .chain(&mut self.track_buses)
            .chain(core::iter::once(&mut self.master))
    }
}

fn process_chain_to_internal(
    chain: &mut ProcessorChain,
    ctx: &ProcessCtx,
    channels: usize,
    frames: usize,
    inputs: &[&[f32]],
    output: &mut [Vec<f32>; 2],
) {
    // A chain only writes as many frames as its shortest input. Clear the complete graph block so a
    // short bus is padded with silence rather than retaining the previous bus's scratch tail.
    for channel in output.iter_mut().take(channels) {
        channel[..frames].fill(0.0);
    }
    match channels {
        1 => chain.process(ctx, inputs, &mut [&mut output[0][..frames]]),
        2 => {
            let (left, right) = output.split_at_mut(1);
            chain.process(
                ctx,
                inputs,
                &mut [&mut left[0][..frames], &mut right[0][..frames]],
            );
        }
        _ => {}
    }
}
