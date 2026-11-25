//! `alpha2.scenes`: the cuts between shots, as instants, and the stretches of black in a video.
//!
//! A frame's picture is a 4x4 grid of luma histograms; the distance between two frames is the mean
//! over the cells of the earth mover's distance between their histograms, in luma from 0 to 1, so
//! it never exceeds the mean change of a pixel and a fade or a dissolve moves it a little a frame.
//! A frame starts a new shot when its distance to the frame before is above `max(0.03, 3 x the
//! median of the 15 distances before it)` times `(3 - 2 sensitivity) / (1 + 2 sensitivity)`, at
//! least twice every other distance within 3 frames on either side, and at least `min_scene_ms`
//! after the previous cut; its `score` is `1 - threshold / distance`. Frames with a mean luma under
//! `black_level` are black; runs of at least `min_black_ms` are stretches, scored by the mean of
//! `1 - mean luma / black_level`. A param the descriptor does not declare or bound fails `begin`.

use std::collections::{BTreeSet, VecDeque};

use alpha2_plugin_sdk::{
    Context, Direction, Emitter, Error, Extractor, Frame, Host, NumberParam, TICKS_PER_SECOND,
    VideoFormat,
};

#[expect(clippy::cast_precision_loss, reason = "120 is exact")]
const TICKS_PER_MS: f64 = (TICKS_PER_SECOND / 1_000) as f64;
const GRID: usize = 4;
const CELLS: usize = GRID * GRID;
const LEVELS: usize = 256;
const DISTANCE_FLOOR: f64 = 0.03;
const MEDIAN_RATIO: f64 = 3.0;
const MEDIAN_WINDOW: usize = 15;
const SPIKE_RATIO: f64 = 2.0;
const NEIGHBORS: usize = 3;

const PARAMS: [NumberParam; 4] = [
    NumberParam {
        key: "sensitivity",
        default: 0.5,
        min: 0.0,
        max: 1.0,
    },
    NumberParam {
        key: "min_scene_ms",
        default: 500.0,
        min: 0.0,
        max: 60_000.0,
    },
    NumberParam {
        key: "black_level",
        default: 0.06,
        min: 0.01,
        max: 0.5,
    },
    NumberParam {
        key: "min_black_ms",
        default: 500.0,
        min: 0.0,
        max: 60_000.0,
    },
];

#[expect(
    clippy::cast_possible_truncation,
    reason = "the params are bounded to a minute"
)]
fn ms_to_ticks(ms: f64) -> i64 {
    (ms * TICKS_PER_MS).round() as i64
}

struct Picture {
    at_or_below: Box<[[u32; LEVELS]; CELLS]>,
    pixels: [u32; CELLS],
    mean_luma: f64,
}

impl Picture {
    fn of(frame: &Frame<'_>) -> Result<Self, Error> {
        if frame.format != VideoFormat::Luma8 {
            return Err("the frame is not luma8".into());
        }
        let (Ok(width), Ok(height)) = (usize::try_from(frame.width), usize::try_from(frame.height))
        else {
            return Err("the frame is too large".into());
        };
        if width == 0 || height == 0 || width.checked_mul(height) != Some(frame.data.len()) {
            return Err(Error(format!(
                "a {width}x{height} luma8 frame of {} bytes",
                frame.data.len()
            )));
        }
        let mut at_or_below = Box::new([[0_u32; LEVELS]; CELLS]);
        let mut pixels = [0_u32; CELLS];
        let mut sum = 0_u64;
        let columns = (0..width).map(|x| x * GRID / width).collect::<Vec<_>>();
        for (y, row) in frame.data.chunks_exact(width).enumerate() {
            let band = y * GRID / height * GRID;
            for (&value, &column) in row.iter().zip(&columns) {
                at_or_below[band + column][usize::from(value)] += 1;
                pixels[band + column] += 1;
                sum += u64::from(value);
            }
        }
        for histogram in &mut *at_or_below {
            for level in 1..LEVELS {
                histogram[level] += histogram[level - 1];
            }
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a frame has far fewer than 2^52 pixels"
        )]
        let mean_luma = sum as f64 / (frame.data.len() as f64 * 255.0);
        Ok(Self {
            at_or_below,
            pixels,
            mean_luma,
        })
    }

    fn distance(&self, other: &Self) -> f64 {
        let mut total = 0.0;
        let mut cells = 0_u32;
        for cell in 0..CELLS {
            let (own, theirs) = (
                u128::from(self.pixels[cell]),
                u128::from(other.pixels[cell]),
            );
            if own == 0 || theirs == 0 {
                continue;
            }
            let moved: u128 = self.at_or_below[cell][..LEVELS - 1]
                .iter()
                .zip(&other.at_or_below[cell][..LEVELS - 1])
                .map(|(&mine, &yours)| {
                    (u128::from(mine) * theirs).abs_diff(u128::from(yours) * own)
                })
                .sum();
            #[expect(
                clippy::cast_precision_loss,
                reason = "a ratio needs no more than 52 bits"
            )]
            let cell_distance = moved as f64 / ((own * theirs) as f64 * 255.0);
            total += cell_distance;
            cells += 1;
        }
        if cells == 0 {
            0.0
        } else {
            total / f64::from(cells)
        }
    }
}

