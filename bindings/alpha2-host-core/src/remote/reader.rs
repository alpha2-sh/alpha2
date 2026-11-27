//! Readers of a remote object: [`RemoteReader`] through the block cache, [`RemoteStream`] past it.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};

use super::fetch::{self, Ended, Known, Progress};
use super::{BLOCK_BYTES, RemoteError, RemoteFailure, RemoteObject};

const FIRST_WINDOW: u64 = 1024 * 1024;
const MAX_WINDOW: u64 = 8 * 1024 * 1024;
const SEQUENTIAL_SKIP: u64 = 1024 * 1024;
const STREAM_CHUNKS: usize = 32;

struct Job {
    generation: u64,
    next_block: u64,
    end_block: u64,
}

struct FetchState {
    generation: u64,
    job: Option<Job>,
    error: Option<RemoteError>,
    closed: bool,
}

struct ReaderFetch {
    state: Mutex<FetchState>,
    arrived: Condvar,
    progress: Progress,
}

impl ReaderFetch {
    fn state(&self) -> MutexGuard<'_, FetchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A cursor over a remote object, reading blocks through the session's cache with one fetch in
/// flight: sequential reads grow the read-ahead window from 1 MiB to 8 MiB, any other seek drops
/// the open fetch and asks for one block.
///
/// A failed or cancelled read returns an [`io::Error`] that [`RemoteError::of`] reads, never one
/// of kind [`io::ErrorKind::Interrupted`].
pub struct RemoteReader {
    object: Arc<RemoteObject>,
    position: u64,
    last_end: u64,
    window: u64,
    fetch: Arc<ReaderFetch>,
}

impl RemoteReader {
    pub(super) fn new(object: Arc<RemoteObject>) -> Self {
        object.shared.readers.fetch_add(1, Ordering::AcqRel);
        Self {
            object,
            position: 0,
            last_end: 0,
            window: BLOCK_BYTES,
            fetch: Arc::new(ReaderFetch {
                state: Mutex::new(FetchState {
                    generation: 0,
                    job: None,
                    error: None,
                    closed: false,
                }),
                arrived: Condvar::new(),
                progress: Progress::new(),
            }),
        }
    }

    /// The object's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.object.len()
    }

    /// Whether the object holds no byte.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.object.is_empty()
    }

    fn blocks(&self) -> u64 {
        self.object.len().div_ceil(BLOCK_BYTES)
    }

    fn grown_window(&self) -> u64 {
        let grown = if self.window < FIRST_WINDOW {
            FIRST_WINDOW
        } else {
            (self.window * 2).min(MAX_WINDOW)
        };
        grown.min(self.object.shared.window_cap())
    }

    fn start(&self, state: &mut FetchState, block: u64, length: u64) {
        state.generation += 1;
        let generation = state.generation;
        let total = self.object.len();
        let first = block * BLOCK_BYTES;
        let end = (first + length.max(BLOCK_BYTES)).min(total);
        state.job = Some(Job {
            generation,
            next_block: block,
            end_block: end.div_ceil(BLOCK_BYTES),
        });
        let (object, fetch) = (Arc::clone(&self.object), Arc::clone(&self.fetch));
        let spawned = std::thread::Builder::new()
            .name("alpha2-remote-fetch".into())
            .spawn(move || run_job(&object, &fetch, generation, first, end));
        if let Err(error) = spawned {
            state.job = None;
            state.error = Some(RemoteError::new(
                RemoteFailure::Unreachable,
                format!("{}: no thread to fetch with: {error}", self.object.shown),
            ));
        }
    }

    fn block(&mut self, index: u64, sequential: bool) -> Result<Arc<[u8]>, RemoteError> {
        let shared = Arc::clone(&self.object.shared);
        let key = (self.object.id, index);
        if let Some(bytes) = shared.cache().get(key) {
            return Ok(bytes);
        }
        {
            let mut state = self.fetch.state();
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            let covered = state
                .job
                .as_ref()
                .is_some_and(|job| job.next_block <= index && index < job.end_block);
            if !covered {
                let length = if sequential {
                    self.window = self.grown_window();
                    self.window
                } else {
                    BLOCK_BYTES
                };
                self.start(&mut state, index, length);
            }
        }
        shared.cache().wait_on(key);
        let fetch = Arc::clone(&self.fetch);
        let result = fetch::wait_for(&shared, &self.object.shown, &fetch.progress, |poll| {
            let mut state = fetch.state();
            if let Some(bytes) = shared.cache().get(key) {
                return Some(Ok(bytes));
            }
            if let Some(error) = &state.error {
                return Some(Err(error.clone()));
            }
            let covered = state
                .job
                .as_ref()
                .is_some_and(|job| job.next_block <= index && index < job.end_block);
            if !covered {
                self.start(&mut state, index, BLOCK_BYTES);
            }
            let _ = fetch
                .arrived
                .wait_timeout(state, poll)
                .unwrap_or_else(PoisonError::into_inner);
            None
        });
        shared.cache().done_waiting(key);
        if result.is_err() {
            let mut state = self.fetch.state();
            state.generation += 1;
            state.job = None;
        }
        result
    }

    fn read_ahead(&mut self, index: u64) {
        let shared = Arc::clone(&self.object.shared);
        let mut state = self.fetch.state();
        if state.job.is_some() || state.error.is_some() {
            return;
        }
        let window = self.grown_window();
        let end = (index + window / BLOCK_BYTES).min(self.blocks());
        let missing = {
            let cache = shared.cache();
            (index..end).find(|block| !cache.contains((self.object.id, *block)))
        };
        if let Some(block) = missing {
            self.window = window;
            self.start(&mut state, block, window);
        }
    }
}

