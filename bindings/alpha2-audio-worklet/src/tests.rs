//! Handle-safety and processing tests for the worklet C-ABI.

use super::*;

#[test]
fn rejects_arbitrary_stale_and_invalid_shapes() {
    assert_eq!(alpha2_worklet_process(0, 128, 2), INVALID_HANDLE);
    assert_eq!(alpha2_worklet_process(u32::MAX, 128, 2), INVALID_HANDLE);
    assert_eq!(alpha2_worklet_create(0, 128, 2), 0);
    assert_eq!(alpha2_worklet_create(MAX_SAMPLE_RATE + 1, 128, 2), 0);
    assert_eq!(alpha2_worklet_create(48_000, 4_097, 2), 0);
    assert_eq!(alpha2_worklet_create(48_000, 128, 3), 0);
    let handle = alpha2_worklet_create(48_000, 128, 2);
    assert_eq!(alpha2_worklet_process(handle, 129, 2), INVALID_SHAPE);
    assert_eq!(alpha2_worklet_process(handle, 128, 1), INVALID_SHAPE);
    assert_eq!(alpha2_worklet_destroy(handle), OK);
    assert_eq!(alpha2_worklet_process(handle, 128, 2), INVALID_HANDLE);
    assert_eq!(alpha2_worklet_destroy(handle), INVALID_HANDLE);
}

#[test]
fn multiple_processors_remain_independently_addressable() {
    let mono = alpha2_worklet_create(48_000, 64, 1);
    let stereo = alpha2_worklet_create(48_000, 128, 2);
    assert_ne!(mono, stereo);
    assert_eq!(alpha2_worklet_process(mono, 64, 1), OK);
    assert_eq!(alpha2_worklet_process(stereo, 128, 2), OK);
    assert_eq!(alpha2_worklet_destroy(mono), OK);
    assert_eq!(alpha2_worklet_process(stereo, 128, 2), OK);
    assert_eq!(alpha2_worklet_destroy(stereo), OK);
}

#[test]
fn processes_mono_and_stereo_frame_sizes_without_crossing_planes() {
    for channels in [1_u32, 2] {
        for max_frames in [1_u32, 63, 64, 127, 128] {
            let handle = alpha2_worklet_create(48_000, max_frames, channels);
            assert_ne!(handle, 0);
            assert_eq!(alpha2_worklet_frames(handle), max_frames);
            assert_eq!(alpha2_worklet_channels(handle), channels);
            let samples = usize::try_from(max_frames * channels).expect("sample count");
            let input = alpha2_worklet_input(handle);
            assert!(!input.is_null());
            // SAFETY: the instance exposes exactly max_frames * channels samples.
            let input = unsafe { core::slice::from_raw_parts_mut(input, samples) };
            // Each channel gets its own level, so a crossed plane shows; the default limiter
            // passes both.
            for (channel, plane) in input.chunks_mut(max_frames as usize).enumerate() {
                plane.fill(if channel == 0 { 0.25 } else { -0.5 });
            }
            let expected = input.to_vec();
            assert_eq!(alpha2_worklet_process(handle, max_frames, channels), OK);
            let output = alpha2_worklet_output(handle);
            assert!(!output.is_null());
            // SAFETY: the output has the same live instance-owned extent.
            let output = unsafe { core::slice::from_raw_parts(output, samples) };
            assert_eq!(output, expected.as_slice());
            assert_eq!(alpha2_worklet_destroy(handle), OK);
        }
    }
}

/// Builds the fixed-width record the host writes into the config region.
fn config(gain: f32, limiter: Option<(f32, f32)>, effects: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&CONFIG_VERSION.to_le_bytes());
    bytes.extend_from_slice(&gain.to_le_bytes());
    if let Some((threshold, release)) = limiter {
        bytes.push(1);
        bytes.extend_from_slice(&threshold.to_le_bytes());
        bytes.extend_from_slice(&release.to_le_bytes());
    } else {
        bytes.push(0);
        bytes.extend_from_slice(&0.0_f32.to_le_bytes());
        bytes.extend_from_slice(&0.0_f32.to_le_bytes());
    }
    bytes.extend_from_slice(&u32::try_from(effects.len()).expect("count").to_le_bytes());
    for effect in effects {
        bytes.extend_from_slice(effect);
    }
    bytes
}