struct Step {
    cut_at: i64,
    distance: f64,
}

struct BlackRun {
    start: i64,
    end: Option<i64>,
    darkness: f64,
    frames: u32,
}

struct Found {
    start: i64,
    end: i64,
    score: f32,
}

struct PassState {
    direction: Direction,
    end_of_first_frame: i64,
    previous: Option<(i64, Picture)>,
    steps: VecDeque<Step>,
    unjudged: usize,
    last_cut: Option<i64>,
    black: Option<BlackRun>,
}

impl PassState {
    fn new(direction: Direction, end_of_first_frame: i64) -> Self {
        Self {
            direction,
            end_of_first_frame,
            previous: None,
            steps: VecDeque::with_capacity(MEDIAN_WINDOW + NEIGHBORS + 2),
            unjudged: 0,
            last_cut: None,
            black: None,
        }
    }
}

/// The extractor.
pub struct Scenes {
    strictness: f64,
    min_scene: i64,
    black_level: f64,
    min_black: i64,
    range: (i64, i64),
    passes: Vec<(i64, Direction)>,
    state: PassState,
    cuts_emitted: BTreeSet<i64>,
    black_emitted: BTreeSet<(i64, i64)>,
    deferred_cuts: Vec<Found>,
    deferred_black: Vec<Found>,
}

impl Scenes {
    fn end_of_first_frame(&self, pass: usize) -> i64 {
        self.passes
            .get(pass)
            .map_or(self.range.1, |(start, _)| (start + 1).min(self.range.1))
    }

    fn judge(&self, index: usize) -> Option<Found> {
        let steps = &self.state.steps;
        let step = &steps[index];
        let mut window = steps
            .range(index.saturating_sub(MEDIAN_WINDOW)..index)
            .map(|step| step.distance)
            .collect::<Vec<_>>();
        window.sort_by(f64::total_cmp);
        let median = window.get(window.len().saturating_sub(1) / 2).copied();
        let threshold = self.strictness * DISTANCE_FLOOR.max(MEDIAN_RATIO * median.unwrap_or(0.0));
        let nearest = steps
            .range(index.saturating_sub(NEIGHBORS)..index)
            .chain(steps.range(index + 1..(index + 1 + NEIGHBORS).min(steps.len())))
            .map(|step| step.distance)
            .fold(0.0, f64::max);
        let isolated = step.distance >= SPIKE_RATIO * nearest;
        let spaced = self
            .state
            .last_cut
            .is_none_or(|last| (step.cut_at - last).abs() >= self.min_scene);
        let inside = (self.range.0..self.range.1).contains(&step.cut_at);
        #[expect(clippy::cast_possible_truncation, reason = "a score from 0 to 1")]
        (step.distance > threshold && isolated && spaced && inside).then(|| Found {
            start: step.cut_at,
            end: step.cut_at,
            score: (1.0 - threshold / step.distance) as f32,
        })
    }

    fn judge_next(&mut self) -> Option<Found> {
        let index = self.state.steps.len() - self.state.unjudged;
        self.state.unjudged -= 1;
        let found = self.judge(index);
        if let Some(cut) = &found {
            self.state.last_cut = Some(cut.start);
        }
        found
    }

    fn take_black(&mut self, end: Option<i64>) -> Option<Found> {
        let run = self.state.black.take()?;
        let (start, end) = (run.start.max(self.range.0), run.end.or(end)?);
        #[expect(clippy::cast_possible_truncation, reason = "a score from 0 to 1")]
        let score = (run.darkness / f64::from(run.frames)) as f32;
        (end - start >= self.min_black.max(1)).then_some(Found { start, end, score })
    }

    fn emit_cut(&mut self, out: &mut Emitter, cut: &Found) {
        if self.cuts_emitted.insert(cut.start) {
            out.interval("cuts", cut.start, cut.end, None, Some(cut.score));
        }
    }

    fn emit_black(&mut self, out: &mut Emitter, black: &Found) {
        if self.black_emitted.insert((black.start, black.end)) {
            out.interval("black", black.start, black.end, None, Some(black.score));
        }
    }

    fn close_pass(&mut self) {
        while self.state.unjudged > 0 {
            if let Some(found) = self.judge_next() {
                self.deferred_cuts.push(found);
            }
        }
        let end = match self.state.direction {
            Direction::Forward => Some(self.range.1),
            Direction::Backward => None,
        };
        if let Some(found) = self.take_black(end) {
            self.deferred_black.push(found);
        }
    }
}

