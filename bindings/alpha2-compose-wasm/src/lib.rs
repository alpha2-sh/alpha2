//! Compose and its templates as a wasm module apart from the engine's, so a page that only plays
//! and exports projects does not load them.

use std::sync::Arc;

use alpha2_compose::ComposeError;
use wasm_bindgen::prelude::*;

/// An `Error` whose message is `path: problem`, with `path` and `problem` as properties.
fn refused(error: &ComposeError) -> JsValue {
    let thrown = js_sys::Error::new(&error.to_string());
    // Setting a property on a fresh `Error` cannot fail.
    let _ = js_sys::Reflect::set(&thrown, &"path".into(), &error.path.as_str().into());
    let _ = js_sys::Reflect::set(&thrown, &"problem".into(), &error.message.as_str().into());
    thrown.into()
}

/// The files a compose spec names, as JSON `{"media": [{path, kind}], "texts": [path]}`.
///
/// # Errors
/// Rejects a spec that does not parse, naming where.
#[wasm_bindgen(js_name = composeRequirements)]
pub fn compose_requirements(spec: &str) -> Result<String, JsValue> {
    alpha2_compose::requirements_json(spec).map_err(|error| refused(&error))
}

/// A compose spec compiled with facts about its files and font files, as JSON
/// `{"project", "assets", "loudness", "warnings"}`.
///
/// # Errors
/// Rejects a spec or facts that do not compile, naming where.
#[wasm_bindgen(js_name = composeCompile)]
pub fn compose_compile(
    spec: &str,
    facts: &str,
    fonts: Vec<js_sys::Uint8Array>,
) -> Result<String, JsValue> {
    let fonts = fonts
        .into_iter()
        .map(|font| Arc::<[u8]>::from(font.to_vec()))
        .collect::<Vec<_>>();
    alpha2_compose::compile_json(spec, facts, &fonts).map_err(|error| refused(&error))
}

/// A template bound to its data, or a spec with its template nodes expanded, as JSON.
/// `options` is `{"theme"?, "templates"?}`.
///
/// # Errors
/// Rejects a template, data or options that do not bind, naming where.
#[wasm_bindgen(js_name = composeBind)]
pub fn compose_bind(document: &str, data: &str, options: &str) -> Result<String, JsValue> {
    alpha2_compose::bind_json(document, data, options).map_err(|error| refused(&error))
}

/// A template's data as a JSON Schema.
///
/// # Errors
/// Rejects a template that does not parse.
#[wasm_bindgen(js_name = composeDataSchema)]
pub fn compose_data_schema(template: &str) -> Result<String, JsValue> {
    alpha2_compose::data_schema_json(template).map_err(|error| refused(&error))
}

/// A template's problems as a JSON list of `{path, code, message, severity}`; `data` may be
/// empty for none.
///
/// # Errors
/// Rejects text that is not JSON.
#[wasm_bindgen(js_name = composeLint)]
pub fn compose_lint(template: &str, data: &str, options: &str) -> Result<String, JsValue> {
    alpha2_compose::lint_json(template, data, options).map_err(|error| refused(&error))
}

/// The 1st-party `alpha2/` templates, as a JSON list.
#[wasm_bindgen(js_name = composeTemplates)]
#[must_use]
pub fn compose_templates() -> String {
    alpha2_compose::templates_json()
}
