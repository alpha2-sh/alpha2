use super::*;

#[test]
fn a_control_request_is_a_json_object_not_its_fields_in_order() {
    assert_eq!(
        parse_status(r#"{"protocolVersion": 2, "requestId": "r", "operation": "status"}"#),
        Ok("r".to_owned())
    );
    assert!(parse_cancel(r#"{"protocolVersion": 2, "requestId": "c", "target": "r"}"#).is_ok());
    let status = r#"[2, "r", 0]"#;
    let cancel = r#" [2, "c", "r"]"#;
    for (line, error) in [
        (status, parse_status(status).err()),
        (cancel, parse_cancel(cancel).err()),
    ] {
        assert_eq!(error, parse_request(line).err(), "{line}");
    }
}
