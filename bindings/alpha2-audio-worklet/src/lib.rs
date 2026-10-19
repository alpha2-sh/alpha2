//! Allocation-free C-ABI wasm processor for an `AudioWorkletProcessor`.
//!
//! The host can only access buffers owned by an instance. It never supplies
//! sample pointers or object addresses to the processing call. A generation
//! token rejects stale and arbitrary handles before accessing instance state.

// The release is built once, with SIMD (`.cargo/config.toml`). A `RUSTFLAGS` that replaces those
// flags would otherwise ship a slower module silently.
#[cfg(all(target_arch = "wasm32", not(target_feature = "simd128")))]
compile_error!(
    "alpha2's wasm modules require WebAssembly SIMD; add `-C target-feature=+simd128` to RUSTFLAGS"
);

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;

mod source;

use source::SourceStream;

use alpha2_audio::{
    Biquad, BiquadKind, ChannelLayout, Compressor, PeakLimiter, ProcessCtx, Processor,
    ProcessorChain, SchroederReverb, TransportState,
};
use alpha2_time::{MediaTime, TICKS_PER_SECOND};

const OK: i32 = 0;
const INVALID_HANDLE: i32 = -1;
const INVALID_SHAPE: i32 = -2;
const INVALID_CONFIG: i32 = -3;
/// The lowest rate any browser's `AudioContext` renders at (Chrome and
/// Safari; Firefox's floor is 8 kHz), so no worklet scope is refused.
const MIN_SAMPLE_RATE: u32 = 3_000;
/// The highest rate the engine's audio code accepts.
const MAX_SAMPLE_RATE: u32 = 768_000;
/// Largest block one instance prepares for; render quanta are smaller.
const MAX_FRAMES: usize = 4_096;

/// Bytes the host may write before calling [`alpha2_worklet_configure`].
const CONFIG_CAPACITY: usize = 4_096;

/// Version accepted by the master-chain configuration decoder.
const CONFIG_VERSION: u32 = 1;

/// Nodes the largest valid master bus expands to: the model's 16 effects
/// (`MAX_CHAIN_EFFECTS`), each an EQ of 16 bands (`MAX_EQ_BANDS`), one node
/// per band. Such a record is 3605 bytes, inside [`CONFIG_CAPACITY`].
const MAX_NODES: u32 = 256;

/// One master-bus processor and the buffers the host reads and writes.
///
/// `input`, `output`, and `scratch` are planar: channel `c` starts at
/// `c * max_frames`.
struct WorkletProcessor {
    sample_rate: u32,
    max_frames: usize,
    channels: usize,
    input: Vec<f32>,
    output: Vec<f32>,
    scratch: Vec<f32>,
    chain: ProcessorChain,
    /// The nodes [`Self::chain`] was built from.
    effects: Vec<ConfigEffect>,
    limiter: Option<PeakLimiter>,
    /// The threshold and release [`Self::limiter`] was built from.
    limiter_settings: Option<(f32, f32)>,
    gain: f32,
    config: Vec<u8>,
    sample_position: i64,
    /// Converts what the host feeds at another rate (the project's) to `sample_rate`; `None`
    /// when the host feeds samples at `sample_rate`.
    source: Option<SourceStream>,
}