fn run_job(object: &RemoteObject, fetch: &ReaderFetch, generation: u64, first: u64, end: u64) {
    let shared = &object.shared;
    let total = object.len();
    let mut known = Known::Facts(object.facts.clone());
    let mut block = first / BLOCK_BYTES;
    let mut partial = Vec::with_capacity(usize::try_from(BLOCK_BYTES).unwrap_or_default());
    let stop = || {
        let state = fetch.state();
        state.closed || state.generation != generation || shared.cancel.is_cancelled()
    };
    let result = fetch::fetch_range(
        shared,
        &object.url,
        &object.shown,
        &mut known,
        first,
        end,
        &fetch.progress,
        &stop,
        &mut |mut chunk| {
            while !chunk.is_empty() {
                let block_end = ((block + 1) * BLOCK_BYTES).min(total);
                let wanted = block_end - block * BLOCK_BYTES - partial.len() as u64;
                let taken = usize::try_from(wanted)
                    .unwrap_or(usize::MAX)
                    .min(chunk.len());
                partial.extend_from_slice(&chunk[..taken]);
                chunk = &chunk[taken..];
                if (partial.len() as u64) < block_end - block * BLOCK_BYTES {
                    continue;
                }
                shared
                    .cache()
                    .insert((object.id, block), Arc::from(std::mem::take(&mut partial)));
                block += 1;
                let mut state = fetch.state();
                if let Some(job) = state
                    .job
                    .as_mut()
                    .filter(|job| job.generation == generation)
                {
                    job.next_block = block;
                }
                drop(state);
                fetch.arrived.notify_all();
            }
            true
        },
    );
    let mut state = fetch.state();
    if state.generation == generation {
        state.job = None;
        match result {
            Err(error) => state.error = Some(error),
            Ok(Ended::Stopped) if shared.cancel.is_cancelled() => {
                state.error = Some(RemoteError::cancelled());
            }
            Ok(_) => {}
        }
    }
    drop(state);
    fetch.arrived.notify_all();
}

impl Read for RemoteReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let total = self.object.len();
        if buffer.is_empty() || self.position >= total {
            return Ok(0);
        }
        let index = self.position / BLOCK_BYTES;
        let sequential =
            self.position >= self.last_end && self.position - self.last_end <= SEQUENTIAL_SKIP;
        if !sequential {
            self.window = BLOCK_BYTES;
        }
        let bytes = self.block(index, sequential)?;
        let offset = usize::try_from(self.position - index * BLOCK_BYTES).unwrap_or(usize::MAX);
        let available = bytes.get(offset..).unwrap_or_default();
        let read = available.len().min(buffer.len());
        buffer[..read].copy_from_slice(&available[..read]);
        self.position += read as u64;
        self.last_end = self.position;
        if sequential {
            self.read_ahead(index);
        }
        Ok(read)
    }
}

