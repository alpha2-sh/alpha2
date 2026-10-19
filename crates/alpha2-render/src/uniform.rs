use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Mutex;

use crate::Shared;
use crate::retire::{Retired, Retirement};

/// The bytes one effect parameter takes in the generated `EffectParams`
/// struct: every type is padded to one 16-byte slot.
pub(crate) const PARAM_SLOT_BYTES: usize = 16;

/// The bytes of an effect's parameter uniform: one slot per parameter, and
/// one for an effect without any, since a uniform cannot be empty. The graph
/// reserves this much and the pass writes it, so both read it from here.
pub(crate) const fn param_uniform_bytes(count: usize) -> usize {
    let slots = if count == 0 { 1 } else { count };
    slots.saturating_mul(PARAM_SLOT_BYTES)
}

/// The 32-bit words of a runtime effect pass's runtime uniform: two `vec4f`
/// of values, then a `vec4u` holding the time in ticks.
pub(crate) const RUNTIME_UNIFORM_WORDS: usize = 12;

/// The `f32` values of one composite pass's `BatchParams` uniform
/// (composite.wesl): three `vec4f` of controls, then four for each of
/// [`crate::composite::COMPOSITE_BATCH_LAYERS`] layers.
pub(crate) const BATCH_UNIFORM_FLOATS: usize = 12 + 16 * crate::composite::COMPOSITE_BATCH_LAYERS;

/// The bytes of `values` floats, as the graph reserves them.
pub(crate) const fn float_uniform_bytes(values: usize) -> u64 {
    (values * size_of::<f32>()) as u64
}

// Idle arenas are retained up to the most of one capacity in use at once, so a frame with
// many sources reuses them all; these bound what an unusual frame can leave behind.
const MAX_IDLE_UNIFORM_BUFFERS: usize = 64;
const MAX_IDLE_UNIFORM_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn f32_bytes<const N: usize>(values: [f32; N]) -> Vec<u8> {
    values
        .into_iter()
        .flat_map(f32::to_ne_bytes)
        .collect::<Vec<_>>()
}

/// Allocation and retention counters for submission-scoped uniform arenas.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct UniformBufferPoolStats {
    /// Physical buffers created since renderer construction.
    pub created: u64,
    /// Graph executions served by an idle buffer.
    pub reused: u64,
    /// Buffers dropped instead of retained, including failed graph encodes.
    pub discarded: u64,
    /// Arenas currently being encoded or awaiting submission.
    pub active: usize,
    /// Bytes currently owned by active arenas.
    pub active_bytes: u64,
    /// Maximum active bytes observed since renderer construction.
    pub high_water_bytes: u64,
    /// Unmapped buffers currently available for reuse.
    pub available: usize,
    /// Bytes retained by idle reusable buffers.
    pub available_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct UniformBufferPool {
    /// The device's `min_uniform_buffer_offset_alignment`.
    alignment: u64,
    state: Mutex<UniformBufferPoolState>,
    retirement: Shared<Retirement>,
}