fn reverb(room: f32, damping: f32, mix: f32) -> Vec<u8> {
    let mut bytes = vec![2_u8];
    for value in [room, damping, mix] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn write_config(handle: u32, bytes: &[u8]) -> i32 {
    let region = alpha2_worklet_config_buffer(handle);
    assert!(!region.is_null());
    // SAFETY: the region is instance-owned and at least CONFIG_CAPACITY long.
    let region = unsafe { core::slice::from_raw_parts_mut(region, CONFIG_CAPACITY) };
    region[..bytes.len()].copy_from_slice(bytes);
    alpha2_worklet_configure(handle, u32::try_from(bytes.len()).expect("length"))
}

#[test]
fn configuration_replaces_the_master_chain_and_gain() {
    let handle = alpha2_worklet_create(48_000, 64, 2);
    assert_eq!(write_config(handle, &config(0.5, None, &[])), OK);

    let input = alpha2_worklet_input(handle);
    // SAFETY: the buffer is instance-owned with 128 samples.
    let input = unsafe { core::slice::from_raw_parts_mut(input, 128) };
    input.fill(1.0);
    assert_eq!(alpha2_worklet_process(handle, 64, 2), OK);
    let output = alpha2_worklet_output(handle);
    // SAFETY: the output buffer has the same live owned extent.
    let output = unsafe { core::slice::from_raw_parts(output, 128) };
    // Master gain applies and no limiter is present to reshape it.
    assert!((output[0] - 0.5).abs() < 1.0e-6, "{}", output[0]);
    assert_eq!(alpha2_worklet_destroy(handle), OK);
}

#[test]
fn an_invalid_record_is_rejected_without_changing_the_chain() {
    let handle = alpha2_worklet_create(48_000, 64, 2);
    assert_eq!(write_config(handle, &config(0.25, None, &[])), OK);

    // A wrong version, an out-of-range gain, and a truncated record all fail.
    let mut wrong_version = config(1.0, None, &[]);
    wrong_version[0] = 9;
    assert_eq!(write_config(handle, &wrong_version), INVALID_CONFIG);
    assert_eq!(
        write_config(handle, &config(99.0, None, &[])),
        INVALID_CONFIG
    );
    assert_eq!(
        write_config(handle, &config(1.0, None, &[])[..6]),
        INVALID_CONFIG
    );
    assert_eq!(
        write_config(handle, &config(1.0, None, &[&reverb(2.0, 0.5, 0.5)])),
        INVALID_CONFIG
    );
    assert_eq!(
        write_config(handle, &config(1.0, Some((0.0, 50.0)), &[])),
        INVALID_CONFIG
    );

    // The chain configured before the failures is still the live one.
    let input = alpha2_worklet_input(handle);
    // SAFETY: the buffer is instance-owned with 128 samples.
    let input = unsafe { core::slice::from_raw_parts_mut(input, 128) };
    input.fill(1.0);
    assert_eq!(alpha2_worklet_process(handle, 64, 2), OK);
    let output = alpha2_worklet_output(handle);
    // SAFETY: the output buffer has the same live owned extent.
    let output = unsafe { core::slice::from_raw_parts(output, 128) };
    assert!((output[0] - 0.25).abs() < 1.0e-6, "{}", output[0]);
    assert_eq!(alpha2_worklet_destroy(handle), OK);
}

/// The exact record `encodeMasterAudio` emits in the package's TypeScript
/// test for a 0.5 gain, a unity limiter, one reverb, and one high-pass band.
/// Both sides assert these bytes, so the wire format cannot drift silently.
#[test]
fn the_typescript_encoder_layout_decodes_here() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&0.5_f32.to_le_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&1.0_f32.to_le_bytes());
    bytes.extend_from_slice(&50.0_f32.to_le_bytes());
    bytes.extend_from_slice(&2_u32.to_le_bytes());
    bytes.push(2);
    for value in [0.5_f32, 0.25, 0.125] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.push(0);
    bytes.push(2);
    for value in [120.0_f32, 0.7, 0.0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(bytes.len(), 48, "record width must match the encoder");

    let config = MasterConfig::decode(&bytes).expect("encoder layout decodes");
    // These values are exactly representable, so bit equality is the point.
    assert_eq!(config.gain.to_bits(), 0.5_f32.to_bits());
    assert_eq!(
        config
            .limiter
            .map(|(threshold, release)| (threshold.to_bits(), release.to_bits())),
        Some((1.0_f32.to_bits(), 50.0_f32.to_bits()))
    );
    assert_eq!(
        config.effects,
        vec![
            ConfigEffect::Reverb {
                room_size: 0.5,
                damping: 0.25,
                mix: 0.125,
            },
            ConfigEffect::EqBand {
                kind: BiquadKind::HighPass,
                frequency_hz: 120.0,
                q: 0.7,
                gain_db: 0.0,
            },
        ]
    );

    // The limiter-free record the encoder emits is 21 bytes.
    let mut empty = Vec::new();
    empty.extend_from_slice(&1_u32.to_le_bytes());
    empty.extend_from_slice(&1.0_f32.to_le_bytes());
    empty.push(0);
    empty.extend_from_slice(&0.0_f32.to_le_bytes());
    empty.extend_from_slice(&0.0_f32.to_le_bytes());
    empty.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(empty.len(), 21);
    let config = MasterConfig::decode(&empty).expect("empty record decodes");
    assert_eq!(config.gain.to_bits(), 1.0_f32.to_bits());
    assert_eq!(config.limiter, None);
    assert_eq!(config.effects, [] as [ConfigEffect; 0]);
}

#[test]
fn configuration_rejects_stale_handles_and_reports_capacity() {
    assert!(alpha2_worklet_config_capacity() >= 1_024);
    assert_eq!(alpha2_worklet_configure(0, 8), INVALID_HANDLE);
    assert_eq!(alpha2_worklet_configure(u32::MAX, 8), INVALID_HANDLE);
    assert!(alpha2_worklet_config_buffer(0).is_null());
}

#[test]
fn processes_owned_planar_buffers() {
    let handle = alpha2_worklet_create(48_000, 128, 2);
    let input = alpha2_worklet_input(handle);
    // SAFETY: the pointer refers to a live instance-owned buffer of 256 samples.
    let input = unsafe { core::slice::from_raw_parts_mut(input, 256) };
    input[..128].fill(2.0);
    input[128..].fill(0.5);
    assert_eq!(alpha2_worklet_process(handle, 128, 2), OK);
    let output = alpha2_worklet_output(handle);
    // SAFETY: output has the same live owned extent.
    let output = unsafe { core::slice::from_raw_parts(output, 256) };
    assert!(output[..128].iter().all(|sample| *sample <= 1.0));
    assert_eq!(alpha2_worklet_destroy(handle), OK);
}

#[test]
fn a_panic_leaves_its_message_for_the_host() {
    let handle = alpha2_worklet_create(48_000, 128, 2);
    let caught = std::panic::catch_unwind(|| panic!("worklet test panic: boom"));
    assert!(caught.is_err());
    let message = panic_record::message();
    assert!(message.contains("worklet test panic: boom"), "{message}");
    assert_eq!(alpha2_worklet_panic_length() as usize, message.len());
    assert!(!alpha2_worklet_panic_message().is_null());
    assert_eq!(alpha2_worklet_destroy(handle), OK);
}

