use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::RenderSize;
use crate::Shared;
use crate::retire::{Retired, Retirement};

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TextureKey {
    size: RenderSize,
    format: wgpu::TextureFormat,
}

/// Observable allocation counters for the intermediate texture pool.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct TexturePoolStats {
    /// Number of physical textures created since renderer construction.
    pub created: u64,
    /// Number of acquisitions served by an existing texture.
    pub reused: u64,
    /// Textures currently owned by render results or in-flight work.
    pub active: usize,
    /// Approximate bytes held by active intermediate textures.
    pub active_bytes: u64,
    /// Maximum active intermediate-texture bytes observed.
    pub high_water_bytes: u64,
    /// Textures currently idle and available for reuse.
    pub available: usize,
    /// Approximate bytes retained by idle textures.
    pub available_bytes: u64,
    /// Current soft limit for idle texture retention.
    pub budget_bytes: u64,
    /// Textures discarded because retaining them would exceed the budget.
    pub evicted: u64,
}

/// Allocation and retention counters for source-upload textures.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct UploadTexturePoolStats {
    /// Physical textures created since renderer construction.
    pub created: u64,
    /// Acquisitions served by an idle texture with the same descriptor.
    pub reused: u64,
    /// Textures dropped instead of retained because the pool was full.
    pub discarded: u64,
    /// Textures currently held by upload or source-frame owners.
    pub active: usize,
    /// Approximate bytes held by active upload textures.
    pub active_bytes: u64,
    /// Maximum active bytes observed since renderer construction.
    pub high_water_bytes: u64,
    /// Textures currently idle and available for reuse.
    pub available: usize,
    /// Approximate bytes retained by idle upload textures.
    pub available_bytes: u64,
    /// Maximum bytes retained by the idle pool.
    pub budget_bytes: u64,
}

#[derive(Debug)]
struct UploadTextureEntry {
    key: TextureKey,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// [`Retirement::generation`] at creation.
    generation: u64,
}

#[derive(Debug)]
pub(crate) struct UploadTexturePool {
    state: Mutex<UploadTexturePoolState>,
    retirement: Shared<Retirement>,
}

#[derive(Debug)]
struct UploadTexturePoolState {
    available: VecDeque<UploadTextureEntry>,
    active_by_key: HashMap<TextureKey, usize>,
    peak_active_by_key: HashMap<TextureKey, usize>,
    created: u64,
    reused: u64,
    discarded: u64,
    active: usize,
    active_bytes: u64,
    high_water_bytes: u64,
    available_bytes: u64,
    budget_bytes: u64,
    /// Generation whose untrusted idle textures were last dropped.
    checked_generation: u64,
}

impl UploadTexturePool {
    pub(crate) fn new(retirement: Shared<Retirement>) -> Self {
        Self {
            retirement,
            state: Mutex::new(UploadTexturePoolState {
                available: VecDeque::new(),
                active_by_key: HashMap::new(),
                peak_active_by_key: HashMap::new(),
                created: 0,
                reused: 0,
                discarded: 0,
                active: 0,
                active_bytes: 0,
                high_water_bytes: 0,
                available_bytes: 0,
                budget_bytes: 128 * 1024 * 1024,
                checked_generation: 0,
            }),
        }
    }
}

/// One source-upload texture retained by the renderer's descriptor-keyed pool.
#[derive(Debug)]
pub struct UploadTexture {
    pool: Shared<UploadTexturePool>,
    entry: Option<UploadTextureEntry>,
}

// Caps object count for workloads of many tiny one-off descriptors; bytes are the main bound.
const MAX_IDLE_UPLOAD_TEXTURES_HARD_LIMIT: usize = 64;

#[derive(Debug)]
pub(crate) struct TexturePool {
    state: Mutex<TexturePoolState>,
    retirement: Shared<Retirement>,
}

/// An idle intermediate texture, oldest release first in the pool.
#[derive(Debug)]
struct IdleTexture {
    key: TextureKey,
    texture: wgpu::Texture,
    /// Created with the texture so pooled passes never create a view.
    view: wgpu::TextureView,
    /// Lets a texture survive one eviction scan (see [`TexturePool::release`]).
    second_chance: bool,
    /// [`Retirement::generation`] at creation.
    generation: u64,
}

