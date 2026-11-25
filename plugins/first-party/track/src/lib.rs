//! `alpha2.track`: follows the box `region` picks on the first frame of each pass through the
//! frames the pass delivers, emitting its centre (`center`) and its motion from the picked frame
//! (`motion`) on every frame.
//!
//! Shi-Tomasi corners inside the box are followed by pyramidal Lucas-Kanade, dropped when tracking
//! them back misses their start, and a similarity from the picked frame is fitted to them by RANSAC
//! with a fixed-seed sampler. Corners are re-seeded inside the box when too few remain. A frame is
//! lost when the box, carried back by that similarity, correlates with the picked box below
//! `confidence`; the box is then searched for around where it was last seen until it is found.
//! A time a previous pass emitted is not emitted again.

mod appearance;
mod corners;
mod fit;
mod flow;
mod image;

use std::collections::BTreeSet;

use alpha2_plugin_sdk::{Context, Direction, Emitter, Error, Extractor, Frame, Host, VideoFormat};

use appearance::Template;
use corners::Area;
use fit::{MIN_INLIERS, Similarity};
use image::{Plane, Pyramid};

const ID: &str = "alpha2.track";
const MIN_BOX: f64 = 0.04;
/// `region`'s fields with the inclusive bounds `descriptor.json` declares.
const REGION_FIELDS: [(&str, f64, f64); 4] = [
    ("x", 0.0, 1.0),
    ("y", 0.0, 1.0),
    ("width", MIN_BOX, 1.0),
    ("height", MIN_BOX, 1.0),
];
/// `confidence` with its default and the inclusive bounds `descriptor.json` declares.
const CONFIDENCE: (&str, f64, f64, f64) = ("confidence", 0.5, 0.0, 1.0);
const EDGE_ROUNDING: f64 = 1e-9;

const MAX_FEATURES: usize = 64;
const REFILL_BELOW: usize = 32;
const FORWARD_BACKWARD_PIXELS: f32 = 1.0;
const SEED_MARGIN_PIXELS: f64 = 3.0;
const MIN_SPACING_PIXELS: f64 = 4.0;
const MAX_SCALE_CHANGE_PER_FRAME: f64 = 2.0;
const SEARCH_RADIUS_BOXES: f64 = 1.0;
const SEARCH_GROWTH_BOXES_PER_LOST_FRAME: f64 = 0.5;
const SEARCH_MAX_BOXES: f64 = 4.0;

const MAX_DATA_COORDINATE: f32 = 16.0;
const MAX_DATA_ROTATION_DEGREES: f32 = 36_000.0;
const MAX_DATA_SCALE: f32 = 1_000.0;

#[derive(Clone, Copy, Debug)]
struct Region {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn bounded(value: Option<&serde_json::Value>, key: &str, min: f64, max: f64) -> Result<f64, Error> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|number| (min..=max).contains(number))
        .ok_or_else(|| Error(format!("params.{key} must be a number from {min} to {max}")))
}

fn region(value: Option<&serde_json::Value>) -> Result<Region, Error> {
    let shape = "params.region must be an object with x, y, width and height";
    let fields = value
        .ok_or_else(|| Error(format!("{shape}; it is required")))?
        .as_object()
        .ok_or_else(|| Error(shape.to_owned()))?;
    if let Some(key) = fields
        .keys()
        .find(|key| REGION_FIELDS.iter().all(|(name, ..)| name != key))
    {
        return Err(Error(format!(
            "params.region.{key} is not a field: {shape}"
        )));
    }
    let mut values = [0.0; 4];
    for (value, (key, min, max)) in values.iter_mut().zip(REGION_FIELDS) {
        *value = bounded(fields.get(key), &format!("region.{key}"), min, max)?;
    }
    let [x, y, width, height] = values;
    if x + width > 1.0 + EDGE_ROUNDING || y + height > 1.0 + EDGE_ROUNDING {
        return Err(Error(
            "params.region must lie inside the frame: x + width and y + height at most 1"
                .to_owned(),
        ));
    }
    Ok(Region {
        x,
        y,
        width,
        height,
    })
}

fn params(context: &Context) -> Result<(Region, f64), Error> {
    if let Some(key) = context
        .params
        .keys()
        .find(|key| *key != "region" && *key != CONFIDENCE.0)
    {
        return Err(Error(format!("params.{key} is not a param of {ID}")));
    }
    let region = region(context.params.get("region"))?;
    let (key, default, min, max) = CONFIDENCE;
    let confidence = match context.params.get(key) {
        None => default,
        given => bounded(given, key, min, max)?,
    };
    Ok((region, confidence))
}

struct Feature {
    at: (f32, f32),
    origin: (f64, f64),
}

fn widen((x, y): (f32, f32)) -> (f64, f64) {
    (f64::from(x), f64::from(y))
}

struct PassState {
    width: usize,
    height: usize,
    template: Template,
    previous: Pyramid,
    features: Vec<Feature>,
    motion: Similarity,
    degrees: f64,
    lost_frames: u32,
    frames: u64,
}

