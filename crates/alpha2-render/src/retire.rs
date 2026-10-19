//! Destroying what the renderer's pools let go of.
//!
//! On `wgpu-core` backends dropping the last handle frees a texture or buffer,
//! but on the browser's WebGPU backend the drop does nothing: the memory stays
//! until garbage collection finalizes the JavaScript wrapper, and no browser
//! tells its collector about GPU memory. So every resource a pool stops
//! keeping (cleared, evicted, declined, or abandoned) is destroyed here, on
//! every backend, which is also the earlier release on `wgpu-core`.
//!
//! A pool can let go of a texture that a command encoder still being recorded
//! uses: a render graph hands an intermediate back as soon as the next pass
//! has read it, so a later pass of the same frame can reuse it, and the
//! budget may evict it right then. Destroying it would invalidate that
//! frame's submission. So while any renderer encode is open, destruction
//! waits: every renderer command encoder holds an [`EncodeScope`] until its
//! commands are submitted, and the last scope to close destroys what waited.
//! Destroying a resource that submitted work uses is valid on both WebGPU and
//! `wgpu-core`; the work completes first.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Shared;

/// A resource a pool no longer keeps.
#[derive(Debug)]
pub(crate) enum Retired {
    Texture(wgpu::Texture),
    Buffer(wgpu::Buffer),
}

impl Retired {
    fn destroy(self) {
        match self {
            Self::Texture(texture) => texture.destroy(),
            Self::Buffer(buffer) => buffer.destroy(),
        }
    }
}

/// The resources pools have let go of, and the encodes they wait for.
#[derive(Debug, Default)]
pub(crate) struct Retirement {
    state: Mutex<RetirementState>,
    /// Which pooled resources are trusted: each records the generation it
    /// was created in, and only the current one's go back to a pool. See
    /// [`Retirement::doubt`].
    generation: AtomicU64,
}

#[derive(Debug, Default)]
struct RetirementState {
    /// Renderer command encoders recorded but not yet submitted.
    open: usize,
    /// Resources let go of while an encode was open.
    waiting: Vec<Retired>,
    /// Bytes of every resource let go of, destroyed or waiting to be.
    retired_bytes: u64,
}

/// One renderer command encoder from creation until its commands are
/// submitted (or it is dropped unsubmitted). While any scope is open, what
/// the pools let go of is not destroyed.
#[derive(Debug)]
pub(crate) struct EncodeScope {
    retirement: Shared<Retirement>,
}

impl Retirement {
    fn lock(&self) -> std::sync::MutexGuard<'_, RetirementState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Destroys each resource, or keeps it until the open encodes are
    /// submitted. Call it without holding a pool's lock: destroying takes
    /// the device's own locks.
    pub(crate) fn retire(&self, resources: impl IntoIterator<Item = (Retired, u64)>) {
        let mut now = Vec::new();
        {
            let mut state = self.lock();
            for (resource, bytes) in resources {
                state.retired_bytes = state.retired_bytes.saturating_add(bytes);
                if state.open == 0 {
                    now.push(resource);
                } else {
                    state.waiting.push(resource);
                }
            }
        }
        for resource in now {
            resource.destroy();
        }
    }

    /// Opens an encode: nothing retired from now until the scope closes is
    /// destroyed before then.
    pub(crate) fn open(self: &Shared<Self>) -> EncodeScope {
        let mut state = self.lock();
        state.open = state.open.saturating_add(1);
        drop(state);
        EncodeScope {
            retirement: Shared::clone(self),
        }
    }

    /// The generation a resource created now belongs to.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether a resource created in `generation` may still be pooled.
    pub(crate) fn trusts(&self, generation: u64) -> bool {
        generation == self.generation()
    }

    /// Stops trusting every pooled resource created so far, after the GPU
    /// reported an error for work that may have used one: an allocation
    /// that failed returns an invalid object that looks like any other, and
    /// every pass that uses it fails. Each pool destroys such a resource
    /// instead of handing it out or keeping it when it comes back (see
    /// [`crate::gpu_errors`]).
    pub(crate) fn doubt(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Bytes of every resource retired so far, whether destroyed yet or
    /// waiting for an open encode.
    pub(crate) fn retired_bytes(&self) -> u64 {
        self.lock().retired_bytes
    }
}

impl Drop for EncodeScope {
    fn drop(&mut self) {
        let waiting = {
            let mut state = self.retirement.lock();
            state.open = state.open.saturating_sub(1);
            if state.open == 0 {
                std::mem::take(&mut state.waiting)
            } else {
                Vec::new()
            }
        };
        for resource in waiting {
            resource.destroy();
        }
    }
}
