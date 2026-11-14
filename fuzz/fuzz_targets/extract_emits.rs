#![no_main]

use alpha2_engine::extract::{
    AudioChannels, AudioInput, Descriptor, Direction, Emitter, ExtractRequest, ExtractSession,
    Extractor, ExtractorError, FrameFrom, Need, PassInfo, RunContext, SuppliedAudio, SuppliedFrame,
    Supplier, VideoInput, delivered_size, drive, emit_from_json,
};
use alpha2_time::MediaTime;
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};

/// Emits the next batch at each frame and the rest at the end.
struct Batches {
    descriptor: Descriptor,
    batches: std::vec::IntoIter<Value>,
}

impl Batches {
    fn emit(&mut self, out: &mut Emitter) {
        let Some(Value::Array(batch)) = self.batches.next() else {
            return;
        };
        for value in &batch {
            if let Ok(emit) = emit_from_json(value) {
                out.push(emit);
            }
        }
    }
}

impl Extractor for Batches {
    fn describe(&self) -> &Descriptor {
        &self.descriptor
    }

    fn begin(&mut self, _context: &RunContext) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn pass(&mut self, _pass: &PassInfo) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn video(&mut self, _frame: &VideoInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        self.emit(out);
        Ok(())
    }

    fn audio(&mut self, _block: &AudioInput, out: &mut Emitter) -> Result<(), ExtractorError> {
        self.emit(out);
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError> {
        while self.batches.len() > 0 {
            self.emit(out);
        }
        Ok(())
    }
}

/// Steps a run may ask for before the fuzz input is dropped, so a long range stays fast.
const MAX_NEEDS: usize = 256;
/// Source `v` has a frame every this many ticks, up to its fingerprinted duration.
const FRAME_TICKS: i64 = 4_000;
const DURATION: i64 = 12_000;

/// Grey frames of the size each need asks for, and silent audio.
struct Frames {
    needs: usize,
}

impl Supplier for Frames {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        self.needs += 1;
        if self.needs > MAX_NEEDS {
            return Err("enough steps".to_owned());
        }
        let (pts, size) = match need {
            Need::TimelineFrame { time, size, .. } => (Some(*time), *size),
            Need::SourceFrame {
                from,
                direction,
                size,
                ..
            } => {
                let pts = match from {
                    FrameFrom::Start(time) => {
                        Some(time.ticks().clamp(0, DURATION - 1) / FRAME_TICKS * FRAME_TICKS)
                    }
                    FrameFrom::After(at) => Some(match direction {
                        Direction::Forward => at.ticks().saturating_add(FRAME_TICKS),
                        Direction::Backward => at.ticks().saturating_sub(FRAME_TICKS),
                    })
                    .filter(|pts| (0..DURATION).contains(pts)),
                };
                (pts.map(MediaTime), *size)
            }
            _ => return Err("not a frame".to_owned()),
        };
        let (width, height) = delivered_size((32, 18), size);
        Ok(pts.map(|pts| SuppliedFrame {
            pts,
            width,
            height,
            rgba: vec![128; (width * height * 4) as usize],
        }))
    }

    fn audio(&mut self, need: &Need) -> Result<SuppliedAudio, String> {
        self.needs += 1;
        if self.needs > MAX_NEEDS {
            return Err("enough steps".to_owned());
        }
        let (Need::SourceAudio {
            frames, channels, ..
        }
        | Need::TimelineAudio {
            frames, channels, ..
        }) = need
        else {
            return Err("not audio".to_owned());
        };
        let channels = if *channels == AudioChannels::Mono {
            1
        } else {
            2
        };
        Ok(SuppliedAudio {
            channels,
            samples: vec![0.0; *frames as usize * channels as usize],
        })
    }
}

const OUTPUTS: &str = r#"[
    { "name": "p", "kind": "points", "channels": ["a"] },
    { "name": "s", "kind": "scalars", "channels": ["x", "y"], "interpolation": "linear" },
    { "name": "m", "kind": "similarity" },
    { "name": "w", "kind": "words" },
    { "name": "i", "kind": "intervals" }
]"#;

// Any request input and any emits, as JSON batches per delivery, are refused or recorded: the
// session never panics, and the data sets it finishes with always validate in the project. The
// input is the batches alone (a timeline run over 12000 ticks), or
// `{ "input": <request input>, "audio": <bool>, "batches": [...] }`.
fuzz_target!(|data: &[u8]| {
    let (input, audio, batches) = match serde_json::from_slice::<Value>(data) {
        Ok(Value::Array(batches)) => (
            json!({ "from": "timeline", "range": { "start": 0, "end": 12_000 }, "size": 16 }),
            false,
            batches,
        ),
        Ok(Value::Object(mut fields)) => {
            let Some(Value::Array(batches)) = fields.remove("batches") else {
                return;
            };
            let audio = fields
                .get("audio")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (
                fields.remove("input").unwrap_or(Value::Null),
                audio,
                batches,
            )
        }
        _ => return,
    };
    let base = json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 32, "height": 18, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [{
            "id": "v", "kind": "video", "locator": "v.mp4", "has_audio": true,
            "fingerprint": { "duration_ticks": DURATION }
        }],
        "timeline": { "tracks": [] }
    });
    let project = alpha2_model::Project::from_json(&base.to_string()).expect("project");
    let media = if audio {
        json!({ "media": "audio", "from": ["source", "timeline"], "format": "f32", "block_frames": 64 })
    } else {
        json!({ "media": "video", "from": ["source", "timeline"], "format": "luma8", "max_size": 16 })
    };
    let descriptor: Descriptor = serde_json::from_value(json!({
        "id": "fuzz.emits", "version": 1, "input": media,
        "outputs": serde_json::from_str::<Value>(OUTPUTS).expect("outputs")
    }))
    .expect("descriptor");
    let Ok(request) = serde_json::from_value::<ExtractRequest>(json!({
        "extractor": "fuzz.emits", "version": 1, "input": input,
        "outputs": { "p": "points", "s": "scalars", "m": "similarity", "w": "words", "i": "intervals" }
    })) else {
        return;
    };
    let Ok(session) = ExtractSession::new(&project, &descriptor, request) else {
        return;
    };
    let mut extractor = Batches {
        descriptor,
        batches: batches.into_iter(),
    };
    let Ok(sets) = drive(session, &mut extractor, &mut Frames { needs: 0 }) else {
        return;
    };
    let mut document = base;
    document["data"] = serde_json::to_value(&sets).expect("data sets serialize");
    if let Err(error) = alpha2_model::Project::from_json(&document.to_string()) {
        panic!("finished data sets do not validate: {error}");
    }
});