/// A master record: gain, optional limiter, and `nodes` already encoded.
fn master_record(gain: f32, limiter: Option<(f32, f32)>, nodes: &[u8], count: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&gain.to_le_bytes());
    let (flag, (threshold, release)) = limiter.map_or((0, (0.0, 0.0)), |pair| (1, pair));
    bytes.push(flag);
    bytes.extend_from_slice(&f32::to_le_bytes(threshold));
    bytes.extend_from_slice(&f32::to_le_bytes(release));
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(nodes);
    bytes
}

fn configure(handle: u32, record: &[u8]) -> i32 {
    let buffer = alpha2_worklet_config_buffer(handle);
    // SAFETY: the configuration region is live and CONFIG_CAPACITY bytes long.
    let buffer = unsafe { core::slice::from_raw_parts_mut(buffer, CONFIG_CAPACITY) };
    buffer[..record.len()].copy_from_slice(record);
    alpha2_worklet_configure(handle, u32::try_from(record.len()).expect("short record"))
}

/// A host may skip `process` while the module reports it transparent, so
/// transparent has to mean the output is the input, bit for bit.
#[test]
fn a_transparent_master_copies_its_input_exactly() {
    let handle = alpha2_worklet_create(48_000, 128, 2);
    // The default master limits to 0 dBFS.
    assert_eq!(alpha2_worklet_transparent(handle), 0);
    assert_eq!(configure(handle, &master_record(1.0, None, &[], 0)), OK);
    assert_eq!(alpha2_worklet_transparent(handle), 1);
    let input = alpha2_worklet_input(handle);
    // SAFETY: the input is a live instance-owned buffer of 256 samples.
    let input = unsafe { core::slice::from_raw_parts_mut(input, 256) };
    for (index, sample) in input.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)] // Small indices.
        let value = (index as f32 * 0.37).sin() * 3.0;
        *sample = if index == 7 { -0.0 } else { value };
    }
    let expected = input.to_vec();
    assert_eq!(alpha2_worklet_process(handle, 128, 2), OK);
    let output = alpha2_worklet_output(handle);
    // SAFETY: the output has the same live owned extent.
    let output = unsafe { core::slice::from_raw_parts(output, 256) };
    let bits = |samples: &[f32]| {
        samples
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(output), bits(&expected));

    // Anything that changes the signal is not transparent.
    assert_eq!(configure(handle, &master_record(0.5, None, &[], 0)), OK);
    assert_eq!(alpha2_worklet_transparent(handle), 0);
    assert_eq!(
        configure(handle, &master_record(1.0, Some((1.0, 50.0)), &[], 0)),
        OK
    );
    assert_eq!(alpha2_worklet_transparent(handle), 0);
    let mut reverb = vec![2_u8];
    for value in [0.5_f32, 0.5, 0.2] {
        reverb.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(configure(handle, &master_record(1.0, None, &reverb, 1)), OK);
    assert_eq!(alpha2_worklet_transparent(handle), 0);
    assert_eq!(alpha2_worklet_destroy(handle), OK);
    assert_eq!(alpha2_worklet_transparent(handle), 0);
}

#[test]
fn an_eq_gain_the_filter_would_not_render_is_refused() {
    // The decoder accepts only the gain the filter renders, so a larger gain is refused
    // rather than silently clamped by the filter.
    let band = |gain_db: f32| {
        let mut bytes = vec![0_u8, 0];
        for value in [1_000.0_f32, 1.0, gain_db] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    };
    let limit = alpha2_audio::MAX_PEAKING_GAIN_DB;
    for gain_db in [limit, -limit, 0.0] {
        assert!(
            MasterConfig::decode(&config(1.0, None, &[&band(gain_db)])).is_some(),
            "{gain_db}"
        );
    }
    for gain_db in [
        limit.next_up(),
        -limit.next_up(),
        42.0,
        -48.0,
        f32::NAN,
        f32::INFINITY,
    ] {
        assert!(
            MasterConfig::decode(&config(1.0, None, &[&band(gain_db)])).is_none(),
            "{gain_db}"
        );
    }
}

/// A live instance and its planar buffers, destroyed when dropped.
struct Worklet {
    handle: u32,
    frames: usize,
    channels: usize,
}

impl Worklet {
    fn new(sample_rate: u32, frames: u32, channels: u32) -> Self {
        let handle = alpha2_worklet_create(sample_rate, frames, channels);
        assert_ne!(
            handle, 0,
            "{sample_rate} Hz, {frames} frames, {channels} channels"
        );
        Self {
            handle,
            frames: frames as usize,
            channels: channels as usize,
        }
    }

    fn configure(&self, record: &[u8]) -> i32 {
        configure(self.handle, record)
    }

    /// Writes one plane per channel at the module's stride, processes them,
    /// and returns what the module wrote for each.
    fn run(&mut self, planes: &[&[f32]]) -> Vec<Vec<f32>> {
        assert_eq!(planes.len(), self.channels);
        let frames = planes[0].len();
        let samples = self.frames * self.channels;
        // SAFETY: the instance owns `frames * channels` input samples and is alive.
        let input =
            unsafe { core::slice::from_raw_parts_mut(alpha2_worklet_input(self.handle), samples) };
        for (channel, plane) in planes.iter().enumerate() {
            input[channel * self.frames..][..frames].copy_from_slice(plane);
        }
        let status = alpha2_worklet_process(
            self.handle,
            u32::try_from(frames).expect("frames"),
            u32::try_from(self.channels).expect("channels"),
        );
        assert_eq!(status, OK);
        // SAFETY: the output has the same live owned extent.
        let output =
            unsafe { core::slice::from_raw_parts(alpha2_worklet_output(self.handle), samples) };
        (0..self.channels)
            .map(|channel| output[channel * self.frames..][..frames].to_vec())
            .collect()
    }

