//! Templates are compose documents with typed parameters, bound to data into plain specs.
//!
//! A template is a compose document with a `template` header and `params`. Binding validates
//! the data, then replaces `{"$param": name}` values, `{{name}}` in strings, and the `$if`,
//! `$switch` and `$each` directives, and expands `owner/name` template nodes, leaving a spec
//! with no `$` left in it. Binding is logic-less and works on the JSON before it is parsed, so
//! every compose field can be a parameter.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::ComposeError;

/// Template nodes nest at most this deep.
const MAX_DEPTH: usize = 16;

/// Keys whose string values are names, not text, so `{{...}}` in them is left alone.
const NAME_KEYS: [&str; 2] = ["type", "id"];

/// Keys whose string values are text a viewer reads, checked for literal copy in templates.
const TEXT_KEYS: [&str; 8] = [
    "text", "subtitle", "title", "name", "role", "label", "cta", "caption",
];

/// Keys whose string values are files, checked for literal media in templates.
const FILE_KEYS: [&str; 4] = ["path", "file", "logo", "fit_to"];

/// What binding takes besides the template and its data.
#[derive(Clone, Debug, Default)]
pub struct BindOptions {
    /// A theme for a video template, replacing the one it names.
    pub theme: Option<Value>,
    /// Node templates the document may name, by their header ids.
    pub templates: Vec<Value>,
}

/// A problem `lint` finds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Issue {
    /// Where, as a dotted path into the template (`template.params[0]`) or the data (`data.title`).
    pub path: String,
    /// A stable code for the kind of problem.
    pub code: &'static str,
    /// What is wrong.
    pub message: String,
    /// `error` fails binding or compiling; `warning` does not.
    pub severity: Severity,
}

/// How serious an [`Issue`] is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Binding or compiling fails.
    Error,
    /// Allowed, but likely a mistake.
    Warning,
}

/// A template's header.
#[derive(Clone, Debug)]
struct Header {
    id: String,
    version: u64,
    kind: Kind,
    title: Option<String>,
    description: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Video,
    Node,
}

/// One declared parameter.
#[derive(Clone, Debug)]
struct Param {
    name: String,
    kind: ParamKind,
    title: Option<String>,
    description: Option<String>,
    default: Option<Value>,
    required: bool,
}

#[derive(Clone, Debug)]
enum ParamKind {
    Text {
        min_chars: Option<u64>,
        max_chars: Option<u64>,
        multiline: bool,
    },
    Media {
        accept: Vec<String>,
        rules: Map<String, Value>,
    },
    Audio {
        rules: Map<String, Value>,
    },
    Number {
        min: Option<f64>,
        max: Option<f64>,
    },
    Bool,
    Enum(Vec<String>),
    Color,
    Font,
    List {
        min_items: Option<u64>,
        max_items: Option<u64>,
        item: Vec<Param>,
    },
    /// A compose node: what a wrapping template (a reveal, a frame) draws its effect on.
    Node,
}

impl ParamKind {
    const fn name(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Media { .. } => "media",
            Self::Audio { .. } => "audio",
            Self::Number { .. } => "number",
            Self::Bool => "bool",
            Self::Enum(_) => "enum",
            Self::Color => "color",
            Self::Font => "font",
            Self::List { .. } => "list",
            Self::Node => "node",
        }
    }
}

fn error(path: &str, message: impl Into<String>) -> ComposeError {
    ComposeError::new(path, message)
}

fn header(document: &Value) -> Result<Option<Header>, ComposeError> {
    let Some(value) = document.get("template") else {
        return Ok(None);
    };
    let path = "template.template";
    let object = value
        .as_object()
        .ok_or_else(|| error(path, "a template header is an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !["id", "version", "kind", "title", "description"].contains(&field.as_str()))
    {
        return Err(error(
            &format!("{path}.{field}"),
            "not a template header field",
        ));
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| error(&format!("{path}.id"), "a template has an `owner/name` id"))?;
    let parts = id.split('/').collect::<Vec<_>>();
    let well_named = |part: &&str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    if parts.len() != 2 || !parts.iter().all(well_named) {
        return Err(error(
            &format!("{path}.id"),
            format!("\"{id}\" is not `owner/name` in lower-case letters, digits, `-` and `_`"),
        ));
    }
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .filter(|version| *version > 0)
        .ok_or_else(|| {
            error(
                &format!("{path}.version"),
                "a version is a positive integer",
            )
        })?;
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("video") | None => Kind::Video,
        Some("node") => Kind::Node,
        Some(other) => {
            return Err(error(
                &format!("{path}.kind"),
                format!("a template is a `video` or a `node`, not \"{other}\""),
            ));
        }
    };
    let text = |field: &str| object.get(field).and_then(Value::as_str).map(str::to_owned);
    Ok(Some(Header {
        id: id.to_owned(),
        version,
        kind,
        title: text("title"),
        description: text("description"),
    }))
}

fn params(value: Option<&Value>, path: &str) -> Result<Vec<Param>, ComposeError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| error(path, "`params` is a list"))?;
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let at = format!("{path}[{index}]");
        let param = param(entry, &at)?;
        if !seen.insert(param.name.clone()) {
            return Err(error(
                &format!("{at}.name"),
                format!("{} is declared twice", param.name),
            ));
        }
        parsed.push(param);
    }
    Ok(parsed)
}

