//! `alpha2.transcribe`: the words spoken in audio and their times, from a whisper.cpp model the
//! caller supplies in the `model` slot, run on the CPU; native only.
//!
//! whisper.cpp transcribes 30 s windows of the 16 kHz mono audio, overlapping by 5 s, each in the
//! `audio` call that completes it (the last in `end`). A word runs from the previous word's last
//! spoken token to its own, as DTW-aligned token timestamps place them, less unvoiced frames at
//! either end. Windows are joined at a word both heard near the middle of their overlap, else at
//! that middle. A forward pass emits words as joins settle them; other passes, in `end`.
//!
//! Params: `language`, `auto` (the default; the first language detected holds) or ISO 639-1.

mod model;
mod seam;

use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};

use alpha2_plugin_sdk::{Audio, Context, Direction, Emitter, Error, Extractor, Host, SlotShape};
use seam::Word;
use whisper_rs::whisper_rs_sys::ggml_log_level;
use whisper_rs::{
    DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
    WhisperState,
};

/// whisper's rate.
const RATE: i64 = 16_000;
/// whisper reads 30 s at a time.
const WINDOW: i64 = 30 * RATE;
const OVERLAP: i64 = 5 * RATE;
const STEP: i64 = WINDOW - OVERLAP;
/// whisper.cpp's timestamps count centiseconds.
const SAMPLES_PER_TIMESTAMP: i64 = RATE / 100;
const VOICE_FRAME: usize = 320;
const VOICE_FLOOR_DB: f32 = -30.0;
const SILENT_RMS: f32 = 1e-4;
/// alpha2's clock.
const TICKS_PER_SECOND: i64 = 120_000;
const SHORTEST_WORD_TICKS: i64 = TICKS_PER_SECOND / 50;
/// The most words one data set holds and the longest text a word may have, per the plugin ABI.
const MAX_WORDS: usize = 200_000;
const MAX_TEXT_BYTES: usize = 256;
const MAX_THREADS: usize = 8;
const MODEL: &str = "model";
const OUTPUT: &str = "words";
const AUTO: &str = "auto";

/// The ISO 639-1 codes whisper transcribes, sorted.
const LANGUAGES: [&str; 98] = [
    "af", "am", "ar", "as", "az", "ba", "be", "bg", "bn", "bo", "br", "bs", "ca", "cs", "cy", "da",
    "de", "el", "en", "es", "et", "eu", "fa", "fi", "fo", "fr", "gl", "gu", "ha", "he", "hi", "hr",
    "ht", "hu", "hy", "id", "is", "it", "ja", "jv", "ka", "kk", "km", "kn", "ko", "la", "lb", "ln",
    "lo", "lt", "lv", "mg", "mi", "mk", "ml", "mn", "mr", "ms", "mt", "my", "ne", "nl", "nn", "no",
    "oc", "pa", "pl", "ps", "pt", "ro", "ru", "sa", "sd", "si", "sk", "sl", "sn", "so", "sq", "sr",
    "su", "sv", "sw", "ta", "te", "tg", "th", "tk", "tl", "tr", "tt", "uk", "ur", "uz", "vi", "yi",
    "yo", "zh",
];

/// whisper's name for an ISO 639-1 code: it calls Javanese `jw`.
fn whisper_language(code: &str) -> &str {
    if code == "jv" { "jw" } else { code }
}

fn language(context: &Context) -> Result<Option<&'static str>, Error> {
    if let Some(key) = context.params.keys().find(|key| *key != "language") {
        return Err(Error(format!(
            "params.{key} is not a param of alpha2.transcribe"
        )));
    }
    match context.params.get("language") {
        None => Ok(None),
        Some(value) if value.as_str() == Some(AUTO) => Ok(None),
        Some(value) => value
            .as_str()
            .and_then(|code| LANGUAGES.iter().find(|known| **known == code))
            .map(|code| Some(whisper_language(code)))
            .ok_or_else(|| {
                Error(format!(
                    "params.language must be auto or an ISO 639-1 code whisper knows, not {value}"
                ))
            }),
    }
}

fn to_sample_ceil(ticks: i64) -> i64 {
    (ticks * RATE).div_euclid(TICKS_PER_SECOND)
        + i64::from((ticks * RATE).rem_euclid(TICKS_PER_SECOND) != 0)
}