impl WorkletProcessor {
    fn new(sample_rate: u32, max_frames: usize, channels: usize) -> Option<Self> {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate)
            || max_frames == 0
            || max_frames > MAX_FRAMES
            || !matches!(channels, 1 | 2)
        {
            return None;
        }
        let samples = max_frames.checked_mul(channels)?;
        let layout = ChannelLayout::from_channels(channels)?;
        let limiter_settings = (1.0, 50.0);
        let mut limiter = PeakLimiter::new(limiter_settings.0, limiter_settings.1);
        limiter.prepare(sample_rate, max_frames);
        let mut chain = ProcessorChain::new(layout);
        chain.prepare(sample_rate, max_frames);
        Some(Self {
            sample_rate,
            max_frames,
            channels,
            input: vec![0.0; samples],
            output: vec![0.0; samples],
            scratch: vec![0.0; samples],
            chain,
            effects: Vec::new(),
            limiter: Some(limiter),
            limiter_settings: Some(limiter_settings),
            gain: 1.0,
            config: vec![0; CONFIG_CAPACITY],
            sample_position: 0,
            source: None,
        })
    }

    /// Installs the master bus in the host-written configuration region.
    ///
    /// A configuration is an edit of the running master, not a restart: the
    /// chain is rebuilt only when its nodes change, and then every node at
    /// the same place and of the same kind as before takes over its
    /// predecessor's history, as the limiter does when its settings change.
    /// Only a node that cannot (a compressor whose lookahead, and so its
    /// delay, changed) starts empty. The gain applies from the next block.
    ///
    /// An `AudioWorkletGlobalScope` delivers messages on its rendering thread,
    /// so this runs there, between two `process` calls, and allocates a new
    /// chain there. Hosts send a configuration when the master bus is edited,
    /// not per block.
    fn configure(&mut self, length: usize) -> i32 {
        let Some(config) = MasterConfig::decode(&self.config[..length.min(self.config.len())])
        else {
            return INVALID_CONFIG;
        };
        if config.effects != self.effects {
            let Some(layout) = ChannelLayout::from_channels(self.channels) else {
                return INVALID_SHAPE;
            };
            let mut chain = ProcessorChain::new(layout);
            for effect in &config.effects {
                let pushed = match *effect {
                    ConfigEffect::EqBand {
                        kind,
                        frequency_hz,
                        q,
                        gain_db,
                    } => chain.push(Biquad::new(kind, frequency_hz, q, gain_db)),
                    ConfigEffect::Compressor {
                        threshold_db,
                        ratio,
                        attack_ms,
                        release_ms,
                        lookahead_ms,
                    } => chain.push(Compressor::new(
                        threshold_db,
                        ratio,
                        attack_ms,
                        release_ms,
                        lookahead_ms,
                    )),
                    ConfigEffect::Reverb {
                        room_size,
                        damping,
                        mix,
                    } => chain.push(SchroederReverb::new(room_size, damping, mix)),
                };
                if pushed.is_err() {
                    return INVALID_CONFIG;
                }
            }
            chain.prepare(self.sample_rate, self.max_frames);
            for (index, (before, after)) in self.effects.iter().zip(&config.effects).enumerate() {
                if core::mem::discriminant(before) != core::mem::discriminant(after) {
                    continue;
                }
                if let Some(state) = self.chain.node_state(index) {
                    // A state the new node refuses leaves it empty, as a new chain's is.
                    let _ = chain.load_node_state(index, &state);
                }
            }
            self.chain = chain;
            self.effects = config.effects;
        }
        if config.limiter != self.limiter_settings {
            let previous = self.limiter.take();
            self.limiter = config.limiter.map(|(threshold, release_ms)| {
                let mut limiter = PeakLimiter::new(threshold, release_ms);
                limiter.prepare(self.sample_rate, self.max_frames);
                if let Some(previous) = &previous {
                    // An envelope a limiter saved always loads into another.
                    let _ = limiter.load_state(&previous.save_state());
                }
                limiter
            });
            self.limiter_settings = config.limiter;
        }
        self.gain = config.gain;
        OK
    }

    /// Whether [`Self::process`] copies its input to its output unchanged:
    /// unity gain, no limiter, and no effects.
    fn is_transparent(&self) -> bool {
        self.gain.to_bits() == 1.0_f32.to_bits()
            && self.limiter.is_none()
            && self.chain.passes_through()
    }

    /// Runs the master bus over the first `frames` of each input plane into
    /// the output planes and advances the transport position.
    fn process(&mut self, frames: usize, channels: usize) -> i32 {
        if frames > self.max_frames || channels != self.channels {
            return INVALID_SHAPE;
        }
        let ticks = self
            .sample_position
            .saturating_mul(TICKS_PER_SECOND)
            .div_euclid(i64::from(self.sample_rate));
        let ctx = ProcessCtx {
            position: MediaTime::from_ticks(ticks),
            sample_position: self.sample_position,
            sample_rate: self.sample_rate,
            transport: TransportState::Playing,
        };
        let offset = self.max_frames;
        // The engine's master order: the authored chain on the summed buses, then the gain, then
        // the ceiling. A sample that is not a finite number plays as silence: the chain's history
        // must not keep it, and the engine never mixes one.
        for channel in 0..channels {
            let base = channel * offset;
            for index in 0..frames {
                let sample = self.input[base + index];
                self.scratch[base + index] = if sample.is_finite() { sample } else { 0.0 };
            }
        }
        match channels {
            1 => self.chain.process(
                &ctx,
                &[&self.scratch[..frames]],
                &mut [&mut self.output[..frames]],
            ),
            2 => {
                let (left, right) = self.output.split_at_mut(offset);
                self.chain.process(
                    &ctx,
                    &[
                        &self.scratch[..frames],
                        &self.scratch[offset..offset + frames],
                    ],
                    &mut [&mut left[..frames], &mut right[..frames]],
                );
            }
            _ => return INVALID_SHAPE,
        }
        if self.gain.to_bits() != 1.0_f32.to_bits() {
            for channel in 0..channels {
                let base = channel * offset;
                for sample in &mut self.output[base..base + frames] {
                    *sample *= self.gain;
                }
            }
        }
        if let Some(limiter) = self.limiter.as_mut() {
            for channel in 0..channels {
                let base = channel * offset;
                self.scratch[base..base + frames]
                    .copy_from_slice(&self.output[base..base + frames]);
            }
            match channels {
                1 => limiter.process(
                    &ctx,
                    &[&self.scratch[..frames]],
                    &mut [&mut self.output[..frames]],
                ),
                2 => {
                    let (left, right) = self.output.split_at_mut(offset);
                    limiter.process(
                        &ctx,
                        &[
                            &self.scratch[..frames],
                            &self.scratch[offset..offset + frames],
                        ],
                        &mut [&mut left[..frames], &mut right[..frames]],
                    );
                }
                _ => return INVALID_SHAPE,
            }
        }
        self.sample_position = self
            .sample_position
            .saturating_add(i64::try_from(frames).unwrap_or(i64::MAX));
        OK
    }
}