    /// Runs one mono block.
    fn mono(&mut self, plane: &[f32]) -> Vec<f32> {
        self.run(&[plane]).remove(0)
    }
}

impl Drop for Worklet {
    fn drop(&mut self) {
        assert_eq!(alpha2_worklet_destroy(self.handle), OK);
    }
}

fn eq_band(kind: u8, frequency_hz: f32, q: f32, gain_db: f32) -> Vec<u8> {
    let mut bytes = vec![0_u8, kind];
    for value in [frequency_hz, q, gain_db] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn compressor(threshold_db: f32, ratio: f32, attack: f32, release: f32, lookahead: f32) -> Vec<u8> {
    let mut bytes = vec![1_u8];
    for value in [threshold_db, ratio, attack, release, lookahead] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// `frames` samples of a sine at `hz`, peak `amplitude`, from sample `start`.
fn sine(hz: f32, amplitude: f32, start: usize, frames: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)] // Short test signals.
    (start..start + frames)
        .map(|index| (index as f32 * hz * core::f32::consts::TAU / 48_000.0).sin() * amplitude)
        .collect()
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|sample| sample.to_bits()).collect()
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
}

/// A configuration that changes only the gain keeps the running
/// chain, so the compressor's lookahead delay is not refilled with silence.
#[test]
fn a_gain_change_keeps_the_running_compressor() {
    let mut worklet = Worklet::new(48_000, 128, 1);
    // 10 ms of lookahead at 48 kHz is 480 samples, under 0 dBFS so no reduction.
    let chain = compressor(0.0, 2.0, 10.0, 100.0, 10.0);
    assert_eq!(worklet.configure(&config(1.0, None, &[&chain])), OK);
    let block = [0.5_f32; 128];
    for _ in 0..8 {
        worklet.mono(&block);
    }
    assert!(worklet.mono(&block).iter().all(|sample| *sample > 0.4));
    assert_eq!(worklet.configure(&config(0.5, None, &[&chain])), OK);
    for index in 0..8 {
        let output = worklet.mono(&block);
        assert!(
            output.iter().all(|sample| (sample - 0.25).abs() < 1.0e-3),
            "block {index} after a gain change: {output:?}"
        );
    }
}

/// The same record sent again changes nothing, so the reverb tail
/// plays on exactly as if no message had come.
#[test]
fn a_repeated_configuration_leaves_the_reverb_tail_untouched() {
    let record = config(1.0, None, &[&reverb(0.8, 0.3, 0.5)]);
    let mut steady = Worklet::new(48_000, 128, 2);
    let mut reconfigured = Worklet::new(48_000, 128, 2);
    assert_eq!(steady.configure(&record), OK);
    assert_eq!(reconfigured.configure(&record), OK);
    for block in 0..40 {
        let left = sine(440.0, 0.5, block * 128, 128);
        let right = sine(660.0, 0.25, block * 128, 128);
        if block == 20 {
            assert_eq!(reconfigured.configure(&record), OK);
        }
        let expected = steady.run(&[&left, &right]);
        let actual = reconfigured.run(&[&left, &right]);
        for channel in 0..2 {
            assert_eq!(
                bits(&actual[channel]),
                bits(&expected[channel]),
                "block {block}"
            );
        }
    }
}

/// An edited node of the same kind takes over the history of the
/// one it replaces; a reverb whose mix changes keeps ringing.
#[test]
fn an_edited_reverb_keeps_its_tail() {
    let mut worklet = Worklet::new(48_000, 128, 1);
    assert_eq!(
        worklet.configure(&config(1.0, None, &[&reverb(0.9, 0.2, 0.5)])),
        OK
    );
    for block in 0..20 {
        worklet.mono(&sine(300.0, 0.5, block * 128, 128));
    }
    assert_eq!(
        worklet.configure(&config(1.0, None, &[&reverb(0.9, 0.2, 0.75)])),
        OK
    );
    let tail = worklet.mono(&[0.0; 128]);
    assert!(peak(&tail) > 1.0e-3, "the tail was cut: {}", peak(&tail));
}

/// A limiter whose release changes keeps its envelope, so a quiet
/// passage right after a loud one is still held down.
#[test]
fn an_edited_limiter_keeps_its_envelope() {
    let mut worklet = Worklet::new(48_000, 128, 1);
    assert_eq!(worklet.configure(&config(1.0, Some((0.5, 50.0)), &[])), OK);
    worklet.mono(&[1.0; 128]);
    assert_eq!(worklet.configure(&config(1.0, Some((0.5, 100.0)), &[])), OK);
    let output = worklet.mono(&[0.25; 128]);
    assert!(output[0] < 0.2, "the envelope was dropped: {}", output[0]);
}

/// The master gain scales the chain's output, as the engine's
/// master stage does, so a compressor sees the summed buses unscaled.
#[test]
fn the_master_gain_follows_the_chain_as_in_the_engine() {
    let mut worklet = Worklet::new(48_000, 128, 1);
    let record = config(0.25, None, &[&compressor(-12.0, 4.0, 10.0, 100.0, 0.0)]);
    assert_eq!(worklet.configure(&record), OK);
    let mut reference = Compressor::new(-12.0, 4.0, 10.0, 100.0, 0.0);
    reference.prepare(48_000, 128);
    let ctx = ProcessCtx {
        position: MediaTime::ZERO,
        sample_position: 0,
        sample_rate: 48_000,
        transport: TransportState::Playing,
    };
    for block in 0..200 {
        let input = sine(1_000.0, 1.0, block * 128, 128);
        let mut expected = vec![0.0; 128];
        reference.process(&ctx, &[&input], &mut [&mut expected]);
        for sample in &mut expected {
            *sample *= 0.25;
        }
        let output = worklet.mono(&input);
        for (actual, expected) in output.iter().zip(&expected) {
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "block {block}: {actual} vs {expected}"
            );
        }
    }
}

