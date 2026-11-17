//! Edit operations for the web, the same function every surface calls.

use wasm_bindgen::prelude::*;

use crate::error::{BindingError, Code};

/// Applies an edit operation (`remove_ranges`, `split_at`, `bake`) to a project, both JSON
/// text, and returns `[project, report]` as the engine's canonical JSON text.
///
/// # Errors
/// `INVALID_INPUT` for an invalid project or request and for every refusal, or
/// `ALPHA2_ERROR` for an edited project that fails validation; `details.refusal` names why.
#[wasm_bindgen(js_name = applyEdit)]
pub fn apply_edit(project: &str, request: &str) -> Result<web_sys::js_sys::Array, JsValue> {
    match alpha2_eval::edit::apply_json(project, request) {
        Ok(edited) => Ok(web_sys::js_sys::Array::of2(
            &JsValue::from_str(&edited.project),
            &JsValue::from_str(&edited.report),
        )),
        Err(failure) => {
            let code = if failure.code == "INVALID_INPUT" {
                Code::INVALID_INPUT
            } else {
                Code::INTERNAL
            };
            let error: JsValue = BindingError::new(code, failure.message).into();
            let details = web_sys::js_sys::Object::new();
            let _ = web_sys::js_sys::Reflect::set(
                &details,
                &"refusal".into(),
                &JsValue::from_str(failure.refusal),
            );
            let _ = web_sys::js_sys::Reflect::set(&error, &"details".into(), &details);
            Err(error)
        }
    }
}
