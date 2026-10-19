//! The binding's cache of uploaded source frames, keyed by source and time,
//! bounded by count and by bytes.
//!
//! Every upload gets a generation number. Aliases share their target's
//! generation, and a replaced entry is retired, still counted, until the host
//! releases its generation or the budget evicts it.
//!
//! An export pins the frames the frame it is making shows (its working set):
//! the budget evicts everything else first and never them, and an upload that
//! would make the working set itself outgrow the budget is refused, so a frame
//! the budget cannot hold fails at once rather than evicting its own sources
//! and waiting for them again.

use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;

struct Entry<T> {
    value: Rc<T>,
    generation: u64,
}

impl<T> Entry<T> {
    fn shared(&self) -> Self {
        Self {
            value: Rc::clone(&self.value),
            generation: self.generation,
        }
    }
}

/// Most timestamped frame keys kept, aliases included.
const MAX_VIDEO_FRAMES: usize = 512;
/// Most time-independent images kept.
const MAX_IMAGES: usize = 128;
/// Most replaced entries kept for the host to release.
const MAX_RETIRED: usize = 128;
/// Bytes of cached frames (live, image, and retired generations) kept by default: 64
/// `Rgba16Float` frames at 1080p, 16 at 4K. The frame-count limit alone would allow 512 such
/// frames, about 8.5 GB at 1080p.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const DEFAULT_VIDEO_FRAME_BYTES: u64 = 1 << 30;

/// An upload refused because the pinned working set would outgrow the byte
/// budget with it ([`FrameStore::pin`]). The store holds what it held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkingSetOverBudget {
    /// Bytes of the other pinned frames the store holds.
    pub(crate) held: u64,
    /// Bytes of the refused upload.
    pub(crate) requested: u64,
    /// The byte budget.
    pub(crate) budget: u64,
}

impl WorkingSetOverBudget {
    /// What the working set needs at least: the pinned frames held and this
    /// one. Frames it has not been given yet are not counted.
    pub(crate) const fn needed(&self) -> u64 {
        self.held.saturating_add(self.requested)
    }
}

/// Timestamped video frames and explicitly registered, time-independent images.
pub(crate) struct FrameStore<T> {
    frames: HashMap<(String, i64), Entry<T>>,
    images: HashMap<String, Entry<T>>,
    retired: HashMap<u64, Entry<T>>,
    /// The keys (source, time) the pinned working set looks up
    /// ([`Self::pin`]).
    pinned_frames: HashSet<(String, i64)>,
    next_generation: u64,
    size: fn(&T) -> u64,
    video_byte_budget: u64,
}

impl<T> Default for FrameStore<T> {
    fn default() -> Self {
        Self::sized(|_| 0, u64::MAX)
    }
}

impl<T> FrameStore<T> {
    /// A store that measures each value with `size` and keeps at most
    /// `video_byte_budget` bytes.
    ///
    /// The budget counts every retained generation once, including retired
    /// entries and images. Eviction releases only cache ownership: submitted
    /// work owns GPU resources, and unsubmitted source work is discarded with
    /// its entry. Queue ordering protects pooled textures reused after
    /// submission.
    ///
    /// A newly inserted frame may exceed the budget on its own so it can be
    /// presented; in that case no other unpinned generation is retained. A
    /// pinned working set may not ([`Self::pin`]). Idle renderer pools and
    /// submitted work have separate lifetime/budget accounting.
    pub(crate) fn sized(size: fn(&T) -> u64, video_byte_budget: u64) -> Self {
        Self {
            frames: HashMap::new(),
            images: HashMap::new(),
            retired: HashMap::new(),
            pinned_frames: HashSet::new(),
            next_generation: 1,
            size,
            video_byte_budget,
        }
    }

    /// Changes the byte budget, evicting oldest generations at once to fit.
    pub(crate) fn set_video_byte_budget(&mut self, bytes: u64) {
        self.video_byte_budget = bytes;
        self.enforce_video_budget(None);
    }

    /// Bytes of distinct video-frame generations currently kept.
    #[cfg(test)]
    pub(crate) fn video_bytes(&self) -> u64 {
        let mut counted = HashSet::new();
        self.frames
            .values()
            .filter(|entry| counted.insert(entry.generation))
            .fold(0_u64, |total, entry| {
                total.saturating_add((self.size)(entry.value.as_ref()))
            })
    }