#[derive(Debug)]
struct TexturePoolState {
    available: VecDeque<IdleTexture>,
    created: u64,
    reused: u64,
    active: usize,
    active_bytes: u64,
    high_water_bytes: u64,
    available_bytes: u64,
    budget_bytes: u64,
    evicted: u64,
    /// Generation whose untrusted idle textures were last dropped.
    checked_generation: u64,
}

impl Default for TexturePoolState {
    fn default() -> Self {
        Self {
            available: VecDeque::new(),
            created: 0,
            reused: 0,
            active: 0,
            active_bytes: 0,
            high_water_bytes: 0,
            available_bytes: 0,
            budget_bytes: 256 * 1024 * 1024,
            evicted: 0,
            checked_generation: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct TextureLease {
    pool: Shared<TexturePool>,
    key: TextureKey,
    texture: Option<(wgpu::Texture, wgpu::TextureView)>,
    generation: u64,
}

#[derive(Debug)]
pub(crate) struct RecycleToken {
    pool: Shared<TexturePool>,
    key: TextureKey,
    generation: u64,
}

const MAX_IDLE_READBACK_BUFFERS: usize = 3;

#[derive(Debug)]
pub(crate) struct ReadbackBufferPool {
    state: Mutex<ReadbackBufferPoolState>,
    retirement: Shared<Retirement>,
}

#[derive(Debug, Default)]
struct ReadbackBufferPoolState {
    /// Size, buffer, and creation generation.
    available: Vec<(u64, wgpu::Buffer, u64)>,
    /// Generation whose untrusted idle buffers were last dropped.
    checked_generation: u64,
    created: u64,
    reused: u64,
    discarded: u64,
    active: usize,
    active_bytes: u64,
    high_water_bytes: u64,
    available_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct ReadbackBufferLease {
    pool: Shared<ReadbackBufferPool>,
    size: u64,
    buffer: Option<wgpu::Buffer>,
    generation: u64,
}

/// Allocation and retention counters for mapped readback buffers.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ReadbackBufferPoolStats {
    /// Physical buffers created since renderer construction.
    pub created: u64,
    /// Acquisitions served by an unmapped idle buffer.
    pub reused: u64,
    /// Buffers dropped instead of retained, including unfinished readbacks.
    pub discarded: u64,
    /// Buffers currently owned by submitted or mapped readbacks.
    pub active: usize,
    /// Bytes currently owned by submitted or mapped readbacks.
    pub active_bytes: u64,
    /// Maximum active bytes observed since renderer construction.
    pub high_water_bytes: u64,
    /// Unmapped buffers currently available for reuse.
    pub available: usize,
    /// Bytes retained by idle reusable buffers.
    pub available_bytes: u64,
}

impl TexturePool {
    pub(crate) fn new(retirement: Shared<Retirement>) -> Self {
        Self {
            state: Mutex::default(),
            retirement,
        }
    }

    pub(crate) fn acquire(
        self: &Shared<Self>,
        device: &wgpu::Device,
        size: RenderSize,
        format: wgpu::TextureFormat,
        label: &'static str,
    ) -> TextureLease {
        let key = TextureKey { size, format };
        let (texture, generation) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let untrusted = self.drop_untrusted(&mut state);
            let index = state
                .available
                .iter()
                .rposition(|available| available.key == key);
            let reused = index
                .and_then(|index| state.available.remove(index))
                .map(|idle| ((idle.texture, idle.view), idle.generation));
            if reused.is_some() {
                state.reused = state.reused.saturating_add(1);
                state.available_bytes = state.available_bytes.saturating_sub(key.byte_len());
            }
            drop(state);
            self.retirement.retire(untrusted);
            reused
        }
        .unwrap_or_else(|| {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.created = state.created.saturating_add(1);
            drop(state);
            let generation = self.retirement.generation();
            let texture = create_texture(device, size, format, label);
            let view = default_view(&texture);
            ((texture, view), generation)
        });
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_add(1);
            state.active_bytes = state.active_bytes.saturating_add(key.byte_len());
            state.high_water_bytes = state.high_water_bytes.max(state.active_bytes);
        }
        TextureLease {
            pool: Shared::clone(self),
            key,
            texture: Some(texture),
            generation,
        }
    }

    /// Removes idle textures created before the last reported GPU error, once per report, for the
    /// caller to retire.
    fn drop_untrusted(&self, state: &mut TexturePoolState) -> Vec<(Retired, u64)> {
        let generation = self.retirement.generation();
        if state.checked_generation == generation {
            return Vec::new();
        }
        state.checked_generation = generation;
        let (kept, untrusted): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut state.available)
            .into_iter()
            .partition(|idle| idle.generation == generation);
        state.available = kept;
        state.available_bytes = state.available.iter().map(|idle| idle.key.byte_len()).sum();
        state.evicted = state
            .evicted
            .saturating_add(u64::try_from(untrusted.len()).unwrap_or(u64::MAX));
        untrusted.into_iter().map(IdleTexture::retired).collect()
    }

