//! Shared provider contract checks against `tests/cases/audio-provider-contract.json` and, with
//! the `visual` feature, `tests/cases/visual-provider-contract.json` ([`visual`]). JavaScript
//! decoders use the same audio fixture.

#[cfg(feature = "visual")]
pub mod visual;

use std::path::{Path, PathBuf};

use alpha2_media::{AudioBlock, AudioBlockProvider, SourceId, Speaker, SpeakerLayout};
use serde::Deserialize;

/// The audio provider contract fixture.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioContract {
    /// The corpus file, relative to `tests/`.
    pub source: String,
    /// The project rate every position is at.
    pub project_sample_rate: u32,
    /// What the source's stream holds.
    pub stream: ContractStream,
    /// The requests a provider answers.
    pub requests: Vec<ContractRequest>,
    /// Samples each of `layouts` holds, from sample 0.
    pub layout_frames: usize,
    /// Sources that declare their speakers in other ways.
    pub layouts: Vec<ContractLayout>,
    /// Lossy sources whose codec header declares their speakers.
    pub codec_layouts: Vec<ContractCodecLayout>,
}

/// A lossy source whose codec declares its speakers, each a tone of its own.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractCodecLayout {
    /// The corpus file, relative to `tests/`.
    pub source: String,
    /// The speaker each decoded plane feeds, by `FFmpeg` name.
    pub speakers: Vec<String>,
    /// Each speaker's tone, in Hz, in the order of `speakers`.
    pub tones: Vec<f64>,
}

impl ContractCodecLayout {
    /// The source file's path.
    #[must_use]
    pub fn source_path(&self) -> PathBuf {
        tests_dir().join(&self.source)
    }

    /// The speakers a provider must report.
    ///
    /// # Panics
    /// Panics on a name `FFmpeg` gives no speaker.
    #[must_use]
    pub fn speaker_layout(&self) -> SpeakerLayout {
        speaker_layout(&self.speakers)
    }

    /// Checks the block's speakers, and that each plane's loudest tone after the first 0.1 s is
    /// its speaker's tone.
    ///
    /// # Panics
    /// Panics, naming the source, where the block differs.
    pub fn check(&self, block: &AudioBlock) {
        let source = &self.source;
        assert_eq!(block.speakers, self.speaker_layout(), "{source}: speakers");
        assert_eq!(block.channels.len(), self.tones.len(), "{source}: planes");
        let rate = f64::from(block.sample_rate);
        for (plane, samples) in block.channels.iter().enumerate() {
            let settled = &samples[samples.len().min(block.sample_rate as usize / 10)..];
            let loudest = self
                .tones
                .iter()
                .copied()
                .max_by(|a, b| {
                    tone_power(settled, *a, rate).total_cmp(&tone_power(settled, *b, rate))
                })
                .expect("tones");
            assert!(
                (loudest - self.tones[plane]).abs() < f64::EPSILON,
                "{source}: plane {plane} ({}) is the {loudest} Hz tone, not {} Hz",
                self.speakers[plane],
                self.tones[plane]
            );
        }
    }
}

/// The power of `frequency` in `samples` (Goertzel).
#[must_use]
pub fn tone_power(samples: &[f32], frequency: f64, rate: f64) -> f64 {
    let coefficient = 2.0 * (std::f64::consts::TAU * frequency / rate).cos();
    let (mut previous, mut before) = (0.0_f64, 0.0_f64);
    for sample in samples {
        let current = f64::from(*sample) + coefficient * previous - before;
        before = previous;
        previous = current;
    }
    previous * previous + before * before - coefficient * previous * before
}

/// A source that declares its speakers, one constant level per plane.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractLayout {
    /// The corpus file, relative to `tests/`.
    pub source: String,
    /// The speaker each plane feeds, in the file's order, by `FFmpeg` name.
    pub speakers: Vec<String>,
    /// Each plane's level, as a fraction of the stream's `scale`.
    pub levels: Vec<i32>,
}

impl ContractLayout {
    /// The source file's path.
    #[must_use]
    pub fn source_path(&self) -> PathBuf {
        tests_dir().join(&self.source)
    }

    /// The speakers a provider must report.
    ///
    /// # Panics
    /// Panics on a name `FFmpeg` gives no speaker, or a count outside the contract.
    #[must_use]
    pub fn speaker_layout(&self) -> SpeakerLayout {
        speaker_layout(&self.speakers)
    }

    /// Checks the declared speakers and each plane's level for `frames` samples from sample 0.
    ///
    /// # Panics
    /// Panics, naming the source, where the block differs.
    pub fn check(&self, block: &AudioBlock, frames: usize, scale: i32) {
        let source = &self.source;
        assert_eq!(block.start_sample, 0, "{source}: start");
        assert_eq!(block.speakers, self.speaker_layout(), "{source}: speakers");
        assert_eq!(block.channels.len(), self.levels.len(), "{source}: planes");
        for (plane, (samples, level)) in block.channels.iter().zip(&self.levels).enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let expected = *level as f32 / scale as f32;
            assert!(samples.len() >= frames, "{source}: plane {plane} is short");
            if let Some(sample) = samples[..frames]
                .iter()
                .find(|sample| sample.to_bits() != expected.to_bits())
            {
                panic!("{source}: plane {plane} holds {sample}, not {expected}");
            }
        }
    }
}

