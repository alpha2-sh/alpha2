//! The renderer a host draws with: the GPU [`Renderer`] on a hardware (or,
//! when asked for, software) adapter, or the [`CpuRenderer`].

use alpha2_engine::TextUpload;
use alpha2_render::{CpuRenderer, Lut3dData, RenderError, Renderer, ResultCacheStats};

/// The renderer a host draws with, chosen once when it starts.
pub enum RendererBackend {
    Gpu(Box<Renderer>),
    Cpu(Box<CpuRenderer>),
}

/// How a result names a DX12 shader compiler: `DXC <path>` gains the file
/// version of the DLL, `DXC 1.9.2609.1 (<path>)`, read once per process.
pub fn describe_shader_compiler(raw: &str) -> String {
    static DESCRIBED: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String)>>> =
        std::sync::OnceLock::new();
    let Some(path) = raw.strip_prefix("DXC ") else {
        return raw.to_owned();
    };
    let cache = DESCRIBED.get_or_init(std::sync::Mutex::default);
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, described)) = cache.iter().find(|(seen, _)| seen == raw) {
        return described.clone();
    }
    let described = alpha2_os::file_version(std::path::Path::new(path)).map_or_else(
        || raw.to_owned(),
        |version| format!("DXC {version} ({path})"),
    );
    cache.push((raw.to_owned(), described.clone()));
    described
}

/// What a result reports about the device that rendered it.
pub struct AdapterIdentity {
    pub name: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
    /// The DX12 shader compiler; empty elsewhere and on the CPU.
    pub shader_compiler: String,
}

impl RendererBackend {
    /// The GPU renderer; `None` on the CPU.
    pub fn gpu(&self) -> Option<&Renderer> {
        match self {
            Self::Gpu(renderer) => Some(renderer),
            Self::Cpu(_) => None,
        }
    }

    /// The GPU renderer, to change; `None` on the CPU.
    pub fn gpu_mut(&mut self) -> Option<&mut Renderer> {
        match self {
            Self::Gpu(renderer) => Some(renderer),
            Self::Cpu(_) => None,
        }
    }

    /// The CPU renderer; `None` on a GPU.
    pub fn cpu(&self) -> Option<&CpuRenderer> {
        match self {
            Self::Cpu(renderer) => Some(renderer),
            Self::Gpu(_) => None,
        }
    }

    /// `"gpu"` or `"cpu"`, as results and capabilities report it.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Gpu(_) => "gpu",
            Self::Cpu(_) => "cpu",
        }
    }

    pub fn adapter(&self) -> AdapterIdentity {
        match self {
            Self::Gpu(renderer) => {
                let adapter = renderer.diagnostics().adapter;
                AdapterIdentity {
                    name: adapter.name,
                    backend: adapter.backend,
                    driver: adapter.driver,
                    driver_info: adapter.driver_info,
                    shader_compiler: describe_shader_compiler(&adapter.shader_compiler),
                }
            }
            Self::Cpu(renderer) => AdapterIdentity {
                name: format!("alpha2 CPU renderer ({} threads)", renderer.threads()),
                backend: "cpu".to_owned(),
                driver: "alpha2".to_owned(),
                driver_info: env!("CARGO_PKG_VERSION").to_owned(),
                shader_compiler: String::new(),
            },
        }
    }

    /// The format the renderer stores pictures in: the CPU stores half
    /// floats, as the GPU's usual intermediate.
    pub fn intermediate_format(&self) -> wgpu::TextureFormat {
        match self {
            Self::Gpu(renderer) => renderer.intermediate_format(),
            Self::Cpu(_) => wgpu::TextureFormat::Rgba16Float,
        }
    }

    pub fn clear_result_cache(&self) {
        match self {
            Self::Gpu(renderer) => renderer.clear_result_cache(),
            Self::Cpu(renderer) => renderer.clear_result_cache(),
        }
    }

    pub fn set_result_cache_budget(&self, budget_bytes: u64) {
        match self {
            Self::Gpu(renderer) => renderer.set_result_cache_budget(budget_bytes),
            Self::Cpu(renderer) => renderer.set_result_cache_budget(budget_bytes),
        }
    }

    pub fn result_cache_stats(&self) -> ResultCacheStats {
        match self {
            Self::Gpu(renderer) => renderer.result_cache_stats(),
            Self::Cpu(renderer) => renderer.result_cache_stats(),
        }
    }

    /// Lets go of what the renderer keeps only for reuse, and returns the
    /// bytes released.
    pub fn release_idle_memory(&self) -> u64 {
        match self {
            Self::Gpu(renderer) => renderer.release_idle_memory(),
            Self::Cpu(renderer) => renderer.release_idle_memory(),
        }
    }

    pub fn clear_effect_resources(&mut self) {
        match self {
            Self::Gpu(renderer) => renderer.clear_effect_resources(),
            Self::Cpu(renderer) => renderer.clear_effect_resources(),
        }
    }

    pub fn register_lut3d(&mut self, id: &str, data: &Lut3dData) -> Result<(), RenderError> {
        match self {
            Self::Gpu(renderer) => renderer.register_lut3d(id, data),
            Self::Cpu(renderer) => renderer.register_lut3d(id, data),
        }
    }

    /// Where prepared text goes: a GPU upload, or a picture the CPU keeps.
    pub fn text_upload(&self) -> &dyn TextUpload {
        match self {
            Self::Gpu(renderer) => &**renderer,
            Self::Cpu(renderer) => &**renderer,
        }
    }
}