    pub(crate) fn stats(&self) -> TexturePoolStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        TexturePoolStats {
            created: state.created,
            reused: state.reused,
            active: state.active,
            active_bytes: state.active_bytes,
            high_water_bytes: state.high_water_bytes,
            available: state.available.len(),
            available_bytes: state.available_bytes,
            budget_bytes: state.budget_bytes,
            evicted: state.evicted,
        }
    }

    /// Destroys every idle texture.
    pub(crate) fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idle = std::mem::take(&mut state.available);
        state.available_bytes = 0;
        drop(state);
        self.retirement
            .retire(idle.into_iter().map(IdleTexture::retired));
    }

    pub(crate) fn set_budget(&self, budget_bytes: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.budget_bytes = budget_bytes;
        let mut evicted = Vec::new();
        while state.available_bytes > state.budget_bytes {
            let Some(idle) = state.available.pop_front() else {
                state.available_bytes = 0;
                break;
            };
            state.available_bytes = state.available_bytes.saturating_sub(idle.key.byte_len());
            state.evicted = state.evicted.saturating_add(1);
            evicted.push(idle.retired());
        }
        drop(state);
        self.retirement.retire(evicted);
    }
}

impl IdleTexture {
    fn retired(self) -> (Retired, u64) {
        (Retired::Texture(self.texture), self.key.byte_len())
    }
}

