//! The validation, lint, and used-sources corpora the TypeScript port reads too.

use super::*;

/// `value` with every repeated run of the validation corpus written out. A case that needs
/// hundreds of nearly equal elements holds them as one array element
/// `{"$repeat": N, "item": T}`: N elements, T at i = 0..N-1, with `{i}` in T's strings replaced by
/// i and `{"$index": {"start": a, "step": b}}` by a + b*i. A `$repeat` inside T counts its own i.
/// `scripts/lib/validation-corpus.mjs` expands it the same way for the TypeScript tests.
fn expand_repeats(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    fn is_repeat(value: &Value) -> bool {
        value
            .as_object()
            .is_some_and(|object| object.contains_key("$repeat"))
    }
    fn instantiate(template: &Value, index: u64) -> Value {
        match template {
            Value::String(text) => Value::String(text.replace("{i}", &index.to_string())),
            Value::Array(elements) => Value::Array(
                elements
                    .iter()
                    .map(|element| instantiate(element, index))
                    .collect(),
            ),
            Value::Object(_) if is_repeat(template) => template.clone(),
            Value::Object(object) => match object.get("$index") {
                Some(counter) if object.len() == 1 => {
                    let start = counter["start"].as_i64().expect("$index start");
                    let step = counter["step"].as_i64().expect("$index step");
                    Value::from(start + step * i64::try_from(index).expect("index"))
                }
                _ => Value::Object(
                    object
                        .iter()
                        .map(|(key, field)| (key.clone(), instantiate(field, index)))
                        .collect(),
                ),
            },
            _ => template.clone(),
        }
    }
    match value {
        Value::Array(elements) => Value::Array(
            elements
                .into_iter()
                .flat_map(|element| {
                    if is_repeat(&element) {
                        let count = element["$repeat"].as_u64().expect("$repeat count");
                        (0..count)
                            .map(|index| expand_repeats(instantiate(&element["item"], index)))
                            .collect()
                    } else {
                        vec![expand_repeats(element)]
                    }
                })
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, field)| (key, expand_repeats(field)))
                .collect(),
        ),
        other => other,
    }
}

#[test]
fn shared_rust_and_typescript_validation_corpus_agrees() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/project-validation-corpus.json");
    let text = std::fs::read_to_string(path).expect("shared validation corpus");
    let cases = expand_repeats(serde_json::from_str(&text).expect("corpus JSON"));
    for case in cases.as_array().expect("case array") {
        let name = case["name"].as_str().expect("case name");
        let expected = case["valid"].as_bool().expect("valid flag");
        // A case is a document, or (`json`) the text of one, for what only text can say: a repeated
        // key, `1920.0`, a lone surrogate.
        let document = match case.get("json") {
            Some(json) => json.as_str().expect("json text").to_owned(),
            None => serde_json::to_string(&case["project"]).expect("project JSON"),
        };
        let loaded = Project::from_json(&document);
        assert_eq!(loaded.is_ok(), expected, "{name}: {loaded:?}");
        if expected {
            continue;
        }
        // Where the engine refuses it: `schema` (the schema and the parser reject it), `parser`
        // (only the parser can, e.g. a rate over the tick resolution), or `model` (validation). A
        // document of another version reports its version: `from_json` reads it first.
        let version = case
            .get("project")
            .and_then(|project| project["schema_version"].as_u64());
        if let Some(found) =
            version.filter(|&found| found != u64::from(crate::CURRENT_SCHEMA_VERSION))
        {
            assert!(
                matches!(
                    &loaded,
                    Err(crate::ProjectLoadError::InvalidProject(
                        crate::ModelError::UnsupportedSchemaVersion { found: reported }
                    )) if u64::from(*reported) == found
                ),
                "{name}: another version must be reported as one: {loaded:?}"
            );
        }
        match case["rejected_by"].as_str() {
            Some("schema") => assert!(
                matches!(
                    loaded,
                    Err(crate::ProjectLoadError::Json(_)
                        | crate::ProjectLoadError::InvalidProject(
                            crate::ModelError::UnsupportedSchemaVersion { .. }
                        ))
                ),
                "{name}: the parser accepted a document the schema rejects: {loaded:?}"
            ),
            Some("parser") => assert!(
                matches!(loaded, Err(crate::ProjectLoadError::Json(_))),
                "{name}: the parser accepted it: {loaded:?}"
            ),
            Some("model") => match &loaded {
                // The error the case is named for, so a case another defect also rejects cannot
                // hide the loss of its own rule.
                Err(crate::ProjectLoadError::InvalidProject(error)) => assert_eq!(
                    Some(error.to_string().as_str()),
                    case["error"].as_str(),
                    "{name}: rejected for another reason"
                ),
                _ => panic!(
                    "{name}: the parser refused a document only validation should: {loaded:?}"
                ),
            },
            other => panic!("{name}: rejected_by must be schema, parser, or model, not {other:?}"),
        }
        if case["rejected_by"] != "model" {
            assert!(
                case.get("error").is_none(),
                "{name}: only a case validation rejects names the engine's error"
            );
        }
    }
}

