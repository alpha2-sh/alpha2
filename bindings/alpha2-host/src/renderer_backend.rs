//! The renderer a host draws with: the GPU [`Renderer`] on a hardware (or,
//! when asked for, software) adapter, or the [`CpuRenderer`].

use alpha2_engine::TextUpload;
use alpha2_render::{CpuRenderer, Lut3dData, RenderError, Renderer, ResultCacheStats};

/// The renderer a host draws with, chosen once when it starts.
pub enum RendererBackend {
    Gpu(Box<Renderer>),
    Cpu(Box<CpuRenderer>),
}

/// What a result reports about the device that rendered it.
pub struct AdapterIdentity {
    pub name: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
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
                }
            }
            Self::Cpu(renderer) => AdapterIdentity {
                name: format!("alpha2 CPU renderer ({} threads)", renderer.threads()),
                backend: "cpu".to_owned(),
                driver: "alpha2".to_owned(),
                driver_info: env!("CARGO_PKG_VERSION").to_owned(),
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
