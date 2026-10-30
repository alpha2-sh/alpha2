//! How every `wgpu` instance of the process is made.
//!
//! One builder serves every instance, so all of them use the same DX12 shader compiler and the
//! same flags. The compiler is a process-wide setting, frozen when the first instance is made: a
//! device opened with one compiler and a pipeline compiled with another would render different
//! bytes, and a compiler picked up from the DLL search path could not be reported.
//!
//! Debug and release builds compile the same DX12 code: on Windows the builder drops
//! `InstanceFlags::DEBUG`, under which `wgpu` compiles HLSL with optimizations skipped
//! (`WGPU_DEBUG=1` turns it back on). Elsewhere there is no DX12 and the flag stays, since on
//! Vulkan it also installs the debug messenger that routes validation messages to the log. Every
//! build drops `VALIDATION_INDIRECT_CALL`, which validates indirect draws and dispatches alpha2
//! never issues. Validation itself is a separate flag and stays.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// The DX12 shader compiler the process's instances use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dx12Compiler {
    /// The FXC compiler built into Windows: no file to ship, slow to compile large shaders.
    Fxc,
    /// DXC loaded from `library` (`dxcompiler.dll`).
    Dxc {
        /// The `dxcompiler.dll` to load.
        library: PathBuf,
    },
}

/// The settings every instance of the process is made with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceSettings {
    /// The DX12 shader compiler.
    pub dx12_compiler: Dx12Compiler,
}

impl InstanceSettings {
    /// The settings the environment asks for: DXC from `ALPHA2_DXC_PATH` when it is set and not
    /// empty, otherwise FXC. Never `wgpu`'s `Auto`, which loads any `dxcompiler.dll` the DLL search
    /// path finds, the working directory included.
    #[must_use]
    pub fn from_environment() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Self {
        let dx12_compiler = match lookup("ALPHA2_DXC_PATH") {
            Some(path) if !path.is_empty() => Dx12Compiler::Dxc {
                library: PathBuf::from(path),
            },
            _ => Dx12Compiler::Fxc,
        };
        Self { dx12_compiler }
    }
}

/// Why [`configure_instances`] refused new settings.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstanceSettingsError {
    /// An instance was already made, or settings were already configured: every instance of a
    /// process uses the settings the first one was made with.
    AlreadyFrozen,
}

impl std::fmt::Display for InstanceSettingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyFrozen => formatter.write_str(
                "the GPU instance settings are fixed once the first instance is made or settings \
                 were configured",
            ),
        }
    }
}

impl std::error::Error for InstanceSettingsError {}

/// The process's settings, and whether any instance has been made with them.
struct SettingsCell {
    settings: OnceLock<InstanceSettings>,
    frozen: AtomicBool,
}

impl SettingsCell {
    const fn new() -> Self {
        Self {
            settings: OnceLock::new(),
            frozen: AtomicBool::new(false),
        }
    }

    fn configure(&self, settings: InstanceSettings) -> Result<(), InstanceSettingsError> {
        if self.frozen.load(Ordering::Acquire) {
            return Err(InstanceSettingsError::AlreadyFrozen);
        }
        self.settings
            .set(settings)
            .map_err(|_| InstanceSettingsError::AlreadyFrozen)
    }

    fn get(&self, environment: impl FnOnce() -> InstanceSettings) -> &InstanceSettings {
        self.settings.get_or_init(environment)
    }

    fn freeze(&self, environment: impl FnOnce() -> InstanceSettings) -> &InstanceSettings {
        let settings = self.get(environment);
        self.frozen.store(true, Ordering::Release);
        settings
    }
}

static SETTINGS: SettingsCell = SettingsCell::new();

/// Sets the settings every instance of this process is made with.
///
/// A host calls this once, before it makes any device. Without a call the first instance takes
/// [`InstanceSettings::from_environment`].
///
/// # Errors
/// [`InstanceSettingsError::AlreadyFrozen`] once an instance was made or settings were set.
pub fn configure_instances(settings: InstanceSettings) -> Result<(), InstanceSettingsError> {
    SETTINGS.configure(settings)
}

/// The settings instances of this process are made with; the first call without a configuration
/// takes them from the environment.
#[must_use]
pub fn instance_settings() -> &'static InstanceSettings {
    SETTINGS.get(InstanceSettings::from_environment)
}

