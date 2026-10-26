//! Compose specs for alpha2: a video described by intent, compiled into a
//! plain project document.
//!
//! A [`VideoSpec`] says what a video is made of: a body (sequences, stacks,
//! grids, media, text, captions, and components such as slideshows, title
//! cards and comparison grids), what is drawn over it, and what plays under
//! it. [`compile`] turns it into a [`alpha2_model::Project`] that
//! [`alpha2_model::Project::validate`] accepts, and the export options the
//! project does not hold. The result is an ordinary project: open it in the
//! editor, change it, render it anywhere.
//!
//! Compiling is a pure function. It reads no files: what it needs to know
//! about the files a spec names (sizes, lengths, caption text) comes in as
//! [`Facts`], which a binding gathers from the list [`requirements`] makes.
//! The same spec, facts, fonts and compiler version give the same project,
//! byte for byte.
//!
//! The spec is data without logic: no loops, conditions or expressions.
//! Programs build specs in their own language; components that take any
//! number of inputs are Rust code here.

mod compile;
mod de;
mod facts;
mod lower;
#[cfg(feature = "schema")]
mod schema;
pub mod spec;
#[cfg(test)]
mod tests;
mod theme;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use alpha2_model::Project;
use serde::{Deserialize, Serialize};

pub use facts::{Facts, MediaFacts, RequiredMedia, Requirements, kind_of_path, requirements};
#[cfg(feature = "schema")]
pub use schema::compose_json_schema;
pub use spec::{COMPOSE_VERSION, VideoSpec};
pub use theme::THEMES;

/// Why a spec does not compile: where in it, and what is wrong.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeError {
    /// Where in the spec, as `spec.body.children[2].path`.
    pub path: String,
    /// What is wrong there.
    pub message: String,
}

impl ComposeError {
    pub(crate) fn new(path: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for ComposeError {}

/// The loudness an export normalizes to.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct LoudnessTarget {
    /// Integrated loudness, -60..=0 LUFS.
    pub target_lufs: f64,
    /// True-peak ceiling, -20..=0 dBTP. Default -1.
    #[serde(default = "default_ceiling")]
    pub true_peak_ceiling_dbtp: f64,
}

const fn default_ceiling() -> f64 {
    -1.0
}

/// A compiled spec: the project, and what an export of it needs besides.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Compiled {
    /// The project document.
    pub project: Project,
    /// Each media source's file, by source id: an export's assets.
    pub assets: BTreeMap<String, String>,
    /// The loudness the spec asks an export to normalize to.
    pub loudness: Option<LoudnessTarget>,
    /// What the compiler did not do as asked, and why: text it could not
    /// measure, nodes that start after the video ends.
    pub warnings: Vec<String>,
}

/// Compiles `spec` with `facts` about its files. `fonts` are the font files
/// an export registers, in its fallback order; with them, text that must fit
/// is measured and shrunk to fit.
///
/// # Errors
/// Returns where in the spec, and why, it cannot be compiled: a value out of
/// range, a file the facts do not describe, a transition only single
/// pictures take between nodes that are not.
pub fn compile(
    spec: &VideoSpec,
    facts: &Facts,
    fonts: &[Arc<[u8]>],
) -> Result<Compiled, ComposeError> {
    if let Some(loudness) = spec.loudness {
        let (target, ceiling) = match loudness {
            spec::Loudness::Lufs(target) => (target, default_ceiling()),
            spec::Loudness::Target(target) => (target.target_lufs, target.true_peak_ceiling_dbtp),
        };
        if !(-60.0..=0.0).contains(&target) || !(-20.0..=0.0).contains(&ceiling) {
            return Err(ComposeError::new(
                "spec.loudness",
                format!(
                    "{target} LUFS under {ceiling} dBTP: the target is -60..=0 LUFS and the ceiling -20..=0 dBTP"
                ),
            ));
        }
    }
    compile::compile(spec, facts, fonts)
}

/// Parses a spec from JSON, naming where a value that does not parse lies.
///
/// # Errors
/// Returns the place and the parser's reason.
pub fn parse_spec(json: &str) -> Result<VideoSpec, ComposeError> {
    parse(json, "spec")
}

/// Parses facts from JSON.
///
/// # Errors
/// Returns the place and the parser's reason.
pub fn parse_facts(json: &str) -> Result<Facts, ComposeError> {
    parse(json, "facts")
}

fn parse<T: serde::de::DeserializeOwned>(json: &str, root: &str) -> Result<T, ComposeError> {
    let value =
        serde_json::from_str(json).map_err(|error| ComposeError::new(root, error.to_string()))?;
    de::from_value(value)
        .map_err(|located| ComposeError::new(&de::join(root, &located.path), located.message))
}

/// [`compile`] across a JSON boundary: the spec and facts as JSON text, the
/// result as JSON text (`{project, assets, loudness, warnings}`).
///
/// # Errors
/// Returns where in the spec or facts, and why, they do not parse or the
/// spec does not compile.
///
/// # Panics
/// Never: a compiled project always serializes.
pub fn compile_json(spec: &str, facts: &str, fonts: &[Arc<[u8]>]) -> Result<String, ComposeError> {
    let spec = parse_spec(spec)?;
    let facts = parse_facts(facts)?;
    let compiled = compile(&spec, &facts, fonts)?;
    Ok(serde_json::to_string(&compiled).expect("a compiled project serializes"))
}

/// [`requirements`] across a JSON boundary.
///
/// # Errors
/// Returns where in the spec, and why, it does not parse.
///
/// # Panics
/// Never: requirements always serialize.
pub fn requirements_json(spec: &str) -> Result<String, ComposeError> {
    let spec = parse_spec(spec)?;
    Ok(serde_json::to_string(&requirements(&spec)).expect("requirements serialize"))
}
