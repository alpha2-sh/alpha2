//! Panic reporting for the browser binding.
//!
//! `wasm32-unknown-unknown` has no stderr, so Rust's default hook formats each
//! panic and then discards it. Without a replacement the host observes only
//! `RuntimeError: unreachable`, which is identical for an arithmetic overflow,
//! an out-of-bounds index, and the `already borrowed` failure that every later
//! call produces once a panic has poisoned an exported object. The binary
//! already carries the messages and source locations; this routes them out.

use std::sync::Once;
use wasm_bindgen::prelude::*;

/// Installs the panic reporter as the module is instantiated.
///
/// `wasm-bindgen` calls this from the generated initializer, so every entry
/// point is covered without the host opting in.
#[wasm_bindgen(start)]
pub fn start() {
    install();
}

/// Forwards panic messages and source locations to `console.error`.
///
/// The panic still aborts; this only makes the cause observable. Reporting
/// through an `Error` gives the console a stack alongside the message.
fn install() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let error = web_sys::js_sys::Error::new(&format!("alpha2 panicked: {info}"));
            error.set_name("Alpha2Panic");
            web_sys::console::error_1(error.as_ref());
        }));
    });
}