#[derive(Debug, Default)]
struct UniformBufferPoolState {
    /// Oldest release first: capacity, buffer, its bytes, and the
    /// [`Retirement::generation`] it was created in.
    available: Vec<(u64, wgpu::Buffer, Vec<u8>, u64)>,
    /// The generation whose untrusted idle buffers were last dropped.
    checked_generation: u64,
    active_by_capacity: HashMap<u64, usize>,
    peak_active_by_capacity: HashMap<u64, usize>,
    created: u64,
    reused: u64,
    discarded: u64,
    active: usize,
    active_bytes: u64,
    high_water_bytes: u64,
    available_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct UniformArenaLease {
    pool: Shared<UniformBufferPool>,
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
    alignment: u64,
    cursor: u64,
    bytes: Vec<u8>,
    generation: u64,
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct UniformSlice {
    offset: u64,
    size: NonZeroU64,
}

impl UniformBufferPool {
    pub(crate) fn new(alignment: u64, retirement: Shared<Retirement>) -> Self {
        Self {
            alignment: alignment.max(1),
            state: Mutex::default(),
            retirement,
        }
    }

    /// Leases an arena of at least `capacity` bytes.
    ///
    /// Capacities round up to a power of two, so graphs whose uniform needs
    /// differ a little share arenas instead of each wanting an exact size.
    pub(crate) fn acquire(
        self: &Shared<Self>,
        device: &wgpu::Device,
        capacity: u64,
    ) -> UniformArenaLease {
        let alignment = self.alignment;
        let capacity = align_up(capacity.max(1), alignment)
            .checked_next_power_of_two()
            .unwrap_or(u64::MAX);
        let reused = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let untrusted = self.drop_untrusted(&mut state);
            let index = state
                .available
                .iter()
                .position(|(available_capacity, _, _, _)| *available_capacity == capacity);
            let reused = index.map(|index| {
                state.reused = state.reused.saturating_add(1);
                state.available_bytes = state.available_bytes.saturating_sub(capacity);
                let (_, buffer, bytes, generation) = state.available.remove(index);
                (buffer, bytes, generation)
            });
            drop(state);
            self.retirement.retire(untrusted);
            reused
        };
        let (buffer, bytes, generation) = reused.unwrap_or_else(|| {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.created = state.created.saturating_add(1);
            drop(state);
            let generation = self.retirement.generation();
            (
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("alpha2 uniform arena"),
                    size: capacity,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                vec![0; usize::try_from(capacity).expect("uniform arena fits host indexing")],
                generation,
            )
        });
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_add(1);
            state.active_bytes = state.active_bytes.saturating_add(capacity);
            state.high_water_bytes = state.high_water_bytes.max(state.active_bytes);
            let active = state.active_by_capacity.entry(capacity).or_default();
            *active = active.saturating_add(1);
            let active = *active;
            let peak = state.peak_active_by_capacity.entry(capacity).or_default();
            *peak = (*peak).max(active);
        }
        UniformArenaLease {
            pool: Shared::clone(self),
            buffer: Some(buffer),
            capacity,
            alignment,
            cursor: 0,
            bytes,
            generation,
        }
    }

    /// Takes the idle arenas created before the GPU last reported an error
    /// out of the pool, once per such report, for the caller to retire.
    fn drop_untrusted(&self, state: &mut UniformBufferPoolState) -> Vec<(Retired, u64)> {
        let generation = self.retirement.generation();
        if state.checked_generation == generation {
            return Vec::new();
        }
        state.checked_generation = generation;
        let (kept, untrusted): (Vec<_>, Vec<_>) = std::mem::take(&mut state.available)
            .into_iter()
            .partition(|(_, _, _, created)| *created == generation);
        state.available = kept;
        state.available_bytes = state
            .available
            .iter()
            .map(|(capacity, _, _, _)| *capacity)
            .sum();
        state.discarded = state
            .discarded
            .saturating_add(u64::try_from(untrusted.len()).unwrap_or(u64::MAX));
        untrusted
            .into_iter()
            .map(|(capacity, buffer, _, _)| {
                prune_capacity(state, capacity);
                (Retired::Buffer(buffer), capacity)
            })
            .collect()
    }

    /// Destroys every idle uniform buffer.
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
                .map(|(capacity, buffer, _, _)| (Retired::Buffer(buffer), capacity)),
        );
    }

    pub(crate) fn stats(&self) -> UniformBufferPoolStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UniformBufferPoolStats {
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

    /// Keeps a finished arena for reuse while its capacity has fewer idle
    /// arenas than were ever in use at once, within the idle bounds, which
    /// evict the oldest release first. What the pool does not keep is
    /// destroyed.
    fn release(&self, capacity: u64, buffer: wgpu::Buffer, bytes: Vec<u8>, generation: u64) {
        if !self.retirement.trusts(generation) {
            self.discard(capacity, buffer);
            return;
        }
        let mut retired = Vec::new();
        self.keep_or_retire(capacity, buffer, bytes, generation, &mut retired);
        self.retirement.retire(retired);
    }

    fn keep_or_retire(
        &self,
        capacity: u64,
        buffer: wgpu::Buffer,
        bytes: Vec<u8>,
        generation: u64,
        retired: &mut Vec<(Retired, u64)>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_active(&mut state, capacity);
        let retention_target = state
            .peak_active_by_capacity
            .get(&capacity)
            .copied()
            .unwrap_or(1);
        let idle_at_capacity = state
            .available
            .iter()
            .filter(|(available, _, _, _)| *available == capacity)
            .count();
        if idle_at_capacity >= retention_target || capacity > MAX_IDLE_UNIFORM_BYTES {
            state.discarded = state.discarded.saturating_add(1);
            retired.push((Retired::Buffer(buffer), capacity));
            return;
        }
        while state.available.len() >= MAX_IDLE_UNIFORM_BUFFERS
            || state.available_bytes.saturating_add(capacity) > MAX_IDLE_UNIFORM_BYTES
        {
            let (discarded, discarded_buffer, _bytes, _) = state.available.remove(0);
            state.available_bytes = state.available_bytes.saturating_sub(discarded);
            state.discarded = state.discarded.saturating_add(1);
            prune_capacity(&mut state, discarded);
            retired.push((Retired::Buffer(discarded_buffer), discarded));
        }
        state.available.push((capacity, buffer, bytes, generation));
        state.available_bytes = state.available_bytes.saturating_add(capacity);
    }

    /// Takes back an arena whose encode failed or was abandoned.
    fn discard(&self, capacity: u64, buffer: wgpu::Buffer) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_active(&mut state, capacity);
        state.discarded = state.discarded.saturating_add(1);
        prune_capacity(&mut state, capacity);
        drop(state);
        self.retirement
            .retire([(Retired::Buffer(buffer), capacity)]);
    }
}

