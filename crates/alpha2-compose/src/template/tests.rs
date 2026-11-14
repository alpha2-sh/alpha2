use super::*;

fn promo() -> Value {
    json!({
        "compose_version": 1,
        "template": {"id": "acme/promo", "version": 2, "kind": "video", "title": "Promo"},
        "params": [
            {"name": "headline", "type": "text", "max_chars": 20},
            {"name": "price", "type": "number", "min": 0, "required": false},
            {"name": "photo", "type": "media", "accept": ["image"]},
            {"name": "show_badge", "type": "bool", "default": false},
            {"name": "layout", "type": "enum", "values": ["wide", "tall"], "default": "wide"},
            {"name": "tags", "type": "list", "max_items": 3, "item": [{"name": "label", "type": "text"}]},
        ],
        "canvas": {"$switch": "layout", "cases": {"wide": "youtube-1080p", "tall": "story"}},
        "duration": 3,
        "body": {
            "type": "stack",
            "children": [
                {"type": "media", "path": {"$param": "photo"}, "fit": "cover"},
                {"type": "text", "text": "{{headline}} only {{price}}"},
                {"$if": "show_badge", "then": {"type": "shape", "shapes": [{"shape": "ellipse"}]}},
                {"$each": "tags", "as": "tag", "node": {"type": "text", "text": "#{{tag.label}}"}},
            ],
        },
    })
}

#[test]
fn a_video_template_binds_its_data_into_a_plain_spec() {
    let data = json!({
        "headline": "Summer",
        "price": 30,
        "photo": "shot.jpg",
        "show_badge": true,
        "layout": "tall",
        "tags": [{"label": "new"}, {"label": "sale"}],
    });
    let spec = bind(&promo(), &data, &BindOptions::default()).expect("binds");
    assert!(!spec.to_string().contains('$'));
    assert_eq!(spec["canvas"], "story");
    let children = spec["body"]["children"].as_array().expect("children");
    assert_eq!(children[0]["path"], "shot.jpg");
    assert_eq!(children[1]["text"], "Summer only 30");
    assert_eq!(children[2]["type"], "shape");
    assert_eq!(children[3]["text"], "#new");
    assert_eq!(children[4]["text"], "#sale");
    assert!(crate::parse_spec(&spec.to_string()).is_ok());
}

#[test]
fn defaults_fill_in_and_absent_optional_values_drop_out() {
    let data = json!({"headline": "Hi", "photo": "a.jpg", "tags": []});
    let spec = bind(&promo(), &data, &BindOptions::default()).expect("binds");
    let children = spec["body"]["children"].as_array().expect("children");
    // No badge, no tags, and the missing price is empty text.
    assert_eq!(children.len(), 2);
    assert_eq!(children[1]["text"], "Hi only ");
    assert_eq!(spec["canvas"], "youtube-1080p");
}

#[test]
fn data_is_checked_against_the_parameters() {
    let refuse = |data: Value| {
        bind(&promo(), &data, &BindOptions::default())
            .expect_err("refused")
            .to_string()
    };
    assert!(refuse(json!({"photo": "a.jpg", "tags": []})).contains("headline is required"));
    assert!(
        refuse(json!({"headline": "x".repeat(21), "photo": "a.jpg", "tags": []}))
            .contains("at most 20")
    );
    assert!(
        refuse(json!({"headline": "Hi", "photo": "a.jpg", "tags": [], "colour": "red"}))
            .contains("not a parameter")
    );
    assert!(
        refuse(json!({"headline": "Hi", "photo": "a.jpg", "tags": [], "layout": "square"}))
            .contains("wide, tall")
    );
    assert!(
        refuse(json!({"headline": "Hi", "photo": "a.jpg", "tags": [{}, {}, {}, {}]}))
            .contains("items")
    );
    assert!(
        refuse(json!({"headline": "Hi", "photo": "a.jpg", "tags": [{"label": 3}]}))
            .contains("text")
    );
}

#[test]
fn a_node_template_is_a_node_with_its_placement_and_theme_overrides_a_video_templates() {
    let card = json!({
        "template": {"id": "acme/card", "version": 1, "kind": "node"},
        "params": [{"name": "title", "type": "text"}],
        "node": {"type": "text", "text": {"$param": "title"}},
    });
    let spec = json!({
        "duration": 2,
        "body": {"type": "acme/card", "with": {"title": "Hello"}, "at": 0.5, "inset": 10},
    });
    let options = BindOptions {
        theme: Some(json!("clean-light")),
        templates: vec![card],
    };
    let bound = bind(&spec, &Value::Null, &options).expect("expands");
    assert_eq!(bound["theme"], "clean-light");
    assert_eq!(bound["body"]["type"], "stack");
    assert_eq!(bound["body"]["at"], 0.5);
    assert_eq!(bound["body"]["children"][0]["text"], "Hello");
    let missing = bind(&spec, &Value::Null, &BindOptions::default()).expect_err("no template");
    assert!(missing.message.contains("no template"));
}

