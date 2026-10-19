use alpha2_time::MediaTime;

/// Validated channel organization used at every host boundary.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChannelLayout {
    /// One centered channel.
    Mono,
    /// Left and right channels.
    Stereo,
}

impl ChannelLayout {
    /// Validates and converts a channel count.
    #[must_use]
    pub const fn from_channels(channels: usize) -> Option<Self> {
        match channels {
            1 => Some(Self::Mono),
            2 => Some(Self::Stereo),
            _ => None,
        }
    }

    /// Returns the number of planar channels.
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }
}

/// Current host transport state supplied without consulting a platform clock.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TransportState {
    /// Processing is paused or stopped.
    #[default]
    Stopped,
    /// Timeline time advances with generated samples.
    Playing,
}

/// Immutable information for one DSP invocation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ProcessCtx {
    /// Timeline position of the first sample in this block.
    pub position: MediaTime,
    /// Exact project-rate sample index of the first sample in this block.
    ///
    /// This avoids losing fractional tick positions at sample rates that do not
    /// divide [`alpha2_time::TICKS_PER_SECOND`].
    pub sample_position: i64,
    /// Project sample rate. Resampling belongs at the host boundary.
    pub sample_rate: u32,
    /// Current transport state.
    pub transport: TransportState,
}

impl ProcessCtx {
    /// The context of the audio `samples` later on the timeline (earlier,
    /// for a negative count): the sample position moved that far, and the
    /// position recomputed from it at the context's rate, rounded down to a
    /// tick. Zero leaves the context exactly as it is.
    #[must_use]
    pub fn offset_by(&self, samples: i64) -> Self {
        if samples == 0 {
            return *self;
        }
        let sample_position = self.sample_position.saturating_add(samples);
        let ticks = (i128::from(sample_position) * i128::from(alpha2_time::TICKS_PER_SECOND))
            .div_euclid(i128::from(self.sample_rate.max(1)));
        let ticks = i64::try_from(ticks).unwrap_or(if ticks < 0 { i64::MIN } else { i64::MAX });
        Self {
            position: MediaTime::from_ticks(ticks),
            sample_position,
            ..*self
        }
    }
}

/// Opaque processor state saved outside the real-time process path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateBlob(Vec<u8>);

impl StateBlob {
    /// Creates an opaque state value from owned bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Returns the serialized bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Why a processor, a [`crate::ProcessorChain`] or an
/// [`crate::AudioGraph`] refuses a saved state. A refused state changes
/// nothing: what refuses it is left as it was.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StateError {
    /// The state does not fit: saved by another kind of processor, by one
    /// prepared otherwise (another rate, lookahead or layout, so its history
    /// has another length), by a chain of other nodes, or cut short.
    Mismatch,
    /// The state fits but holds what the processor never keeps: a cursor
    /// past its history, or a value that is not finite.
    Invalid,
}

impl core::fmt::Display for StateError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Mismatch => "the saved state was made by a processor prepared otherwise",
            Self::Invalid => "the saved state holds a value the processor never keeps",
        })
    }
}

impl std::error::Error for StateError {}

/// Stateful DSP unit whose process method performs no allocation or locking.
pub trait Processor: Send {
    /// Configures non-real-time state before processing begins.
    fn prepare(&mut self, sample_rate: u32, max_block: usize);

    /// Processes planar channel slices supplied by the host.
    ///
    /// A prepared processor that reads one input per output
    /// ([`Self::inputs_per_output`]) writes every sample of each output it
    /// has an input for, up to the shortest of them: a
    /// [`crate::ProcessorChain`] hands that output on as the next node's
    /// input.
    fn process(&mut self, ctx: &ProcessCtx, inputs: &[&[f32]], outputs: &mut [&mut [f32]]);

    /// How many input channels the processor reads for each output channel:
    /// 1 for an effect, 2 for one that also reads a second bus after the
    /// first (a sidechain, or the other side of a crossfade). A
    /// [`crate::ProcessorChain`] gives each node only its own channels, so
    /// it takes only processors that read one.
    fn inputs_per_output(&self) -> usize {
        1
    }

    /// Clears signal history without changing authored parameters.
    fn reset(&mut self);

    /// Algorithmic delay that the graph host must compensate.
    fn latency_samples(&self) -> u32 {
        0
    }

    /// Additional source history for a cold seek, excluding algorithmic delay.
    /// Built-in recursive processors use a finite settling horizon; this is an
    /// approximation to uninterrupted playback, not a bit-exact state snapshot.
    fn history_samples(&self) -> u32 {
        0
    }

    /// Serializes history at an offline chunk boundary.
    fn save_state(&self) -> StateBlob;

    /// Restores history saved by a processor of the same type, prepared
    /// alike, whole or not at all.
    ///
    /// A state the processor saved itself always loads back: a
    /// [`crate::ProcessorChain`] relies on that to undo the nodes it has
    /// loaded when a later one refuses its part.
    ///
    /// # Errors
    /// Returns [`StateError`], and leaves the processor as it was, for a
    /// state that does not fit it or holds a value it never keeps.
    fn load_state(&mut self, state: &StateBlob) -> Result<(), StateError>;
}

/// Thirty-two exponential time constants leave e^-32 of an initial state in
/// exact arithmetic. Floating-point rounding and cascaded gain remain separate.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn settling_samples(coefficient: f64) -> u32 {
    if coefficient <= 0.0 {
        0
    } else if coefficient >= 1.0 || !coefficient.is_finite() {
        u32::MAX
    } else {
        libm::ceil(-32.0 / libm::log(coefficient)) as u32
    }
}