impl UploadTexturePool {
    pub(crate) fn acquire(
        self: &Shared<Self>,
        device: &wgpu::Device,
        size: RenderSize,
        format: wgpu::TextureFormat,
        label: &'static str,
    ) -> UploadTexture {
        let key = TextureKey { size, format };
        let reused = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let untrusted = self.drop_untrusted(&mut state);
            let index = state.available.iter().position(|entry| entry.key == key);
            let reused = index.and_then(|index| {
                let entry = state.available.remove(index)?;
                state.reused = state.reused.saturating_add(1);
                state.available_bytes = state.available_bytes.saturating_sub(key.byte_len());
                Some(entry)
            });
            drop(state);
            self.retirement.retire(untrusted);
            reused
        };
        let entry = reused.unwrap_or_else(|| {
            let generation = self.retirement.generation();
            let texture = create_upload_texture(device, size, format, label);
            let view = default_view(&texture);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.created = state.created.saturating_add(1);
            UploadTextureEntry {
                key,
                texture,
                view,
                generation,
            }
        });
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_add(1);
            state.active_bytes = state.active_bytes.saturating_add(key.byte_len());
            state.high_water_bytes = state.high_water_bytes.max(state.active_bytes);
            let active = state.active_by_key.entry(key).or_default();
            *active = active.saturating_add(1);
            let active = *active;
            let peak = state.peak_active_by_key.entry(key).or_default();
            *peak = (*peak).max(active);
        }
        UploadTexture {
            pool: Shared::clone(self),
            entry: Some(entry),
        }
    }

    pub(crate) fn stats(&self) -> UploadTexturePoolStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UploadTexturePoolStats {
            created: state.created,
            reused: state.reused,
            discarded: state.discarded,
            active: state.active,
            active_bytes: state.active_bytes,
            high_water_bytes: state.high_water_bytes,
            available: state.available.len(),
            available_bytes: state.available_bytes,
            budget_bytes: state.budget_bytes,
        }
    }

    /// Destroys every idle upload texture.
    pub(crate) fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let discarded = u64::try_from(state.available.len()).unwrap_or(u64::MAX);
        let idle = std::mem::take(&mut state.available);
        state.discarded = state.discarded.saturating_add(discarded);
        state.available_bytes = 0;
        drop(state);
        self.retirement
            .retire(idle.into_iter().map(UploadTextureEntry::retired));
    }

    /// Test-only idle byte budget.
    #[cfg(test)]
    pub(crate) fn set_budget_bytes(&self, bytes: u64) {
        self.set_budget(bytes);
    }

    /// Sets the idle byte budget, destroying idle textures past it.
    pub(crate) fn set_budget(&self, budget_bytes: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.budget_bytes = budget_bytes;
        let mut retired = Vec::new();
        while state.available_bytes > state.budget_bytes {
            let Some(idle) = state.available.pop_front() else {
                state.available_bytes = 0;
                break;
            };
            state.available_bytes = state.available_bytes.saturating_sub(idle.key.byte_len());
            state.discarded = state.discarded.saturating_add(1);
            retired.push(idle);
        }
        drop(state);
        self.retirement
            .retire(retired.into_iter().map(UploadTextureEntry::retired));
    }

    fn release(&self, entry: UploadTextureEntry) {
        let mut retired = Vec::new();
        self.keep_or_retire(entry, &mut retired);
        self.retirement.retire(retired);
    }

    /// Removes idle textures created before the last reported GPU error, once per report.
    fn drop_untrusted(&self, state: &mut UploadTexturePoolState) -> Vec<(Retired, u64)> {
        let generation = self.retirement.generation();
        if state.checked_generation == generation {
            return Vec::new();
        }
        state.checked_generation = generation;
        let (kept, untrusted): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut state.available)
            .into_iter()
            .partition(|entry| entry.generation == generation);
        state.available = kept;
        state.available_bytes = state
            .available
            .iter()
            .map(|entry| entry.key.byte_len())
            .sum();
        state.discarded = state
            .discarded
            .saturating_add(u64::try_from(untrusted.len()).unwrap_or(u64::MAX));
        for entry in &untrusted {
            prune_upload_texture_key(state, entry.key);
        }
        untrusted
            .into_iter()
            .map(UploadTextureEntry::retired)
            .collect()
    }

    /// Keeps a returned texture idle, evicting older ones for room, or retires it.
    fn keep_or_retire(&self, entry: UploadTextureEntry, retired: &mut Vec<(Retired, u64)>) {
        let bytes = entry.key.byte_len();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        state.active_bytes = state.active_bytes.saturating_sub(bytes);
        if let Some(active) = state.active_by_key.get_mut(&entry.key) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_by_key.remove(&entry.key);
            }
        }
        if !self.retirement.trusts(entry.generation) {
            state.discarded = state.discarded.saturating_add(1);
            prune_upload_texture_key(&mut state, entry.key);
            retired.push(entry.retired());
            return;
        }
        let retention_target = state
            .peak_active_by_key
            .get(&entry.key)
            .copied()
            .unwrap_or(1);
        let available_for_key = state
            .available
            .iter()
            .filter(|available| available.key == entry.key)
            .count();
        if available_for_key >= retention_target {
            state.discarded = state.discarded.saturating_add(1);
            retired.push(entry.retired());
            return;
        }
        // Evicting for a texture the budget can never hold would empty the pool without making
        // room.
        if bytes > state.budget_bytes {
            state.discarded = state.discarded.saturating_add(1);
            prune_upload_texture_key(&mut state, entry.key);
            retired.push(entry.retired());
            return;
        }
        while state.available.len() >= MAX_IDLE_UPLOAD_TEXTURES_HARD_LIMIT
            || state.available_bytes.saturating_add(bytes) > state.budget_bytes
        {
            let Some(discarded) = state.available.pop_front() else {
                state.discarded = state.discarded.saturating_add(1);
                prune_upload_texture_key(&mut state, entry.key);
                retired.push(entry.retired());
                return;
            };
            state.available_bytes = state
                .available_bytes
                .saturating_sub(discarded.key.byte_len());
            state.discarded = state.discarded.saturating_add(1);
            prune_upload_texture_key(&mut state, discarded.key);
            retired.push(discarded.retired());
        }
        state.available.push_back(entry);
        state.available_bytes = state.available_bytes.saturating_add(bytes);
    }
}

