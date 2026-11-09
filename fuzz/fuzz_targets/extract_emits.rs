#![no_main]

use alpha2_engine::extract::{
    AudioInput, Descriptor, Emitter, ExtractRequest, ExtractSession, Extractor, ExtractorError,
    Need, PassInfo, RunContext, SuppliedAudio, SuppliedFrame, Supplier, VideoInput, delivered_size,
    drive, emit_from_json,
};
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

    fn audio(&mut self, _block: &AudioInput, _out: &mut Emitter) -> Result<(), ExtractorError> {
        Ok(())
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), ExtractorError> {
        while self.batches.len() > 0 {
            self.emit(out);
        }
        Ok(())
    }
}

/// Grey timeline frames of the size each need asks for.
struct Frames;

impl Supplier for Frames {
    fn frame(&mut self, need: &Need) -> Result<Option<SuppliedFrame>, String> {
        let Need::TimelineFrame { time, size, .. } = need else {
            return Err("only timeline frames".to_owned());
        };
        let (width, height) = delivered_size((32, 18), *size);
        Ok(Some(SuppliedFrame {
            pts: *time,
            width,
            height,
            rgba: vec![128; (width * height * 4) as usize],
        }))
    }

    fn audio(&mut self, _need: &Need) -> Result<SuppliedAudio, String> {
        Err("no audio".to_owned())
    }
}

// Any emits, as JSON batches per frame, are refused or recorded: the session never panics, and
// the data sets it finishes with always validate in a project.
fuzz_target!(|data: &[u8]| {
    let Ok(Value::Array(batches)) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let base = json!({
        "schema_version": 1, "id": "p", "name": "P",
        "settings": { "width": 32, "height": 18, "frame_rate": { "num": 30, "den": 1 }, "sample_rate": 48_000 },
        "sources": [], "timeline": { "tracks": [] }
    });
    let project = alpha2_model::Project::from_json(&base.to_string()).expect("project");
    let descriptor: Descriptor = serde_json::from_value(json!({
        "id": "fuzz.emits", "version": 1,
        "input": { "media": "video", "from": ["timeline"], "format": "luma8", "max_size": 16 },
        "outputs": [
            { "name": "p", "kind": "points", "channels": ["a"] },
            { "name": "s", "kind": "scalars", "channels": ["x", "y"], "interpolation": "linear" },
            { "name": "m", "kind": "similarity" },
            { "name": "w", "kind": "words" },
            { "name": "i", "kind": "intervals" }
        ]
    }))
    .expect("descriptor");
    let request: ExtractRequest = serde_json::from_value(json!({
        "extractor": "fuzz.emits", "version": 1,
        "input": { "from": "timeline", "range": { "start": 0, "end": 12_000 }, "size": 16 },
        "outputs": { "p": "points", "s": "scalars", "m": "similarity", "w": "words", "i": "intervals" }
    }))
    .expect("request");
    let session = ExtractSession::new(&project, &descriptor, request).expect("session");
    let mut extractor = Batches {
        descriptor,
        batches: batches.into_iter(),
    };
    let Ok(sets) = drive(session, &mut extractor, &mut Frames) else {
        return;
    };
    let mut document = base;
    document["data"] = serde_json::to_value(&sets).expect("data sets serialize");
    if let Err(error) = alpha2_model::Project::from_json(&document.to_string()) {
        panic!("finished data sets do not validate: {error}");
    }
});