impl Extractor for Scenes {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, _host: &Host) -> Result<Self, Error> {
        let [sensitivity, min_scene_ms, black_level, min_black_ms] =
            context.number_params("alpha2.scenes", &PARAMS)?;
        let passes = context
            .passes
            .iter()
            .map(|pass| (pass.start, pass.direction))
            .collect::<Vec<_>>();
        let mut scenes = Self {
            strictness: (3.0 - 2.0 * sensitivity) / (1.0 + 2.0 * sensitivity),
            min_scene: ms_to_ticks(min_scene_ms),
            black_level,
            min_black: ms_to_ticks(min_black_ms),
            range: (context.range.start, context.range.end),
            passes,
            state: PassState::new(Direction::Forward, context.range.end),
            cuts_emitted: BTreeSet::new(),
            black_emitted: BTreeSet::new(),
            deferred_cuts: Vec::new(),
            deferred_black: Vec::new(),
        };
        let direction = scenes
            .passes
            .first()
            .map_or(Direction::Forward, |(_, direction)| *direction);
        scenes.state = PassState::new(direction, scenes.end_of_first_frame(0));
        Ok(scenes)
    }

    fn pass(&mut self, pass: u32, direction: Direction) -> Result<(), Error> {
        self.close_pass();
        let end_of_first_frame =
            self.end_of_first_frame(usize::try_from(pass).unwrap_or(usize::MAX));
        self.state = PassState::new(direction, end_of_first_frame);
        Ok(())
    }

    fn video(&mut self, frame: &Frame<'_>, out: &mut Emitter) -> Result<(), Error> {
        let picture = Picture::of(frame)?;
        let forward = self.state.direction == Direction::Forward;
        let previous_time = self.state.previous.as_ref().map(|(time, _)| *time);

        if picture.mean_luma < self.black_level {
            let darkness = 1.0 - picture.mean_luma / self.black_level;
            match &mut self.state.black {
                Some(run) => {
                    if !forward {
                        run.start = frame.time;
                    }
                    run.darkness += darkness;
                    run.frames += 1;
                }
                None => {
                    self.state.black = Some(BlackRun {
                        start: frame.time,
                        end: (!forward)
                            .then_some(previous_time.unwrap_or(self.state.end_of_first_frame)),
                        darkness,
                        frames: 1,
                    });
                }
            }
        } else if let Some(found) = self.take_black(Some(frame.time)) {
            self.emit_black(out, &found);
        }

        if let Some((time, before)) = self.state.previous.take() {
            if self.state.steps.len() == MEDIAN_WINDOW + 1 + NEIGHBORS {
                self.state.steps.pop_front();
            }
            self.state.steps.push_back(Step {
                cut_at: if forward { frame.time } else { time },
                distance: before.distance(&picture),
            });
            self.state.unjudged += 1;
            if self.state.unjudged > NEIGHBORS
                && let Some(found) = self.judge_next()
            {
                self.emit_cut(out, &found);
            }
        }
        self.state.previous = Some((frame.time, picture));
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        self.close_pass();
        for found in std::mem::take(&mut self.deferred_cuts) {
            self.emit_cut(out, &found);
        }
        for found in std::mem::take(&mut self.deferred_black) {
            self.emit_black(out, &found);
        }
        Ok(())
    }
}

alpha2_plugin_sdk::export_extractor!(Scenes);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_declares_the_defaults_and_bounds_the_plugin_uses() {
        let descriptor: serde_json::Value =
            serde_json::from_str(<Scenes as Extractor>::DESCRIPTOR).expect("descriptor");
        for param in PARAMS {
            let schema = &descriptor["params_schema"]["properties"][param.key];
            assert_eq!(schema["default"].as_f64(), Some(param.default), "{param:?}");
            assert_eq!(schema["minimum"].as_f64(), Some(param.min), "{param:?}");
            assert_eq!(schema["maximum"].as_f64(), Some(param.max), "{param:?}");
        }
    }

    fn flat(level: u8) -> Picture {
        let data = vec![level; 16 * 9];
        Picture::of(&Frame {
            time: 0,
            width: 16,
            height: 9,
            format: VideoFormat::Luma8,
            data: &data,
        })
        .expect("picture")
    }

    #[test]
    fn the_distance_is_the_mean_luma_shift_of_flat_frames() {
        assert!((flat(0).distance(&flat(255)) - 1.0).abs() < 1e-12);
        assert!((flat(100).distance(&flat(151)) - 51.0 / 255.0).abs() < 1e-12);
        assert!(flat(80).distance(&flat(80)).abs() < 1e-12);
    }
}