impl UploadTextureEntry {
    fn retired(self) -> (Retired, u64) {
        (Retired::Texture(self.texture), self.key.byte_len())
    }
}

fn prune_upload_texture_key(state: &mut UploadTexturePoolState, key: TextureKey) {
    if !state.active_by_key.contains_key(&key)
        && !state.available.iter().any(|entry| entry.key == key)
    {
        state.peak_active_by_key.remove(&key);
    }
}

impl UploadTexture {
    fn entry(&self) -> &UploadTextureEntry {
        self.entry.as_ref().expect("live upload texture lease")
    }

    /// Returns the texture used as a copy destination.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn texture(&self) -> &wgpu::Texture {
        &self.entry().texture
    }

    /// Returns the stable default view cached beside the texture.
    #[must_use]
    #[cfg_attr(not(feature = "wgpu-interop"), doc(hidden))]
    pub fn view(&self) -> &wgpu::TextureView {
        &self.entry().view
    }

    /// Returns the physical texture storage retained by this lease.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.entry().key.byte_len()
    }
}

impl Drop for UploadTexture {
    fn drop(&mut self) {
        if let Some(entry) = self.entry.take() {
            self.pool.release(entry);
        }
    }
}

impl ReadbackBufferPool {
    pub(crate) fn new(retirement: Shared<Retirement>) -> Self {
        Self {
            state: Mutex::default(),
            retirement,
        }
    }