/// The descriptor every instance on `backends` is made from: the process's DX12 compiler, the
/// build's flags without `VALIDATION_INDIRECT_CALL` (and on Windows without `DEBUG`), then the
/// environment (`WGPU_BACKEND`,
/// `WGPU_DX12_COMPILER`, `WGPU_DEBUG`, `WGPU_VALIDATION`, ...) on top, as a developer override.
#[must_use]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub fn instance_descriptor(backends: wgpu::Backends) -> wgpu::InstanceDescriptor {
    descriptor_for(
        backends,
        SETTINGS.freeze(InstanceSettings::from_environment),
    )
}

fn descriptor_for(
    backends: wgpu::Backends,
    settings: &InstanceSettings,
) -> wgpu::InstanceDescriptor {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = backends;
    descriptor.flags =
        wgpu::InstanceFlags::from_build_config() - wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL;
    if cfg!(target_os = "windows") {
        descriptor.flags -= wgpu::InstanceFlags::DEBUG;
    }
    descriptor.backend_options.dx12.shader_compiler = match &settings.dx12_compiler {
        Dx12Compiler::Fxc => wgpu::Dx12Compiler::Fxc,
        Dx12Compiler::Dxc { library } => wgpu::Dx12Compiler::DynamicDxc {
            dxc_path: library.to_string_lossy().into_owned(),
        },
    };
    descriptor.with_env()
}

/// A new instance on `backends`, made from [`instance_descriptor`] within the process's
/// acquisition turn.
#[must_use]
#[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
pub fn new_instance(backends: wgpu::Backends) -> wgpu::Instance {
    let _turn = crate::serialize_acquisition();
    keep(backends);
    wgpu::Instance::new(instance_descriptor(backends))
}

/// Under `serial-acquisition`, keeps an instance on `backends` until the process exits unless a
/// kept one already covers them; see [`crate::serialize_acquisition`]. The caller holds the turn.
#[allow(clippy::missing_const_for_fn, unused_variables)]
pub(crate) fn keep(backends: wgpu::Backends) {
    #[cfg(all(feature = "serial-acquisition", not(target_arch = "wasm32")))]
    {
        use std::sync::{Mutex, PoisonError};
        static KEPT: Mutex<Vec<(wgpu::Backends, wgpu::Instance)>> = Mutex::new(Vec::new());
        let backends = wgpu::Backends::from_env().unwrap_or(backends);
        let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.iter().any(|(covered, _)| covered.contains(backends)) {
            return;
        }
        let instance = wgpu::Instance::new(instance_descriptor(backends));
        kept.push((backends, instance));
    }
}

/// The DX12 shader compiler a descriptor asks for, as reports name it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShaderCompiler {
    /// The descriptor enables no DX12 backend.
    NotDx12,
    /// FXC.
    Fxc,
    /// DXC from the named library.
    Dxc {
        /// The `dxcompiler.dll` path.
        library: String,
    },
    /// DXC from whatever `dxcompiler.dll` the DLL search path finds (`WGPU_DX12_COMPILER=dxc`).
    DxcFromSearchPath,
    /// DXC linked into the binary (`WGPU_DX12_COMPILER=staticdxc`).
    StaticDxc,
    /// `wgpu`'s own choice (`WGPU_DX12_COMPILER=auto`).
    Auto,
}

impl std::fmt::Display for ShaderCompiler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDx12 => Ok(()),
            Self::Fxc => formatter.write_str("FXC"),
            Self::Dxc { library } => write!(formatter, "DXC {library}"),
            Self::DxcFromSearchPath => formatter.write_str("DXC from the DLL search path"),
            Self::StaticDxc => formatter.write_str("static DXC"),
            Self::Auto => formatter.write_str("auto"),
        }
    }
}

