//! Checks a wasm artifact. It must validate without relaxed SIMD, threads or shared memory, import
//! only the `alpha2` host functions, and export every `alpha2-plugin/1` function.

use wasmparser::{Parser, Payload, TypeRef, Validator, WasmFeatures};

const IMPORTS: [&str; 5] = ["emit", "resource_size", "resource_read", "fail", "log"];
const EXPORTS: [&str; 10] = [
    "memory",
    "alpha2_abi",
    "alpha2_alloc",
    "alpha2_free",
    "alpha2_describe",
    "alpha2_begin",
    "alpha2_pass",
    "alpha2_video",
    "alpha2_audio",
    "alpha2_end",
];

/// The default features minus those whose results differ between runtimes or that need threads.
fn features() -> WasmFeatures {
    let mut features = WasmFeatures::default();
    features.remove(
        WasmFeatures::RELAXED_SIMD
            | WasmFeatures::THREADS
            | WasmFeatures::SHARED_EVERYTHING_THREADS
            | WasmFeatures::COMPONENT_MODEL,
    );
    features
}

/// # Errors
/// Why `module` is not a plugin module this ABI loads.
pub fn wasm_module(module: &[u8]) -> Result<(), String> {
    Validator::new_with_features(features())
        .validate_all(module)
        .map_err(|error| {
            format!("the module uses a feature plugins may not use, or is invalid: {error}")
        })?;
    let mut exports = Vec::new();
    for payload in Parser::new(0).parse_all(module) {
        match payload.map_err(|error| error.to_string())? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.map_err(|error| error.to_string())?;
                    if import.module != "alpha2" || !IMPORTS.contains(&import.name) {
                        return Err(format!(
                            "the module imports {}.{}; plugins import only alpha2.{{{}}}",
                            import.module,
                            import.name,
                            IMPORTS.join(", ")
                        ));
                    }
                    if !matches!(import.ty, TypeRef::Func(_)) {
                        return Err(format!(
                            "alpha2.{} is imported as a non-function",
                            import.name
                        ));
                    }
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    exports.push(export.map_err(|error| error.to_string())?.name.to_owned());
                }
            }
            _ => {}
        }
    }
    let missing = EXPORTS
        .iter()
        .filter(|name| !exports.iter().any(|export| export == *name))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("the module does not export {}", missing.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    /// A module with one function `() -> v128` whose body is `v128.const 0` and then `extra`.
    fn simd_module(extra: &[u8]) -> Vec<u8> {
        let mut body = vec![0x00, 0xfd, 0x0c];
        body.extend_from_slice(&[0; 16]);
        body.extend_from_slice(extra);
        body.push(0x0b);
        let mut module = HEADER.to_vec();
        module.extend_from_slice(&[0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7b]);
        module.extend_from_slice(&[0x03, 0x02, 0x01, 0x00]);
        let size = u8::try_from(body.len()).expect("small body");
        module.extend_from_slice(&[0x0a, size + 2, 0x01, size]);
        module.extend_from_slice(&body);
        module
    }

    fn refusal(module: &[u8]) -> String {
        wasm_module(module).expect_err("refused")
    }

    #[test]
    fn relaxed_simd_is_refused_and_plain_simd_is_not() {
        // `i32x4.relaxed_trunc_f32x4_s` is 0xfd 0x101.
        let relaxed = refusal(&simd_module(&[0xfd, 0x81, 0x02]));
        assert!(relaxed.contains("relaxed SIMD"), "{relaxed}");
        // Without it the module validates and fails only on the missing exports.
        let plain = refusal(&simd_module(&[]));
        assert!(
            plain.starts_with("the module does not export memory"),
            "{plain}"
        );
    }

    #[test]
    fn shared_memory_is_refused() {
        let mut module = HEADER.to_vec();
        // One memory, shared, minimum and maximum one page.
        module.extend_from_slice(&[0x05, 0x04, 0x01, 0x03, 0x01, 0x01]);
        let shared = refusal(&module);
        assert!(shared.contains("threads must be enabled"), "{shared}");
    }

    #[test]
    fn only_alpha2_host_functions_are_imported() {
        let import = |module: &str, name: &str| {
            let mut entry = vec![u8::try_from(module.len()).expect("short")];
            entry.extend_from_slice(module.as_bytes());
            entry.push(u8::try_from(name.len()).expect("short"));
            entry.extend_from_slice(name.as_bytes());
            entry.extend_from_slice(&[0x00, 0x00]);
            let mut bytes = HEADER.to_vec();
            bytes.extend_from_slice(&[0x01, 0x04, 0x01, 0x60, 0x00, 0x00]);
            bytes.extend_from_slice(&[0x02, u8::try_from(entry.len() + 1).expect("short"), 0x01]);
            bytes.extend_from_slice(&entry);
            bytes
        };
        assert!(
            refusal(&import("wasi_snapshot_preview1", "fd_write"))
                .contains("imports wasi_snapshot_preview1.fd_write")
        );
        assert!(refusal(&import("alpha2", "clock")).contains("imports alpha2.clock"));
        assert!(refusal(&import("alpha2", "log")).starts_with("the module does not export"));
    }
}
