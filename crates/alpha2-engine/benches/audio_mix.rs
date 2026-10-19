//! Standalone benchmark for the timeline audio mixer.
//!
//! Mixes unity clips of a sine source, one clip per track, over a matrix of
//! clip count, project rate, effect load, and block size, plus retimed clips
//! that interpolate their source. Prints one machine-readable line per case:
//! time per block and the share of real time one core spends mixing. The
//! `audio_seek` cases time a discontinuous render, which replays the graph's
//! settling history first, and the `audio_nudge` cases a render one sample
//! past where the last ended, under each continuity policy: an exact
//! renderer replays as for a seek, a preview one mixes the one block. The
//! `audio_hold` cases time playback and seeks of a renderer that holds
//! incomplete blocks, as an export's does, against one that does not: the
//! hold checks every source is supplied before it mixes. Times are recorded,
//! never gated. An argument (`mix`, `seek`, `nudge`, or `hold`) runs only the
//! cases whose name holds it; `audio_hold_on_16_dry` and the like run one
//! hold case, for a profiler. The `audio_idle` cases mix 64 tracks of which
//! only 4 sound at a time ([`staggered`]), against 4 tracks that always do:
//! the buses no clip reaches should cost next to nothing.

use std::{hint::black_box, time::Instant};

use alpha2_audio::TransportState;
use alpha2_engine::{AudioContinuity, AudioRenderer, EngineProject};

#[path = "support/audio.rs"]
mod audio;

use alpha2_model::{CubicBezier, Interpolation};
use audio::{SECONDS, Sine, automated, lengthened, project, retimed, staggered};

fn main() {
    let only = std::env::args().skip(1).find(|arg| !arg.starts_with("--"));
    let runs = |name: &str| only.as_deref().is_none_or(|only| name.contains(only));
    if runs("audio_mix") {
        mix_cases();
    }
    if runs("audio_idle") {
        for effects in [false, true] {
            for block in [4_096, 128] {
                for tracks in [4, 64] {
                    idle(tracks, effects, block);
                }
            }
        }
    }
    if runs("audio_seek") {
        for (clips, effects, one_reverb) in [
            (16, false, false),
            (16, false, true),
            (16, true, false),
            (64, true, false),
        ] {
            seek(clips, effects, one_reverb);
        }
    }
    if runs("audio_nudge") {
        for clips in [16, 64] {
            for block in [4_096, 1_024] {
                for preview in [false, true] {
                    nudge(clips, block, preview);
                }
            }
        }
    }
    for clips in [16, 64] {
        for effects in [false, true] {
            for hold in [false, true] {
                let name = format!(
                    "audio_hold_{}_{clips}_{}",
                    if hold { "on" } else { "off" },
                    if effects { "fx" } else { "dry" }
                );
                if runs(&name) {
                    held(clips, effects, hold);
                }
            }
        }
    }
}

fn mix_cases() {
    for clips in [16, 64] {
        for rate in [48_000, 44_100] {
            for effects in [false, true] {
                for block in [4_096, 128] {
                    run(clips, rate, effects, block, None);
                }
            }
        }
    }
    for retime in [0, 1] {
        for block in [4_096, 128] {
            run(16, 48_000, false, block, Some(retime));
        }
    }
    // Keyframed gain and pan on every clip: linear, then CSS `ease`, whose cubic Bezier solve
    // runs at each control point (every 32 samples).
    for (name, curve) in [
        ("linear", Interpolation::Linear),
        (
            "bezier",
            Interpolation::CubicBezier(CubicBezier::new(0.25, 0.1, 0.25, 1.0)),
        ),
    ] {
        for block in [4_096, 128] {
            run_automated(16, block, name, curve);
        }
    }
}

/// Nudges the playhead one sample forward deep inside clips with effects,
/// time after time, as `seek` does with its jumps. With `preview` the
/// renderer continues small steps ([`AudioContinuity::ContinueSmallGaps`]).
/// It also times as many blocks of uninterrupted playback after them, the
/// least a nudge can cost.
fn nudge(clips: usize, block: usize, preview: bool) {
    const RATE: u32 = 48_000;
    const SECONDS_LONG: i64 = 40;
    const NUDGES: usize = 8;
    let project = lengthened(project(clips, RATE, true), SECONDS_LONG);
    let snapshot = EngineProject::new(project).expect("valid project");
    let provider = Sine::lasting(RATE, SECONDS_LONG + 1);
    let mut renderer = AudioRenderer::new(&snapshot, 2, block).expect("renderer");
    if preview {
        renderer.set_continuity(AudioContinuity::ContinueSmallGaps);
    }
    let frames = i64::try_from(block).expect("small");
    let mut start = 30 * i64::from(RATE);
    // A seek there, then the measured nudges, then the measured blocks.
    renderer
        .render(&snapshot, start, block, &provider, TransportState::Playing)
        .expect("render");
    let mut timed = |step: i64| {
        let started = Instant::now();
        for _ in 0..NUDGES {
            start += frames + step;
            let status = renderer
                .render(&snapshot, start, block, &provider, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete(), "every source is supplied");
            black_box(renderer.channel(0));
        }
        #[allow(clippy::cast_precision_loss)]
        let each = started.elapsed().as_secs_f64() / NUDGES as f64;
        each
    };
    let per_nudge = timed(1);
    let per_block = timed(0);
    #[allow(clippy::cast_precision_loss)]
    let history = renderer.required_history_samples() as f64 / f64::from(RATE);
    println!(
        "audio_nudge clips={clips} rate={RATE} effects=on block={block} policy={} \
         history_s={history:.2} ms_per_nudge={:.3} ms_per_block={:.3}",
        if preview {
            "continue-small-gaps"
        } else {
            "exact"
        },
        per_nudge * 1e3,
        per_block * 1e3,
    );
}