/// A non-finite input sample plays as silence at the module's
/// edge, whatever the master holds, and the chain plays on.
#[test]
fn a_non_finite_input_sample_is_silence_and_the_master_plays_on() {
    let masters = [
        config(0.5, None, &[]),
        config(1.0, None, &[&eq_band(0, 1_000.0, 1.0, 6.0)]),
        config(1.0, None, &[&reverb(0.5, 0.5, 0.5)]),
        config(1.0, Some((1.0, 50.0)), &[&eq_band(2, 100.0, 0.7, 0.0)]),
    ];
    for (index, record) in masters.iter().enumerate() {
        for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut worklet = Worklet::new(48_000, 128, 2);
            assert_eq!(worklet.configure(record), OK);
            let mut left = sine(440.0, 0.25, 0, 128);
            let right = sine(440.0, 0.25, 0, 128);
            left[17] = poison;
            let output = worklet.run(&[&left, &right]);
            assert!(
                output.iter().flatten().all(|sample| sample.is_finite()),
                "master {index}, {poison}"
            );
            for block in 1..40 {
                let plane = sine(440.0, 0.25, block * 128, 128);
                let output = worklet.run(&[&plane, &plane]);
                assert!(
                    output.iter().all(
                        |channel| channel.iter().all(|s| s.is_finite()) && peak(channel) > 0.01
                    ),
                    "master {index}, {poison}, block {block}"
                );
            }
        }
    }
}

/// A rate no browser renders at is refused at creation, so no
/// later configuration can meet it.
#[test]
fn a_sample_rate_below_any_audio_context_is_refused() {
    for rate in [1, 8, 20, 21, 2_999] {
        assert_eq!(alpha2_worklet_create(rate, 128, 2), 0, "{rate} Hz");
    }
    let worklet = Worklet::new(3_000, 128, 2);
    assert_eq!(
        worklet.configure(&config(1.0, None, &[&eq_band(0, 1_000.0, 1.0, 6.0)])),
        OK
    );
}

/// The largest master the model accepts, every effect an EQ of
/// the most bands, fits the record and configures.
#[test]
fn the_largest_valid_master_configures() {
    let worklet = Worklet::new(48_000, 128, 2);
    let largest = alpha2_model::MAX_CHAIN_EFFECTS * alpha2_model::MAX_EQ_BANDS;
    assert_eq!(MAX_NODES as usize, largest);
    let band = eq_band(0, 1_000.0, 1.0, 1.0);
    let nodes = vec![band.as_slice(); largest];
    let record = config(1.0, Some((1.0, 50.0)), &nodes);
    assert!(record.len() <= CONFIG_CAPACITY, "{} bytes", record.len());
    assert_eq!(worklet.configure(&record), OK);
    let over = vec![band.as_slice(); largest + 1];
    assert_eq!(worklet.configure(&config(1.0, None, &over)), INVALID_CONFIG);
}

/// The record `encodeMasterAudio` emits for the master in the package's
/// TypeScript test `every node kind keeps its layout`: every node kind and
/// band kind, with no field at a default. Both sides assert these bytes.
const EVERY_NODE_RECORD: &str = "\
01000000\
0000403f\
01\
0000603f\
0000a042\
05000000\
01\
000090c1\
00004040\
0000a040\
0000f042\
00002040\
00\
00\
00007a44\
0000c03f\
0000c040\
00\
01\
00000046\
0000003f\
000040c0\
02\
0000003f\
0000803e\
0000003e\
00\
02\
0000f042\
0000403f\
00000000";

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex"))
        .collect()
}

/// Every node kind decodes field by field from the bytes the
/// TypeScript encoder writes.
#[test]
fn every_node_kind_decodes_from_the_typescript_layout() {
    let config = MasterConfig::decode(&hex_bytes(EVERY_NODE_RECORD)).expect("record decodes");
    assert_eq!(config.gain.to_bits(), 0.75_f32.to_bits());
    assert_eq!(
        config
            .limiter
            .map(|(threshold, release)| (threshold.to_bits(), release.to_bits())),
        Some((0.875_f32.to_bits(), 80.0_f32.to_bits()))
    );
    assert_eq!(
        config.effects,
        vec![
            ConfigEffect::Compressor {
                threshold_db: -18.0,
                ratio: 3.0,
                attack_ms: 5.0,
                release_ms: 120.0,
                lookahead_ms: 2.5,
            },
            ConfigEffect::EqBand {
                kind: BiquadKind::Peaking,
                frequency_hz: 1_000.0,
                q: 1.5,
                gain_db: 6.0,
            },
            ConfigEffect::EqBand {
                kind: BiquadKind::LowPass,
                frequency_hz: 8_192.0,
                q: 0.5,
                gain_db: -3.0,
            },
            ConfigEffect::Reverb {
                room_size: 0.5,
                damping: 0.25,
                mix: 0.125,
            },
            ConfigEffect::EqBand {
                kind: BiquadKind::HighPass,
                frequency_hz: 120.0,
                q: 0.75,
                gain_db: 0.0,
            },
        ]
    );
}

