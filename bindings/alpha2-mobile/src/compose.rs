//! Compose and its templates on the device, with the functions the other hosts use.

use std::sync::Arc;

use crate::{Alpha2Error, FontFile};

/// A compose error as `INVALID_INPUT`, its one problem `path: problem`.
fn refused(error: &alpha2_compose::ComposeError) -> Alpha2Error {
    Alpha2Error::InvalidInput {
        problems: vec![error.to_string()],
    }
}

/// The files a compose spec names, as JSON `{"media": [{path, kind}], "texts": [path]}`.
///
/// # Errors
/// `InvalidInput` naming where the spec does not parse.
#[uniffi::export]
pub fn compose_requirements(spec_json: String) -> Result<String, Alpha2Error> {
    alpha2_compose::requirements_json(&spec_json).map_err(|error| refused(&error))
}

/// A compose spec compiled with facts about its files, as JSON
/// `{"project", "assets", "loudness", "warnings"}`. Text is measured with `fonts`, the first face
/// of each.
///
/// # Errors
/// `InvalidInput` naming where the spec or facts do not compile.
#[uniffi::export]
pub fn compose_compile(
    spec_json: String,
    facts_json: String,
    fonts: Vec<FontFile>,
) -> Result<String, Alpha2Error> {
    let fonts = fonts
        .into_iter()
        .map(|font| Arc::<[u8]>::from(font.bytes))
        .collect::<Vec<_>>();
    alpha2_compose::compile_json(&spec_json, &facts_json, &fonts).map_err(|error| refused(&error))
}

/// A template bound to its data, or a spec with its template nodes expanded, as JSON.
/// `options_json` is `{"theme"?, "templates"?}`.
///
/// # Errors
/// `InvalidInput` naming where the template, data or options do not bind.
#[uniffi::export]
pub fn compose_bind(
    document_json: String,
    data_json: String,
    options_json: String,
) -> Result<String, Alpha2Error> {
    alpha2_compose::bind_json(&document_json, &data_json, &options_json)
        .map_err(|error| refused(&error))
}

/// A template's data as a JSON Schema.
///
/// # Errors
/// `InvalidInput` for a template that does not parse.
#[uniffi::export]
pub fn compose_data_schema(template_json: String) -> Result<String, Alpha2Error> {
    alpha2_compose::data_schema_json(&template_json).map_err(|error| refused(&error))
}

/// A template's problems as a JSON list of `{path, code, message, severity}`; `data_json` may be
/// empty for none.
///
/// # Errors
/// `InvalidInput` for text that is not JSON.
#[uniffi::export]
pub fn compose_lint(
    template_json: String,
    data_json: String,
    options_json: String,
) -> Result<String, Alpha2Error> {
    alpha2_compose::lint_json(&template_json, &data_json, &options_json)
        .map_err(|error| refused(&error))
}

/// The 1st-party `alpha2/` templates, as a JSON list.
#[uniffi::export]
#[must_use]
pub fn compose_templates() -> String {
    alpha2_compose::templates_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_binds_and_compiles_on_the_device() {
        let template = r#"{"template": {"id": "acme/t", "version": 1}, "params": [{"name": "t", "type": "text"}], "duration": 1, "body": {"type": "alpha2/circle-reveal", "with": {"content": {"type": "text", "text": {"$param": "t"}}}}}"#;
        let spec = compose_bind(
            template.to_owned(),
            r#"{"t": "Hi"}"#.to_owned(),
            "{}".to_owned(),
        )
        .expect("binds");
        let compiled = compose_compile(spec, "{}".to_owned(), Vec::new()).expect("compiles");
        assert!(compiled.contains("\"schema_version\":1"));
        let refused = compose_bind(template.to_owned(), "{}".to_owned(), "{}".to_owned())
            .expect_err("t is required");
        assert!(
            matches!(refused, Alpha2Error::InvalidInput { problems } if problems[0].starts_with("data.t:"))
        );
    }
}