/// Plays the clips from the start in blocks, then seeks back and forth deep
/// inside them as `seek` does, with or without holding incomplete blocks
/// ([`AudioRenderer::set_hold_incomplete_blocks`]). Every source is
/// supplied, so a held render mixes what one that does not hold mixes.
fn held(clips: usize, effects: bool, hold: bool) {
    const RATE: u32 = 48_000;
    const BLOCK: usize = 4_096;
    const SECONDS_LONG: i64 = 40;
    const SEEKS: usize = 8;
    let snapshot = EngineProject::new(lengthened(project(clips, RATE, effects), SECONDS_LONG))
        .expect("valid project");
    let provider = Sine::lasting(RATE, SECONDS_LONG + 1);
    let mut renderer = AudioRenderer::new(&snapshot, 2, BLOCK).expect("renderer");
    renderer.set_hold_incomplete_blocks(hold);
    let mut render = |start: i64| {
        let status = renderer
            .render(&snapshot, start, BLOCK, &provider, TransportState::Playing)
            .expect("render");
        assert!(status.is_complete(), "every source is supplied");
        black_box(renderer.channel(0));
    };
    let blocks = usize::try_from(SECONDS).expect("small") * RATE as usize / BLOCK;
    let frames = i64::try_from(BLOCK).expect("small");
    let mut per_block = 0.0;
    // One pass to warm caches and processor state, then the measured one.
    for _ in 0..2 {
        let started = Instant::now();
        for index in 0..blocks {
            render(i64::try_from(index).expect("small") * frames);
        }
        #[allow(clippy::cast_precision_loss)]
        let each = started.elapsed().as_secs_f64() / blocks as f64;
        per_block = each;
    }
    let rate = i64::from(RATE);
    let targets = [30 * rate, 35 * rate];
    render(targets[1]);
    let started = Instant::now();
    for index in 0..SEEKS {
        render(targets[index % 2]);
    }
    #[allow(clippy::cast_precision_loss)]
    let per_seek = started.elapsed().as_secs_f64() / SEEKS as f64;
    println!(
        "audio_hold clips={clips} rate={RATE} effects={} block={BLOCK} hold={} \
         us_per_block={:.2} ms_per_seek={:.2}",
        if effects { "on" } else { "off" },
        if hold { "on" } else { "off" },
        per_block * 1e6,
        per_seek * 1e3,
    );
}

/// Seeks back and forth deep inside clips longer than any history, so every
/// seek replays the whole history over audible clips. With `one_reverb`, the
/// first track alone carries a long reverb.
fn seek(clips: usize, effects: bool, one_reverb: bool) {
    const RATE: u32 = 48_000;
    const BLOCK: usize = 4_096;
    const SECONDS_LONG: i64 = 40;
    const SEEKS: usize = 8;
    let mut project = lengthened(project(clips, RATE, effects), SECONDS_LONG);
    if one_reverb {
        project.timeline.tracks[0]
            .audio
            .effects
            .push(alpha2_model::AudioEffect::Reverb {
                room_size: 0.8,
                damping: 0.3,
                mix: 0.3,
            });
    }
    let snapshot = EngineProject::new(project).expect("valid project");
    let provider = Sine::lasting(RATE, SECONDS_LONG + 1);
    let mut renderer = AudioRenderer::new(&snapshot, 2, BLOCK).expect("renderer");
    let rate = i64::from(RATE);
    let targets = [30 * rate, 35 * rate];
    // One seek to warm caches, then the measured ones.
    renderer
        .render(
            &snapshot,
            targets[1],
            BLOCK,
            &provider,
            TransportState::Playing,
        )
        .expect("render");
    let started = Instant::now();
    for index in 0..SEEKS {
        let status = renderer
            .render(
                &snapshot,
                targets[index % 2],
                BLOCK,
                &provider,
                TransportState::Playing,
            )
            .expect("render");
        assert!(status.is_complete(), "every source is supplied");
        black_box(renderer.channel(0));
    }
    #[allow(clippy::cast_precision_loss)]
    let per_seek = started.elapsed().as_secs_f64() / SEEKS as f64;
    #[allow(clippy::cast_precision_loss)]
    let history = renderer.required_history_samples() as f64 / f64::from(RATE);
    println!(
        "audio_seek clips={clips} rate={RATE} effects={} block={BLOCK} \
         history_s={history:.2} ms_per_seek={:.2}",
        match (effects, one_reverb) {
            (true, _) => "on",
            (false, true) => "one-reverb",
            (false, false) => "off",
        },
        per_seek * 1e3,
    );
}

