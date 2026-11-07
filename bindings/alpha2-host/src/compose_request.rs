//! The `compose` operation, answered with the functions the browser and Python bindings use.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::protocol::{ComposeRequest, HostError};

/// A compose error as `INVALID_INPUT`, with `path` and `problem` in its details.
fn refused(error: &alpha2_compose::ComposeError) -> HostError {
    HostError::new("INVALID_INPUT", error.to_string())
        .with_details(json!({"path": error.path, "problem": error.message}))
}

fn required<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a Value, HostError> {
    value.ok_or_else(|| {
        HostError::new(
            "INVALID_REQUEST",
            format!("this compose action needs `{field}`"),
        )
    })
}

fn parsed(text: &str) -> Result<Value, HostError> {
    serde_json::from_str(text).map_err(|error| HostError::new("INTERNAL", error.to_string()))
}

/// Answers a compose request.
///
/// # Errors
/// `INVALID_REQUEST` for an unknown action or a missing field, `INVALID_INPUT` for a spec,
/// template or data that does not compile or bind, and an I/O failure reading a font file.
pub fn answer(request: &ComposeRequest) -> Result<Value, HostError> {
    let options = || {
        json!({
            "theme": request.theme,
            "templates": request.templates,
        })
        .to_string()
    };
    let text = |value: Option<&Value>| value.map_or_else(|| "{}".to_owned(), Value::to_string);
    match request.action.as_str() {
        "compile" => {
            let mut spec = required(request.spec.as_ref(), "spec")?.clone();
            if !request.templates.is_empty() {
                spec = parsed(
                    &alpha2_compose::bind_json(&spec.to_string(), "null", &options())
                        .map_err(|error| refused(&error))?,
                )?;
            }
            let fonts = request
                .fonts
                .iter()
                .map(|path| {
                    std::fs::read(path).map(Arc::<[u8]>::from).map_err(|error| {
                        HostError::new(
                            "INVALID_INPUT",
                            format!("cannot read font {}: {error}", path.display()),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let compiled = alpha2_compose::compile_json(
                &spec.to_string(),
                &text(request.facts.as_ref()),
                &fonts,
            )
            .map_err(|error| refused(&error))?;
            parsed(&compiled)
        }
        "bind" => parsed(
            &alpha2_compose::bind_json(
                &required(request.template.as_ref(), "template")?.to_string(),
                &text(request.data.as_ref()),
                &options(),
            )
            .map_err(|error| refused(&error))?,
        )
        .map(|spec| json!({"spec": spec})),
        "dataSchema" => parsed(
            &alpha2_compose::data_schema_json(
                &required(request.template.as_ref(), "template")?.to_string(),
            )
            .map_err(|error| refused(&error))?,
        )
        .map(|schema| json!({"schema": schema})),
        "lint" => parsed(
            &alpha2_compose::lint_json(
                &required(request.template.as_ref(), "template")?.to_string(),
                &request
                    .data
                    .as_ref()
                    .map_or_else(String::new, Value::to_string),
                &options(),
            )
            .map_err(|error| refused(&error))?,
        )
        .map(|issues| json!({"issues": issues})),
        "requirements" => parsed(
            &alpha2_compose::requirements_json(
                &required(request.spec.as_ref(), "spec")?.to_string(),
            )
            .map_err(|error| refused(&error))?,
        ),
        "templates" => {
            parsed(&alpha2_compose::templates_json()).map(|list| json!({"templates": list}))
        }
        other => Err(HostError::new(
            "INVALID_REQUEST",
            format!(
                "unknown compose action `{other}`, expected one of `compile`, `bind`, `dataSchema`, `lint`, `requirements`, `templates`"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(fields: &Value) -> ComposeRequest {
        let mut line = json!({"protocolVersion": 1, "requestId": "1"});
        line.as_object_mut()
            .expect("an object")
            .extend(fields.as_object().expect("an object").clone());
        serde_json::from_value(line).expect("a compose request")
    }

    #[test]
    fn compose_requests_bind_compile_and_describe_templates() {
        let template = json!({
            "template": {"id": "acme/t", "version": 1},
            "params": [{"name": "t", "type": "text", "max_chars": 3}],
            "duration": 1,
            "body": {"type": "text", "text": "{{t}}"},
        });
        let bound = answer(&request(
            &json!({"action": "bind", "template": template, "data": {"t": "Hi"}}),
        ))
        .expect("binds");
        assert_eq!(bound["spec"]["body"]["text"], "Hi");
        assert!(bound["spec"].get("theme").is_none());
        let compiled = answer(&request(
            &json!({"action": "compile", "spec": bound["spec"]}),
        ))
        .expect("compiles");
        assert_eq!(compiled["project"]["schema_version"], 1);
        let schema = answer(&request(
            &json!({"action": "dataSchema", "template": template}),
        ))
        .expect("a schema");
        assert_eq!(schema["schema"]["properties"]["t"]["maxLength"], 3);
        let issues =
            answer(&request(&json!({"action": "lint", "template": template}))).expect("issues");
        assert_eq!(issues["issues"], json!([]));
        let listed = answer(&request(&json!({"action": "templates"}))).expect("templates");
        assert!(
            listed["templates"]
                .as_array()
                .is_some_and(|list| !list.is_empty())
        );
    }

    #[test]
    fn compose_refusals_name_where_and_why() {
        let refused = answer(&request(
            &json!({"action": "compile", "spec": {"body": {"type": "nope"}}}),
        ))
        .expect_err("refused");
        assert_eq!(refused.code, "INVALID_INPUT");
        assert_eq!(refused.details.expect("details")["path"], "spec.body.type");
        let unknown = answer(&request(&json!({"action": "nope"}))).expect_err("refused");
        assert_eq!(unknown.code, "INVALID_REQUEST");
        let missing = answer(&request(&json!({"action": "bind"}))).expect_err("refused");
        assert!(missing.message.contains("`template`"));
    }
}