impl PassState {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "frame sizes and pixel positions far inside every float and integer type"
    )]
    fn seed(&mut self, picture: &Pyramid) {
        if self.features.len() >= MAX_FEATURES {
            return;
        }
        let corners = self
            .template
            .corners()
            .map(|corner| self.motion.apply(corner));
        let (mut left, mut top, mut right, mut bottom) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in corners {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
        let width = self.width as f64;
        let height = self.height as f64;
        let area = Area {
            left: left.ceil().max(SEED_MARGIN_PIXELS).min(width) as usize,
            top: top.ceil().max(SEED_MARGIN_PIXELS).min(height) as usize,
            right: (right.floor() + 1.0)
                .min(width - SEED_MARGIN_PIXELS)
                .max(0.0) as usize,
            bottom: (bottom.floor() + 1.0)
                .min(height - SEED_MARGIN_PIXELS)
                .max(0.0) as usize,
        };
        let scale = self.motion.scale();
        let spacing = (self.template.width * self.template.height * scale * scale
            / (2 * MAX_FEATURES) as f64)
            .sqrt()
            .max(MIN_SPACING_PIXELS);
        let inverse = self.motion.inverse();
        let template = &self.template;
        let taken = self
            .features
            .iter()
            .map(|feature| feature.at)
            .collect::<Vec<_>>();
        let found = corners::strongest(
            picture.base(),
            area,
            |x, y| template.holds(inverse.apply(widen((x, y)))),
            spacing as f32,
            &taken,
            MAX_FEATURES - self.features.len(),
        );
        self.features.extend(found.into_iter().map(|at| Feature {
            at,
            origin: inverse.apply(widen(at)),
        }));
    }

    fn follow_features(&mut self, picture: &Pyramid) -> Option<Similarity> {
        if self.features.len() < MIN_INLIERS {
            self.features.clear();
            return None;
        }
        let mut kept = Vec::new();
        let mut pairs = Vec::new();
        for feature in &self.features {
            let Some(to) = flow::track(&self.previous, picture, feature.at) else {
                continue;
            };
            let Some(back) = flow::track(picture, &self.previous, to) else {
                continue;
            };
            let (dx, dy) = (back.0 - feature.at.0, back.1 - feature.at.1);
            if dx * dx + dy * dy <= FORWARD_BACKWARD_PIXELS * FORWARD_BACKWARD_PIXELS {
                pairs.push((feature.origin, widen(to)));
                kept.push(Feature {
                    at: to,
                    origin: feature.origin,
                });
            }
        }
        let change = |motion: &Similarity| motion.scale() / self.motion.scale();
        match fit::ransac(&pairs, self.frames) {
            Some((motion, inliers))
                if (1.0 / MAX_SCALE_CHANGE_PER_FRAME..=MAX_SCALE_CHANGE_PER_FRAME)
                    .contains(&change(&motion)) =>
            {
                self.features = kept
                    .into_iter()
                    .zip(inliers)
                    .filter_map(|(feature, inlier)| inlier.then_some(feature))
                    .collect();
                Some(motion)
            }
            _ => {
                self.features.clear();
                None
            }
        }
    }

    fn refined(&self, picture: &Pyramid, motion: Similarity) -> Similarity {
        let refined = self.template.refine(picture, &motion);
        if refined != motion
            && self.template.fit(picture, &refined) >= self.template.fit(picture, &motion)
        {
            refined
        } else {
            motion
        }
    }

    fn carry(&mut self, motion: Similarity) {
        let degrees = motion.degrees();
        self.degrees = degrees + 360.0 * ((self.degrees - degrees) / 360.0).round();
        self.motion = motion;
    }

    fn follow(&mut self, picture: &Pyramid, confidence: f32) -> bool {
        self.frames += 1;
        let estimate = self
            .follow_features(picture)
            .map(|motion| self.refined(picture, motion));
        let fit = estimate.map_or(0.0, |motion| self.template.fit(picture, &motion));
        let found = match estimate {
            Some(motion) if fit >= confidence => Some(motion),
            _ => {
                let around = estimate.unwrap_or(self.motion);
                let boxes = (SEARCH_RADIUS_BOXES
                    + SEARCH_GROWTH_BOXES_PER_LOST_FRAME * f64::from(self.lost_frames))
                .min(SEARCH_MAX_BOXES);
                let radius = boxes * self.template.extent() * around.scale();
                let (candidate, _) = self.template.search(picture, &around, radius);
                let candidate = self.refined(picture, candidate);
                let score = self.template.fit(picture, &candidate);
                if score >= confidence && (estimate.is_none() || score > fit) {
                    self.features.clear();
                    Some(candidate)
                } else {
                    if let Some(motion) = estimate {
                        self.carry(motion);
                    }
                    None
                }
            }
        };
        let Some(motion) = found else {
            self.lost_frames = self.lost_frames.saturating_add(1);
            return false;
        };
        self.carry(motion);
        self.lost_frames = 0;
        if self.features.len() < REFILL_BELOW {
            self.seed(picture);
        }
        true
    }

    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "frame sizes and pixel positions far inside every float and integer type"
    )]
    fn values(&self) -> Option<([f32; 2], [f32; 4])> {
        let (width, height) = (self.width as f64, self.height as f64);
        let centre = self.motion.apply(self.template.centre());
        let picture = (0.5 * width - 0.5, 0.5 * height - 0.5);
        let moved = self.motion.apply(picture);
        let point = [
            ((centre.0 + 0.5) / width) as f32,
            ((centre.1 + 0.5) / height) as f32,
        ];
        let motion = [
            ((moved.0 - picture.0) / width) as f32,
            ((moved.1 - picture.1) / height) as f32,
            self.degrees as f32,
            self.motion.scale() as f32,
        ];
        let inside = point
            .iter()
            .chain(&motion[..2])
            .all(|value| value.abs() <= MAX_DATA_COORDINATE)
            && motion[2].abs() <= MAX_DATA_ROTATION_DEGREES
            && motion[3] > 0.0
            && motion[3] <= MAX_DATA_SCALE;
        inside.then_some((point, motion))
    }
}