/// With `retime`, every clip plays at 1.25 with that many ticks more of
/// source ([`retimed`]).
fn run(clips: usize, rate: u32, effects: bool, block: usize, retime: Option<i64>) {
    let mut project = project(clips, rate, effects);
    if let Some(extra_ticks) = retime {
        project = retimed(project, extra_ticks);
    }
    let snapshot = EngineProject::new(project).expect("valid project");
    let provider = Sine::new(rate);
    let mut renderer = AudioRenderer::new(&snapshot, 2, block).expect("renderer");
    let total = usize::try_from(SECONDS).expect("small") * rate as usize;
    let blocks = total / block;
    // One pass to warm caches and processor state, then the measured one.
    for pass in 0..2 {
        renderer.reset();
        let started = Instant::now();
        for index in 0..blocks {
            let start = i64::try_from(index * block).expect("small");
            let status = renderer
                .render(&snapshot, start, block, &provider, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete(), "every source is supplied");
            black_box(renderer.channel(0));
        }
        if pass == 1 {
            let elapsed = started.elapsed();
            #[allow(clippy::cast_precision_loss)]
            let per_block = elapsed.as_secs_f64() / blocks as f64;
            #[allow(clippy::cast_precision_loss)]
            let realtime = per_block / (block as f64 / f64::from(rate)) * 100.0;
            println!(
                "audio_mix clips={clips} rate={rate} effects={} block={block} speed={} \
                 us_per_block={:.2} realtime_pct={realtime:.3}",
                if effects { "on" } else { "off" },
                match retime {
                    None => "1",
                    Some(0) => "1.25",
                    Some(_) => "1.25+1tick",
                },
                per_block * 1e6,
            );
        }
    }
}

/// Like [`run`] at 48 kHz without effects, with every clip's gain and pan
/// keyframed with `curve`.
fn run_automated(clips: usize, block: usize, name: &str, curve: Interpolation) {
    let rate = 48_000;
    let snapshot =
        EngineProject::new(automated(project(clips, rate, false), curve)).expect("valid project");
    let provider = Sine::new(rate);
    let mut renderer = AudioRenderer::new(&snapshot, 2, block).expect("renderer");
    let total = usize::try_from(SECONDS).expect("small") * rate as usize;
    let blocks = total / block;
    for pass in 0..2 {
        renderer.reset();
        let started = Instant::now();
        for index in 0..blocks {
            let start = i64::try_from(index * block).expect("small");
            let status = renderer
                .render(&snapshot, start, block, &provider, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete(), "every source is supplied");
            black_box(renderer.channel(0));
        }
        if pass == 1 {
            #[allow(clippy::cast_precision_loss)]
            let per_block = started.elapsed().as_secs_f64() / blocks as f64;
            #[allow(clippy::cast_precision_loss)]
            let realtime = per_block / (block as f64 / f64::from(rate)) * 100.0;
            println!(
                "audio_mix clips={clips} rate={rate} effects=off block={block} automation={name} \
                 us_per_block={:.2} realtime_pct={realtime:.3}",
                per_block * 1e6,
            );
        }
    }
}

/// Like [`run`] at 48 kHz with `tracks` tracks, 4 of them sounding at a time
/// ([`staggered`]); with effects, every track's compressor runs throughout.
fn idle(tracks: usize, effects: bool, block: usize) {
    const ACTIVE: usize = 4;
    let rate = 48_000;
    let snapshot = EngineProject::new(staggered(project(tracks, rate, effects), ACTIVE))
        .expect("valid project");
    let provider = Sine::new(rate);
    let mut renderer = AudioRenderer::new(&snapshot, 2, block).expect("renderer");
    let total = usize::try_from(SECONDS).expect("small") * rate as usize;
    let blocks = total / block;
    for pass in 0..2 {
        renderer.reset();
        let started = Instant::now();
        for index in 0..blocks {
            let start = i64::try_from(index * block).expect("small");
            let status = renderer
                .render(&snapshot, start, block, &provider, TransportState::Playing)
                .expect("render");
            assert!(status.is_complete(), "every source is supplied");
            black_box(renderer.channel(0));
        }
        if pass == 1 {
            #[allow(clippy::cast_precision_loss)]
            let per_block = started.elapsed().as_secs_f64() / blocks as f64;
            println!(
                "audio_idle tracks={tracks} active={ACTIVE} rate={rate} effects={} block={block} \
                 us_per_block={:.2}",
                if effects { "on" } else { "off" },
                per_block * 1e6,
            );
        }
    }
}
