//! The PDF provider in its own wasm module, with the same `ExtensionRegistry` interface as the
//! main extension module. hayro is the largest dependency of the providers and only PDF uses it.

pub use alpha2_extensions_web::ExtensionRegistry;