fn to_ticks(sample: i64) -> i64 {
    (sample * TICKS_PER_SECOND).div_euclid(RATE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grid {
    first: i64,
    last: i64,
}

impl Grid {
    fn count(self) -> usize {
        if self.last <= self.first {
            return 0;
        }
        let beyond = (self.last - self.first - WINDOW).max(0);
        usize::try_from(1 + (beyond + STEP - 1) / STEP).unwrap_or(usize::MAX)
    }

    fn bounds(self, index: usize) -> (i64, i64) {
        let start = self.first + i64::try_from(index).unwrap_or(i64::MAX) * STEP;
        (start, (start + WINDOW).min(self.last))
    }

    fn touched(self, start: i64, end: i64) -> std::ops::Range<usize> {
        let count = self.count();
        if end <= start || count == 0 {
            return 0..0;
        }
        let low = (start - self.first - WINDOW).div_euclid(STEP) + 1;
        let high = (end - 1 - self.first).div_euclid(STEP) + 1;
        let low = usize::try_from(low.max(0)).unwrap_or(0).min(count);
        let high = usize::try_from(high.max(0)).unwrap_or(0).min(count);
        low..high.max(low)
    }
}

#[derive(Default)]
struct Window {
    samples: Vec<f32>,
    filled: i64,
    done: bool,
}

/// The extractor.
pub struct Transcribe {
    state: WhisperState,
    end_of_text: i32,
    threads: i32,
    language: Option<&'static str>,
    detect_language: bool,
    grid: Grid,
    windows: Vec<Window>,
    unjoined: BTreeMap<usize, Vec<Word>>,
    next_to_join: usize,
    replaceable: Vec<Word>,
    unemitted: Vec<Word>,
    forward: bool,
    range: (i64, i64),
    emitted: usize,
    emitted_end: i64,
}

unsafe extern "C" fn quiet(_: ggml_log_level, _: *const c_char, _: *mut c_void) {}

fn load(context: &Context, host: &Host, language: Option<&str>) -> Result<WhisperContext, Error> {
    let slot = context
        .supplied
        .get(MODEL)
        .ok_or("the model slot is empty; fill it with a whisper.cpp ggml model (ggml-*.bin)")?;
    if slot.shape != SlotShape::File {
        return Err("the model slot takes one file, a whisper.cpp ggml model".into());
    }
    let path = host.resource_path(MODEL).ok_or(
        "the host gives no path to the model; alpha2.transcribe needs a native host with resource_path",
    )?;
    let header = model::check(&path).map_err(|reason| {
        Error(format!(
            "the file in the model slot is not a whole whisper.cpp ggml model: {reason}"
        ))
    })?;
    if let Some(code) = language.filter(|code| *code != "en" && !header.multilingual()) {
        return Err(Error(format!(
            "params.language is {code}, but the model in the slot transcribes English only"
        )));
    }
    // SAFETY: `quiet` neither unwinds nor keeps what it is given.
    unsafe { whisper_rs::set_log_callback(Some(quiet), std::ptr::null_mut()) };
    let mut parameters = WhisperContextParameters::default();
    parameters
        .use_gpu(false)
        .flash_attn(false)
        .dtw_parameters(DtwParameters {
            mode: header.alignment(),
            ..DtwParameters::default()
        });
    WhisperContext::new_with_params(&path, parameters)
        .map_err(|error| Error(format!("whisper.cpp could not load the model: {error}")))
}

fn voiced(samples: &[f32]) -> Vec<bool> {
    let loudness: Vec<f32> = samples
        .chunks(VOICE_FRAME)
        .map(|frame| {
            #[expect(clippy::cast_precision_loss, reason = "a frame holds 320 samples")]
            let count = frame.len() as f32;
            (frame.iter().map(|value| value * value).sum::<f32>() / count).sqrt()
        })
        .collect();
    let mut sorted = loudness.clone();
    sorted.sort_by(f32::total_cmp);
    let loud = sorted.get(sorted.len() * 19 / 20).copied().unwrap_or(0.0);
    let floor = (loud * 10_f32.powf(VOICE_FLOOR_DB / 20.0)).max(SILENT_RMS);
    loudness.iter().map(|&value| value >= floor).collect()
}

fn trimmed(start: i64, end: i64, window_start: i64, voiced: &[bool]) -> (i64, i64) {
    let frame = i64::try_from(VOICE_FRAME).unwrap_or(1);
    let first = usize::try_from((start - window_start) / frame).unwrap_or(0);
    let last = usize::try_from((end - window_start + frame - 1) / frame)
        .unwrap_or(0)
        .min(voiced.len());
    let Some(on) = (first..last).find(|&at| voiced[at]) else {
        return (start, end);
    };
    let off = (first..last).rev().find(|&at| voiced[at]).unwrap_or(on);
    let sample = |at: usize| window_start + i64::try_from(at).unwrap_or(0) * frame;
    let start = start.max(sample(on));
    (start, end.min(sample(off + 1)).max(start))
}

fn truncated(text: &str) -> &str {
    let mut end = text.len().min(MAX_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl Transcribe {
    fn transcribe(&mut self, index: usize) -> Result<(), Error> {
        let (start, end) = self.grid.bounds(index);
        let window = &mut self.windows[index];
        window.done = true;
        let mut samples = std::mem::take(&mut window.samples);
        samples.resize(usize::try_from(end - start).unwrap_or(0), 0.0);
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 5 });
        params.set_n_threads(self.threads);
        params.set_language(Some(self.language.unwrap_or(AUTO)));
        params.set_no_context(true);
        params.set_token_timestamps(true);
        params.set_max_len(1);
        params.set_split_on_word(true);
        params.set_suppress_nst(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        self.state
            .full(params, &samples)
            .map_err(|error| Error(format!("whisper.cpp failed: {error}")))?;
        let voiced = voiced(&samples);
        let at = |timestamp: i64| {
            (start + timestamp.saturating_mul(SAMPLES_PER_TIMESTAMP)).clamp(start, end)
        };
        let mut words = Vec::new();
        let mut spoken_until = start;
        for segment in self.state.as_iter() {
            let tokens: Vec<(bool, i64)> = (0..segment.n_tokens())
                .filter_map(|index| segment.get_token(index))
                .filter(|token| token.token_id() < self.end_of_text)
                .map(|token| {
                    let data = token.token_data();
                    let spoken = token
                        .to_str_lossy()
                        .is_ok_and(|text| text.chars().any(char::is_alphanumeric));
                    (
                        spoken,
                        at(if data.t_dtw >= 0 { data.t_dtw } else { data.t1 }),
                    )
                })
                .collect();
            let Some(&(_, spoken_end)) = tokens.iter().rev().find(|(spoken, _)| *spoken) else {
                continue;
            };
            let text = segment
                .to_str_lossy()
                .map_err(|error| Error(format!("whisper.cpp gave no text: {error}")))?;
            let text = truncated(text.trim());
            let word_start = spoken_until;
            spoken_until = spoken_until.max(spoken_end);
            let (word_start, word_end) = trimmed(word_start, spoken_until, start, &voiced);
            words.push(Word {
                start: word_start,
                end: word_end,
                text: text.to_owned(),
            });
        }
        if self.detect_language && self.language.is_none() && !words.is_empty() {
            self.language = whisper_rs::get_lang_str(self.state.full_lang_id_from_state());
        }
        self.unjoined.insert(index, words);
        Ok(())
    }

    fn join(&mut self) {
        let count = self.windows.len();
        while let Some(words) = self.unjoined.remove(&self.next_to_join) {
            if self.next_to_join == 0 {
                self.replaceable = words;
            } else {
                let overlap = (
                    self.grid.bounds(self.next_to_join).0,
                    self.grid.bounds(self.next_to_join - 1).1,
                );
                let (keep, from) = seam::join(&self.replaceable, &words, overlap);
                self.unemitted.extend(self.replaceable.drain(..keep));
                self.replaceable = words.into_iter().skip(from).collect();
            }
            self.next_to_join += 1;
            if self.next_to_join == count {
                self.unemitted.append(&mut self.replaceable);
            }
        }
    }

    fn emit(&mut self, out: &mut Emitter) -> Result<(), Error> {
        let (range_start, range_end) = self.range;
        for word in std::mem::take(&mut self.unemitted) {
            if self.emitted == MAX_WORDS {
                return Err(Error(format!(
                    "the range holds more than {MAX_WORDS} words, the most one data set holds"
                )));
            }
            let start = to_ticks(word.start).max(range_start).max(self.emitted_end);
            let end = to_ticks(word.end)
                .max(start + SHORTEST_WORD_TICKS)
                .min(range_end);
            if end <= start {
                continue;
            }
            out.word(OUTPUT, start, end, &word.text, None);
            self.emitted += 1;
            self.emitted_end = end;
        }
        Ok(())
    }
}

impl Extractor for Transcribe {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, host: &Host) -> Result<Self, Error> {
        let language = language(context)?;
        let whisper = load(context, host, language)?;
        let detect = language.is_none() && whisper.is_multilingual();
        let language = language.or_else(|| (!detect).then_some("en"));
        let state = whisper
            .create_state()
            .map_err(|error| Error(format!("whisper.cpp has no state: {error}")))?;
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(MAX_THREADS);
        let grid = Grid {
            first: to_sample_ceil(context.range.start),
            last: to_sample_ceil(context.range.end),
        };
        Ok(Self {
            end_of_text: whisper.token_eot(),
            state,
            threads: i32::try_from(threads).unwrap_or(1),
            language,
            detect_language: detect,
            grid,
            windows: (0..grid.count()).map(|_| Window::default()).collect(),
            unjoined: BTreeMap::new(),
            next_to_join: 0,
            replaceable: Vec::new(),
            unemitted: Vec::new(),
            forward: true,
            range: (context.range.start, context.range.end),
            emitted: 0,
            emitted_end: context.range.start,
        })
    }

    fn pass(&mut self, _pass: u32, direction: Direction) -> Result<(), Error> {
        self.forward = direction == Direction::Forward;
        Ok(())
    }

    fn audio(&mut self, block: &Audio<'_>, out: &mut Emitter) -> Result<(), Error> {
        if i64::from(block.rate) != RATE || block.channels != 1 {
            return Err(Error(format!(
                "audio of {} Hz in {} channels, not 16000 Hz mono",
                block.rate, block.channels
            )));
        }
        let frames = usize::try_from(block.frames)
            .unwrap_or(0)
            .min(block.samples.len());
        let samples = &block.samples[..frames];
        let start = block.start_sample;
        let end = start + i64::try_from(frames).unwrap_or(0);
        let last = self.windows.len().saturating_sub(1);
        let at = |sample: i64| usize::try_from(sample).unwrap_or(0);
        for index in self.grid.touched(start, end) {
            let (window_start, window_end) = self.grid.bounds(index);
            let window = &mut self.windows[index];
            if window.done {
                continue;
            }
            let (from, to) = (start.max(window_start), end.min(window_end));
            if window.samples.is_empty() {
                window.samples = vec![0.0; at(window_end - window_start)];
            }
            window.samples[at(from - window_start)..at(to - window_start)]
                .copy_from_slice(&samples[at(from - start)..at(to - start)]);
            window.filled += to - from;
            if window.filled >= window_end - window_start && index != last {
                self.transcribe(index)?;
            }
        }
        self.join();
        if self.forward {
            self.emit(out)?;
        }
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        for index in 0..self.windows.len() {
            if !self.windows[index].done {
                self.transcribe(index)?;
            }
        }
        self.join();
        self.emit(out)
    }
}

alpha2_plugin_sdk::export_extractor!(Transcribe);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_offers_auto_and_every_language_the_plugin_takes() {
        let descriptor: serde_json::Value =
            serde_json::from_str(<Transcribe as Extractor>::DESCRIPTOR).expect("descriptor");
        let schema = &descriptor["params_schema"]["properties"]["language"];
        assert_eq!(schema["default"], AUTO);
        let offered: Vec<&str> = schema["enum"]
            .as_array()
            .expect("enum")
            .iter()
            .map(|code| code.as_str().expect("a code"))
            .collect();
        assert_eq!(offered[0], AUTO);
        assert_eq!(offered[1..], LANGUAGES);
    }

    #[test]
    fn whisper_knows_every_language_the_plugin_takes() {
        for code in LANGUAGES {
            assert!(
                whisper_rs::get_lang_id(whisper_language(code)).is_some(),
                "{code}"
            );
        }
        assert!(LANGUAGES.is_sorted());
    }

    #[test]
    fn windows_overlap_and_the_last_reaches_the_end_of_the_range() {
        let grid = Grid {
            first: 3,
            last: 3 + 70 * RATE,
        };
        assert_eq!(grid.count(), 3);
        assert_eq!(grid.bounds(0), (3, 3 + WINDOW));
        assert_eq!(grid.bounds(1), (3 + STEP, 3 + STEP + WINDOW));
        assert_eq!(grid.bounds(2), (3 + 2 * STEP, 3 + 70 * RATE));
        assert_eq!(Grid { first: 0, last: 1 }.count(), 1);
        assert_eq!(
            Grid {
                first: 0,
                last: WINDOW
            }
            .count(),
            1
        );
        assert_eq!(
            Grid {
                first: 0,
                last: WINDOW + 1
            }
            .count(),
            2
        );
        assert_eq!(Grid { first: 5, last: 5 }.count(), 0);
    }

    #[test]
    fn a_block_touches_the_windows_that_hold_its_samples() {
        let grid = Grid {
            first: 0,
            last: 100 * RATE,
        };
        assert_eq!(grid.touched(0, RATE), 0..1);
        assert_eq!(grid.touched(STEP - 1, STEP), 0..1);
        assert_eq!(grid.touched(STEP, STEP + 1), 0..2);
        assert_eq!(grid.touched(WINDOW - 1, WINDOW), 0..2);
        assert_eq!(grid.touched(WINDOW, WINDOW + 1), 1..2);
        assert_eq!(grid.touched(99 * RATE, 100 * RATE), 3..4);
        assert_eq!(grid.touched(STEP - 10, WINDOW + 10), 0..2);
    }

    #[test]
    fn a_long_text_is_cut_at_a_character_boundary() {
        let text = "\u{e9}".repeat(200);
        assert_eq!(truncated(&text).len(), MAX_TEXT_BYTES);
        let text = format!("a{}", "\u{e9}".repeat(200));
        assert_eq!(truncated(&text).len(), MAX_TEXT_BYTES - 1);
    }
}