/// Each node the record names is the DSP it builds, with its
/// fields in their places: the output matches that processor run directly.
#[test]
fn each_node_kind_builds_its_processor() {
    let ctx = ProcessCtx {
        position: MediaTime::ZERO,
        sample_position: 0,
        sample_rate: 48_000,
        transport: TransportState::Playing,
    };
    let cases: [(Vec<u8>, Box<dyn Processor>); 4] = [
        (
            compressor(-18.0, 3.0, 5.0, 120.0, 2.5),
            Box::new(Compressor::new(-18.0, 3.0, 5.0, 120.0, 2.5)),
        ),
        (
            eq_band(0, 1_000.0, 1.5, 6.0),
            Box::new(Biquad::new(BiquadKind::Peaking, 1_000.0, 1.5, 6.0)),
        ),
        (
            eq_band(1, 2_000.0, 0.5, 0.0),
            Box::new(Biquad::new(BiquadKind::LowPass, 2_000.0, 0.5, 0.0)),
        ),
        (
            eq_band(2, 500.0, 0.75, 0.0),
            Box::new(Biquad::new(BiquadKind::HighPass, 500.0, 0.75, 0.0)),
        ),
    ];
    for (index, (node, mut reference)) in cases.into_iter().enumerate() {
        let mut worklet = Worklet::new(48_000, 128, 1);
        assert_eq!(worklet.configure(&config(1.0, None, &[&node])), OK);
        reference.prepare(48_000, 128);
        for block in 0..50 {
            let low = sine(200.0, 0.6, block * 128, 128);
            let high = sine(9_000.0, 0.4, block * 128, 128);
            let input: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
            let mut expected = vec![0.0; 128];
            reference.process(&ctx, &[&input], &mut [&mut expected]);
            assert_eq!(
                bits(&worklet.mono(&input)),
                bits(&expected),
                "node {index}, block {block}"
            );
        }
    }
}

/// A reset forgets the limiter's envelope and the reverb's
/// tail, so the next block plays as on a new instance.
#[test]
fn a_reset_plays_the_next_block_as_a_new_instance() {
    let record = config(0.5, Some((0.5, 50.0)), &[&reverb(0.8, 0.2, 0.5)]);
    let mut used = Worklet::new(48_000, 128, 2);
    let mut fresh = Worklet::new(48_000, 128, 2);
    assert_eq!(used.configure(&record), OK);
    assert_eq!(fresh.configure(&record), OK);
    for block in 0..20 {
        let loud = sine(200.0, 4.0, block * 128, 128);
        used.run(&[&loud, &loud]);
    }
    assert_eq!(alpha2_worklet_reset(used.handle), OK);
    for block in 0..20 {
        let quiet = sine(300.0, 0.1, block * 128, 128);
        let expected = fresh.run(&[&quiet, &quiet]);
        let actual = used.run(&[&quiet, &quiet]);
        for channel in 0..2 {
            assert_eq!(
                bits(&actual[channel]),
                bits(&expected[channel]),
                "block {block}"
            );
        }
    }
    assert_eq!(alpha2_worklet_reset(0), INVALID_HANDLE);
}

/// A block shorter than the prepared size reads and writes each
/// channel at the module's stride, sample for sample.
#[test]
fn a_short_block_keeps_every_channel_in_its_own_plane() {
    for channels in [1_u32, 2] {
        for frames in [1_usize, 37, 64, 127] {
            let mut worklet = Worklet::new(48_000, 128, channels);
            #[allow(clippy::cast_precision_loss)] // Small indices.
            let planes: Vec<Vec<f32>> = (0..channels as usize)
                .map(|channel| {
                    (0..frames)
                        .map(|index| {
                            (index as f32 + 1.0) / 256.0 * if channel == 0 { 1.0 } else { -0.5 }
                        })
                        .collect()
                })
                .collect();
            let views: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
            // The default master's 0 dBFS limiter passes these unchanged.
            let output = worklet.run(&views);
            for channel in 0..channels as usize {
                assert_eq!(
                    bits(&output[channel]),
                    bits(&planes[channel]),
                    "{channels} channel(s), {frames} frames, channel {channel}"
                );
            }
        }
    }
}