impl Seek for RemoteReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.object.len().checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        };
        let target = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a seek before the first byte")
        })?;
        self.position = target;
        Ok(target)
    }
}

impl Drop for RemoteReader {
    fn drop(&mut self) {
        {
            let mut state = self.fetch.state();
            state.closed = true;
            state.generation += 1;
            state.job = None;
        }
        self.object.shared.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

enum StreamMessage {
    Bytes(Vec<u8>),
    End,
    Failed(RemoteError),
}

/// The whole object from its first byte in one fetch, kept only until read: for reading a file
/// whole or hashing it, without filling the block cache.
pub struct RemoteStream {
    object: Arc<RemoteObject>,
    receiver: Option<mpsc::Receiver<StreamMessage>>,
    pending: Vec<u8>,
    taken: usize,
    ended: bool,
    stopped: Arc<AtomicBool>,
    progress: Arc<Progress>,
}

impl RemoteStream {
    pub(super) fn new(object: Arc<RemoteObject>) -> Self {
        Self {
            object,
            receiver: None,
            pending: Vec::new(),
            taken: 0,
            ended: false,
            stopped: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Progress::new()),
        }
    }

    /// The object's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.object.len()
    }

    /// Whether the object holds no byte.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.object.is_empty()
    }

    fn receiver(&mut self) -> Result<&mpsc::Receiver<StreamMessage>, RemoteError> {
        if self.receiver.is_none() {
            let (sender, receiver) = mpsc::sync_channel(STREAM_CHUNKS);
            let (object, stopped, progress) = (
                Arc::clone(&self.object),
                Arc::clone(&self.stopped),
                Arc::clone(&self.progress),
            );
            std::thread::Builder::new()
                .name("alpha2-remote-stream".into())
                .spawn(move || {
                    let shared = &object.shared;
                    let mut known = Known::Facts(object.facts.clone());
                    let stop = || stopped.load(Ordering::Acquire) || shared.cancel.is_cancelled();
                    let result = fetch::fetch_range(
                        shared,
                        &object.url,
                        &object.shown,
                        &mut known,
                        0,
                        object.len(),
                        &progress,
                        &stop,
                        &mut |chunk| sender.send(StreamMessage::Bytes(chunk.to_vec())).is_ok(),
                    );
                    let _ = sender.send(match result {
                        Ok(Ended::Whole) => StreamMessage::End,
                        Ok(Ended::Stopped) => StreamMessage::Failed(RemoteError::cancelled()),
                        Err(error) => StreamMessage::Failed(error),
                    });
                })
                .map_err(|error| {
                    RemoteError::new(
                        RemoteFailure::Unreachable,
                        format!("{}: no thread to fetch with: {error}", self.object.shown),
                    )
                })?;
            self.receiver = Some(receiver);
        }
        Ok(self
            .receiver
            .as_ref()
            .unwrap_or_else(|| unreachable!("set above")))
    }
}

impl Read for RemoteStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.taken == self.pending.len() {
            if self.ended {
                return Ok(0);
            }
            let shared = Arc::clone(&self.object.shared);
            let (shown, progress) = (self.object.shown.clone(), Arc::clone(&self.progress));
            let receiver = self.receiver()?;
            let message = fetch::wait_for(&shared, &shown, &progress, |poll| {
                match receiver.recv_timeout(poll) {
                    Ok(message) => Some(Ok(message)),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(RemoteError::new(
                        RemoteFailure::Unreachable,
                        format!("{shown}: the fetch ended without an answer"),
                    ))),
                }
            })?;
            match message {
                StreamMessage::Bytes(bytes) => {
                    self.pending = bytes;
                    self.taken = 0;
                }
                StreamMessage::End => self.ended = true,
                StreamMessage::Failed(error) => return Err(error.into()),
            }
        }
        let read = (self.pending.len() - self.taken).min(buffer.len());
        buffer[..read].copy_from_slice(&self.pending[self.taken..self.taken + read]);
        self.taken += read;
        Ok(read)
    }
}

impl Drop for RemoteStream {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}
