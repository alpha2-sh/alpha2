//! Compose specs: a video described by intent, compiled into a plain project document.
//!
//! [`compile`] turns a [`VideoSpec`] into an [`alpha2_model::Project`] that
//! [`alpha2_model::Project::validate`] accepts, plus the export options the project does not
//! hold.
//!
//! Compiling is pure and reads no files. Facts about the files a spec names come in as
//! [`Facts`], gathered by a binding from [`requirements`]. The same spec, facts, fonts and
//! compiler version give a byte-identical project.
//!
//! A spec has no logic. A [`template`] adds parameters, `$if`, `$switch`, `$each` and `{{name}}`,
//! which binding removes. Components that take any number of inputs are Rust code here.

mod compile;
mod de;
mod facts;
mod library;
mod lower;
#[cfg(feature = "schema")]
mod schema;
pub mod spec;
mod svg_path;
pub mod template;
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
pub use schema::{
    COMPOSE_CORE_SCHEMA_ID, COMPOSE_SCHEMA_ID, compose_core_json_schema, compose_json_schema,
};
pub use spec::{COMPOSE_VERSION, VideoSpec};
pub use theme::THEMES;

/// Why a spec does not compile, and where.
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
    /// What the compiler did not do as asked, and why.
    pub warnings: Vec<String>,
}

/// Compiles `spec` with `facts` about its files.
///
/// `fonts` are the font files an export registers, in fallback order; with them, text that
/// must fit is measured and shrunk to fit.
///
/// # Errors
/// Returns where in the spec, and why, it cannot be compiled.
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

/// Parses a spec from JSON, naming where an unparsable value lies.
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

/// [`compile`] across a JSON boundary. The result is `{project, assets, loudness, warnings}`.
///
/// # Errors
/// Returns where in the spec or facts, and why, they do not parse or compile.
///
/// # Panics
/// Never; a compiled project always serializes.
pub fn compile_json(spec: &str, facts: &str, fonts: &[Arc<[u8]>]) -> Result<String, ComposeError> {
    let spec = parse_spec(&expand_builtins(spec)?)?;
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
/// Never; requirements always serialize.
pub fn requirements_json(spec: &str) -> Result<String, ComposeError> {
    let spec = parse_spec(&expand_builtins(spec)?)?;
    Ok(serde_json::to_string(&requirements(&spec)).expect("requirements serialize"))
}

/// `spec` with its 1st-party template nodes expanded (`alpha2/` ids and their bare names).
fn expand_builtins(spec: &str) -> Result<String, ComposeError> {
    let value: serde_json::Value =
        serde_json::from_str(spec).map_err(|error| ComposeError::new("spec", error.to_string()))?;
    Ok(template::bind(
        &value,
        &serde_json::Value::Null,
        &template::BindOptions::default(),
    )?
    .to_string())
}

fn json_value(json: &str, root: &str) -> Result<serde_json::Value, ComposeError> {
    serde_json::from_str(json).map_err(|error| ComposeError::new(root, error.to_string()))
}

fn bind_options(options: &str) -> Result<template::BindOptions, ComposeError> {
    let options = json_value(options, "options")?;
    Ok(template::BindOptions {
        theme: options
            .get("theme")
            .filter(|theme| !theme.is_null())
            .cloned(),
        templates: options
            .get("templates")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
}

/// [`template::bind`] across a JSON boundary: `options` is `{theme?, templates?}`.
///
/// # Errors
/// Returns where in the template, data or options, and why, they do not bind.
pub fn bind_json(document: &str, data: &str, options: &str) -> Result<String, ComposeError> {
    let bound = template::bind(
        &json_value(document, "template")?,
        &json_value(data, "data")?,
        &bind_options(options)?,
    )?;
    Ok(bound.to_string())
}

/// [`template::data_schema`] across a JSON boundary.
///
/// # Errors
/// Returns where the template does not parse.
pub fn data_schema_json(template: &str) -> Result<String, ComposeError> {
    Ok(template::data_schema(&json_value(template, "template")?)?.to_string())
}

/// [`template::lint`] across a JSON boundary: `data` may be empty for none, and `options` is
/// `{templates?}`. Returns the issues as a JSON list.
///
/// # Errors
/// Returns where the template, data or options are not JSON.
///
/// # Panics
/// Never; issues always serialize.
pub fn lint_json(template: &str, data: &str, options: &str) -> Result<String, ComposeError> {
    let data = if data.trim().is_empty() {
        None
    } else {
        Some(json_value(data, "data")?)
    };
    let issues = template::lint(
        &json_value(template, "template")?,
        data.as_ref(),
        &bind_options(options)?.templates,
    );
    Ok(serde_json::to_string(&issues).expect("issues serialize"))
}

/// The 1st-party `alpha2/` templates, as a JSON list.
#[must_use]
pub fn templates_json() -> String {
    serde_json::Value::Array(library::TEMPLATES.clone()).to_string()
}