/// `seconds` of `signal(index)` at `rate` through a mono worklet configured with `record`, in
/// render quanta; returns the output.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // Short test signals.
fn render_mono(rate: u32, record: &[u8], seconds: f64, signal: impl Fn(usize) -> f32) -> Vec<f32> {
    let mut worklet = Worklet::new(rate, 128, 1);
    assert_eq!(worklet.configure(record), OK, "{rate} Hz");
    let frames = (seconds * f64::from(rate)) as usize;
    let mut output = Vec::with_capacity(frames);
    let mut block = Vec::with_capacity(128);
    for start in (0..frames).step_by(128) {
        block.clear();
        block.extend((start..(start + 128).min(frames)).map(&signal));
        output.extend(worklet.mono(&block));
    }
    output
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // Short test signals.
fn rms(samples: &[f32]) -> f64 {
    let energy = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>();
    (energy / samples.len() as f64).sqrt()
}

/// A browser may run its `AudioContext` at any rate the binding accepts, up to 768 kHz, and a
/// low EQ band there filters as it does at 48 kHz: it neither runs away (a 10 Hz high-pass at
/// 768 kHz turning into full-scale noise under the limiter) nor silences (a 10 Hz low-pass at
/// 384 kHz passing nothing).
#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // Short test signals.
fn a_low_eq_band_filters_alike_at_every_context_rate() {
    let limiter = Some((1.0, 50.0));
    let mut failures = Vec::new();
    for rate in [3_000, 48_000, 96_000, 384_000, 768_000] {
        let last_second = |output: &[f32]| output[output.len() - rate as usize..].to_vec();
        let low_pass = render_mono(
            rate,
            &config(1.0, None, &[&eq_band(1, 10.0, 0.707, 0.0)]),
            3.0,
            |_| 0.25,
        );
        let level = rms(&last_second(&low_pass));
        if (level - 0.25).abs() >= 0.25 * 0.012 {
            failures.push(format!("{rate} Hz low-pass 10 Hz: DC 0.25 -> {level:.4}"));
        }

        let tone = |index: usize| {
            (0.25 * (core::f64::consts::TAU * 1_000.0 * index as f64 / f64::from(rate)).sin())
                as f32
        };
        let high_pass = render_mono(
            rate,
            &config(1.0, limiter, &[&eq_band(2, 10.0, 0.707, 0.0)]),
            3.0,
            tone,
        );
        let tail = last_second(&high_pass);
        let level = rms(&tail);
        if peak(&tail) > 0.2501 || (level - 0.25 / 2.0_f64.sqrt()).abs() >= 0.002 {
            failures.push(format!(
                "{rate} Hz high-pass 10 Hz + limiter: 1 kHz 0.25 -> peak {:.4e}, rms {level:.4}",
                peak(&tail)
            ));
        }

        let low_tone = |index: usize| {
            (0.25 * (core::f64::consts::TAU * 10.0 * index as f64 / f64::from(rate)).sin()) as f32
        };
        let peaking = render_mono(
            rate,
            &config(1.0, None, &[&eq_band(0, 10.0, 0.707, 12.0)]),
            3.0,
            low_tone,
        );
        let input: Vec<f32> = (peaking.len() - rate as usize..peaking.len())
            .map(low_tone)
            .collect();
        let gain_db = 20.0 * (rms(&last_second(&peaking)) / rms(&input)).log10();
        if (gain_db - 12.0).abs() >= 0.1 {
            failures.push(format!(
                "{rate} Hz peaking +12 dB at 10 Hz: {gain_db:.2} dB"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The reverb's lines keep their lengths in seconds at every rate a context may run at, so its
/// first echo comes when it does at 48 kHz, 25.31 ms. Lines sized for a rate clamped to 8 to
/// 192 kHz would echo at 12.65 ms at 384 kHz and 67.33 ms at 3 kHz.
#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // Short test signals.
fn the_reverb_echoes_at_the_same_time_at_every_context_rate() {
    let first_echo_ms = |rate: u32| {
        let output = render_mono(
            rate,
            &config(1.0, None, &[&reverb(0.5, 0.5, 1.0)]),
            0.1,
            |index| {
                if index == 0 { 1.0 } else { 0.0 }
            },
        );
        let first = output
            .iter()
            .position(|sample| sample.abs() > 1e-6)
            .expect("an echo");
        first as f64 * 1_000.0 / f64::from(rate)
    };
    let reference = first_echo_ms(48_000);
    assert!((reference - 25.31).abs() < 0.01, "{reference} ms");
    let mut failures = Vec::new();
    for rate in [3_000, 8_000, 192_000, 384_000, 768_000] {
        let echo = first_echo_ms(rate);
        // Each line rounds to a whole sample at its rate.
        let sample_ms = 1_000.0 / f64::from(rate);
        if (echo - reference).abs() > sample_ms + 1_000.0 / 48_000.0 {
            failures.push(format!(
                "{rate} Hz: first echo at {echo:.2} ms, not {reference:.2} ms"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The master chain's delay is what the module reports: an impulse comes out
/// that many samples late, so a host clock can subtract it.
#[test]
fn the_reported_latency_is_how_late_an_impulse_comes_out() {
    for (lookahead_ms, expected) in [(0.0, 0), (10.0, 480), (50.0, 2_400)] {
        let mut worklet = Worklet::new(48_000, 128, 1);
        let chain = compressor(0.0, 2.0, 10.0, 100.0, lookahead_ms);
        assert_eq!(
            worklet.configure(&config(1.0, Some((1.0, 50.0)), &[&chain])),
            OK
        );
        assert_eq!(
            alpha2_worklet_latency(worklet.handle),
            expected,
            "{lookahead_ms} ms"
        );
        let mut output = Vec::new();
        for block in 0..24 {
            let mut input = [0.0_f32; 128];
            if block == 0 {
                input[0] = 0.5;
            }
            output.extend(worklet.mono(&input));
        }
        let arrival = output
            .iter()
            .position(|sample| sample.abs() > 0.25)
            .expect("the impulse comes out");
        assert_eq!(arrival, expected as usize, "{lookahead_ms} ms");
    }
    // Without effects nothing is delayed, and an unknown handle reports nothing.
    let worklet = Worklet::new(48_000, 128, 2);
    assert_eq!(worklet.configure(&config(1.0, None, &[])), OK);
    assert_eq!(alpha2_worklet_latency(worklet.handle), 0);
    assert_eq!(alpha2_worklet_latency(0), 0);
}

/// Feeds `signal` (one plane per channel, at `source_rate`) through an instance at `rate` in
/// calls of `frames` outputs, as the processor does from its ring, and returns `outputs` samples
/// per channel of what the transparent master plays.
fn stream(
    source_rate: u32,
    rate: u32,
    frames: u32,
    signal: &[Vec<f32>],
    outputs: usize,
) -> (Vec<Vec<f32>>, usize) {
    let channels = u32::try_from(signal.len()).expect("channels");
    let handle = alpha2_worklet_create(rate, frames, channels);
    assert_ne!(handle, 0);
    assert_eq!(alpha2_worklet_set_source_rate(handle, source_rate), OK);
    let stride = alpha2_worklet_source_frames(handle) as usize;
    assert!(stride > 0);
    let mut fed = 0;
    let mut played = vec![Vec::new(); signal.len()];
    while played[0].len() < outputs {
        let need = alpha2_worklet_source_need(handle, frames) as usize;
        assert!(need <= stride, "{need} fits the {stride}-sample region");
        let region = alpha2_worklet_source(handle);
        // SAFETY: the region holds `stride` samples per channel.
        let region = unsafe { core::slice::from_raw_parts_mut(region, stride * signal.len()) };
        for (channel, plane) in signal.iter().enumerate() {
            for index in 0..need {
                region[channel * stride + index] = plane.get(fed + index).copied().unwrap_or(0.0);
            }
        }
        fed += need;
        assert_eq!(
            alpha2_worklet_resample(handle, u32::try_from(need).expect("need"), frames),
            OK
        );
        let input = alpha2_worklet_input(handle);
        // SAFETY: the input planes hold `frames` samples per channel.
        let input = unsafe { core::slice::from_raw_parts(input, frames as usize * signal.len()) };
        for (channel, plane) in played.iter_mut().enumerate() {
            let base = channel * frames as usize;
            plane.extend_from_slice(&input[base..base + frames as usize]);
        }
    }
    assert_eq!(alpha2_worklet_destroy(handle), OK);
    for plane in &mut played {
        plane.truncate(outputs);
    }
    (played, fed)
}

#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)] // Counts of a quarter second's samples.
fn audio_at_another_rate_is_converted_as_the_engine_converts_it_whole() {
    // A 48 kHz project in a 44.1 kHz context (and the reverse, and a falling 96 kHz one): fed
    // a quantum at a time, the stream matches the engine's converter on the whole signal.
    for (source_rate, rate, frames) in [
        (48_000_u32, 44_100_u32, 128_u32),
        (44_100, 48_000, 128),
        (96_000, 44_100, 1_024),
        (48_000, 44_100, 1),
    ] {
        let seconds = 0.25;
        let source_frames = (f64::from(source_rate) * seconds) as usize;
        let left = sine(1_000.0, 0.5, 0, source_frames);
        let right: Vec<f32> = left.iter().map(|sample| -sample / 2.0).collect();
        let outputs = (f64::from(rate) * seconds) as usize - 64;
        let (played, fed) = stream(
            source_rate,
            rate,
            frames,
            &[left.clone(), right.clone()],
            outputs,
        );
        let whole = alpha2_audio::Resampler::new(source_rate, rate).expect("converter");
        for (plane, source) in played.iter().zip([&left, &right]) {
            let mut expected = vec![0.0; outputs];
            whole.resample(source, 0, 0, &mut expected);
            assert_eq!(
                bits(plane),
                bits(&expected),
                "{source_rate} -> {rate} by {frames}"
            );
        }
        // It reads the source at the source's rate, ahead of the outputs by the filter's reach.
        let position = outputs as f64 * f64::from(source_rate) / f64::from(rate);
        let ahead = fed as f64 - position;
        let half = whole.half_taps() as f64;
        assert!(
            ahead >= half - 1.0
                && ahead
                    <= half + f64::from(frames) * f64::from(source_rate) / f64::from(rate) + 2.0,
            "{source_rate} -> {rate}: fed {fed} for outputs reaching {position}"
        );
    }
}

#[test]
fn a_source_at_the_instance_rate_or_one_it_cannot_convert_takes_no_conversion() {
    let handle = alpha2_worklet_create(44_100, 128, 2);
    assert_eq!(alpha2_worklet_resample(handle, 0, 128), INVALID_CONFIG);
    assert_eq!(alpha2_worklet_set_source_rate(handle, 44_100), OK);
    assert!(alpha2_worklet_source(handle).is_null());
    assert_eq!(alpha2_worklet_source_frames(handle), 0);
    assert_eq!(alpha2_worklet_source_need(handle, 128), 0);
    assert_eq!(alpha2_worklet_source_latency(handle), 0);
    assert_eq!(alpha2_worklet_set_source_rate(handle, 0), INVALID_CONFIG);
    assert_eq!(
        alpha2_worklet_set_source_rate(handle, 768_001),
        INVALID_CONFIG
    );
    assert_eq!(alpha2_worklet_set_source_rate(handle, 48_000), OK);
    assert!(!alpha2_worklet_source(handle).is_null());
    // A falling rate widens the filter to 18 taps a side at 48 kHz: 16.5 samples at 44.1 kHz.
    assert_eq!(alpha2_worklet_source_latency(handle), 17);
    assert_eq!(alpha2_worklet_source_need(handle, 129), 0);
    assert_eq!(alpha2_worklet_resample(handle, 0, 129), INVALID_SHAPE);
    let stride = alpha2_worklet_source_frames(handle);
    assert_eq!(
        alpha2_worklet_resample(handle, stride + 1, 128),
        INVALID_SHAPE
    );
    assert_eq!(alpha2_worklet_set_source_rate(handle, 44_100), OK);
    assert!(alpha2_worklet_source(handle).is_null());
    assert_eq!(alpha2_worklet_destroy(handle), OK);
    assert_eq!(
        alpha2_worklet_set_source_rate(handle, 48_000),
        INVALID_HANDLE
    );
}

#[test]
fn a_reset_starts_the_conversion_again() {
    let handle = alpha2_worklet_create(44_100, 128, 1);
    assert_eq!(alpha2_worklet_set_source_rate(handle, 48_000), OK);
    let first = alpha2_worklet_source_need(handle, 128);
    for _ in 0..4 {
        let need = alpha2_worklet_source_need(handle, 128);
        assert_eq!(alpha2_worklet_resample(handle, need, 128), OK);
    }
    assert_ne!(alpha2_worklet_source_need(handle, 128), first);
    assert_eq!(alpha2_worklet_reset(handle), OK);
    assert_eq!(alpha2_worklet_source_need(handle, 128), first);
    assert_eq!(alpha2_worklet_destroy(handle), OK);
}