    /// Pins the working set of the frame being made: the frames it looks up
    /// at `keys` (source and time), and the image of each of those sources,
    /// which answers a time with no exact frame. It replaces the pins before.
    ///
    /// Until [`Self::unpin`], the budget never evicts a pinned generation, and
    /// an upload or alias that would make the pinned generations other than
    /// the one it replaces, with it, outgrow the budget is refused. A working
    /// set of one frame is kept over the budget, as any newest upload is.
    pub(crate) fn pin(&mut self, keys: impl IntoIterator<Item = (String, i64)>) {
        self.pinned_frames.clear();
        self.pinned_frames.extend(keys);
    }

    /// Drops the pins, leaving every generation to the budget again.
    pub(crate) fn unpin(&mut self) {
        self.pinned_frames.clear();
    }

    /// The generations the pinned working set looks up, each key as
    /// [`Self::get`] answers it, but for what an upload is about to replace:
    /// the frame at key `frame`, or the image of `image` where it answers.
    fn pinned_generations(
        &self,
        frame: Option<&(String, i64)>,
        image: Option<&str>,
    ) -> HashSet<u64> {
        self.pinned_frames
            .iter()
            .filter(|key| Some(*key) != frame)
            .filter_map(|key| match self.frames.get(key) {
                Some(entry) => Some(entry.generation),
                None if Some(key.0.as_str()) == image => None,
                None => self.images.get(&key.0).map(|entry| entry.generation),
            })
            .collect()
    }

    /// Whether `source`'s image is part of the pinned working set: a pinned
    /// key of that source has no exact frame, so the image answers it.
    fn image_pinned(&self, source: &str) -> bool {
        self.pinned_frames
            .iter()
            .any(|key| key.0 == source && !self.frames.contains_key(key))
    }

    /// Bytes of the generations in `generations`, each counted once.
    fn generation_bytes(&self, generations: &HashSet<u64>) -> u64 {
        let mut counted = HashSet::new();
        self.frames
            .values()
            .chain(self.images.values())
            .filter(|entry| generations.contains(&entry.generation))
            .filter(|entry| counted.insert(entry.generation))
            .fold(0_u64, |total, entry| {
                total.saturating_add((self.size)(entry.value.as_ref()))
            })
    }

    /// Refuses `requested` bytes joining the pinned working set when the
    /// other pinned generations (`others`) and it outgrow the budget, unless
    /// it is the working set's only frame or already one of `others`.
    fn admit_pinned(
        &self,
        others: &HashSet<u64>,
        generation: Option<u64>,
        requested: u64,
    ) -> Result<(), WorkingSetOverBudget> {
        if others.is_empty() || generation.is_some_and(|generation| others.contains(&generation)) {
            return Ok(());
        }
        let held = self.generation_bytes(others);
        if held.saturating_add(requested) > self.video_byte_budget {
            return Err(WorkingSetOverBudget {
                held,
                requested,
                budget: self.video_byte_budget,
            });
        }
        Ok(())
    }

    /// Evicts complete generations, including aliases, oldest first, never a
    /// pinned one. Only the newly inserted generation and the pinned working
    /// set can exceed the budget.
    fn enforce_video_budget(&mut self, keep: Option<u64>) {
        let pinned = self.pinned_generations(None, None);
        while self.retained_bytes(self.size) > self.video_byte_budget {
            let oldest = self
                .generations()
                .into_iter()
                .filter(|generation| Some(*generation) != keep && !pinned.contains(generation))
                .min();
            let Some(oldest) = oldest else {
                break;
            };
            self.release_generation(oldest);
        }
    }
    fn entry(&mut self, value: T) -> Entry<T> {
        let generation = self.next_generation;
        self.next_generation = generation.checked_add(1).unwrap_or(1);
        if self.next_generation == 1 {
            self.frames.clear();
            self.images.clear();
            self.retired.clear();
        }
        Entry {
            value: Rc::new(value),
            generation,
        }
    }

