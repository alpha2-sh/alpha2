use alpha2_engine::TextUpload;
use alpha2_render::{CpuRenderer, Lut3dData, RenderError, Renderer, ResultCacheStats};

pub enum RendererBackend {
    Gpu(Box<Renderer>),
    Cpu(Box<CpuRenderer>),
}

/// `DXC <path>` becomes `DXC <file version> (<path>)`, read once per process.
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

pub struct AdapterIdentity {
    pub name: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
    /// Empty except on DX12.
    pub shader_compiler: String,
}

impl RendererBackend {
    pub fn gpu(&self) -> Option<&Renderer> {
        match self {
            Self::Gpu(renderer) => Some(renderer),
            Self::Cpu(_) => None,
        }
    }

    pub fn gpu_mut(&mut self) -> Option<&mut Renderer> {
        match self {
            Self::Gpu(renderer) => Some(renderer),
            Self::Cpu(_) => None,
        }
    }

    pub fn cpu(&self) -> Option<&CpuRenderer> {
        match self {
            Self::Cpu(renderer) => Some(renderer),
            Self::Gpu(_) => None,
        }
    }

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

    /// The CPU stores half floats, like the GPU's usual intermediate.
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

    /// Returns the bytes released.
    pub fn release_idle_memory(&self) -> u64 {
        match self {
            Self::Gpu(renderer) => renderer.release_idle_memory(),
            Self::Cpu(renderer) => renderer.release_idle_memory(),
        }
    }

    /// Drops the CPU renderer's frame buffers kept for the next frame; the GPU keeps none.
    pub fn release_frame_buffers(&self) {
        if let Self::Cpu(renderer) = self {
            renderer.release_frame_buffers();
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

    pub fn text_upload(&self) -> &dyn TextUpload {
        match self {
            Self::Gpu(renderer) => &**renderer,
            Self::Cpu(renderer) => &**renderer,
        }
    }
}