/// One decoded master-chain node.
#[derive(Copy, Clone, Debug, PartialEq)]
enum ConfigEffect {
    EqBand {
        kind: BiquadKind,
        frequency_hz: f32,
        q: f32,
        gain_db: f32,
    },
    Compressor {
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        lookahead_ms: f32,
    },
    Reverb {
        room_size: f32,
        damping: f32,
        mix: f32,
    },
}

/// A decoded master bus description.
///
/// The wire form is a little-endian record of fixed-width fields so that the
/// worklet does not carry a JSON parser. Every field is validated here; an unreadable or
/// out-of-range record leaves the previous configuration untouched.
#[derive(Clone, Debug, PartialEq)]
struct MasterConfig {
    gain: f32,
    limiter: Option<(f32, f32)>,
    effects: Vec<ConfigEffect>,
}

struct ConfigCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> ConfigCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.at.checked_add(N)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        <[u8; N]>::try_from(slice).ok()
    }

    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|bytes| bytes[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_le_bytes)
    }

    fn f32(&mut self) -> Option<f32> {
        let value = self.take::<4>().map(f32::from_le_bytes)?;
        value.is_finite().then_some(value)
    }
}

impl MasterConfig {
    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut cursor = ConfigCursor::new(bytes);
        if cursor.u32()? != CONFIG_VERSION {
            return None;
        }
        let gain = cursor.f32()?;
        if !(0.0..=16.0).contains(&gain) {
            return None;
        }
        let limiter = match cursor.u8()? {
            0 => {
                // Skip the unused pair so the record stays fixed width.
                cursor.f32()?;
                cursor.f32()?;
                None
            }
            1 => {
                let threshold = cursor.f32()?;
                let release_ms = cursor.f32()?;
                if threshold <= 0.0 || threshold > 16.0 || release_ms <= 0.0 {
                    return None;
                }
                Some((threshold, release_ms))
            }
            _ => return None,
        };
        let count = cursor.u32()?;
        if count > MAX_NODES {
            return None;
        }
        let mut effects = Vec::with_capacity(count as usize);
        for _ in 0..count {
            effects.push(match cursor.u8()? {
                0 => {
                    let kind = match cursor.u8()? {
                        0 => BiquadKind::Peaking,
                        1 => BiquadKind::LowPass,
                        2 => BiquadKind::HighPass,
                        _ => return None,
                    };
                    let frequency_hz = cursor.f32()?;
                    let q = cursor.f32()?;
                    let gain_db = cursor.f32()?;
                    // The gain bound is the one the filter renders and the model accepts.
                    if frequency_hz <= 0.0
                        || q <= 0.0
                        || !gain_db.is_finite()
                        || gain_db.abs() > alpha2_audio::MAX_PEAKING_GAIN_DB
                    {
                        return None;
                    }
                    ConfigEffect::EqBand {
                        kind,
                        frequency_hz,
                        q,
                        gain_db,
                    }
                }
                1 => {
                    let threshold_db = cursor.f32()?;
                    let ratio = cursor.f32()?;
                    let attack_ms = cursor.f32()?;
                    let release_ms = cursor.f32()?;
                    let lookahead_ms = cursor.f32()?;
                    if ratio < 1.0 || attack_ms <= 0.0 || release_ms <= 0.0 || lookahead_ms < 0.0 {
                        return None;
                    }
                    ConfigEffect::Compressor {
                        threshold_db,
                        ratio,
                        attack_ms,
                        release_ms,
                        lookahead_ms,
                    }
                }
                2 => {
                    let room_size = cursor.f32()?;
                    let damping = cursor.f32()?;
                    let mix = cursor.f32()?;
                    let unit = |value: f32| (0.0..=1.0).contains(&value);
                    if !unit(room_size) || !unit(damping) || !unit(mix) {
                        return None;
                    }
                    ConfigEffect::Reverb {
                        room_size,
                        damping,
                        mix,
                    }
                }
                _ => return None,
            });
        }
        Some(Self {
            gain,
            limiter,
            effects,
        })
    }
}