    pub(crate) fn acquire(
        self: &Shared<Self>,
        device: &wgpu::Device,
        size: u64,
        label: &'static str,
    ) -> ReadbackBufferLease {
        let reused = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let untrusted = self.drop_untrusted(&mut state);
            let index = state
                .available
                .iter()
                .position(|(available_size, _, _)| *available_size == size);
            let reused = index.map(|index| {
                state.reused = state.reused.saturating_add(1);
                state.available_bytes = state.available_bytes.saturating_sub(size);
                let (_, buffer, generation) = state.available.remove(index);
                (buffer, generation)
            });
            drop(state);
            self.retirement.retire(untrusted);
            reused
        };
        let (buffer, generation) = reused.unwrap_or_else(|| {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.created = state.created.saturating_add(1);
            drop(state);
            let generation = self.retirement.generation();
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            (buffer, generation)
        });
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_add(1);
            state.active_bytes = state.active_bytes.saturating_add(size);
            state.high_water_bytes = state.high_water_bytes.max(state.active_bytes);
        }
        ReadbackBufferLease {
            pool: Shared::clone(self),
            size,
            buffer: Some(buffer),
            generation,
        }
    }

    /// Removes idle buffers created before the last reported GPU error, once per report.
    fn drop_untrusted(&self, state: &mut ReadbackBufferPoolState) -> Vec<(Retired, u64)> {
        let generation = self.retirement.generation();
        if state.checked_generation == generation {
            return Vec::new();
        }
        state.checked_generation = generation;
        let (kept, untrusted): (Vec<_>, Vec<_>) = std::mem::take(&mut state.available)
            .into_iter()
            .partition(|(_, _, created)| *created == generation);
        state.available = kept;
        state.available_bytes = state.available.iter().map(|(size, _, _)| *size).sum();
        state.discarded = state
            .discarded
            .saturating_add(u64::try_from(untrusted.len()).unwrap_or(u64::MAX));
        untrusted
            .into_iter()
            .map(|(size, buffer, _)| (Retired::Buffer(buffer), size))
            .collect()
    }

    pub(crate) fn stats(&self) -> ReadbackBufferPoolStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ReadbackBufferPoolStats {
            created: state.created,
            reused: state.reused,
            discarded: state.discarded,
            active: state.active,
            active_bytes: state.active_bytes,
            high_water_bytes: state.high_water_bytes,
            available: state.available.len(),
            available_bytes: state.available_bytes,
        }
    }

    /// Destroys every idle readback buffer.
    pub(crate) fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let discarded = u64::try_from(state.available.len()).unwrap_or(u64::MAX);
        let idle = std::mem::take(&mut state.available);
        state.discarded = state.discarded.saturating_add(discarded);
        state.available_bytes = 0;
        drop(state);
        self.retirement.retire(
            idle.into_iter()
                .map(|(size, buffer, _)| (Retired::Buffer(buffer), size)),
        );
    }

    fn release(&self, size: u64, buffer: wgpu::Buffer, generation: u64) {
        if !self.retirement.trusts(generation) {
            self.discard(size, buffer);
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        state.active_bytes = state.active_bytes.saturating_sub(size);
        let mut retired = None;
        if state.available.len() == MAX_IDLE_READBACK_BUFFERS {
            let (discarded_size, discarded, _) = state.available.remove(0);
            state.available_bytes = state.available_bytes.saturating_sub(discarded_size);
            state.discarded = state.discarded.saturating_add(1);
            retired = Some((Retired::Buffer(discarded), discarded_size));
        }
        state.available.push((size, buffer, generation));
        state.available_bytes = state.available_bytes.saturating_add(size);
        drop(state);
        self.retirement.retire(retired);
    }

    /// Takes back a buffer whose readback was abandoned.
    fn discard(&self, size: u64, buffer: wgpu::Buffer) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
        state.active_bytes = state.active_bytes.saturating_sub(size);
        state.discarded = state.discarded.saturating_add(1);
        drop(state);
        self.retirement.retire([(Retired::Buffer(buffer), size)]);
    }
}

impl ReadbackBufferLease {
    pub(crate) fn buffer(&self) -> &wgpu::Buffer {
        self.buffer.as_ref().expect("live readback buffer lease")
    }

    pub(crate) fn recycle(mut self) {
        let buffer = self.buffer.take().expect("live readback buffer lease");
        self.pool.release(self.size, buffer, self.generation);
    }
}

impl Drop for ReadbackBufferLease {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            self.pool.discard(self.size, buffer);
        }
    }
}

impl TextureLease {
    pub(crate) fn texture(&self) -> &wgpu::Texture {
        &self.texture.as_ref().expect("live texture lease").0
    }

    pub(crate) fn view(&self) -> &wgpu::TextureView {
        &self.texture.as_ref().expect("live texture lease").1
    }

    pub(crate) fn into_parts(mut self) -> (wgpu::Texture, wgpu::TextureView, RecycleToken) {
        let (texture, view) = self.texture.take().expect("live texture lease");
        let token = RecycleToken {
            pool: Shared::clone(&self.pool),
            key: self.key,
            generation: self.generation,
        };
        (texture, view, token)
    }
}

impl Drop for TextureLease {
    fn drop(&mut self) {
        if let Some((texture, view)) = self.texture.take() {
            self.pool.release(self.key, texture, view, self.generation);
        }
    }
}

impl RecycleToken {
    pub(crate) fn recycle(self, texture: wgpu::Texture, view: wgpu::TextureView) {
        self.pool.release(self.key, texture, view, self.generation);
    }
}

impl TexturePool {
    /// Returns a texture to the idle pool, evicting older idle textures of other descriptors to fit
    /// the budget.
    ///
    /// Eviction is second-chance (CLOCK), oldest release first, so stale descriptors leave on the
    /// next scan while two alternating working sets do not evict each other on every release. If
    /// nothing can be evicted the incoming texture is dropped. Dropped textures are destroyed via
    /// [`crate::retire`] after any frame still encoding them is submitted.
    fn release(
        &self,
        key: TextureKey,
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        generation: u64,
    ) {
        let mut retired = Vec::new();
        self.keep_or_retire(
            IdleTexture {
                key,
                texture,
                view,
                second_chance: true,
                generation,
            },
            &mut retired,
        );
        self.retirement.retire(retired);
    }

