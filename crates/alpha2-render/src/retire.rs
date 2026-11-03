//! Destroying what the renderer's pools let go of.
//!
//! Browser `WebGPU` frees a dropped texture or buffer only at garbage collection, which is blind
//! to GPU memory, so every resource a pool releases is destroyed explicitly (also freeing it
//! sooner on `wgpu-core`).
//!
//! A pool can release a texture an unsubmitted encoder still uses (a graph returns
//! intermediates once read), and destroying it would invalidate that submission. So every
//! renderer encoder holds an [`EncodeScope`] until submission, and the last scope to close
//! destroys what waited. Destroying resources used by already submitted work is valid; the work
//! completes first.

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

#[derive(Debug, Default)]
pub(crate) struct Retirement {
    state: Mutex<RetirementState>,
    /// Current trust generation; only resources of the current one return to a pool (see
    /// [`Retirement::doubt`]).
    generation: AtomicU64,
}

#[derive(Debug, Default)]
struct RetirementState {
    open: usize,
    waiting: Vec<Retired>,
    retired_bytes: u64,
}

/// One renderer encoder from creation until submission or drop; defers destruction while open.
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

    /// Destroys each resource, or defers it until open encodes are submitted. Do not hold a pool
    /// lock: destroying takes the device's locks.
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

    /// Destroys buffers made for one encode, after open encodes are submitted. They add nothing
    /// to [`Retirement::retired_bytes`], which counts what pools give up.
    pub(crate) fn discard(&self, buffers: impl IntoIterator<Item = wgpu::Buffer>) {
        self.retire(
            buffers
                .into_iter()
                .map(|buffer| (Retired::Buffer(buffer), 0)),
        );
    }

    /// Opens an encode scope; nothing retired until it closes is destroyed before then.
    pub(crate) fn open(self: &Shared<Self>) -> EncodeScope {
        let mut state = self.lock();
        state.open = state.open.saturating_add(1);
        drop(state);
        EncodeScope {
            retirement: Shared::clone(self),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn trusts(&self, generation: u64) -> bool {
        generation == self.generation()
    }

    /// Distrusts every pooled resource created so far, after a reported GPU error. A failed
    /// allocation returns an invalid object indistinguishable from a valid one, so pools destroy
    /// these instead of reusing them (see [`crate::gpu_errors`]).
    pub(crate) fn doubt(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Bytes retired so far, destroyed or waiting.
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

/// Buffers made for one encode, discarded ([`Retirement::discard`]) when this drops, on every
/// return path.
#[derive(Debug)]
pub(crate) struct TransientBuffers<'a> {
    retirement: &'a Retirement,
    buffers: Vec<wgpu::Buffer>,
}

impl<'a> TransientBuffers<'a> {
    pub(crate) const fn new(retirement: &'a Retirement) -> Self {
        Self {
            retirement,
            buffers: Vec::new(),
        }
    }

    /// Discards `buffer` with the rest, and hands it back.
    pub(crate) fn keep(&mut self, buffer: wgpu::Buffer) -> wgpu::Buffer {
        self.buffers.push(buffer.clone());
        buffer
    }
}

impl Drop for TransientBuffers<'_> {
    fn drop(&mut self) {
        self.retirement.discard(std::mem::take(&mut self.buffers));
    }
}
