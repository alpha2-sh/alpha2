//! The extraction cases of `tests/cases/surface-conformance.json` through the app-facing API.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alpha2_engine::extract::fixtures::{digest_values, fnv1a};
use serde_json::Value;

use crate::{
    Alpha2, Alpha2Error, ExtractAudioBlock, ExtractRequest, ExtractVideoFrame, ExtractionHandle,
    Extractor, ExtractorFailure,
};

/// The repository's `tests/`, or `ALPHA2_TESTS_DIR` where a device runs the tests from a copy.
pub(crate) fn tests_dir() -> PathBuf {
    std::env::var_os("ALPHA2_TESTS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests"),
        PathBuf::from,
    )
}

pub(crate) fn table() -> Value {
    let path = tests_dir().join("cases/surface-conformance.json");
    serde_json::from_slice(&std::fs::read(&path).expect("the surface table")).expect("json")
}

/// The table's extraction case `name`.
pub(crate) fn case(table: &Value, name: &str) -> Value {
    table["extract"]
        .as_array()
        .expect("extract cases")
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("no case {name}"))
        .clone()
}

/// The request `case` makes, its assets taken from the tests directory.
pub(crate) fn request(table: &Value, case: &Value) -> ExtractRequest {
    let tests = tests_dir();
    let assets = case["assets"]
        .as_object()
        .expect("assets")
        .iter()
        .map(|(id, path)| {
            let path = path.as_str().expect("path");
            let file = path
                .strip_prefix("tests/")
                .map_or_else(|| tests.join("..").join(path), |inside| tests.join(inside));
            (id.clone(), file.to_string_lossy().into_owned())
        })
        .collect::<HashMap<_, _>>();
    ExtractRequest {
        project_json: table["projects"][case["project"].as_str().expect("project")].to_string(),
        request_json: case["request"].to_string(),
        assets,
        made_from_json: None,
    }
}

/// The digest fixture as an app writes it: each frame's FNV-1a as two scalars.
pub(crate) struct Digest {
    descriptor: String,
    /// Cancels this handle from the first frame on, once set.
    pub(crate) cancel: std::sync::OnceLock<Arc<ExtractionHandle>>,
    pub(crate) frames: std::sync::atomic::AtomicUsize,
}

impl Digest {
    pub(crate) fn new(case: &Value) -> Self {
        Self {
            descriptor: case["extractor"]["client"].to_string(),
            cancel: std::sync::OnceLock::new(),
            frames: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Extractor for Digest {
    fn describe(&self) -> String {
        self.descriptor.clone()
    }

    fn begin(&self, _context_json: String) -> Result<(), ExtractorFailure> {
        Ok(())
    }

    fn pass(&self, _pass: u32, _backward: bool) -> Result<(), ExtractorFailure> {
        Ok(())
    }

    fn video(&self, frame: ExtractVideoFrame) -> Result<String, ExtractorFailure> {
        self.frames
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.cancel.get() {
            handle.cancel();
        }
        let [hi, lo] = digest_values(fnv1a(frame.data));
        Ok(
            serde_json::json!([{ "output": "digest", "time": frame.time, "values": [hi, lo] }])
                .to_string(),
        )
    }

    fn audio(&self, _block: ExtractAudioBlock) -> Result<String, ExtractorFailure> {
        Ok("[]".into())
    }

    fn end(&self) -> Result<String, ExtractorFailure> {
        Ok("[]".into())
    }
}

/// The handle's outcome, failing the test when it has not settled within `seconds`.
pub(crate) fn settled(handle: Arc<ExtractionHandle>, seconds: u64) -> Result<String, Alpha2Error> {
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(pollster::block_on(handle.outcome()));
    });
    wait.recv_timeout(Duration::from_secs(seconds))
        .expect("the extraction settled in time")
}

/// Runs the client-extractor case `name` on `engine`, returning its data hash and the pinned one.
///
/// # Errors
/// The extraction's failure.
pub(crate) fn client_case_hash(
    engine: &Arc<Alpha2>,
    name: &str,
) -> Result<(String, String), Alpha2Error> {
    let table = table();
    let case = case(&table, name);
    let handle =
        Arc::clone(engine).extract(request(&table, &case), Arc::new(Digest::new(&case)))?;
    let sets: Value = serde_json::from_str(&settled(handle, 120)?).expect("json");
    Ok((
        alpha2_test_support::extract::data_hash(&sets),
        case["sha256"].as_str().expect("sha256").to_owned(),
    ))
}