/// The last panic's message, kept where the host can read it after the trap.
///
/// In wasm a panic aborts with a trap, which reaches JavaScript as an
/// exception with no message. The worklet processor reads this instead.
mod panic_record {
    use std::cell::{Cell, RefCell};
    use std::sync::Once;

    const CAPACITY: usize = 256;

    thread_local! {
        static MESSAGE: RefCell<[u8; CAPACITY]> = const { RefCell::new([0; CAPACITY]) };
        static LENGTH: Cell<usize> = const { Cell::new(0) };
    }

    /// Records every later panic, then runs the hook that was installed.
    pub fn install() {
        static INSTALLED: Once = Once::new();
        INSTALLED.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                record(&info.to_string());
                previous(info);
            }));
        });
    }

    fn record(message: &str) {
        let mut end = message.len().min(CAPACITY);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        MESSAGE.with_borrow_mut(|buffer| buffer[..end].copy_from_slice(&message.as_bytes()[..end]));
        LENGTH.set(end);
    }

    pub fn address() -> *const u8 {
        MESSAGE.with_borrow(|buffer| buffer.as_ptr())
    }

    pub fn length() -> usize {
        LENGTH.get()
    }

    #[cfg(test)]
    pub fn message() -> String {
        MESSAGE.with_borrow(|buffer| String::from_utf8_lossy(&buffer[..length()]).into_owned())
    }
}

mod boundary {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::{INVALID_HANDLE, WorkletProcessor};

    struct Instances {
        next_handle: u32,
        live: HashMap<u32, WorkletProcessor>,
    }

    impl Default for Instances {
        fn default() -> Self {
            Self {
                next_handle: 1,
                live: HashMap::new(),
            }
        }
    }

    thread_local! {
        static INSTANCES: RefCell<Instances> = RefCell::new(Instances::default());
    }

    /// Stores `instance` under the next free nonzero handle; 0 when every
    /// handle is taken.
    pub fn create(instance: WorkletProcessor) -> u32 {
        INSTANCES.with_borrow_mut(|instances| {
            let start = instances.next_handle;
            let mut handle = start;
            while instances.live.contains_key(&handle) {
                handle = handle.wrapping_add(1).max(1);
                if handle == start {
                    return 0;
                }
            }
            instances.next_handle = handle.wrapping_add(1).max(1);
            instances.live.insert(handle, instance);
            handle
        })
    }

    pub fn with_instance<T>(
        handle: u32,
        operation: impl FnOnce(&mut WorkletProcessor) -> T,
    ) -> Result<T, i32> {
        INSTANCES.with_borrow_mut(|instances| {
            if handle == 0 {
                return Err(INVALID_HANDLE);
            }
            instances
                .live
                .get_mut(&handle)
                .map(operation)
                .ok_or(INVALID_HANDLE)
        })
    }

    pub fn destroy(handle: u32) -> i32 {
        INSTANCES.with_borrow_mut(|instances| {
            if handle == 0 || instances.live.remove(&handle).is_none() {
                return INVALID_HANDLE;
            }
            super::OK
        })
    }
}

/// Creates one processor and returns an opaque nonzero handle, or 0 for a
/// sample rate outside 3 to 768 kHz, a block size of 0 or above 4096 frames,
/// or a channel count other than 1 or 2.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_create(sample_rate: u32, max_frames: u32, channels: u32) -> u32 {
    panic_record::install();
    let Some(instance) = WorkletProcessor::new(sample_rate, max_frames as usize, channels as usize)
    else {
        return 0;
    };
    boundary::create(instance)
}

