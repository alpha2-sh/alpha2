#![no_main]

use alpha2_engine::{AudioRenderer, EngineProject};
use alpha2_media::{AudioBlock, AudioBlockProvider, AudioSpan, SourceId};
use libfuzzer_sys::fuzz_target;
use typed_arena::Arena;

/// Answers every request with a constant block made at the requested start, as a whole source, so
/// every request is covered and the render mixes instead of reporting a missing source. The arena
/// keeps each block for as long as the provider lends it.
struct Constant {
    sample_rate: u32,
    blocks: Arena<AudioBlock>,
}

impl AudioBlockProvider for Constant {
    fn block(&self, _source: &SourceId, start_sample: i64, frames: usize) -> Option<AudioSpan<'_>> {
        let block = self.blocks.alloc(AudioBlock {
            start_sample,
            sample_rate: self.sample_rate,
            speakers: alpha2_media::SpeakerLayout::STEREO,
            channels: vec![vec![0.25; frames]; 2],
        });
        Some(AudioSpan::whole(block))
    }
}

// `renderAudio` takes its start sample from JavaScript, so any i64 arrives here.
// A render answers or refuses: it never panics, which in wasm would trap and take the engine
// instance with it.
fuzz_target!(|input: (i64, u16, bool, &str)| {
    let (start, frames, paused, json) = input;
    let Ok(project) = alpha2_model::Project::from_json(json) else {
        return;
    };
    let sample_rate = project.settings.sample_rate;
    let Ok(snapshot) = EngineProject::new(project) else {
        return;
    };
    let Ok(mut renderer) = AudioRenderer::new(&snapshot, 2, 1_024) else {
        return;
    };
    let provider = Constant {
        sample_rate,
        blocks: Arena::new(),
    };
    let transport = if paused {
        alpha2_audio::TransportState::Stopped
    } else {
        alpha2_audio::TransportState::Playing
    };
    let _ = renderer.render(&snapshot, start, usize::from(frames), &provider, transport);
});