#[test]
fn if_takes_then_for_a_true_bool_or_a_value_that_is_given_and_not_empty() {
    let template = json!({
        "template": {"id": "acme/t", "version": 1, "kind": "node"},
        "params": [{"name": "subtitle", "type": "text", "required": false}],
        "node": {"type": "stack", "children": [
            {"$if": "subtitle", "then": {"type": "text", "text": "{{subtitle}}"}},
        ]},
    });
    let children = |data: Value| {
        bind(&template, &data, &BindOptions::default()).expect("binds")["children"]
            .as_array()
            .expect("children")
            .len()
    };
    assert_eq!(children(json!({"subtitle": "Hi"})), 1);
    assert_eq!(children(json!({"subtitle": ""})), 0);
    assert_eq!(children(json!({})), 0);
    assert!(
        lint(&template, None, &[])
            .iter()
            .all(|issue| issue.code != "param-type")
    );
}

#[test]
fn a_template_cannot_name_itself() {
    let looping = json!({
        "template": {"id": "acme/loop", "version": 1, "kind": "node"},
        "node": {"type": "acme/loop"},
    });
    let spec = json!({"duration": 1, "body": {"type": "acme/loop"}});
    let options = BindOptions {
        theme: None,
        templates: vec![looping],
    };
    assert!(
        bind(&spec, &Value::Null, &options)
            .expect_err("a cycle")
            .message
            .contains("names itself")
    );
}

#[test]
fn the_built_in_reveals_wrap_any_node() {
    let spec = json!({
        "duration": 2,
        "body": {"type": "alpha2/wipe-reveal", "with": {
            "content": {"type": "color", "color": "#ff0000"},
            "from": "top",
        }},
    });
    let bound = bind(&spec, &Value::Null, &BindOptions::default()).expect("binds");
    let mask = &bound["body"]["children"][0];
    assert_eq!(mask["type"], "mask");
    assert_eq!(mask["shapes"][0]["center"]["from"], json!([0.5, 0]));
    assert!(crate::parse_spec(&bound.to_string()).is_ok());
    for template in crate::library::TEMPLATES.iter() {
        let issues = lint(template, None, &[]);
        assert!(
            issues
                .iter()
                .all(|issue| issue.severity == Severity::Warning),
            "{issues:?}"
        );
    }
}

#[test]
fn the_data_schema_says_what_each_parameter_takes() {
    let schema = data_schema(&promo()).expect("a schema");
    assert_eq!(schema["title"], "Promo");
    assert_eq!(schema["properties"]["headline"]["maxLength"], 20);
    assert_eq!(
        schema["properties"]["layout"]["enum"],
        json!(["wide", "tall"])
    );
    assert_eq!(
        schema["properties"]["photo"]["x-alpha2-media"]["accept"],
        json!(["image"])
    );
    assert_eq!(
        schema["properties"]["tags"]["items"]["properties"]["label"]["type"],
        "string"
    );
    let required = schema["required"].as_array().expect("required");
    assert!(required.contains(&json!("headline")) && !required.contains(&json!("price")));
    assert_eq!(schema["additionalProperties"], false);
}

#[test]
fn lint_finds_unknown_names_unused_parameters_and_literal_content() {
    let template = json!({
        "template": {"id": "acme/bad", "version": 1, "kind": "node"},
        "params": [
            {"name": "title", "type": "text"},
            {"name": "unused", "type": "bool", "default": true},
            {"name": "flag", "type": "text"},
        ],
        "node": {"type": "stack", "children": [
            {"type": "text", "text": "{{title}} {{subtitle}}"},
            {"type": "text", "text": "Buy now"},
            {"type": "media", "path": "logo.png"},
            {"type": "color", "color": "#ff3366"},
            {"type": "color", "color": "#000000"},
            {"$switch": "flag", "cases": {"a": {"type": "color"}}},
        ]},
    });
    let issues = lint(&template, None, &[]);
    let codes = issues.iter().map(|issue| issue.code).collect::<Vec<_>>();
    assert!(codes.contains(&"unknown-param"), "{issues:?}");
    assert!(codes.contains(&"unused-param"), "{issues:?}");
    assert!(codes.contains(&"literal-text"), "{issues:?}");
    assert!(codes.contains(&"literal-media"), "{issues:?}");
    assert!(codes.contains(&"param-type"), "{issues:?}");
    assert_eq!(
        codes
            .iter()
            .filter(|code| **code == "literal-color")
            .count(),
        1,
        "{issues:?}"
    );
}

fn holds_template_node(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(holds_template_node),
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.contains('/') || bare_template(kind).is_some())
                || object.values().any(holds_template_node)
        }
        _ => false,
    }
}

#[test]
fn a_spec_without_template_nodes_binds_to_itself() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/old-specs.golden.json");
    let cases: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(path).expect("golden")).expect("JSON");
    let mut plain = 0;
    for spec in cases.iter().map(|case| &case["spec"]) {
        if !holds_template_node(spec) {
            assert_eq!(
                &bind(spec, &Value::Null, &BindOptions::default()).expect("binds"),
                spec
            );
            plain += 1;
        }
    }
    assert!(plain > 0);
}