/// Returns the planar input buffer address in wasm linear memory.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_input(handle: u32) -> *mut f32 {
    boundary::with_instance(handle, |instance| instance.input.as_mut_ptr())
        .unwrap_or(core::ptr::null_mut())
}

/// Returns the planar output buffer address in wasm linear memory.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_output(handle: u32) -> *mut f32 {
    boundary::with_instance(handle, |instance| instance.output.as_mut_ptr())
        .unwrap_or(core::ptr::null_mut())
}

/// Returns the prepared planar stride in frames for one channel.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_frames(handle: u32) -> u32 {
    boundary::with_instance(handle, |instance| {
        u32::try_from(instance.max_frames).unwrap_or(0)
    })
    .unwrap_or(0)
}

/// Returns the number of planar input and output channels.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_channels(handle: u32) -> u32 {
    boundary::with_instance(handle, |instance| {
        u32::try_from(instance.channels).unwrap_or(0)
    })
    .unwrap_or(0)
}

/// Processes at most the prepared number of frames without allocation or locks.
///
/// Returns `OK`, `INVALID_HANDLE`, or `INVALID_SHAPE` for more frames than
/// prepared or a channel count other than the instance's.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_process(handle: u32, frames: u32, channels: u32) -> i32 {
    boundary::with_instance(handle, |instance| {
        instance.process(frames as usize, channels as usize)
    })
    .unwrap_or(INVALID_HANDLE)
}

/// Returns 1 when processing is the identity (unity gain, no limiter, no
/// effects), 0 when it is not, or for an invalid handle.
///
/// A host may then copy its input to its output itself and skip
/// [`alpha2_worklet_process`] until the next configuration. Skipped blocks do
/// not advance the transport position, which none of the effects a
/// configuration can install reads.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_transparent(handle: u32) -> u32 {
    boundary::with_instance(handle, |instance| u32::from(instance.is_transparent())).unwrap_or(0)
}

/// Returns how many samples the master bus delays its input by: the sum of
/// its compressors' lookahead, 0 without one (the limiter has none), and 0 for
/// an invalid handle.
///
/// The engine reads each path's input early by its latency, so its mix is
/// not delayed. A live worklet cannot read ahead of its input, so its output
/// is this much late; a host clock subtracts it to say what is being heard.
/// It changes only with a configuration.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_latency(handle: u32) -> u32 {
    boundary::with_instance(handle, |instance| instance.chain.latency_samples()).unwrap_or(0)
}

/// Sets the rate of the samples the host feeds through [`alpha2_worklet_source`] (the
/// project's, which the engine mixes at): at a rate other than the instance's,
/// [`alpha2_worklet_resample`] converts them to it with the engine's band-limited converter. At
/// the instance's own rate there is nothing to convert, and the source region goes away.
///
/// Builds the converter's coefficients, so a host calls it before the node plays. Returns `OK`,
/// `INVALID_HANDLE`, or `INVALID_CONFIG` for a rate the converter cannot take (zero, above
/// 768 kHz, or more than 8192 times the instance's).
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_set_source_rate(handle: u32, source_rate: u32) -> i32 {
    boundary::with_instance(handle, |instance| {
        if source_rate == instance.sample_rate {
            instance.source = None;
            return OK;
        }
        match SourceStream::new(
            source_rate,
            instance.sample_rate,
            instance.channels,
            instance.max_frames,
        ) {
            Some(stream) => {
                instance.source = Some(stream);
                OK
            }
            None => INVALID_CONFIG,
        }
    })
    .unwrap_or(INVALID_HANDLE)
}

/// Returns the planar region the host writes source samples into before
/// [`alpha2_worklet_resample`], channel `c` at `c * alpha2_worklet_source_frames(handle)`;
/// null without a source rate other than the instance's, or for an invalid handle.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_source(handle: u32) -> *mut f32 {
    boundary::with_instance(handle, |instance| {
        instance
            .source
            .as_mut()
            .map_or(core::ptr::null_mut(), |source| source.append().as_mut_ptr())
    })
    .unwrap_or(core::ptr::null_mut())
}

/// Returns the per-channel stride of [`alpha2_worklet_source`]'s planes, in samples; 0 without
/// a source rate other than the instance's, or for an invalid handle.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_source_frames(handle: u32) -> u32 {
    boundary::with_instance(handle, |instance| {
        instance.source.as_ref().map_or(0, |source| {
            u32::try_from(source.append_frames()).unwrap_or(0)
        })
    })
    .unwrap_or(0)
}

