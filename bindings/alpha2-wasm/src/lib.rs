//! Web binding public API.

// The release is built with SIMD (`.cargo/config.toml`); a `RUSTFLAGS` override would silently
// ship a slower module.
#[cfg(all(target_arch = "wasm32", not(target_feature = "simd128")))]
compile_error!(
    "alpha2's wasm modules require WebAssembly SIMD; add `-C target-feature=+simd128` to RUSTFLAGS"
);

/// Largest audio block the browser binding mixes in one call (`audioMaxBlock` in JS). Bounds
/// the wasm-side scratch buffers.
#[cfg(target_arch = "wasm32")]
pub(crate) const AUDIO_BLOCK: usize = 4_096;

#[cfg(target_arch = "wasm32")]
mod assets;
#[cfg(any(target_arch = "wasm32", test))]
mod audio_store;
#[cfg(target_arch = "wasm32")]
mod captions;
#[cfg(any(target_arch = "wasm32", test))]
mod completion;
#[cfg(any(target_arch = "wasm32", test))]
mod effect;
#[cfg(any(target_arch = "wasm32", test))]
mod error;
#[cfg(any(target_arch = "wasm32", test))]
#[cfg(any(target_arch = "wasm32", test))]
mod geometry;
#[cfg(target_arch = "wasm32")]
mod interop;
#[cfg(target_arch = "wasm32")]
mod loudness;
#[cfg(target_arch = "wasm32")]
mod panic_hook;
#[cfg(target_arch = "wasm32")]
mod playback;
#[cfg(target_arch = "wasm32")]
mod present;
#[cfg(target_arch = "wasm32")]
mod project_audio;
#[cfg(any(target_arch = "wasm32", test))]
mod project_cache;
#[cfg(any(target_arch = "wasm32", test))]
mod rate_control;
#[cfg(any(target_arch = "wasm32", test))]
mod readback_wait;
#[cfg(target_arch = "wasm32")]
mod resample;
#[cfg(any(target_arch = "wasm32", test))]
mod text;
#[cfg(target_arch = "wasm32")]
mod waveform;
#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
mod web_cpu;
