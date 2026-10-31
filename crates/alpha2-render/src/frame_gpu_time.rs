//! How long an export frame's graph and its pack into the encoder's planes kept the GPU busy,
//! sampled on every eighth frame with timestamp queries.
//!
//! The CPU-side timings of a frame say when work was recorded and submitted, not how long the GPU
//! ran it, and the GPU's share otherwise lands in whichever wait a backend blocks on (a readback
//! map, an encoder input fence). Three empty compute passes bracket the graph and the pack in the
//! frame's own command encoder: the end of the first before the graph, the beginning of the second
//! between graph and pack, and the beginning of the third after the pack. Their timestamps resolve
//! into one of a ring of mappable buffers that is read on a later frame once its map completes;
//! nothing ever waits on it, so sampling costs the export nothing it would notice.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Every this many frames one is sampled.
const SAMPLE_EVERY: u64 = 8;
/// Readback buffers in the ring: samples whose map has not completed yet before a frame skips its
/// own.
const RING: usize = 4;
/// Bytes of the three resolved timestamps.
const RESOLVED_BYTES: u64 = 3 * 8;

/// What the sampled frames of an export measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameGpuTimes {
    /// The sampled frames' GPU time between the start of their graph and its end, summed.
    pub render_ms: f64,
    /// Their GPU time from the end of the graph to the end of the pack, summed.
    pub pack_ms: f64,
    /// Frames whose timestamps were read.
    pub sampled_frames: u64,
    /// Frames whose three timestamps read the same tick: a backend that does not advance its
    /// counter across empty passes (Metal samples them at encoder boundaries it merges). Not
    /// counted in `sampled_frames`.
    pub flat_frames: u64,
}

impl FrameGpuTimes {
    /// The mean graph time of a sampled frame, when any was sampled.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "a frame count far below 2^52")]
    pub fn render_ms_per_frame(&self) -> Option<f64> {
        (self.sampled_frames > 0).then(|| self.render_ms / self.sampled_frames as f64)
    }

    /// The mean pack time of a sampled frame, when any was sampled.
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "a frame count far below 2^52")]
    pub fn pack_ms_per_frame(&self) -> Option<f64> {
        (self.sampled_frames > 0).then(|| self.pack_ms / self.sampled_frames as f64)
    }
}

/// One readback buffer of the ring.
#[derive(Debug)]
struct Slot {
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// Submitted and not read back yet.
    pending: AtomicBool,
    /// How its map ended, set by the map callback and read on a later frame: [`MAP_WAITING`],
    /// [`MAP_DONE`] or [`MAP_FAILED`].
    mapped: Arc<AtomicU8>,
}

const MAP_WAITING: u8 = 0;
const MAP_DONE: u8 = 1;
const MAP_FAILED: u8 = 2;

/// The query set and ring a renderer samples its export frames with.
#[derive(Debug)]
pub(crate) struct FrameGpuTimer {
    queries: wgpu::QuerySet,
    slots: Vec<Slot>,
    /// Frames offered for sampling since the renderer started.
    frames: AtomicU64,
    /// Nanoseconds per timestamp tick.
    period: f32,
    totals: Mutex<FrameGpuTimes>,
}

/// The passes one sampled frame writes its timestamps from.
pub(crate) struct FrameSample<'a> {
    timer: &'a FrameGpuTimer,
    slot: usize,
}