    /// Stores a frame at `key` and returns its new generation. A frame already
    /// at `key` is retired, and older unpinned generations are evicted to fit
    /// the budget.
    ///
    /// # Errors
    /// Refuses a frame at a pinned key that would make the working set
    /// outgrow the budget ([`Self::pin`]), keeping what the store held.
    pub(crate) fn insert(
        &mut self,
        key: (String, i64),
        value: T,
    ) -> Result<u64, WorkingSetOverBudget> {
        if self.pinned_frames.contains(&key) {
            let others = self.pinned_generations(Some(&key), None);
            self.admit_pinned(&others, None, (self.size)(&value))?;
        }
        if !self.frames.contains_key(&key) && self.frames.len() >= MAX_VIDEO_FRAMES {
            self.evict_oldest_frame();
        }
        let entry = self.entry(value);
        let generation = entry.generation;
        if let Some(old) = self.frames.insert(key, entry) {
            self.retire(old);
        }
        self.enforce_video_budget(Some(generation));
        Ok(generation)
    }

    /// Makes `requested` answer with the frame at `existing`, sharing its
    /// generation and bytes. Returns `false` when `existing` holds no frame.
    ///
    /// # Errors
    /// Refuses to alias into a pinned key a generation that would make the
    /// working set outgrow the budget ([`Self::pin`]), keeping what the store
    /// held.
    pub(crate) fn alias(
        &mut self,
        existing: &(String, i64),
        requested: (String, i64),
    ) -> Result<bool, WorkingSetOverBudget> {
        let Some(entry) = self.frames.get(existing).map(Entry::shared) else {
            return Ok(false);
        };
        if existing == &requested {
            return Ok(true);
        }
        let generation = entry.generation;
        if self.pinned_frames.contains(&requested) {
            let others = self.pinned_generations(Some(&requested), None);
            self.admit_pinned(&others, Some(generation), (self.size)(entry.value.as_ref()))?;
        }
        if !self.frames.contains_key(&requested) && self.frames.len() >= MAX_VIDEO_FRAMES {
            self.evict_oldest_frame();
        }
        if let Some(old) = self.frames.insert(requested, entry)
            && old.generation != generation
        {
            self.retire(old);
        }
        Ok(true)
    }

    /// Stores `source`'s time-independent image and returns its new
    /// generation; the image answers every time that has no exact frame.
    ///
    /// # Errors
    /// Refuses the image of a pinned source that would make the working set
    /// outgrow the budget ([`Self::pin`]), keeping what the store held.
    pub(crate) fn insert_image(
        &mut self,
        source: String,
        value: T,
    ) -> Result<u64, WorkingSetOverBudget> {
        if self.image_pinned(&source) {
            let others = self.pinned_generations(None, Some(&source));
            self.admit_pinned(&others, None, (self.size)(&value))?;
        }
        if !self.images.contains_key(&source) && self.images.len() >= MAX_IMAGES {
            self.evict_oldest_image();
        }
        let entry = self.entry(value);
        let generation = entry.generation;
        if let Some(old) = self.images.insert(source, entry) {
            self.retire(old);
        }
        self.enforce_video_budget(Some(generation));
        Ok(generation)
    }

