//! The test capability contract shared by every Rust test suite.
//!
//! A test that cannot run on this machine says which capability is missing through [`skip!`] or
//! [`available!`]. When `ALPHA2_REQUIRE` (a comma list of names from `tests/capabilities.json`)
//! names that capability, the skip is a failure instead. Every skip that is not a failure is
//! printed and, when `ALPHA2_SKIP_LOG` names an absolute path, appended to that file as one JSON
//! line, so a run leaves an inventory of what the machine lacks.
//!
//! Tests that cannot apply to an OS at all stay `#[cfg]`-gated and are not recorded: their set
//! does not change from machine to machine.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// A machine capability a test can need. The names and their probes are listed in
/// `tests/capabilities.json`, which JavaScript and Python read too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// A Vulkan driver the loader finds.
    Vulkan,
    /// Vulkan Video decode on the shared device.
    VulkanVideo,
    /// Vulkan Video H.264 encode on the shared device.
    VulkanVideoEncode,
    /// The Khronos validation layer, 1.4 or newer.
    VulkanValidation,
    /// A Khronos validation layer older than 1.4.
    OldVulkanValidationLayer,
    /// D3D12VA decode on the renderer's D3D12 device.
    D3d12Video,
    /// D3D12VA H.264 encode on an accepted adapter.
    D3d12Encode,
    /// The D3D12 debug layer (the Graphics Tools optional feature).
    D3d12DebugLayer,
    /// A Metal device shared with `VideoToolbox`.
    Metal,
    /// `VideoToolbox` hardware H.264 decode.
    Videotoolbox,
    /// `VideoToolbox` hardware H.264 encode.
    VideotoolboxEncode,
    /// A GPU-resident H.264 output path.
    GpuResident,
    /// Decoded hardware surfaces imported by a GPU-resident host.
    HardwareDecodeImport,
    /// NVIDIA NVENC encode.
    Nvenc,
    /// Media Foundation encode.
    MediaFoundation,
    /// DXC as the test device's DX12 shader compiler.
    Dxc,
    /// A hardware H.264 encoder ranked first.
    HardwareH264Encode,
    /// The `OpenH264` runtime.
    Openh264,
    /// A C compiler that builds shared libraries.
    CCompiler,
    /// Two or more GPU adapters.
    MultiAdapter,
    /// 16-bit normalized textures on the test device.
    Unorm16,
    /// GPU timestamp queries on the test device.
    TimestampQuery,
    /// A GPU stall the render stall calibration can measure.
    MeasurableGpuStall,
    /// The packaged wasm builds.
    Wasm,
    /// The browser a harness was asked for.
    Browser,
    /// A Python that imports alpha2.
    Python,
    /// A built alpha2-host.
    Host,
    /// H.264 output from the host.
    H264,
    /// `WebGPU` in the browser.
    Webgpu,
    /// The engine's `WebGL2` backend.
    Webgl2,
    /// Presenting a `WebGPU` canvas.
    WebgpuPresent,
    /// Importing a `VideoFrame` into `WebGPU`.
    WebgpuVideoImport,
    /// Capturing what a page presents.
    PresentationCapture,
    /// `WebCodecs` audio.
    WebcodecsAudio,
    /// `WebCodecs` video encode.
    WebcodecsVideo,
    /// AAC encode in the browser.
    AacEncode,
    /// AC-3 and E-AC-3 decode in the browser.
    Ac3Decode,
    /// An `AudioContext` render quantum of the asked size.
    AudioRenderQuantum,
    /// An `AudioContext` output timestamp on the common origin.
    AudioOutputTimestamp,
    /// An `AudioContext` that starts without a user gesture.
    AudioAutostart,
    /// Decoded `VideoFrame`s in 8-bit 4:2:0.
    YuvVideoFrames,
}