fn param(entry: &Value, path: &str) -> Result<Param, ComposeError> {
    let object = entry
        .as_object()
        .ok_or_else(|| error(path, "a parameter is an object"))?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| {
            let mut chars = name.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .ok_or_else(|| {
            error(
                &format!("{path}.name"),
                "a parameter's name is a letter or `_`, then letters, digits or `_`",
            )
        })?
        .to_owned();
    let kind = param_kind(object, path)?;
    let default = object.get("default").cloned();
    let required = object
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(default.is_none());
    let text = |field: &str| object.get(field).and_then(Value::as_str).map(str::to_owned);
    let param = Param {
        name,
        kind,
        title: text("title"),
        description: text("description"),
        default,
        required,
    };
    if let Some(default) = &param.default {
        check_value(&param, default, &format!("{path}.default"))?;
    }
    Ok(param)
}

/// A parameter's type and the checks it declares.
fn param_kind(object: &Map<String, Value>, path: &str) -> Result<ParamKind, ComposeError> {
    let u64_field = |field: &str| object.get(field).and_then(Value::as_u64);
    let f64_field = |field: &str| object.get(field).and_then(Value::as_f64);
    Ok(match object.get("type").and_then(Value::as_str) {
        Some("text") => ParamKind::Text {
            min_chars: u64_field("min_chars"),
            max_chars: u64_field("max_chars"),
            multiline: object
                .get("multiline")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        Some("media") => ParamKind::Media {
            accept: match object.get("accept") {
                None => vec!["image".to_owned(), "video".to_owned()],
                Some(Value::Array(kinds)) => kinds
                    .iter()
                    .map(|kind| match kind.as_str() {
                        Some(kind @ ("image" | "video")) => Ok(kind.to_owned()),
                        _ => Err(error(
                            &format!("{path}.accept"),
                            "accept lists `image` and `video`",
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => return Err(error(&format!("{path}.accept"), "accept is a list")),
            },
            rules: rules(
                object,
                &[
                    "aspect",
                    "min_width",
                    "min_height",
                    "min_duration",
                    "max_duration",
                ],
            ),
        },
        Some("audio") => ParamKind::Audio {
            rules: rules(object, &["min_duration", "max_duration"]),
        },
        Some("number") => ParamKind::Number {
            min: f64_field("min"),
            max: f64_field("max"),
        },
        Some("bool") => ParamKind::Bool,
        Some("enum") => {
            let values = object
                .get("values")
                .and_then(Value::as_array)
                .filter(|values| !values.is_empty())
                .ok_or_else(|| error(&format!("{path}.values"), "an enum lists its values"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .or_else(|| value.get("value").and_then(Value::as_str))
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            error(
                                &format!("{path}.values"),
                                "a value is a string or `{value, title}`",
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            ParamKind::Enum(values)
        }
        Some("color") => ParamKind::Color,
        Some("font") => ParamKind::Font,
        Some("node") => ParamKind::Node,
        Some("list") => ParamKind::List {
            min_items: u64_field("min_items"),
            max_items: u64_field("max_items"),
            item: params(object.get("item"), &format!("{path}.item"))?,
        },
        other => {
            return Err(error(
                &format!("{path}.type"),
                format!(
                    "{} is not a parameter type; the types are text, media, audio, number, bool, \
                     enum, color, font, list and node",
                    other.map_or_else(|| "nothing".to_owned(), |name| format!("\"{name}\""))
                ),
            ));
        }
    })
}

fn rules(object: &Map<String, Value>, names: &[&str]) -> Map<String, Value> {
    object
        .iter()
        .filter(|(field, _)| names.contains(&field.as_str()))
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect()
}

/// Checks `value` against `param` where it can be checked without facts about files.
fn check_value(param: &Param, value: &Value, path: &str) -> Result<(), ComposeError> {
    let wrong = || {
        error(
            path,
            format!(
                "{} takes a {} value, not {value}",
                param.name,
                param.kind.name()
            ),
        )
    };
    match &param.kind {
        ParamKind::Text {
            min_chars,
            max_chars,
            multiline,
        } => {
            let text = value.as_str().ok_or_else(wrong)?;
            let count = text.chars().count() as u64;
            if min_chars.is_some_and(|min| count < min) || max_chars.is_some_and(|max| count > max)
            {
                return Err(error(
                    path,
                    format!(
                        "{} takes {}{} characters, not {count}",
                        param.name,
                        min_chars.map_or_else(String::new, |min| format!("at least {min} ")),
                        max_chars.map_or_else(String::new, |max| format!("at most {max}"))
                    ),
                ));
            }
            if !multiline && text.contains('\n') {
                return Err(error(path, format!("{} is one line", param.name)));
            }
        }
        ParamKind::Media { .. } | ParamKind::Audio { .. } | ParamKind::Font => {
            if value.as_str().is_none_or(str::is_empty) {
                return Err(wrong());
            }
        }
        ParamKind::Number { min, max } => {
            let number = value.as_f64().ok_or_else(wrong)?;
            if min.is_some_and(|min| number < min) || max.is_some_and(|max| number > max) {
                return Err(error(
                    path,
                    format!("{} is out of its range: {number}", param.name),
                ));
            }
        }
        ParamKind::Bool => {
            value.as_bool().ok_or_else(wrong)?;
        }
        ParamKind::Enum(values) => {
            let chosen = value.as_str().ok_or_else(wrong)?;
            if !values.iter().any(|value| value == chosen) {
                return Err(error(
                    path,
                    format!(
                        "{} is one of {}, not \"{chosen}\"",
                        param.name,
                        values.join(", ")
                    ),
                ));
            }
        }
        ParamKind::Color => {
            serde_json::from_value::<crate::spec::SolidColor>(value.clone())
                .map_err(|_| wrong())?;
        }
        ParamKind::Node => {
            if value.get("type").and_then(Value::as_str).is_none() {
                return Err(wrong());
            }
        }
        ParamKind::List {
            min_items,
            max_items,
            item,
        } => {
            let items = value.as_array().ok_or_else(wrong)?;
            let count = items.len() as u64;
            if min_items.is_some_and(|min| count < min) || max_items.is_some_and(|max| count > max)
            {
                return Err(error(
                    path,
                    format!(
                        "{} holds a number of items out of its range: {count}",
                        param.name
                    ),
                ));
            }
            for (index, entry) in items.iter().enumerate() {
                resolve(item, entry, &format!("{path}[{index}]"))?;
            }
        }
    }
    Ok(())
}

/// The data for `params`: each given value checked, each missing one defaulted, refused when
/// required, or absent.
fn resolve(params: &[Param], data: &Value, path: &str) -> Result<Map<String, Value>, ComposeError> {
    let empty = Map::new();
    let given = match data {
        Value::Null => &empty,
        Value::Object(object) => object,
        _ => return Err(error(path, "data is an object of parameter values")),
    };
    if let Some(unknown) = given
        .keys()
        .find(|name| !params.iter().any(|param| &param.name == *name))
    {
        let names = params
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>();
        return Err(error(
            &format!("{path}.{unknown}"),
            format!(
                "{unknown} is not a parameter; they are {}",
                if names.is_empty() {
                    "none".to_owned()
                } else {
                    names.join(", ")
                }
            ),
        ));
    }
    let mut resolved = Map::new();
    for param in params {
        let at = format!("{path}.{}", param.name);
        match given.get(&param.name).filter(|value| !value.is_null()) {
            Some(value) => {
                check_value(param, value, &at)?;
                let value = match &param.kind {
                    ParamKind::List { item, .. } => Value::Array(
                        value
                            .as_array()
                            .into_iter()
                            .flatten()
                            .enumerate()
                            .map(|(index, entry)| {
                                resolve(item, entry, &format!("{at}[{index}]")).map(Value::Object)
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                    _ => value.clone(),
                };
                resolved.insert(param.name.clone(), value);
            }
            None => match &param.default {
                Some(default) => {
                    resolved.insert(param.name.clone(), default.clone());
                }
                None if param.required => {
                    return Err(error(&at, format!("{} is required", param.name)));
                }
                None => {}
            },
        }
    }
    Ok(resolved)
}

/// What evaluating a piece of the template gives.
enum Eval {
    Value(Value),
    Absent,
    /// Several values, spliced into the list that holds them.
    Splice(Vec<Value>),
}

/// The values in scope: parameters, and `$each` aliases over them.
#[derive(Clone, Default)]
struct Scope {
    values: Map<String, Value>,
    /// Set inside a template body, the only place directives and `{{name}}` are evaluated.
    template: bool,
}

impl Scope {
    /// The value at `name`, a dotted path from a parameter or alias.
    fn lookup(&self, name: &str) -> Option<&Value> {
        let mut parts = name.split('.');
        let mut value = self.values.get(parts.next()?)?;
        for part in parts {
            value = value.get(part)?;
        }
        Some(value)
    }
}

struct Binder<'a> {
    templates: BTreeMap<String, &'a Value>,
    stack: Vec<String>,
}

impl Binder<'_> {
    fn eval(
        &mut self,
        value: &Value,
        scope: &Scope,
        key: Option<&str>,
        path: &str,
    ) -> Result<Eval, ComposeError> {
        match value {
            Value::Object(object) => self.eval_object(object, scope, path),
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match self.eval(item, scope, None, &format!("{path}[{index}]"))? {
                        Eval::Value(value) => out.push(value),
                        Eval::Absent => {}
                        Eval::Splice(values) => out.extend(values),
                    }
                }
                Ok(Eval::Value(Value::Array(out)))
            }
            Value::String(text)
                if scope.template && !key.is_some_and(|key| NAME_KEYS.contains(&key)) =>
            {
                Ok(Eval::Value(Value::String(interpolate(text, scope, path)?)))
            }
            other => Ok(Eval::Value(other.clone())),
        }
    }

    /// A `$param`, `$if`, `$switch` or `$each` object evaluated, or `None` for any other object.
    fn directive(
        &mut self,
        object: &Map<String, Value>,
        scope: &Scope,
        path: &str,
    ) -> Result<Option<Eval>, ComposeError> {
        if let Some(name) = object.get("$param") {
            let name = name
                .as_str()
                .ok_or_else(|| error(path, "`$param` names a parameter"))?;
            if object.len() != 1 {
                return Err(error(path, "a `$param` object has no other fields"));
            }
            return Ok(Some(
                scope
                    .lookup(name)
                    .map_or(Eval::Absent, |value| Eval::Value(value.clone())),
            ));
        }
        if let Some(name) = object.get("$if") {
            let name = name
                .as_str()
                .ok_or_else(|| error(path, "`$if` names a parameter"))?;
            let branch = if is_set(scope.lookup(name)) {
                "then"
            } else {
                "else"
            };
            return match object.get(branch) {
                Some(node) => self
                    .eval(node, scope, None, &format!("{path}.{branch}"))
                    .map(Some),
                None => Ok(Some(Eval::Absent)),
            };
        }
        if let Some(name) = object.get("$switch") {
            let name = name
                .as_str()
                .ok_or_else(|| error(path, "`$switch` names an enum parameter"))?;
            let chosen = scope.lookup(name).and_then(Value::as_str);
            let cases = object.get("cases").and_then(Value::as_object);
            let picked = chosen
                .and_then(|chosen| {
                    cases
                        .and_then(|cases| cases.get(chosen))
                        .map(|node| (node, format!("{path}.cases.{chosen}")))
                })
                .or_else(|| {
                    object
                        .get("default")
                        .map(|node| (node, format!("{path}.default")))
                });
            return match picked {
                Some((node, at)) => self.eval(node, scope, None, &at).map(Some),
                None => Ok(Some(Eval::Absent)),
            };
        }
        if let Some(name) = object.get("$each") {
            let name = name
                .as_str()
                .ok_or_else(|| error(path, "`$each` names a list parameter"))?;
            let alias = object
                .get("as")
                .and_then(Value::as_str)
                .ok_or_else(|| error(path, "`$each` names each item with `as`"))?;
            let node = object
                .get("node")
                .ok_or_else(|| error(path, "`$each` repeats a `node`"))?;
            let items = scope
                .lookup(name)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.into_iter().enumerate() {
                let mut inner = scope.clone();
                let mut entry = item;
                if let Some(fields) = entry.as_object_mut() {
                    fields.insert("$index".to_owned(), json!(index));
                }
                inner.values.insert(alias.to_owned(), entry);
                match self.eval(node, &inner, None, &format!("{path}.node"))? {
                    Eval::Value(value) => out.push(value),
                    Eval::Absent => {}
                    Eval::Splice(values) => out.extend(values),
                }
            }
            return Ok(Some(Eval::Splice(out)));
        }
        Ok(None)
    }

    fn eval_object(
        &mut self,
        object: &Map<String, Value>,
        scope: &Scope,
        path: &str,
    ) -> Result<Eval, ComposeError> {
        if scope.template
            && let Some(eval) = self.directive(object, scope, path)?
        {
            return Ok(eval);
        }
        if let Some(kind) = object.get("type").and_then(Value::as_str) {
            if kind.contains('/') {
                return self.instance(kind, object, scope, path).map(Eval::Value);
            }
            if let Some(id) = bare_template(kind) {
                return self.bare(id, object, scope, path).map(Eval::Value);
            }
        }
        let mut out = Map::with_capacity(object.len());
        for (field, value) in object {
            let at = format!("{path}.{field}");
            match self.eval(value, scope, Some(field), &at)? {
                Eval::Value(value) => {
                    out.insert(field.clone(), value);
                }
                Eval::Absent => {}
                Eval::Splice(_) => return Err(error(&at, "`$each` goes in a list")),
            }
        }
        Ok(Eval::Value(Value::Object(out)))
    }

    /// A node template named by `id`, bound to the reference's `with`, as a stack carrying the
    /// reference's placement.
    fn instance(
        &mut self,
        id: &str,
        object: &Map<String, Value>,
        scope: &Scope,
        path: &str,
    ) -> Result<Value, ComposeError> {
        self.reference(id, object, scope, &format!("{path}.with"), path)
    }

    /// Binds a 1st-party template named by its bare type, whose other fields are its data.
    fn bare(
        &mut self,
        id: &str,
        object: &Map<String, Value>,
        scope: &Scope,
        path: &str,
    ) -> Result<Value, ComposeError> {
        let (mut reference, data): (Map<_, _>, Map<_, _>) = object
            .iter()
            .filter(|(field, _)| field.as_str() != "type")
            .map(|(field, value)| (field.clone(), value.clone()))
            .partition(|(field, _)| PLACEMENT.contains(&field.as_str()));
        reference.insert("type".to_owned(), json!(id));
        reference.insert("with".to_owned(), Value::Object(data));
        self.reference(id, &reference, scope, path, path)
    }

    /// [`Self::instance`], its data's errors named from `data_path`.
    fn reference(
        &mut self,
        id: &str,
        object: &Map<String, Value>,
        scope: &Scope,
        data_path: &str,
        path: &str,
    ) -> Result<Value, ComposeError> {
        if let Some(field) = object.keys().find(|field| {
            !["type", "with", "version"].contains(&field.as_str())
                && !PLACEMENT.contains(&field.as_str())
        }) {
            return Err(error(
                &format!("{path}.{field}"),
                "a template node takes `with`, `version` and placement fields",
            ));
        }
        let template = *self.templates.get(id).ok_or_else(|| {
            error(
                &format!("{path}.type"),
                format!("no template \"{id}\" was given"),
            )
        })?;
        let header = header(template)?.expect("templates are indexed by their headers");
        if header.kind != Kind::Node {
            return Err(error(
                &format!("{path}.type"),
                format!("{id} is a video template, not a node"),
            ));
        }
        if let Some(version) = object.get("version")
            && version.as_u64() != Some(header.version)
        {
            return Err(error(
                &format!("{path}.version"),
                format!("{id} is version {}, not {version}", header.version),
            ));
        }
        if self.stack.iter().any(|open| open == id) || self.stack.len() >= MAX_DEPTH {
            return Err(error(
                path,
                format!(
                    "{id} names itself through its templates, or they nest deeper than {MAX_DEPTH}"
                ),
            ));
        }
        let data = match object.get("with") {
            Some(with) => match self.eval(with, scope, None, &format!("{path}.with"))? {
                Eval::Value(value) => value,
                _ => Value::Null,
            },
            None => Value::Null,
        };
        self.stack.push(id.to_owned());
        let bound = self.bind_body(
            template,
            &header,
            &data,
            data_path,
            &format!("{path}<{id}>"),
        );
        self.stack.pop();
        let node = bound?;
        let mut stack = Map::new();
        stack.insert("type".to_owned(), json!("stack"));
        stack.insert("children".to_owned(), json!([node]));
        for field in PLACEMENT {
            if let Some(value) = object.get(field)
                && let Eval::Value(value) =
                    self.eval(value, scope, Some(field), &format!("{path}.{field}"))?
            {
                stack.insert(field.to_owned(), value);
            }
        }
        Ok(Value::Object(stack))
    }

    /// A template's body bound to `data`: a video template's spec, or a node template's node.
    fn bind_body(
        &mut self,
        template: &Value,
        header: &Header,
        data: &Value,
        data_path: &str,
        path: &str,
    ) -> Result<Value, ComposeError> {
        let params = params(template.get("params"), &format!("{path}.params"))?;
        let values = resolve(&params, data, data_path)?;
        let scope = Scope {
            values,
            template: true,
        };
        match header.kind {
            Kind::Node => {
                let node = template
                    .get("node")
                    .ok_or_else(|| error(path, "a node template has a `node`"))?;
                match self.eval(node, &scope, None, &format!("{path}.node"))? {
                    Eval::Value(value) => Ok(value),
                    _ => Err(error(
                        &format!("{path}.node"),
                        "a node template draws one node",
                    )),
                }
            }
            Kind::Video => {
                let mut body = template.as_object().cloned().unwrap_or_default();
                body.remove("template");
                body.remove("params");
                match self.eval(&Value::Object(body), &scope, None, path)? {
                    Eval::Value(value) => Ok(value),
                    _ => Err(error(path, "a video template is one spec")),
                }
            }
        }
    }
}

/// `text` with each `{{name}}` replaced by its parameter's text, number or choice; `{{{{` is a
/// literal `{{`.
fn interpolate(text: &str, scope: &Scope, path: &str) -> Result<String, ComposeError> {
    if !text.contains("{{") {
        return Ok(text.to_owned());
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(stripped) = after.strip_prefix("{{") {
            out.push_str("{{");
            rest = stripped;
            continue;
        }
        let end = after
            .find("}}")
            .ok_or_else(|| error(path, "a `{{` has no `}}`"))?;
        let name = after[..end].trim();
        match scope.lookup(name) {
            None => {}
            Some(Value::String(value)) => out.push_str(value),
            Some(Value::Number(number)) => out.push_str(&format_number(number)),
            Some(Value::Bool(flag)) => out.push_str(if *flag { "true" } else { "false" }),
            Some(_) => {
                return Err(error(
                    path,
                    format!("{{{{{name}}}}} is not text, a number or a choice"),
                ));
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A number as decimal text: whole numbers without a fraction.
fn format_number(number: &serde_json::Number) -> String {
    match number.as_f64() {
        Some(value) if value.fract() == 0.0 && value.abs() < 1e15 => format!("{value:.0}"),
        _ => number.to_string(),
    }
}

fn index(templates: &[Value]) -> Result<BTreeMap<String, &Value>, ComposeError> {
    let mut found = BTreeMap::new();
    for (position, template) in templates.iter().enumerate() {
        let header = header(template)
            .map_err(|problem| {
                error(
                    &format!("templates[{position}].{}", problem.path),
                    problem.message,
                )
            })?
            .ok_or_else(|| {
                error(
                    &format!("templates[{position}]"),
                    "a template has a `template` header",
                )
            })?;
        found.insert(header.id, template);
    }
    for template in crate::library::TEMPLATES.iter() {
        let header = header(template)?.expect("a built-in template has a header");
        found.entry(header.id).or_insert(template);
    }
    Ok(found)
}

/// Binds `document` to `data`: a template's parameters replaced and its template nodes
/// expanded, giving a plain spec (or, for a node template, a plain node). A document without a
/// header has only its template nodes expanded; directives and `{{name}}` exist only in template
/// bodies. Without a header, template nodes or a theme, `bind` returns `document` unchanged.
///
/// # Errors
/// Returns where in the template or data, and why, it does not bind.
pub fn bind(document: &Value, data: &Value, options: &BindOptions) -> Result<Value, ComposeError> {
    let mut binder = Binder {
        templates: index(&options.templates)?,
        stack: Vec::new(),
    };
    let header = header(document)?;
    let mut bound = match &header {
        Some(header) => {
            binder.stack.push(header.id.clone());
            binder.bind_body(document, header, data, "data", "template")?
        }
        None => match binder.eval(document, &Scope::default(), None, "spec")? {
            Eval::Value(value) => value,
            _ => return Err(error("spec", "a spec is one object")),
        },
    };
    if header
        .as_ref()
        .is_none_or(|header| header.kind == Kind::Video)
        && let Some(theme) = &options.theme
        && let Some(object) = bound.as_object_mut()
    {
        object.insert("theme".to_owned(), theme.clone());
    }
    Ok(bound)
}

/// Fields of a template node that place it, beside its template's data.
const PLACEMENT: [&str; 6] = ["id", "at", "duration", "inset", "enter", "exit"];

/// Node types that name a 1st-party template without its `alpha2/` owner.
pub(crate) const BARE: [(&str, &str); 7] = [
    ("title", "alpha2/title"),
    ("title_card", "alpha2/title-card"),
    ("lower_third", "alpha2/lower-third"),
    ("end_card", "alpha2/end-card"),
    ("logo", "alpha2/logo"),
    ("reframe", "alpha2/reframe"),
    ("comparison_grid", "alpha2/comparison-grid"),
];

fn bare_template(kind: &str) -> Option<&'static str> {
    BARE.iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, id)| *id)
}

/// Whether `$if` takes its `then`, for a true bool or any other value that is given and not empty.
fn is_set(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
        Some(Value::Number(_)) => true,
    }
}

/// The JSON Schema (2020-12) of a template's data, with each parameter's title, description,
/// default and checks a schema can say; file rules are in `x-alpha2-media`.
///
/// # Errors
/// Returns where the template's header or parameters do not parse.
///
/// # Panics
/// Never; an object schema is always an object.
pub fn data_schema(template: &Value) -> Result<Value, ComposeError> {
    let header =
        header(template)?.ok_or_else(|| error("template", "a template has a `template` header"))?;
    let params = params(template.get("params"), "template.params")?;
    let mut schema = object_schema(&params);
    let object = schema.as_object_mut().expect("an object schema");
    object.insert(
        "$schema".to_owned(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    if let Some(name) = header.id.strip_prefix("alpha2/") {
        object.insert(
            "$id".to_owned(),
            json!(format!(
                "https://alpha2.sh/templates/{name}/v{}/data.json",
                header.version
            )),
        );
    }
    object.insert("title".to_owned(), json!(header.title.unwrap_or(header.id)));
    if let Some(description) = header.description {
        object.insert("description".to_owned(), json!(description));
    }
    Ok(schema)
}

/// Where a data schema's references to compose types point.
#[derive(Clone, Copy)]
enum Refs {
    /// The published compose schema, for a data schema of its own.
    Published,
    /// `#/$defs`, inside the compose schema.
    #[cfg_attr(not(feature = "schema"), allow(dead_code))]
    Local,
}

impl Refs {
    fn to(self, name: &str) -> Value {
        match self {
            Self::Published => {
                json!({"$ref": format!("{}#/$defs/{name}", crate::spec::COMPOSE_SCHEMA_ID)})
            }
            Self::Local => json!({"$ref": format!("#/$defs/{name}")}),
        }
    }
}

/// Compose schema variants for template nodes. Each 1st-party template gets typed variants for
/// its `alpha2/` id and bare name, and one variant takes any other `owner/name`.
#[cfg(feature = "schema")]
pub(crate) fn node_schemas() -> Vec<Value> {
    let placement = |properties: &mut Map<String, Value>| {
        properties.insert("id".to_owned(), json!({"type": "string", "description": "The node's name, for other nodes to refer to."}));
        properties.insert("at".to_owned(), json!({"type": "number", "description": "Seconds from the parent's start (an overlay's: the video's). Ignored in a sequence."}));
        properties.insert("duration".to_owned(), json!({"type": "number", "description": "Length in seconds. Default: the template's own, else what its parent gives it."}));
        properties.insert("inset".to_owned(), Refs::Local.to("Inset"));
        properties.insert("enter".to_owned(), Refs::Local.to("Motion"));
        properties.insert("exit".to_owned(), Refs::Local.to("Motion"));
    };
    let mut variants = Vec::new();
    for template in crate::library::TEMPLATES.iter() {
        let header = header(template)
            .ok()
            .flatten()
            .expect("a built-in template has a header");
        let params = params(template.get("params"), "template.params")
            .expect("a built-in template's parameters parse");
        let data = object_schema_with(&params, Refs::Local);
        let description = header.description.clone().unwrap_or_default();
        if let Some((name, _)) = BARE.iter().find(|(_, id)| *id == header.id) {
            let mut properties = data["properties"].as_object().cloned().unwrap_or_default();
            properties.insert("type".to_owned(), json!({"const": name}));
            placement(&mut properties);
            let mut required = data["required"].as_array().cloned().unwrap_or_default();
            required.insert(0, json!("type"));
            variants.push(json!({
                "type": "object",
                "description": description,
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            }));
        }
        let mut properties = Map::new();
        properties.insert("type".to_owned(), json!({"const": header.id}));
        properties.insert("version".to_owned(), json!({"const": header.version}));
        properties.insert("with".to_owned(), data.clone());
        placement(&mut properties);
        let required = if data["required"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
        {
            json!(["type", "with"])
        } else {
            json!(["type"])
        };
        variants.push(json!({
            "type": "object",
            "description": description,
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }));
    }
    let mut properties = Map::new();
    properties.insert(
        "type".to_owned(),
        json!({
            "type": "string",
            "pattern": "^[a-z0-9_-]+/[a-z0-9_-]+$",
            "not": {"pattern": "^alpha2/"},
            "description": "A template's `owner/name` id; the spec is compiled with it.",
        }),
    );
    properties.insert(
        "version".to_owned(),
        json!({"type": "integer", "minimum": 1}),
    );
    properties.insert(
        "with".to_owned(),
        json!({"type": "object", "description": "The template's data."}),
    );
    placement(&mut properties);
    variants.push(json!({
        "type": "object",
        "description": "A node template by its id, bound to `with`.",
        "properties": properties,
        "required": ["type"],
        "additionalProperties": false,
    }));
    variants
}

fn object_schema(params: &[Param]) -> Value {
    object_schema_with(params, Refs::Published)
}

fn object_schema_with(params: &[Param], refs: Refs) -> Value {
    let properties = params
        .iter()
        .map(|param| (param.name.clone(), param_schema(param, refs)))
        .collect::<Map<_, _>>();
    let required = params
        .iter()
        .filter(|param| param.required && param.default.is_none())
        .map(|param| json!(param.name))
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn param_schema(param: &Param, refs: Refs) -> Value {
    let mut schema = match &param.kind {
        ParamKind::Text {
            min_chars,
            max_chars,
            ..
        } => json!({"type": "string", "minLength": min_chars, "maxLength": max_chars}),
        ParamKind::Media { accept, rules } => {
            let mut media = rules.clone();
            media.insert("accept".to_owned(), json!(accept));
            json!({"type": "string", "format": "uri-reference", "x-alpha2-media": media})
        }
        ParamKind::Audio { rules } => {
            json!({"type": "string", "format": "uri-reference", "x-alpha2-audio": rules})
        }
        ParamKind::Number { min, max } => json!({"type": "number", "minimum": min, "maximum": max}),
        ParamKind::Bool => json!({"type": "boolean"}),
        ParamKind::Enum(values) => json!({"enum": values}),
        ParamKind::Color => refs.to("SolidColor"),
        ParamKind::Font => json!({"type": "string", "description": "A font family the host has."}),
        ParamKind::Node => refs.to("Node"),
        ParamKind::List {
            min_items,
            max_items,
            item,
        } => json!({
            "type": "array",
            "items": object_schema_with(item, refs),
            "minItems": min_items,
            "maxItems": max_items,
        }),
    };
    let object = schema.as_object_mut().expect("an object schema");
    object.retain(|_, value| !value.is_null());
    if let Some(title) = &param.title {
        object.insert("title".to_owned(), json!(title));
    }
    if let Some(description) = &param.description {
        object.insert("description".to_owned(), json!(description));
    }
    if let Some(default) = &param.default {
        object.insert("default".to_owned(), default.clone());
    }
    schema
}

/// Problems with a template, and with `data` for it when given: its header and parameters, the
/// parameters its body names, literal content a template leaves to its data or theme, and
/// whether it binds to a spec that parses. Without data it binds to sample data.
#[must_use]
pub fn lint(template: &Value, data: Option<&Value>, templates: &[Value]) -> Vec<Issue> {
    let mut issues = Vec::new();
    let fail = |issues: &mut Vec<Issue>, code, problem: ComposeError| {
        issues.push(Issue {
            path: problem.path,
            code,
            message: problem.message,
            severity: Severity::Error,
        });
    };
    let header = match header(template) {
        Ok(Some(header)) => header,
        Ok(None) => {
            fail(
                &mut issues,
                "header",
                error("template", "a template has a `template` header"),
            );
            return issues;
        }
        Err(problem) => {
            fail(&mut issues, "header", problem);
            return issues;
        }
    };
    let declared = match params(template.get("params"), "template.params") {
        Ok(params) => params,
        Err(problem) => {
            fail(&mut issues, "params", problem);
            return issues;
        }
    };
    let body = match header.kind {
        Kind::Node => template.get("node").cloned().unwrap_or(Value::Null),
        Kind::Video => {
            let mut body = template.as_object().cloned().unwrap_or_default();
            body.remove("template");
            body.remove("params");
            Value::Object(body)
        }
    };
    let mut used = BTreeSet::new();
    let mut scope = declared
        .iter()
        .map(|param| param.name.clone())
        .collect::<BTreeSet<_>>();
    names(
        &body,
        &mut scope,
        &mut used,
        "template",
        &mut issues,
        &declared,
    );
    for param in &declared {
        if !used.contains(&param.name) {
            issues.push(Issue {
                path: format!("template.params.{}", param.name),
                code: "unused-param",
                message: format!("{} is declared but never used", param.name),
                severity: Severity::Warning,
            });
        }
    }
    content(&body, None, "template", &mut issues);
    let sample = data.cloned().unwrap_or_else(|| sample_data(&declared));
    let options = BindOptions {
        theme: None,
        templates: templates.to_vec(),
    };
    match bind(template, &sample, &options) {
        Err(problem) => fail(&mut issues, "bind", problem),
        Ok(bound) => {
            let parsed = match header.kind {
                Kind::Video => crate::parse_spec(&bound.to_string()).map(|_| ()),
                Kind::Node => crate::de::from_value::<crate::spec::Node>(bound)
                    .map(|_| ())
                    .map_err(|located| {
                        error(&crate::de::join("node", &located.path), located.message)
                    }),
            };
            if let Err(problem) = parsed {
                fail(&mut issues, "spec", problem);
            }
        }
    }
    issues
}

/// Notes the parameters `value` names, flagging names no parameter or alias has and directives
/// on parameters of the wrong type.
fn names(
    value: &Value,
    scope: &mut BTreeSet<String>,
    used: &mut BTreeSet<String>,
    path: &str,
    issues: &mut Vec<Issue>,
    declared: &[Param],
) {
    let mut note = |name: &str, path: &str, want: Option<&str>, issues: &mut Vec<Issue>| {
        let root = name.split('.').next().unwrap_or(name);
        used.insert(root.to_owned());
        if !scope.contains(root) {
            issues.push(Issue {
                path: path.to_owned(),
                code: "unknown-param",
                message: format!("{name} is not a parameter"),
                severity: Severity::Error,
            });
        } else if let Some(want) = want
            && let Some(param) = declared.iter().find(|param| param.name == root)
            && !name.contains('.')
            && param.kind.name() != want
        {
            issues.push(Issue {
                path: path.to_owned(),
                code: "param-type",
                message: format!("{name} is a {} parameter, not a {want}", param.kind.name()),
                severity: Severity::Error,
            });
        }
    };
    match value {
        Value::Object(object) => {
            for (directive, want) in [
                ("$param", None),
                ("$if", None),
                ("$switch", Some("enum")),
                ("$each", Some("list")),
            ] {
                if let Some(name) = object.get(directive).and_then(Value::as_str) {
                    note(name, path, want, issues);
                }
            }
            let alias = object
                .get("$each")
                .and(object.get("as"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let added = alias
                .as_ref()
                .is_some_and(|alias| scope.insert(alias.clone()));
            for (field, inner) in object {
                if field.starts_with('$') && field != "$theme" {
                    continue;
                }
                names(
                    inner,
                    scope,
                    used,
                    &format!("{path}.{field}"),
                    issues,
                    declared,
                );
            }
            if let Some(node) = object.get("node").filter(|_| alias.is_some()) {
                names(node, scope, used, &format!("{path}.node"), issues, declared);
            }
            if added && let Some(alias) = alias {
                scope.remove(&alias);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                names(
                    item,
                    scope,
                    used,
                    &format!("{path}[{index}]"),
                    issues,
                    declared,
                );
            }
        }
        Value::String(text) => {
            let mut rest = text.as_str();
            while let Some(start) = rest.find("{{") {
                let after = &rest[start + 2..];
                if let Some(stripped) = after.strip_prefix("{{") {
                    rest = stripped;
                    continue;
                }
                let Some(end) = after.find("}}") else { break };
                note(after[..end].trim(), path, None, issues);
                rest = &after[end + 2..];
            }
        }
        _ => {}
    }
}

/// Flags literal files, colors and copy, which a template takes from its data and theme.
fn content(value: &Value, key: Option<&str>, path: &str, issues: &mut Vec<Issue>) {
    let mut flag = |code, message: String| {
        issues.push(Issue {
            path: path.to_owned(),
            code,
            message,
            severity: Severity::Error,
        });
    };
    match value {
        Value::Object(object) => {
            if object.contains_key("$param") || object.contains_key("$theme") {
                return;
            }
            if let Ok(color) = serde_json::from_value::<alpha2_model::Rgba>(value.clone())
                && object.contains_key("red")
                && !structural(color)
            {
                flag(
                    "literal-color",
                    "a template takes colors from its theme or data".to_owned(),
                );
                return;
            }
            for (field, inner) in object {
                content(inner, Some(field), &format!("{path}.{field}"), issues);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                content(item, key, &format!("{path}[{index}]"), issues);
            }
        }
        Value::String(text) => {
            let key = key.unwrap_or_default();
            if FILE_KEYS.contains(&key) {
                flag(
                    "literal-media",
                    format!("a template takes files from its data, not \"{text}\""),
                );
            } else if text.starts_with('#')
                && let Some(color) = alpha2_model::Rgba::from_hex(text)
                && !structural(color)
            {
                flag(
                    "literal-color",
                    format!("a template takes colors from its theme or data, not {text}"),
                );
            } else if TEXT_KEYS.contains(&key) && literal_copy(text) {
                flag(
                    "literal-text",
                    format!("a template takes its words from its data, not \"{text}\""),
                );
            }
        }
        _ => {}
    }
}

/// Whether `color` is structural: transparent, or black or white at any opacity.
#[allow(clippy::float_cmp)]
fn structural(color: alpha2_model::Rgba) -> bool {
    color.alpha == 0.0
        || [color.red, color.green, color.blue]
            .iter()
            .all(|part| *part == 0.0)
        || [color.red, color.green, color.blue]
            .iter()
            .all(|part| *part == 1.0)
}

/// Whether `text`, its `{{...}}` taken out, still holds a letter or digit.
fn literal_copy(text: &str) -> bool {
    let mut rest = text;
    let mut plain = String::new();
    while let Some(start) = rest.find("{{") {
        plain.push_str(&rest[..start]);
        match rest[start + 2..].find("}}") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
            }
        }
    }
    plain.push_str(rest);
    plain.chars().any(char::is_alphanumeric)
}

/// Data that satisfies `params`, for checking a template without data of its own.
fn sample_data(params: &[Param]) -> Value {
    let mut data = Map::new();
    for param in params {
        if param.default.is_some() || !param.required {
            continue;
        }
        let value = match &param.kind {
            ParamKind::Text {
                min_chars,
                max_chars,
                ..
            } => {
                let length = min_chars
                    .unwrap_or(4)
                    .max(1)
                    .min(max_chars.unwrap_or(4).max(1));
                json!("x".repeat(usize::try_from(length).unwrap_or(4)))
            }
            ParamKind::Media { accept, .. } => {
                json!(if accept.iter().any(|kind| kind == "video") {
                    "sample.mp4"
                } else {
                    "sample.png"
                })
            }
            ParamKind::Audio { .. } => json!("sample.m4a"),
            ParamKind::Number { min, max } => json!(min.or(*max).unwrap_or(0.0)),
            ParamKind::Bool => json!(false),
            ParamKind::Enum(values) => json!(values[0]),
            ParamKind::Color => json!("#808080"),
            ParamKind::Font => json!("sans-serif"),
            ParamKind::Node => json!({"type": "color"}),
            ParamKind::List {
                min_items, item, ..
            } => {
                let count = usize::try_from(min_items.unwrap_or(1).max(1)).unwrap_or(1);
                Value::Array(vec![sample_data(item); count])
            }
        };
        data.insert(param.name.clone(), value);
    }
    Value::Object(data)
}

#[cfg(test)]
mod tests;