    fn keep_or_retire(&self, incoming: IdleTexture, retired: &mut Vec<(Retired, u64)>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = incoming.key;
        let bytes = key.byte_len();
        state.active = state.active.saturating_sub(1);
        state.active_bytes = state.active_bytes.saturating_sub(bytes);
        // Too big for the budget, or created before a reported GPU error and possibly invalid.
        if bytes > state.budget_bytes || !self.retirement.trusts(incoming.generation) {
            state.evicted = state.evicted.saturating_add(1);
            retired.push(incoming.retired());
            return;
        }
        let mut index = 0;
        while state.available_bytes.saturating_add(bytes) > state.budget_bytes {
            let Some(idle) = state.available.get_mut(index) else {
                // Everything left is protected or has this descriptor.
                state.evicted = state.evicted.saturating_add(1);
                retired.push(incoming.retired());
                return;
            };
            if idle.key == key {
                index += 1;
            } else if idle.second_chance {
                idle.second_chance = false;
                index += 1;
            } else if let Some(evicted) = state.available.remove(index) {
                state.available_bytes =
                    state.available_bytes.saturating_sub(evicted.key.byte_len());
                state.evicted = state.evicted.saturating_add(1);
                retired.push(evicted.retired());
            }
        }
        state.available.push_back(incoming);
        state.available_bytes = state.available_bytes.saturating_add(bytes);
    }
}

impl TextureKey {
    fn byte_len(self) -> u64 {
        texture_byte_len(self.size.width, self.size.height, self.format)
    }
}

fn texture_byte_len(width: u32, height: u32, format: wgpu::TextureFormat) -> u64 {
    let block = format.block_copy_size(None).unwrap_or(4);
    u64::from(width)
        .saturating_mul(u64::from(height))
        .saturating_mul(u64::from(block))
}

#[cfg(test)]
thread_local! {
    /// Test-only count of views created through [`default_view`] on this thread.
    pub(crate) static VIEWS_CREATED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Creates a whole-texture default view; all such views go through here so tests can count them.
pub(crate) fn default_view(texture: &wgpu::Texture) -> wgpu::TextureView {
    #[cfg(test)]
    VIEWS_CREATED.set(VIEWS_CREATED.get() + 1);
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

#[cfg(test)]
thread_local! {
    /// Test-only: how many of this thread's next intermediate textures come back invalid, as a
    /// failed `wgpu` allocation does.
    pub(crate) static INVALID_TEXTURES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn create_texture(
    device: &wgpu::Device,
    size: RenderSize,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> wgpu::Texture {
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC
        | wgpu::TextureUsages::COPY_DST;
    #[cfg(test)]
    let usage = if INVALID_TEXTURES.get() > 0 {
        INVALID_TEXTURES.set(INVALID_TEXTURES.get() - 1);
        wgpu::TextureUsages::empty()
    } else {
        usage
    };
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn create_upload_texture(
    device: &wgpu::Device,
    size: RenderSize,
    format: wgpu::TextureFormat,
    label: &'static str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: upload_texture_usage(format),
        view_formats: &[],
    })
}

/// Usages of an upload texture: copy destination and sampled.
///
/// Only RGBA8 uploads are also render attachments, as the browser's
/// `copyExternalImageToTexture` requires. Requesting more than a format allows invalidates the
/// texture: 16-bit normalized planes are never renderable, and half floats are not on WebGL2
/// without `EXT_color_buffer_float`.
pub(crate) fn upload_texture_usage(format: wgpu::TextureFormat) -> wgpu::TextureUsages {
    let base = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING;
    if format == wgpu::TextureFormat::Rgba8Unorm {
        base | wgpu::TextureUsages::RENDER_ATTACHMENT
    } else {
        base
    }
}