impl UniformArenaLease {
    pub(crate) fn write(&mut self, contents: &[u8]) -> UniformSlice {
        let offset = align_up(self.cursor, self.alignment);
        let size = u64::try_from(contents.len()).expect("uniform contents fit u64");
        let end = offset.checked_add(size).expect("uniform arena offset");
        assert!(end <= self.capacity, "uniform arena capacity was reserved");
        let start = usize::try_from(offset).expect("uniform offset fits host indexing");
        let end = usize::try_from(end).expect("uniform end fits host indexing");
        self.bytes[start..end].copy_from_slice(contents);
        self.cursor = u64::try_from(end).expect("uniform cursor fits u64");
        UniformSlice {
            offset,
            size: NonZeroU64::new(size).expect("uniform binding is non-empty"),
        }
    }

    pub(crate) fn binding(&self, slice: UniformSlice) -> wgpu::BindingResource<'_> {
        wgpu::BindingResource::Buffer(wgpu::BufferBinding {
            buffer: self.buffer.as_ref().expect("live uniform arena"),
            offset: slice.offset,
            size: Some(slice.size),
        })
    }

    #[cfg(test)]
    pub(crate) fn buffer(&self) -> &wgpu::Buffer {
        self.buffer.as_ref().expect("live uniform arena")
    }

    pub(crate) fn flush(&self, queue: &wgpu::Queue) {
        if self.cursor > 0 {
            let used = usize::try_from(self.cursor).expect("uniform cursor fits host indexing");
            queue.write_buffer(
                self.buffer.as_ref().expect("live uniform arena"),
                0,
                &self.bytes[..used],
            );
        }
    }

    pub(crate) fn recycle(mut self) {
        let buffer = self.buffer.take().expect("live uniform arena");
        let bytes = std::mem::take(&mut self.bytes);
        self.pool
            .release(self.capacity, buffer, bytes, self.generation);
    }
}

impl Drop for UniformArenaLease {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            self.pool.discard(self.capacity, buffer);
        }
    }
}

fn finish_active(state: &mut UniformBufferPoolState, capacity: u64) {
    state.active = state.active.saturating_sub(1);
    state.active_bytes = state.active_bytes.saturating_sub(capacity);
    if let Some(active) = state.active_by_capacity.get_mut(&capacity) {
        *active = active.saturating_sub(1);
        if *active == 0 {
            state.active_by_capacity.remove(&capacity);
        }
    }
}

/// Forgets a capacity's peak once nothing of it is active or idle, so the
/// retention map stays as small as the capacities in use.
fn prune_capacity(state: &mut UniformBufferPoolState, capacity: u64) {
    if !state.active_by_capacity.contains_key(&capacity)
        && !state
            .available
            .iter()
            .any(|(available, _, _, _)| *available == capacity)
    {
        state.peak_active_by_capacity.remove(&capacity);
    }
}

pub(crate) const fn aligned_uniform_end(cursor: u64, size: u64, alignment: u64) -> u64 {
    align_up(cursor, alignment).saturating_add(size)
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    value
        .saturating_add(alignment.saturating_sub(1))
        .div_euclid(alignment)
        .saturating_mul(alignment)
}