impl Capability {
    /// Every capability, in the order of `tests/capabilities.json`.
    pub const ALL: &'static [Capability] = &[
        Self::Vulkan,
        Self::VulkanVideo,
        Self::VulkanVideoEncode,
        Self::VulkanValidation,
        Self::OldVulkanValidationLayer,
        Self::D3d12Video,
        Self::D3d12Encode,
        Self::D3d12DebugLayer,
        Self::Metal,
        Self::Videotoolbox,
        Self::VideotoolboxEncode,
        Self::GpuResident,
        Self::HardwareDecodeImport,
        Self::Nvenc,
        Self::MediaFoundation,
        Self::Dxc,
        Self::HardwareH264Encode,
        Self::Openh264,
        Self::CCompiler,
        Self::MultiAdapter,
        Self::Unorm16,
        Self::TimestampQuery,
        Self::MeasurableGpuStall,
        Self::Wasm,
        Self::Browser,
        Self::Python,
        Self::Host,
        Self::H264,
        Self::Webgpu,
        Self::Webgl2,
        Self::WebgpuPresent,
        Self::WebgpuVideoImport,
        Self::PresentationCapture,
        Self::WebcodecsAudio,
        Self::WebcodecsVideo,
        Self::AacEncode,
        Self::Ac3Decode,
        Self::AudioRenderQuantum,
        Self::AudioOutputTimestamp,
        Self::AudioAutostart,
        Self::YuvVideoFrames,
    ];

    /// The capability's name in `ALPHA2_REQUIRE` and the skip log.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vulkan => "vulkan",
            Self::VulkanVideo => "vulkan-video",
            Self::VulkanVideoEncode => "vulkan-video-encode",
            Self::VulkanValidation => "vulkan-validation",
            Self::OldVulkanValidationLayer => "old-vulkan-validation-layer",
            Self::D3d12Video => "d3d12-video",
            Self::D3d12Encode => "d3d12-encode",
            Self::D3d12DebugLayer => "d3d12-debug-layer",
            Self::Metal => "metal",
            Self::Videotoolbox => "videotoolbox",
            Self::VideotoolboxEncode => "videotoolbox-encode",
            Self::GpuResident => "gpu-resident",
            Self::HardwareDecodeImport => "hardware-decode-import",
            Self::Nvenc => "nvenc",
            Self::MediaFoundation => "media-foundation",
            Self::Dxc => "dxc",
            Self::HardwareH264Encode => "hardware-h264-encode",
            Self::Openh264 => "openh264",
            Self::CCompiler => "c-compiler",
            Self::MultiAdapter => "multi-adapter",
            Self::Unorm16 => "unorm16",
            Self::TimestampQuery => "timestamp-query",
            Self::MeasurableGpuStall => "measurable-gpu-stall",
            Self::Wasm => "wasm",
            Self::Browser => "browser",
            Self::Python => "python",
            Self::Host => "host",
            Self::H264 => "h264",
            Self::Webgpu => "webgpu",
            Self::Webgl2 => "webgl2",
            Self::WebgpuPresent => "webgpu-present",
            Self::WebgpuVideoImport => "webgpu-video-import",
            Self::PresentationCapture => "presentation-capture",
            Self::WebcodecsAudio => "webcodecs-audio",
            Self::WebcodecsVideo => "webcodecs-video",
            Self::AacEncode => "aac-encode",
            Self::Ac3Decode => "ac3-decode",
            Self::AudioRenderQuantum => "audio-render-quantum",
            Self::AudioOutputTimestamp => "audio-output-timestamp",
            Self::AudioAutostart => "audio-autostart",
            Self::YuvVideoFrames => "yuv-video-frames",
        }
    }

    /// The capability named `name`, if it is one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|capability| capability.name() == name)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Parses an `ALPHA2_REQUIRE` value: a comma list, entries trimmed, empty entries ignored.
