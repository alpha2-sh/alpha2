#![no_main]

//! PDF documents through the PDF provider: probe, open the page the probe describes, render (see
//! `extension_common`).

#[path = "extension_common.rs"]
mod common;

use alpha2_extensions::PdfProvider;
use alpha2_model::{ProviderSource, Rgba};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    common::exercise(&PdfProvider, bytes, |report| ProviderSource::Pdf {
        page: report
            .item
            .as_deref()
            .and_then(|item| item.parse().ok())
            .unwrap_or(1),
        background: Rgba::new(1.0, 1.0, 1.0, 1.0),
    });
});
