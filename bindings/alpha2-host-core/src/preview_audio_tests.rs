use std::path::Path;

use alpha2_container::TrackInfo;
use alpha2_media::{DecodeEvent, Decoder};
use alpha2_model::Project;

use super::*;
use crate::DecodedAudioSources;
use crate::audio_stream::{AudioBackends, AudioDecodeBackend, AudioStream, WithPcm};
use crate::audio_windows::{OpenAudio, WindowedAudioSources};
use crate::recovery::DecodePolicy;
use crate::video_stream::BackendError;

const RATE: u32 = 48_000;
const HOUR_TICKS: i64 = 3_600 * alpha2_time::TICKS_PER_SECOND;

/// Reads nothing itself; PCM goes through `WithPcm`.
struct NoDecoders;

impl AudioBackends for NoDecoders {
    fn open(&self, _track: &TrackInfo) -> Result<Box<dyn AudioDecodeBackend>, BackendError> {
        Err(BackendError::Unsupported("PCM only".into()))
    }
}

/// `seconds` of a 16-bit mono 8 kHz WAV whose samples follow their index, so any two reads of
/// the same span agree and a misplaced one does not.
fn write_wav(path: &Path, seconds: u32) {
    let frames = 8_000 * seconds;
    let mut wav = Vec::with_capacity(44 + frames as usize * 2);
    wav.extend(b"RIFF");
    wav.extend((36 + frames * 2).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend([1, 0, 1, 0]);
    wav.extend(8_000_u32.to_le_bytes());
    wav.extend(16_000_u32.to_le_bytes());
    wav.extend([2, 0, 16, 0]);
    wav.extend(b"data");
    wav.extend((frames * 2).to_le_bytes());
    for index in 0..frames {
        let sample = i16::try_from(i64::from(index * 37 % 20_000) - 10_000).expect("in range");
        wav.extend(sample.to_le_bytes());
    }
    std::fs::write(path, wav).expect("wav");
}

/// The audio project with its audio clip stretched over an hour of the source.
fn hour_project() -> EngineProject {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    let mut project: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tests.join("cases/host-audio-export-project.json"))
            .expect("project"),
    )
    .expect("JSON");
    for track in project["timeline"]["tracks"]
        .as_array_mut()
        .expect("tracks")
    {
        for clip in track["clips"].as_array_mut().expect("clips") {
            clip["timeline_range"]["end"] = HOUR_TICKS.into();
            clip["source_range"]["end"] = HOUR_TICKS.into();
        }
    }
    let project = Project::from_json(&project.to_string()).expect("loads");
    EngineProject::new(project).expect("snapshot")
}

/// Windowed sources that report the most bytes they held.
struct Peaked(WindowedAudioSources<AudioStream>, Arc<AtomicU64>);

impl alpha2_media::AudioBlockProvider for Peaked {
    fn block(
        &self,
        source: &alpha2_media::SourceId,
        stream: Option<u32>,
        start_sample: i64,
        frames: usize,
    ) -> Option<alpha2_media::AudioSpan<'_>> {
        self.0.block(source, stream, start_sample, frames)
    }
}

impl ProjectAudioSources for Peaked {
    fn prepare(
        &mut self,
        needs: &[alpha2_engine::AudioNeed],
        cancel: &CancelToken,
    ) -> Result<(), crate::audio_windows::AudioWindowError> {
        let prepared = self.0.prepare(needs, cancel);
        self.1.store(self.0.peak_bytes(), Ordering::Relaxed);
        prepared
    }
}

fn windowed(path: &Path, budget: u64) -> WindowedAudioSources<AudioStream> {
    let path = path.to_path_buf();
    let open: OpenAudio<AudioStream> = Box::new(move |_, stream| {
        AudioStream::open_file(
            &path,
            &WithPcm(NoDecoders),
            RATE,
            stream,
            DecodePolicy::default(),
        )
        .map_err(crate::audio_windows::AudioWindowError::Decode)
    });
    WindowedAudioSources::new(RATE, budget, open)
}

fn decoded_whole(path: &Path) -> DecodedAudioSources {
    let mut stream = AudioStream::open_file(
        path,
        &WithPcm(NoDecoders),
        RATE,
        None,
        DecodePolicy::default(),
    )
    .expect("opens");
    let mut all: Option<AudioBlock> = None;
    loop {
        match stream
            .decode_next(&CancelToken::default())
            .expect("decodes")
        {
            DecodeEvent::Audio(block) => match all.as_mut() {
                None => all = Some(block),
                Some(all) => {
                    for (into, from) in all.channels.iter_mut().zip(block.channels) {
                        into.extend(from);
                    }
                }
            },
            DecodeEvent::EndOfStream => break,
            DecodeEvent::Retry | DecodeEvent::Video(_) => {}
        }
    }
    let mut sources = DecodedAudioSources::new();
    sources.insert(
        alpha2_media::SourceId::new("audio-source").expect("id"),
        all.expect("samples"),
    );
    sources
}

fn mix_range(mixer: &mut Mixer, start: i64, blocks: usize) -> Vec<f32> {
    (0..blocks)
        .flat_map(|index| {
            let at = start + i64::try_from(index * MIX_BLOCK).expect("small");
            let block = mixer.block(at, &CancelToken::default()).expect("mixes");
            block.channels[0].clone()
        })
        .collect()
}

#[test]
fn the_preview_mix_reads_an_hour_of_audio_in_windows_with_the_samples_of_the_whole_source_mix() {
    let dir = std::env::temp_dir();
    let hour = dir.join(format!("alpha2-preview-hour-{}.wav", std::process::id()));
    let start = dir.join(format!("alpha2-preview-start-{}.wav", std::process::id()));
    write_wav(&hour, 3_600);
    write_wav(&start, 11);
    let budget = 4 << 20;
    let peak = Arc::new(AtomicU64::new(0));
    let sources = Peaked(windowed(&hour, budget), Arc::clone(&peak));
    let mut windowed = Mixer::new(hour_project(), Box::new(sources)).expect("mix");
    let mut whole = Mixer::new(hour_project(), Box::new(decoded_whole(&start))).expect("mix");
    let ten_seconds = 10 * RATE as usize / MIX_BLOCK;
    let from_windows = mix_range(&mut windowed, 0, ten_seconds);
    let from_whole = mix_range(&mut whole, 0, ten_seconds);
    assert!(
        from_whole.iter().any(|sample| sample.abs() > 0.01),
        "the mix is not silent"
    );
    assert_eq!(from_windows, from_whole);
    // A second of mix at points across the hour, as seeks while playing read it.
    for minute in [7, 23, 41, 58, 12] {
        let at = i64::from(minute) * 60 * i64::from(RATE);
        let block = mix_range(&mut windowed, at, RATE as usize / MIX_BLOCK);
        assert!(
            block.iter().any(|sample| sample.abs() > 0.01),
            "minute {minute}"
        );
    }
    let peak = peak.load(Ordering::Relaxed);
    assert!(peak <= budget, "{peak} bytes held, over {budget}");
    let _ = std::fs::remove_file(hour);
    let _ = std::fs::remove_file(start);
}