///
/// # Errors
/// An unknown name, with the list of known ones.
pub fn parse_required(value: &str) -> Result<BTreeSet<Capability>, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            Capability::from_name(name).ok_or_else(|| {
                format!(
                    "ALPHA2_REQUIRE names an unknown capability {name:?}; known: {}",
                    Capability::ALL
                        .iter()
                        .map(|capability| capability.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect()
}

fn required_set() -> &'static BTreeSet<Capability> {
    static REQUIRED: OnceLock<BTreeSet<Capability>> = OnceLock::new();
    REQUIRED.get_or_init(|| {
        let value = std::env::var("ALPHA2_REQUIRE").unwrap_or_default();
        parse_required(&value).unwrap_or_else(|error| panic!("{error}"))
    })
}

/// Whether `ALPHA2_REQUIRE` names `capability`.
///
/// # Panics
/// When `ALPHA2_REQUIRE` names an unknown capability.
#[must_use]
pub fn required(capability: Capability) -> bool {
    required_set().contains(&capability)
}

/// One skip record, as the line appended to the skip log.
#[must_use]
pub fn skip_record(suite: &str, test: &str, capability: Capability, reason: &str) -> String {
    let mut line = serde_json::json!({
        "lang": "rust",
        "suite": suite,
        "test": test,
        "capability": capability.name(),
        "reason": reason,
        "pid": std::process::id(),
    })
    .to_string();
    line.push('\n');
    line
}

/// Appends one record to the skip log at `path` with a single write on an append-mode handle, so
/// records of several processes never interleave.
///
/// # Panics
/// For a relative path (cargo runs each test binary in its crate directory, so a relative log
/// would scatter) or when the log cannot be written.
pub fn append_record(path: &Path, line: &str) {
    // Threads of one test binary write one at a time.
    static WRITING: Mutex<()> = Mutex::new(());
    assert!(
        path.is_absolute(),
        "ALPHA2_SKIP_LOG must be absolute: cargo runs each test binary in its crate directory ({})",
        path.display()
    );
    let _writing = WRITING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("cannot create {}: {error}", parent.display()));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .unwrap_or_else(|error| panic!("cannot append to {}: {error}", path.display()));
}

/// The test a skip belongs to: the libtest thread name, which is the test's path, or `module`
/// when the skip happens on another thread.
fn current_test(module: &str) -> String {
    match std::thread::current().name() {
        Some(name) if name.contains("::") => name.to_owned(),
        _ => module.to_owned(),
    }
}

/// The capability is missing on this machine.
///
/// Fails when `ALPHA2_REQUIRE` names it; otherwise prints `skipped [<name>]: <reason>` and appends
/// the record to `ALPHA2_SKIP_LOG` when that is set. Use [`skip!`], which fills in the suite and
/// module.
///
/// # Panics
/// When the capability is required, or the skip log cannot be written.
pub fn skip_at(suite: &str, module: &str, capability: Capability, reason: std::fmt::Arguments<'_>) {
    let reason = reason.to_string();
    assert!(
        !required(capability),
        "{capability} is required (ALPHA2_REQUIRE): {reason}"
    );
    eprintln!("skipped [{capability}]: {reason}");
    if let Some(path) = std::env::var_os("ALPHA2_SKIP_LOG") {
        let test = current_test(module);
        append_record(
            Path::new(&path),
            &skip_record(suite, &test, capability, &reason),
        );
    }
}

/// Records that `$capability` is missing here (a failure when `ALPHA2_REQUIRE` names it); the
/// rest is a `format!` message saying why. The caller returns after it.
#[macro_export]
macro_rules! skip {
    ($capability:expr, $($reason:tt)+) => {
        $crate::skip_at(
            env!("CARGO_PKG_NAME"),
            module_path!(),
            $capability,
            format_args!($($reason)+),
        )
    };
}

/// `Some(value)` for `Ok(value)`; for `Err(error)`, [`skip!`] with the error and `None`.
#[macro_export]
macro_rules! available {
    ($capability:expr, $made:expr) => {
        match $made {
            Ok(value) => Some(value),
            Err(error) => {
                $crate::skip!($capability, "{error}");
                None
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_enum_names_every_capability_in_the_json() {
        let listed: serde_json::Value =
            serde_json::from_str(include_str!("../../capabilities.json")).expect("valid JSON");
        let names: BTreeSet<&str> = listed
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        let ours: BTreeSet<&str> = Capability::ALL
            .iter()
            .map(|capability| capability.name())
            .collect();
        assert_eq!(
            ours, names,
            "Capability and tests/capabilities.json disagree"
        );
        for (name, entry) in listed.as_object().expect("an object") {
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
                "{name} is not lowercase ASCII"
            );
            for field in ["description", "probe"] {
                assert!(
                    entry[field].as_str().is_some_and(|text| !text.is_empty()),
                    "{name}.{field}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_required_name_is_refused() {
        assert_eq!(
            parse_required(" vulkan , ,gpu-resident,").expect("known names"),
            BTreeSet::from([Capability::Vulkan, Capability::GpuResident])
        );
        let error = parse_required("vulkan,vulkan-vidoe").expect_err("a typo");
        assert!(error.contains("\"vulkan-vidoe\""), "{error}");
        assert!(error.contains("vulkan-video"), "{error}");
        assert_eq!(parse_required("").expect("empty"), BTreeSet::new());
    }

    #[test]
    fn a_record_is_one_json_line_with_every_field() {
        let line = skip_record(
            "alpha2-host",
            "tests::a::b",
            Capability::GpuResident,
            "no \"path\"",
        );
        assert!(line.ends_with('\n') && !line.trim_end().contains('\n'));
        let record: serde_json::Value = serde_json::from_str(&line).expect("JSON");
        assert_eq!(record["lang"], "rust");
        assert_eq!(record["suite"], "alpha2-host");
        assert_eq!(record["test"], "tests::a::b");
        assert_eq!(record["capability"], "gpu-resident");
        assert_eq!(record["reason"], "no \"path\"");
        assert_eq!(record["pid"], std::process::id());
    }

    #[test]
    fn records_from_many_threads_are_whole_lines() {
        let path =
            std::env::temp_dir().join(format!("alpha2-skip-log-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::thread::scope(|scope| {
            for thread in 0..16 {
                let path = &path;
                scope.spawn(move || {
                    for record in 0..200 {
                        let reason = format!("thread {thread} record {record} {}", "x".repeat(100));
                        append_record(
                            path,
                            &skip_record("suite", "test", Capability::Wasm, &reason),
                        );
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&path).expect("log");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 16 * 200);
        for line in lines {
            let record: serde_json::Value = serde_json::from_str(line).expect("a whole line");
            assert_eq!(record["capability"], "wasm");
        }
        std::fs::remove_file(&path).expect("clean up");
    }

    #[test]
    #[should_panic(expected = "must be absolute")]
    fn a_relative_log_path_is_refused() {
        append_record(Path::new("skip-log.jsonl"), "{}\n");
    }
}