#[test]
fn the_shared_lint_corpus_reports_its_issues() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases/lint-corpus.json");
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("lint corpus")).expect("JSON");
    for case in cases.as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let project = crate::Project::from_json(&case["project"].to_string())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let issues = serde_json::to_value(project.lint()).expect("issues serialize");
        assert_eq!(issues, case["issues"], "{name}");
    }
}

#[test]
fn the_shared_used_sources_corpus_reads_what_the_planner_reads() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/used-sources-corpus.json");
    let cases: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("used-sources corpus"))
            .expect("JSON");
    for case in cases.as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let project = crate::Project::from_json(&case["project"].to_string())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let mut used = project
            .used_sources()
            .into_iter()
            .map(|source| source.id.as_str())
            .collect::<Vec<_>>();
        used.sort_unstable();
        assert_eq!(serde_json::json!(used), case["used"], "{name}");
        // The lint calls every other source unused.
        let unused = project
            .lint()
            .into_iter()
            .filter_map(|issue| match issue {
                crate::LintIssue::UnusedSource { source } => Some(source.as_str().to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected = project
            .sources
            .iter()
            .map(|source| source.id.as_str())
            .filter(|id| !used.contains(id))
            .collect::<Vec<_>>();
        assert_eq!(unused, expected, "{name}");
    }
}

/// Where `value` holds a color or paint in a form the engine reads but never writes: a hex
/// string where a color lies, or a paint without its tag.
fn non_canonical(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
    use serde_json::Value;
    match value {
        Value::Object(object) => {
            for (key, field) in object {
                let at = format!("{path}.{key}");
                // A paint, or an animated one (a layout's background is always animated).
                let paints = match field.get("mode").and_then(Value::as_str) {
                    Some("constant") => vec![&field["value"]],
                    Some("keyframes") => field["keyframes"]
                        .as_array()
                        .map(|keys| keys.iter().map(|key| &key["value"]).collect())
                        .unwrap_or_default(),
                    _ if key == "paint" => vec![field],
                    _ => Vec::new(),
                };
                let untagged = matches!(key.as_str(), "paint" | "background")
                    && paints
                        .iter()
                        .any(|paint| paint.is_string() || paint.get("type").is_none());
                if (key == "color" && field.is_string()) || untagged {
                    found.push(at.clone());
                }
                non_canonical(field, &at, found);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                non_canonical(item, &format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}

#[test]
fn every_valid_document_round_trips_and_writes_canonical_forms() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/cases");
    let mut documents = Vec::new();
    for entry in std::fs::read_dir(&root).expect("tests/cases") {
        let path = entry.expect("an entry").path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-project.json"))
        {
            let text = std::fs::read_to_string(&path).expect("a fixture");
            documents.push((path.display().to_string(), text));
        }
    }
    let corpus = expand_repeats(
        serde_json::from_str(
            &std::fs::read_to_string(root.join("project-validation-corpus.json")).expect("corpus"),
        )
        .expect("corpus JSON"),
    );
    for case in corpus.as_array().expect("cases") {
        if case["valid"] == true && case.get("json").is_none() {
            documents.push((
                case["name"].as_str().expect("a name").to_owned(),
                case["project"].to_string(),
            ));
        }
    }
    assert!(documents.len() > 50, "{}", documents.len());
    for (name, text) in documents {
        let project = Project::from_json(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
        let written = serde_json::to_string(&project).expect("writes");
        let again = Project::from_json(&written).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(again, project, "{name}");
        assert_eq!(
            serde_json::to_string(&again).expect("writes"),
            written,
            "{name}"
        );
        let mut found = Vec::new();
        non_canonical(
            &serde_json::from_str(&written).expect("JSON"),
            "project",
            &mut found,
        );
        assert!(found.is_empty(), "{name}: {found:?}");
    }
}
