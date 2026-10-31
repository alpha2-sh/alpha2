//! Panic reporting for the browser binding.
//!
//! `wasm32-unknown-unknown` has no stderr, so the default hook discards every panic and the
//! host sees only `RuntimeError: unreachable`, followed by `already borrowed` on later calls to
//! a poisoned object. This routes the message and location to the console.

use std::sync::Once;
use wasm_bindgen::prelude::*;

/// Installs the panic reporter when the module is instantiated, so every entry point is
/// covered without the host opting in.
#[wasm_bindgen(start)]
pub fn start() {
    install();
}

/// Forwards panic messages and locations to `console.error`, through an `Error` so the console
/// shows a stack. The panic still aborts.
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
