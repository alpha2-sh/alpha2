#![no_main]

//! A request header through the host's parsers, as `serve` reads one.

use alpha2_host_core::requests::{
    PROTOCOL_VERSION, Request, operation_from_header, parse_cancel, parse_request, parse_status,
    request_id_from_header,
};
use libfuzzer_sys::fuzz_target;

const CODES: [&str; 3] = ["INVALID_REQUEST", "INVALID_INPUT", "UNSUPPORTED_PROTOCOL"];

fuzz_target!(|bytes: &[u8]| {
    let Ok(line) = std::str::from_utf8(bytes) else {
        return;
    };
    let id = request_id_from_header(line);
    let operation = operation_from_header(line);
    match parse_request(line) {
        Ok(request) => {
            assert_eq!(request.protocol_version(), PROTOCOL_VERSION);
            assert_eq!(
                request.request_id(),
                id,
                "the reader names the request it answers"
            );
            let name = match &request {
                Request::Capabilities { .. } => "capabilities",
                Request::Export(export) => {
                    export
                        .check()
                        .expect("an accepted export passes its check again");
                    export
                        .format()
                        .expect("an accepted export resolves its format");
                    "export"
                }
                Request::Conform(_) => "conform",
                Request::Still(still) => {
                    still
                        .check()
                        .expect("an accepted still passes its check again");
                    "still"
                }
                Request::Compose(_) => "compose",
                Request::Edit(_) => "edit",
                Request::Extract(extract) => {
                    extract
                        .check()
                        .expect("an accepted extract passes its check again");
                    "extract"
                }
            };
            assert_eq!(operation.as_deref(), Some(name));
        }
        Err(error) => assert!(CODES.contains(&error.code), "{error}"),
    }
    if let Ok(cancel) = parse_cancel(line) {
        assert_eq!(cancel.request_id, id);
        assert!(!cancel.target.is_empty());
    }
    if let Ok(status) = parse_status(line) {
        assert_eq!(status, id);
    }
});