impl FrameGpuTimer {
    /// A timer on `device`, which must have `TIMESTAMP_QUERY`.
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("alpha2 frame GPU time"),
            ty: wgpu::QueryType::Timestamp,
            count: 3,
        });
        let slots = (0..RING)
            .map(|_| Slot {
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("alpha2 frame GPU time resolve"),
                    size: RESOLVED_BYTES,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("alpha2 frame GPU time readback"),
                    size: RESOLVED_BYTES,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                pending: AtomicBool::new(false),
                mapped: Arc::new(AtomicU8::new(MAP_WAITING)),
            })
            .collect();
        Self {
            queries,
            slots,
            frames: AtomicU64::new(0),
            period: queue.get_timestamp_period(),
            totals: Mutex::default(),
        }
    }

    /// Reads every slot whose map has completed into the totals.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a frame's span in timestamp ticks, far below 2^52"
    )]
    fn collect(&self) {
        let mut totals = self
            .totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for slot in &self.slots {
            if !slot.pending.load(Ordering::Acquire) {
                continue;
            }
            match slot.mapped.load(Ordering::Acquire) {
                MAP_WAITING => continue,
                // A map that failed (the device was lost) frees the slot with no sample.
                MAP_FAILED => {
                    slot.mapped.store(MAP_WAITING, Ordering::Release);
                    slot.pending.store(false, Ordering::Release);
                    continue;
                }
                _ => {}
            }
            if let Ok(bytes) = slot.readback.slice(..).get_mapped_range() {
                let tick = |at: usize| {
                    let mut word = [0; 8];
                    word.copy_from_slice(&bytes[at * 8..at * 8 + 8]);
                    u64::from_le_bytes(word)
                };
                let (start, graph_end, pack_end) = (tick(0), tick(1), tick(2));
                // A reset or wrapped counter gives timestamps out of order; such a sample is
                // dropped rather than counted as a huge or negative time.
                if start == pack_end {
                    totals.flat_frames += 1;
                } else if start <= graph_end && graph_end <= pack_end {
                    let milliseconds =
                        |ticks: u64| ticks as f64 * f64::from(self.period) / 1_000_000.0;
                    totals.render_ms += milliseconds(graph_end - start);
                    totals.pack_ms += milliseconds(pack_end - graph_end);
                    totals.sampled_frames += 1;
                }
            }
            slot.readback.unmap();
            slot.mapped.store(MAP_WAITING, Ordering::Release);
            slot.pending.store(false, Ordering::Release);
        }
    }

    /// Whether this frame is sampled, and the slot it writes to: every eighth frame, when a slot
    /// is free.
    pub(crate) fn begin(&self) -> Option<FrameSample<'_>> {
        let frame = self.frames.fetch_add(1, Ordering::Relaxed);
        if !frame.is_multiple_of(SAMPLE_EVERY) {
            return None;
        }
        self.collect();
        let slot = self
            .slots
            .iter()
            .position(|slot| !slot.pending.load(Ordering::Acquire))?;
        Some(FrameSample { timer: self, slot })
    }

    /// What the frames sampled since the last take measured, read from the slots whose maps have
    /// completed; the totals start again from zero.
    pub(crate) fn take(&self) -> FrameGpuTimes {
        self.collect();
        std::mem::take(
            &mut *self
                .totals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl FrameSample<'_> {
    /// An empty compute pass writing timestamp `index`: at its end for the first, at its
    /// beginning for the others.
    fn mark(&self, encoder: &mut wgpu::CommandEncoder, index: u32) {
        let writes = if index == 0 {
            wgpu::ComputePassTimestampWrites {
                query_set: &self.timer.queries,
                beginning_of_pass_write_index: None,
                end_of_pass_write_index: Some(index),
            }
        } else {
            wgpu::ComputePassTimestampWrites {
                query_set: &self.timer.queries,
                beginning_of_pass_write_index: Some(index),
                end_of_pass_write_index: None,
            }
        };
        drop(encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("alpha2 frame GPU time mark"),
            timestamp_writes: Some(writes),
        }));
    }

    /// Marks the start of the graph.
    pub(crate) fn graph_start(&self, encoder: &mut wgpu::CommandEncoder) {
        self.mark(encoder, 0);
    }

    /// Marks the end of the graph and the start of the pack.
    pub(crate) fn graph_end(&self, encoder: &mut wgpu::CommandEncoder) {
        self.mark(encoder, 1);
    }

    /// Marks the end of the pack and copies the three timestamps toward the slot's readback.
    pub(crate) fn pack_end(&self, encoder: &mut wgpu::CommandEncoder) {
        self.mark(encoder, 2);
        let slot = &self.timer.slots[self.slot];
        encoder.resolve_query_set(&self.timer.queries, 0..3, &slot.resolve, 0);
        encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.readback, 0, RESOLVED_BYTES);
    }

    /// After the frame's submission on `gpu`: maps the slot's readback, which a later frame
    /// reads.
    pub(crate) fn submitted(self, gpu: &alpha2_gpu::GpuContext) {
        let slot = &self.timer.slots[self.slot];
        slot.pending.store(true, Ordering::Release);
        let mapped = Arc::clone(&slot.mapped);
        gpu.map_async(
            &slot.readback.slice(..),
            wgpu::MapMode::Read,
            move |result| {
                mapped.store(
                    if result.is_ok() { MAP_DONE } else { MAP_FAILED },
                    Ordering::Release,
                );
            },
        );
    }
}