    /// Whether `source`'s image is still the upload `generation` names. The
    /// store drops images on its own, by the byte budget and by the image
    /// count, so a host that caches what it uploaded asks before it relies on
    /// an upload being here.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn holds_image(&self, source: &str, generation: u64) -> bool {
        self.images
            .get(source)
            .is_some_and(|entry| entry.generation == generation)
    }

    pub(crate) fn get(&self, source: &str, time: i64) -> Option<&T> {
        self.lookup(source, time).map(|entry| entry.value.as_ref())
    }

    /// The upload generation `get` would answer from. Generations are never
    /// reused by one store, so a generation names one uploaded content.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn generation(&self, source: &str, time: i64) -> Option<u64> {
        self.lookup(source, time).map(|entry| entry.generation)
    }

    fn lookup(&self, source: &str, time: i64) -> Option<&Entry<T>> {
        self.frames
            .get(&(source.to_owned(), time))
            .or_else(|| self.images.get(source))
    }

    pub(crate) fn remove(&mut self, source: &str, time: i64) -> bool {
        self.frames.remove(&(source.to_owned(), time)).is_some()
    }

    pub(crate) fn len(&self) -> usize {
        self.generations().len()
    }

    pub(crate) fn retired_len(&self) -> usize {
        self.retired.len()
    }

    pub(crate) fn retained_bytes(&self, bytes: impl Fn(&T) -> u64) -> u64 {
        let mut counted = HashSet::new();
        self.frames
            .values()
            .chain(self.images.values())
            .chain(self.retired.values())
            .filter(|entry| counted.insert(entry.generation))
            .fold(0_u64, |total, entry| {
                total.saturating_add(bytes(entry.value.as_ref()))
            })
    }

    /// Drops every frame, alias, image, and retired entry of `generation`.
    /// Returns whether anything was dropped.
    pub(crate) fn release_generation(&mut self, generation: u64) -> bool {
        let retired = self.retired.remove(&generation).is_some();
        let before_frames = self.frames.len();
        self.frames
            .retain(|_, entry| entry.generation != generation);
        let before_images = self.images.len();
        self.images
            .retain(|_, entry| entry.generation != generation);
        retired || before_frames != self.frames.len() || before_images != self.images.len()
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
        self.images.clear();
        self.retired.clear();
    }

    /// Invalidates derived values everywhere, including aliases and retirees.
    pub(crate) fn retain_values(&mut self, keep: impl Fn(&T) -> bool) {
        self.frames.retain(|_, entry| keep(&entry.value));
        self.images.retain(|_, entry| keep(&entry.value));
        self.retired.retain(|_, entry| keep(&entry.value));
    }

    /// Keeps only the frames and images `keep` accepts, given their source.
    ///
    /// Retired entries stay, as for [`Self::retain_sources`].
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn retain_entries(&mut self, keep: impl Fn(&str, &T) -> bool) {
        self.frames
            .retain(|(source, _), entry| keep(source, &entry.value));
        self.images
            .retain(|source, entry| keep(source, &entry.value));
    }

    /// Keeps only the frames and images of sources `keep` accepts.
    ///
    /// Retired entries stay until the host releases their generation or the
    /// cache budget evicts it, as after any other replacement.
    pub(crate) fn retain_sources(&mut self, keep: impl Fn(&str) -> bool) {
        self.frames.retain(|(source, _), _| keep(source));
        self.images.retain(|source, _| keep(source));
    }

    /// Drops the frames and image of one source, for a host whose media for
    /// it changed. Retired entries stay until their generation is released,
    /// as after any other replacement.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn release_source(&mut self, source: &str) -> bool {
        let before = self.frames.len() + self.images.len();
        self.retain_sources(|kept| kept != source);
        before != self.frames.len() + self.images.len()
    }

    /// Drops `source`'s frames before `before`, a time on that source's own
    /// timeline; other sources' frames are on other timelines and stay.
    pub(crate) fn trim_before(&mut self, source: &str, before: i64) {
        self.frames
            .retain(|(kept, time), _| kept != source || *time >= before);
    }

    fn retire(&mut self, entry: Entry<T>) {
        while self.retired.len() >= MAX_RETIRED {
            let Some(oldest) = self.retired.keys().min().copied() else {
                break;
            };
            self.retired.remove(&oldest);
        }
        self.retired.insert(entry.generation, entry);
    }

    fn generations(&self) -> HashSet<u64> {
        self.frames
            .values()
            .chain(self.images.values())
            .chain(self.retired.values())
            .map(|entry| entry.generation)
            .collect()
    }

    fn evict_oldest_frame(&mut self) {
        let pinned = self.pinned_generations(None, None);
        let key = self
            .frames
            .iter()
            .filter(|(_, entry)| !pinned.contains(&entry.generation))
            .min_by_key(|(_, entry)| entry.generation)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            self.frames.remove(&key);
        }
    }

    fn evict_oldest_image(&mut self) {
        let pinned = self.pinned_generations(None, None);
        let key = self
            .images
            .iter()
            .filter(|(_, entry)| !pinned.contains(&entry.generation))
            .min_by_key(|(_, entry)| entry.generation)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            self.images.remove(&key);
        }
    }
}

#[cfg(test)]
#[path = "frame_store_tests.rs"]
mod tests;