/// Returns how many source samples per channel the host writes before the next
/// [`alpha2_worklet_resample`] of `frames` outputs; 0 without a source rate other than the
/// instance's, for more frames than prepared, or for an invalid handle.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_source_need(handle: u32, frames: u32) -> u32 {
    boundary::with_instance(handle, |instance| {
        let frames = frames as usize;
        match instance.source.as_ref() {
            Some(source) if frames <= instance.max_frames => {
                u32::try_from(source.need(frames)).unwrap_or(0)
            }
            _ => 0,
        }
    })
    .unwrap_or(0)
}

/// Returns how many of the instance's samples the conversion delays what the host feeds by: an
/// output weighs source samples past its own position (16 at most rates), which the host must
/// have fed before it plays. 0 without a source rate other than the instance's, or for an invalid
/// handle. A host clock adds it to [`alpha2_worklet_latency`] while it feeds samples to convert.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_source_latency(handle: u32) -> u32 {
    boundary::with_instance(handle, |instance| {
        instance
            .source
            .as_ref()
            .map_or(0, SourceStream::delay_target_samples)
    })
    .unwrap_or(0)
}

/// Takes the first `written` samples of each [`alpha2_worklet_source`] plane (what
/// [`alpha2_worklet_source_need`] asked for) and writes the next `frames` converted samples into
/// the input planes, for [`alpha2_worklet_process`] or for the host to copy out when the master is
/// transparent. Allocates nothing.
///
/// Returns `OK`, `INVALID_HANDLE`, `INVALID_CONFIG` without a source rate other than the
/// instance's, or `INVALID_SHAPE` for more frames than prepared or more samples than the region
/// holds.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_resample(handle: u32, written: u32, frames: u32) -> i32 {
    boundary::with_instance(handle, |instance| {
        let stride = instance.max_frames;
        let Some(source) = instance.source.as_mut() else {
            return INVALID_CONFIG;
        };
        if source.convert(
            written as usize,
            frames as usize,
            &mut instance.input,
            stride,
        ) {
            OK
        } else {
            INVALID_SHAPE
        }
    })
    .unwrap_or(INVALID_HANDLE)
}

/// Returns the host-writable configuration region in wasm linear memory.
///
/// The host writes at most [`alpha2_worklet_config_capacity`] bytes there and
/// then calls [`alpha2_worklet_configure`].
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_config_buffer(handle: u32) -> *mut u8 {
    boundary::with_instance(handle, |instance| instance.config.as_mut_ptr())
        .unwrap_or(core::ptr::null_mut())
}

/// Returns the configuration region's capacity in bytes.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_config_capacity() -> u32 {
    u32::try_from(CONFIG_CAPACITY).unwrap_or(u32::MAX)
}

/// Installs the master bus in the configuration region, keeping the running
/// chain's history wherever the edit allows (`WorkletProcessor::configure`).
///
/// Returns `INVALID_CONFIG` and keeps the previous chain when the record is
/// unreadable, so a bad message never silences an already-running graph.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_configure(handle: u32, length: u32) -> i32 {
    boundary::with_instance(handle, |instance| instance.configure(length as usize))
        .unwrap_or(INVALID_HANDLE)
}

/// Resets transport position and DSP history, the conversion's included: the next sample the
/// host feeds is the stream's first.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_reset(handle: u32) -> i32 {
    boundary::with_instance(handle, |instance| {
        instance.sample_position = 0;
        instance.chain.reset();
        if let Some(limiter) = instance.limiter.as_mut() {
            limiter.reset();
        }
        instance.input.fill(0.0);
        instance.output.fill(0.0);
        if let Some(source) = instance.source.as_mut() {
            source.reset();
        }
        OK
    })
    .unwrap_or(INVALID_HANDLE)
}

/// Address of the last panic's UTF-8 message in wasm linear memory.
///
/// After a call traps, the host reads [`alpha2_worklet_panic_length`] bytes
/// here to say why. Empty until something has panicked.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_panic_message() -> *const u8 {
    panic_record::address()
}

/// Byte length of the last panic's message.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_panic_length() -> u32 {
    u32::try_from(panic_record::length()).unwrap_or(0)
}

/// Destroys a generation handle exactly once.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn alpha2_worklet_destroy(handle: u32) -> i32 {
    boundary::destroy(handle)
}
