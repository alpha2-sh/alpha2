//! Bind groups, all made through [`BindGroups::create`], kept by what they bind when it is ours.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::sync::Mutex;

use crate::retire::Retired;

/// Bind group creation and reuse counters.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BindGroupStats {
    /// Bind groups created since renderer construction.
    pub created: u64,
    /// Requests served by a kept bind group.
    pub reused: u64,
    /// Bind groups kept for reuse now.
    pub kept: usize,
}

#[derive(Debug)]
pub(crate) struct OffsetBindGroup<const N: usize> {
    pub(crate) group: wgpu::BindGroup,
    // WebGPU takes dynamic offsets in binding number order.
    pub(crate) offsets: [u32; N],
}

impl<const N: usize> OffsetBindGroup<N> {
    pub(crate) fn set(&self, pass: &mut wgpu::RenderPass<'_>, index: u32) {
        pass.set_bind_group(index, &self.group, &self.offsets);
    }
}

const UNUSED_FRAMES_KEPT: u64 = 64;

#[derive(Debug, Default)]
pub(crate) struct BindGroups {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    owned_textures: HashSet<wgpu::Texture>,
    owned_buffers: HashSet<wgpu::Buffer>,
    kept: HashMap<Box<[Bound]>, Kept>,
    key: Vec<Bound>,
    frame: u64,
    created: u64,
    reused: u64,
}

#[derive(Debug)]
struct Kept {
    group: wgpu::BindGroup,
    used_frame: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Bound {
    Layout(wgpu::BindGroupLayout),
    View(u32, wgpu::TextureView),
    Sampler(u32, wgpu::Sampler),
    Buffer(u32, wgpu::Buffer, u64, Option<NonZeroU64>),
}

impl Bound {
    fn binds_texture(&self, texture: &wgpu::Texture) -> bool {
        matches!(self, Self::View(_, view) if view.texture() == texture)
    }

    fn binds_buffer(&self, buffer: &wgpu::Buffer) -> bool {
        matches!(self, Self::Buffer(_, bound, ..) if bound == buffer)
    }
}

impl BindGroups {
    pub(crate) fn create(
        &self,
        device: &wgpu::Device,
        descriptor: &wgpu::BindGroupDescriptor<'_>,
        frame: u64,
    ) -> wgpu::BindGroup {
        let mut state = self.lock();
        if state.frame != frame {
            state.frame = frame;
            state
                .kept
                .retain(|_, kept| frame.saturating_sub(kept.used_frame) < UNUSED_FRAMES_KEPT);
        }
        let mut key = std::mem::take(&mut state.key);
        key.clear();
        let keepable = state.key_of(descriptor, &mut key);
        if keepable && let Some(kept) = state.kept.get_mut(key.as_slice()) {
            kept.used_frame = frame;
            let group = kept.group.clone();
            state.reused = state.reused.saturating_add(1);
            state.key = key;
            return group;
        }
        state.created = state.created.saturating_add(1);
        #[expect(
            clippy::disallowed_methods,
            reason = "the one place a bind group is created"
        )]
        let group = device.create_bind_group(descriptor);
        if keepable {
            state.kept.insert(
                key.into_boxed_slice(),
                Kept {
                    group: group.clone(),
                    used_frame: frame,
                },
            );
        } else {
            state.key = key;
        }
        group
    }

    pub(crate) fn adopt_texture(&self, texture: &wgpu::Texture) {
        self.lock().owned_textures.insert(texture.clone());
    }

    pub(crate) fn adopt_buffer(&self, buffer: &wgpu::Buffer) {
        self.lock().owned_buffers.insert(buffer.clone());
    }

    pub(crate) fn release(&self, resource: &Retired) {
        match resource {
            Retired::Texture(texture) => self.release_texture(texture),
            Retired::Buffer(buffer) => self.release_buffer(buffer),
        }
    }

    pub(crate) fn release_texture(&self, texture: &wgpu::Texture) {
        let mut state = self.lock();
        if state.owned_textures.remove(texture) {
            state
                .kept
                .retain(|key, _| !key.iter().any(|bound| bound.binds_texture(texture)));
        }
    }

    pub(crate) fn release_buffer(&self, buffer: &wgpu::Buffer) {
        let mut state = self.lock();
        if state.owned_buffers.remove(buffer) {
            state
                .kept
                .retain(|key, _| !key.iter().any(|bound| bound.binds_buffer(buffer)));
        }
    }

    pub(crate) fn clear(&self) {
        self.lock().kept.clear();
    }

    pub(crate) fn stats(&self) -> BindGroupStats {
        let state = self.lock();
        BindGroupStats {
            created: state.created,
            reused: state.reused,
            kept: state.kept.len(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl State {
    fn key_of(&self, descriptor: &wgpu::BindGroupDescriptor<'_>, key: &mut Vec<Bound>) -> bool {
        key.push(Bound::Layout(descriptor.layout.clone()));
        for entry in descriptor.entries {
            let bound = match &entry.resource {
                wgpu::BindingResource::TextureView(view)
                    if self.owned_textures.contains(view.texture()) =>
                {
                    Bound::View(entry.binding, (*view).clone())
                }
                wgpu::BindingResource::Sampler(sampler) => {
                    Bound::Sampler(entry.binding, (*sampler).clone())
                }
                wgpu::BindingResource::Buffer(binding)
                    if self.owned_buffers.contains(binding.buffer) =>
                {
                    Bound::Buffer(
                        entry.binding,
                        binding.buffer.clone(),
                        binding.offset,
                        binding.size,
                    )
                }
                _ => return false,
            };
            key.push(bound);
        }
        true
    }
}