/// The DX12 shader compiler `descriptor` asks for, read after its environment overrides.
#[must_use]
pub fn shader_compiler_of(descriptor: &wgpu::InstanceDescriptor) -> ShaderCompiler {
    if !descriptor.backends.contains(wgpu::Backends::DX12) {
        return ShaderCompiler::NotDx12;
    }
    match &descriptor.backend_options.dx12.shader_compiler {
        wgpu::Dx12Compiler::Fxc => ShaderCompiler::Fxc,
        // `WGPU_DX12_COMPILER=dxc` names the bare file, which the DLL search path resolves.
        wgpu::Dx12Compiler::DynamicDxc { dxc_path } if dxc_path == "dxcompiler.dll" => {
            ShaderCompiler::DxcFromSearchPath
        }
        wgpu::Dx12Compiler::DynamicDxc { dxc_path } => ShaderCompiler::Dxc {
            library: dxc_path.clone(),
        },
        wgpu::Dx12Compiler::StaticDxc => ShaderCompiler::StaticDxc,
        wgpu::Dx12Compiler::Auto => ShaderCompiler::Auto,
    }
}

/// The DX12 shader compiler this process's DX12 instances use.
#[must_use]
pub fn dx12_shader_compiler() -> ShaderCompiler {
    shader_compiler_of(&instance_descriptor(wgpu::Backends::DX12))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_take_the_dxc_path_from_the_environment_and_otherwise_fxc() {
        let unset = InstanceSettings::from_lookup(|_| None);
        assert_eq!(unset.dx12_compiler, Dx12Compiler::Fxc);
        let empty = InstanceSettings::from_lookup(|_| Some("".into()));
        assert_eq!(empty.dx12_compiler, Dx12Compiler::Fxc);
        let set = InstanceSettings::from_lookup(|name| {
            (name == "ALPHA2_DXC_PATH").then(|| r"C:\alpha2\dxcompiler.dll".into())
        });
        assert_eq!(
            set.dx12_compiler,
            Dx12Compiler::Dxc {
                library: PathBuf::from(r"C:\alpha2\dxcompiler.dll")
            }
        );
    }

    #[test]
    fn the_descriptor_carries_the_compiler_and_drops_debug_on_windows_and_indirect_validation() {
        // The environment may name a compiler or flags on a developer's machine; the descriptor
        // is checked as the builder makes it before the overrides.
        if std::env::var_os("WGPU_DX12_COMPILER").is_some()
            || std::env::var_os("WGPU_DEBUG").is_some()
            || std::env::var_os("WGPU_VALIDATION_INDIRECT_CALL").is_some()
        {
            return;
        }
        let dxc = InstanceSettings {
            dx12_compiler: Dx12Compiler::Dxc {
                library: PathBuf::from("/opt/dxc/dxcompiler.dll"),
            },
        };
        let descriptor = descriptor_for(wgpu::Backends::DX12, &dxc);
        assert_eq!(
            descriptor.flags.contains(wgpu::InstanceFlags::DEBUG),
            cfg!(debug_assertions) && !cfg!(target_os = "windows")
        );
        assert!(
            !descriptor
                .flags
                .contains(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL)
        );
        assert_eq!(
            shader_compiler_of(&descriptor),
            ShaderCompiler::Dxc {
                library: "/opt/dxc/dxcompiler.dll".to_owned()
            }
        );
        assert_eq!(
            shader_compiler_of(&descriptor).to_string(),
            "DXC /opt/dxc/dxcompiler.dll"
        );
        let fxc = InstanceSettings {
            dx12_compiler: Dx12Compiler::Fxc,
        };
        assert_eq!(
            shader_compiler_of(&descriptor_for(wgpu::Backends::DX12, &fxc)).to_string(),
            "FXC"
        );
        if std::env::var_os("WGPU_BACKEND").is_none() {
            assert_eq!(
                shader_compiler_of(&descriptor_for(wgpu::Backends::VULKAN, &fxc)),
                ShaderCompiler::NotDx12
            );
        }
    }

    #[test]
    fn settings_cannot_change_after_the_first_instance() {
        let fxc = || InstanceSettings {
            dx12_compiler: Dx12Compiler::Fxc,
        };
        let cell = SettingsCell::new();
        cell.freeze(fxc);
        assert_eq!(
            cell.configure(fxc()),
            Err(InstanceSettingsError::AlreadyFrozen)
        );
        let configured = SettingsCell::new();
        assert_eq!(configured.configure(fxc()), Ok(()));
        assert_eq!(
            configured.configure(fxc()),
            Err(InstanceSettingsError::AlreadyFrozen)
        );
        assert_eq!(configured.freeze(|| unreachable!()), &fxc());
    }
}
