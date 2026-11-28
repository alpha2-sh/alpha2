//! The blocks one request holds, least recently used evicted first.

use std::collections::HashMap;
use std::sync::Arc;

pub(super) type BlockKey = (u64, u64);

struct Cached {
    bytes: Arc<[u8]>,
    used: u64,
}

pub(super) struct BlockCache {
    budget: u64,
    held: u64,
    tick: u64,
    blocks: HashMap<BlockKey, Cached>,
    waited: HashMap<BlockKey, usize>,
}

impl BlockCache {
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            held: 0,
            tick: 0,
            blocks: HashMap::new(),
            waited: HashMap::new(),
        }
    }

    pub const fn budget(&self) -> u64 {
        self.budget
    }

    pub const fn held(&self) -> u64 {
        self.held
    }

    pub fn contains(&self, key: BlockKey) -> bool {
        self.blocks.contains_key(&key)
    }

    pub fn get(&mut self, key: BlockKey) -> Option<Arc<[u8]>> {
        self.tick += 1;
        let tick = self.tick;
        self.blocks.get_mut(&key).map(|cached| {
            cached.used = tick;
            Arc::clone(&cached.bytes)
        })
    }

    pub fn insert(&mut self, key: BlockKey, bytes: Arc<[u8]>) {
        let length = bytes.len() as u64;
        if let Some(old) = self.blocks.remove(&key) {
            self.held -= old.bytes.len() as u64;
        }
        while self.held + length > self.budget {
            let victim = self
                .blocks
                .iter()
                .filter(|(key, _)| !self.waited.contains_key(key))
                .min_by_key(|(_, cached)| cached.used)
                .map(|(key, _)| *key);
            let Some(victim) = victim else { break };
            if let Some(evicted) = self.blocks.remove(&victim) {
                self.held -= evicted.bytes.len() as u64;
            }
        }
        self.tick += 1;
        self.held += length;
        self.blocks.insert(
            key,
            Cached {
                bytes,
                used: self.tick,
            },
        );
    }

    pub fn wait_on(&mut self, key: BlockKey) {
        *self.waited.entry(key).or_default() += 1;
    }

    pub fn done_waiting(&mut self, key: BlockKey) {
        if let Some(count) = self.waited.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.waited.remove(&key);
            }
        }
    }
}