/// The extractor.
pub struct Track {
    region: Region,
    confidence: f32,
    emitted: BTreeSet<i64>,
    pass: Option<PassState>,
}

impl Track {
    fn emit(&mut self, out: &mut Emitter, time: i64, values: Option<([f32; 2], [f32; 4])>) {
        if !self.emitted.insert(time) {
            return;
        }
        if let Some((point, motion)) = values {
            out.sample("center", time, &point.map(Some));
            out.sample("motion", time, &motion.map(Some));
        } else {
            out.sample("center", time, &[None; 2]);
            out.sample("motion", time, &[None; 4]);
        }
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "frame sizes and pixel positions far inside every float and integer type"
    )]
    fn start(&self, width: usize, height: usize, pyramid: Pyramid) -> PassState {
        let (frame_width, frame_height) = (width as f64, height as f64);
        let template = Template::new(
            pyramid.clone(),
            self.region.x * frame_width - 0.5,
            self.region.y * frame_height - 0.5,
            self.region.width * frame_width,
            self.region.height * frame_height,
        );
        let mut state = PassState {
            width,
            height,
            template,
            previous: pyramid,
            features: Vec::new(),
            motion: Similarity::IDENTITY,
            degrees: 0.0,
            lost_frames: 0,
            frames: 0,
        };
        let previous = state.previous.clone();
        state.seed(&previous);
        state
    }
}

impl Extractor for Track {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, _host: &Host) -> Result<Self, Error> {
        let (region, confidence) = params(context)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "frame sizes and pixel positions far inside every float and integer type"
        )]
        let confidence = confidence as f32;
        Ok(Self {
            region,
            confidence,
            emitted: BTreeSet::new(),
            pass: None,
        })
    }

    fn pass(&mut self, _pass: u32, _direction: Direction) -> Result<(), Error> {
        self.pass = None;
        Ok(())
    }

    fn video(&mut self, frame: &Frame<'_>, out: &mut Emitter) -> Result<(), Error> {
        if frame.format != VideoFormat::Luma8 {
            return Err("frames must be luma8".into());
        }
        let width = usize::try_from(frame.width).map_err(|_| "the frame is too wide")?;
        let height = usize::try_from(frame.height).map_err(|_| "the frame is too tall")?;
        if width == 0 || height == 0 || frame.data.len() != width * height {
            return Err("the frame's bytes are not width * height".into());
        }
        let pyramid = Pyramid::new(Plane::from_luma(width, height, frame.data));
        let values = match self.pass.take() {
            None => {
                let state = self.start(width, height, pyramid);
                let values = state.values();
                self.pass = Some(state);
                values
            }
            Some(state) if (state.width, state.height) != (width, height) => {
                self.pass = Some(state);
                None
            }
            Some(mut state) => {
                let found = state.follow(&pyramid, self.confidence);
                state.previous = pyramid;
                let values = if found { state.values() } else { None };
                self.pass = Some(state);
                values
            }
        };
        self.emit(out, frame.time, values);
        Ok(())
    }
}

alpha2_plugin_sdk::export_extractor!(Track);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_declares_the_defaults_and_bounds_the_plugin_uses() {
        let descriptor: serde_json::Value =
            serde_json::from_str(<Track as Extractor>::DESCRIPTOR).expect("descriptor");
        let properties = &descriptor["params_schema"]["properties"];
        let (name, default, min, max) = CONFIDENCE;
        assert_eq!(properties[name]["default"].as_f64(), Some(default));
        assert_eq!(properties[name]["minimum"].as_f64(), Some(min));
        assert_eq!(properties[name]["maximum"].as_f64(), Some(max));
        let region = &properties["region"];
        assert_eq!(region["x-alpha2-display"], "box");
        assert_eq!(
            region["required"],
            serde_json::json!(["x", "y", "width", "height"])
        );
        for (name, min, max) in REGION_FIELDS {
            assert_eq!(
                region["properties"][name]["minimum"].as_f64(),
                Some(min),
                "{name}"
            );
            assert_eq!(
                region["properties"][name]["maximum"].as_f64(),
                Some(max),
                "{name}"
            );
        }
        assert_eq!(
            properties.as_object().map(serde_json::Map::len),
            Some(2),
            "region and confidence only"
        );
    }
}