fn speaker_layout(names: &[String]) -> SpeakerLayout {
    let speakers = names
        .iter()
        .map(|name| Speaker::from_name(name).unwrap_or_else(|| panic!("speaker {name}")))
        .collect::<Vec<_>>();
    SpeakerLayout::new(&speakers).expect("a contract layout")
}

/// Where the source's audio is, and its constant level per channel.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractStream {
    /// The stream's first sample.
    pub first_sample: i64,
    /// One past the stream's last sample.
    pub end_sample: i64,
    /// The speaker each channel feeds, by `FFmpeg` name.
    pub speakers: Vec<String>,
    /// Each channel's level, in speaker order, as a fraction of `scale`.
    pub levels: Vec<i32>,
    /// The levels' full scale.
    pub scale: i32,
}

/// One request: source samples `[start_sample, start_sample + frames)`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractRequest {
    /// What the request is about.
    pub name: String,
    /// The first sample asked for.
    pub start_sample: i64,
    /// How many samples are asked for.
    pub frames: usize,
}

impl ContractRequest {
    /// One past the last sample asked for.
    #[must_use]
    pub fn end_sample(&self) -> i64 {
        self.start_sample
            .saturating_add(i64::try_from(self.frames).unwrap_or(i64::MAX))
    }
}

impl AudioContract {
    /// Reads `tests/cases/audio-provider-contract.json`.
    ///
    /// # Panics
    /// Panics when the fixture is missing or malformed.
    #[must_use]
    pub fn load() -> Self {
        let path = tests_dir()
            .join("cases")
            .join("audio-provider-contract.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    /// The source file's path.
    #[must_use]
    pub fn source_path(&self) -> PathBuf {
        tests_dir().join(&self.source)
    }

    /// The speakers every block of the source names.
    #[must_use]
    pub fn speakers(&self) -> SpeakerLayout {
        speaker_layout(&self.stream.speakers)
    }

    /// What channel `channel` holds at source sample `sample`.
    #[must_use]
    pub fn expected(&self, channel: usize, sample: i64) -> f32 {
        let stream = &self.stream;
        if sample < stream.first_sample || sample >= stream.end_sample {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let level = stream.levels[channel] as f32 / stream.scale as f32;
        level
    }

    /// Samples `[start, end)` as a host would upload them, zeros where the source is silent.
    #[must_use]
    pub fn decoded(&self, start: i64, end: i64) -> AudioBlock {
        AudioBlock {
            start_sample: start,
            sample_rate: self.project_sample_rate,
            speakers: self.speakers(),
            channels: (0..self.stream.levels.len())
                .map(|channel| {
                    (start..end)
                        .map(|sample| self.expected(channel, sample))
                        .collect()
                })
                .collect(),
        }
    }
}

/// Checks `provider`'s answer to every request of `contract` for `source`. `prepare` runs before
/// each request, where a host would decode or upload.
///
/// # Panics
/// Panics, naming the request, at the first answer that breaks the contract.
pub fn check_audio_provider<P: AudioBlockProvider>(
    provider: &mut P,
    source: &SourceId,
    contract: &AudioContract,
    mut prepare: impl FnMut(&mut P, &ContractRequest),
) {
    let channels = contract.stream.levels.len();
    let speakers = contract.speakers();
    assert_eq!(speakers.channels(), channels, "a level per speaker");
    for request in &contract.requests {
        prepare(provider, request);
        let name = &request.name;
        let (start, end) = (request.start_sample, request.end_sample());
        let span = provider
            .block(source, start, request.frames)
            .unwrap_or_else(|| panic!("{name}: no span"));
        assert!(
            span.covers(start, end),
            "{name}: the span does not cover it"
        );
        let block = span.block;
        assert_eq!(
            block.sample_rate, contract.project_sample_rate,
            "{name}: rate"
        );
        assert_eq!(block.channels.len(), channels, "{name}: channels");
        assert_eq!(block.speakers, speakers, "{name}: speakers");
        for channel in 0..channels {
            for sample in start..end {
                let actual = sample
                    .checked_sub(block.start_sample)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| block.channels[channel].get(index).copied())
                    .unwrap_or(0.0);
                let expected = contract.expected(channel, sample);
                assert!(
                    actual.to_bits() == expected.to_bits(),
                    "{name}: channel {channel} at sample {sample} is {actual}, not {expected}"
                );
            }
        }
    }
}

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests/")
        .to_owned()
}
