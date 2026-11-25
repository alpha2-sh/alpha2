//! The files one run of a plugin may read, and the reader that serves them.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alpha2_engine::extract::{MAX_PLUGIN_RESOURCE_BYTES, PluginManifest, SlotShape, SuppliedSlot};
use alpha2_media::CancelToken;
use alpha2_model::{MAX_SUPPLIED_FILES, MAX_SUPPLIED_NAME_BYTES, SuppliedFile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CANCELLED, PluginError, ResourceRoot, codes, slot_reasons};

const READ_STEP_BYTES: usize = 1 << 20;
/// The deepest directory below a directory slot's own.
pub(super) const MAX_SLOT_DEPTH: usize = 8;

/// A pinned file, checked against its pin at load.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct PinnedFile {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
    pub(super) bytes: u64,
    /// A local file the caller named.
    pub(super) local: bool,
}

/// A file of a filled slot, as the enumeration found it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct SlotFile {
    pub(super) slot: String,
    pub(super) path: PathBuf,
    pub(super) bytes: u64,
    pub(super) max_bytes: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum Entry {
    Pinned(PinnedFile),
    Supplied(SlotFile),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct FilledSlot {
    shape: SlotShape,
    /// The canonical path of what the caller named.
    root: PathBuf,
    /// Its files' resource names, in byte order.
    files: Vec<String>,
}

/// What one run may read by resource name: pinned resources and the files of filled slots.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct ResourceSet {
    entries: BTreeMap<String, Entry>,
    slots: BTreeMap<String, FilledSlot>,
    /// A wasm module reads it, which addresses at most [`MAX_PLUGIN_RESOURCE_BYTES`] per file.
    wasm: bool,
    /// The directory supplied files must lie under, links resolved, when the caller names one.
    root: Option<PathBuf>,
}

fn invalid(slot: &str, reason: &str, message: impl Into<String>) -> PluginError {
    PluginError::resource_invalid(slot, reason, message)
}

fn fetch_failed(path: &Path, error: &std::io::Error) -> PluginError {
    PluginError::new(codes::FETCH_FAILED, format!("{}: {error}", path.display()))
}

/// `path` without the `\\?\` prefix Windows gives canonical paths, where a plain form exists.
fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            if !text.contains('\u{fffd}') {
                return PathBuf::from(format!(r"\\{rest}"));
            }
        } else if let Some(rest) = text.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
            && !text.contains('\u{fffd}')
        {
            return PathBuf::from(rest);
        }
    }
    path
}

pub(super) struct Hashed {
    pub(super) sha256: String,
    pub(super) bytes: u64,
    pub(super) stopped_by_sink: bool,
}

pub(super) fn hash_file(
    path: &Path,
    limit: u64,
    mut before_read: impl FnMut() -> Result<(), PluginError>,
    mut sink: impl FnMut(u64, &[u8]) -> bool,
) -> Result<Hashed, PluginError> {
    let file = std::fs::File::open(path).map_err(|error| fetch_failed(path, &error))?;
    let mut reader = file.take(limit);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; READ_STEP_BYTES];
    let mut bytes = 0_u64;
    let stopped_by_sink = loop {
        before_read()?;
        let read = match reader.read(&mut buffer) {
            Ok(0) => break false,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(fetch_failed(path, &error)),
        };
        let chunk = &buffer[..read];
        hasher.update(chunk);
        let offset = bytes;
        bytes = bytes.saturating_add(read as u64);
        if !sink(offset, chunk) {
            break true;
        }
    };
    Ok(Hashed {
        sha256: super::hex(&hasher.finalize()),
        bytes,
        stopped_by_sink,
    })
}

pub(super) fn canonical(path: &Path) -> Result<PathBuf, PluginError> {
    std::fs::canonicalize(path)
        .map(plain)
        .map_err(|error| fetch_failed(path, &error))
}

/// Whether a canonical `path` lies under the caller's root.
fn allowed(root: &ResourceRoot, path: &Path) -> bool {
    match root {
        ResourceRoot::Any => true,
        ResourceRoot::None => false,
        ResourceRoot::Dir(dir) => path.starts_with(dir),
    }
}

/// A refusal by the caller's [`ResourceRoot`], with `details.policy` `resource_root`.
fn root_refusal(message: String) -> PluginError {
    let mut details = serde_json::Map::new();
    details.insert("policy".to_owned(), "resource_root".into());
    PluginError::new(codes::REFUSED, message).with_details(details)
}

fn outside_root(path: &Path) -> PluginError {
    root_refusal(format!(
        "{} lies outside the directory the caller allows supplied files in",
        path.display()
    ))
}

struct Walk<'a> {
    slot: &'a str,
    root: &'a ResourceRoot,
    max_bytes: Option<u64>,
    /// Files the slots resolved before this one offer.
    before: usize,
    files: Vec<(String, SlotFile)>,
    /// Canonical paths of the directories being walked, the slot's first.
    ancestors: Vec<PathBuf>,
}

impl Walk<'_> {
    fn directory(&mut self, dir: &Path, names: &mut Vec<String>) -> Result<(), PluginError> {
        if names.len() > MAX_SLOT_DEPTH {
            return Err(invalid(
                self.slot,
                slot_reasons::TOO_DEEP,
                format!(
                    "{} lies more than {MAX_SLOT_DEPTH} directories below slot {}",
                    dir.display(),
                    self.slot
                ),
            ));
        }
        let mut entries = std::fs::read_dir(dir)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|error| fetch_failed(dir, &error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let file_name = entry.file_name();
            if file_name.as_encoded_bytes().starts_with(b".") {
                continue;
            }
            let path = entry.path();
            let Some(name) = file_name.to_str() else {
                return Err(invalid(
                    self.slot,
                    slot_reasons::NOT_UTF8,
                    format!("{} has a name that is not UTF-8", path.display()),
                ));
            };
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(invalid(
                        self.slot,
                        slot_reasons::MISSING,
                        format!("{} is a link whose target is missing", path.display()),
                    ));
                }
                Err(error) => return Err(fetch_failed(&path, &error)),
            };
            names.push(name.to_owned());
            if metadata.is_dir() {
                let resolved = canonical(&path)?;
                if self.ancestors.contains(&resolved) {
                    return Err(invalid(
                        self.slot,
                        slot_reasons::TOO_DEEP,
                        format!("{} is its own ancestor", path.display()),
                    ));
                }
                self.ancestors.push(resolved);
                self.directory(&path, names)?;
                self.ancestors.pop();
            } else if metadata.is_file() {
                self.file(path, &metadata, names)?;
            }
            names.pop();
        }
        Ok(())
    }

    fn file(
        &mut self,
        path: PathBuf,
        metadata: &std::fs::Metadata,
        names: &[String],
    ) -> Result<(), PluginError> {
        let name = format!("{}/{}", self.slot, names.join("/"));
        if name.len() > MAX_SUPPLIED_NAME_BYTES {
            return Err(invalid(
                self.slot,
                slot_reasons::NAME_TOO_LONG,
                format!("{name} is longer than {MAX_SUPPLIED_NAME_BYTES} bytes"),
            ));
        }
        if self.files.len() + self.before == MAX_SUPPLIED_FILES {
            return Err(invalid(
                self.slot,
                slot_reasons::TOO_MANY_FILES,
                format!(
                    "slot {} brings the run's supplied files past {MAX_SUPPLIED_FILES}",
                    self.slot
                ),
            ));
        }
        if !matches!(self.root, ResourceRoot::Any) && !allowed(self.root, &canonical(&path)?) {
            return Err(outside_root(&path));
        }
        self.files.push((
            name,
            SlotFile {
                slot: self.slot.to_owned(),
                path,
                bytes: metadata.len(),
                max_bytes: self.max_bytes,
            },
        ));
        Ok(())
    }
}

impl ResourceSet {
    /// The filled slots of `manifest`, from the caller's `supplied` paths, checked against the
    /// slots and `root`; nothing is read. `wasm` refuses a file slot past what a module reads.
    pub(super) fn resolve(
        manifest: &PluginManifest,
        supplied: &BTreeMap<String, PathBuf>,
        root: &ResourceRoot,
        wasm: bool,
    ) -> Result<Self, PluginError> {
        if let Some(name) = supplied.keys().find(|name| {
            !manifest
                .resources
                .iter()
                .any(|resource| &resource.name == *name && resource.slot().is_some())
        }) {
            return Err(invalid(
                name,
                slot_reasons::UNKNOWN_SLOT,
                format!("{} declares no resource slot {name}", manifest.id),
            ));
        }
        if !supplied.is_empty() && matches!(root, ResourceRoot::None) {
            return Err(root_refusal(
                "the caller allows no supplied files; a request may not fill a resource slot"
                    .to_owned(),
            ));
        }
        let root = match root {
            ResourceRoot::Dir(dir) if !supplied.is_empty() => ResourceRoot::Dir(canonical(dir)?),
            other => other.clone(),
        };
        let mut set = Self {
            wasm,
            root: match &root {
                ResourceRoot::Dir(dir) => Some(dir.clone()),
                _ => None,
            },
            ..Self::default()
        };
        for resource in &manifest.resources {
            let Some(slot) = resource.slot() else {
                continue;
            };
            let name = resource.name.as_str();
            let Some(named) = supplied.get(name) else {
                if slot.required {
                    return Err(invalid(
                        name,
                        slot_reasons::MISSING,
                        format!("{} needs a file for its resource slot {name}", manifest.id),
                    ));
                }
                continue;
            };
            let resolved = canonical(named)?;
            if !allowed(&root, &resolved) {
                return Err(outside_root(named));
            }
            let metadata =
                std::fs::metadata(&resolved).map_err(|error| fetch_failed(named, &error))?;
            let wanted = match slot.shape {
                SlotShape::File => metadata.is_file(),
                SlotShape::Directory => metadata.is_dir(),
            };
            if !wanted {
                let shape = match slot.shape {
                    SlotShape::File => "a file",
                    SlotShape::Directory => "a directory",
                };
                return Err(invalid(
                    name,
                    slot_reasons::SHAPE,
                    format!("slot {name} takes {shape}; {} is not one", named.display()),
                ));
            }
            let files = set.slot_files(name, slot, &resolved, &metadata, &root)?;
            if set.supplied_files() + files.len() > MAX_SUPPLIED_FILES {
                return Err(invalid(
                    name,
                    slot_reasons::TOO_MANY_FILES,
                    format!(
                        "slot {name} brings the run's supplied files past {MAX_SUPPLIED_FILES}"
                    ),
                ));
            }
            set.slots.insert(
                name.to_owned(),
                FilledSlot {
                    shape: slot.shape,
                    root: resolved,
                    files: files.iter().map(|(name, _)| name.clone()).collect(),
                },
            );
            for (name, file) in files {
                set.entries.insert(name, Entry::Supplied(file));
            }
        }
        Ok(set)
    }

    /// The files a slot filled with `resolved` offers, by resource name in byte order.
    fn slot_files(
        &self,
        name: &str,
        slot: &SuppliedSlot,
        resolved: &Path,
        metadata: &std::fs::Metadata,
        root: &ResourceRoot,
    ) -> Result<Vec<(String, SlotFile)>, PluginError> {
        Ok(match slot.shape {
            SlotShape::File => {
                let file = SlotFile {
                    slot: name.to_owned(),
                    path: resolved.to_path_buf(),
                    bytes: metadata.len(),
                    max_bytes: slot.max_bytes,
                };
                self.check_size(name, &file)?;
                vec![(name.to_owned(), file)]
            }
            SlotShape::Directory => {
                let mut walk = Walk {
                    slot: name,
                    root,
                    max_bytes: slot.max_bytes,
                    before: self.supplied_files(),
                    files: Vec::new(),
                    ancestors: vec![resolved.to_path_buf()],
                };
                walk.directory(resolved, &mut Vec::new())?;
                let mut files = walk.files;
                files.sort_by(|(a, _), (b, _)| a.cmp(b));
                files
            }
        })
    }

    fn supplied_files(&self) -> usize {
        self.slots.values().map(|slot| slot.files.len()).sum()
    }

    /// Adds a pinned resource whose file was checked against its pin.
    pub(super) fn add_pinned(&mut self, name: &str, file: PinnedFile) {
        self.entries.insert(name.to_owned(), Entry::Pinned(file));
    }

    /// # Errors
    /// `too_large` for a file past the slot's `max_bytes`, or past what a wasm module reads.
    fn check_size(&self, name: &str, file: &SlotFile) -> Result<(), PluginError> {
        if let Some(max) = file.max_bytes.filter(|max| file.bytes > *max) {
            return Err(invalid(
                &file.slot,
                slot_reasons::TOO_LARGE,
                format!(
                    "{name} is {} bytes; slot {} takes at most {max}",
                    file.bytes, file.slot
                ),
            ));
        }
        if self.wasm && file.bytes > MAX_PLUGIN_RESOURCE_BYTES {
            return Err(invalid(
                &file.slot,
                slot_reasons::TOO_LARGE,
                format!(
                    "{name} is {} bytes; a wasm plugin reads at most {MAX_PLUGIN_RESOURCE_BYTES}",
                    file.bytes
                ),
            ));
        }
        Ok(())
    }

    /// The supplied files by name, with their enumerated lengths.
    pub(super) fn supplied_lengths(&self) -> BTreeMap<String, u64> {
        self.entries
            .iter()
            .filter_map(|(name, entry)| match entry {
                Entry::Supplied(file) => Some((name.clone(), file.bytes)),
                Entry::Pinned(_) => None,
            })
            .collect()
    }

    /// The filled slots as `begin`'s context names them: shape and files with their sizes.
    #[must_use]
    pub(super) fn context(&self) -> serde_json::Value {
        let slots = self
            .slots
            .iter()
            .map(|(name, slot)| {
                let files = slot
                    .files
                    .iter()
                    .filter_map(|name| match self.entries.get(name) {
                        Some(Entry::Supplied(file)) => {
                            Some(serde_json::json!({ "name": name, "bytes": file.bytes }))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let shape = match slot.shape {
                    SlotShape::File => "file",
                    SlotShape::Directory => "directory",
                };
                (
                    name.clone(),
                    serde_json::json!({ "shape": shape, "files": files }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::Value::Object(slots)
    }
}

/// Serves a run's `resource` calls from its [`ResourceSet`], hashing supplied files as they are
/// read and latching the first failure for the call it happened in.
pub(super) struct ResourceReader {
    set: ResourceSet,
    cancel: CancelToken,
    /// When the running call must have returned, and the deadline it was given.
    ends: Option<(Instant, Duration)>,
    /// Supplied files read, by name: SHA-256 and length.
    read: BTreeMap<String, (String, u64)>,
    /// Bytes handed to a native plugin, kept until the instance is destroyed.
    kept: HashMap<String, Vec<u8>>,
    /// Paths handed to a native plugin, by name, kept until the instance is destroyed.
    paths: HashMap<String, String>,
    failure: Option<PluginError>,
}

impl ResourceReader {
    pub(super) fn new(set: ResourceSet, cancel: CancelToken) -> Self {
        Self {
            set,
            cancel,
            ends: None,
            read: BTreeMap::new(),
            kept: HashMap::new(),
            paths: HashMap::new(),
            failure: None,
        }
    }

    /// Starts a call that must return by `ends`, if it has a deadline.
    pub(super) fn start_call(&mut self, ends: Option<(Instant, Duration)>) {
        self.ends = ends;
        self.failure = None;
    }

    /// The failure latched during the running call.
    pub(super) fn take_failure(&mut self) -> Option<PluginError> {
        self.failure.take()
    }

    pub(super) const fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// The supplied files read so far, sorted by name.
    pub(super) fn supplied(&self) -> Vec<SuppliedFile> {
        self.read
            .iter()
            .map(|(name, (sha256, bytes))| SuppliedFile {
                name: name.clone(),
                sha256: sha256.clone(),
                bytes: *bytes,
            })
            .collect()
    }

    fn latch(&mut self, error: PluginError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }

    fn check_step(&self) -> Result<(), PluginError> {
        if self.cancel.is_cancelled() {
            return Err(PluginError::new(
                CANCELLED,
                "the extraction was cancelled while the plugin read a resource",
            ));
        }
        if let Some((ends, deadline)) = self.ends
            && Instant::now() >= ends
        {
            return Err(PluginError::new(
                codes::LIMIT,
                format!("a plugin call ran longer than {} ms", deadline.as_millis()),
            ));
        }
        Ok(())
    }

    /// Reads `path` in steps, handing each to `sink`, and gives its SHA-256 and length. The read
    /// stops past `expected` bytes.
    fn stream(
        &self,
        path: &Path,
        expected: u64,
        sink: impl FnMut(u64, &[u8]) -> bool,
    ) -> Result<(String, u64), PluginError> {
        let past = expected.saturating_add(1);
        let hashed = hash_file(path, past, || self.check_step(), sink)?;
        let bytes = if hashed.stopped_by_sink {
            hashed.bytes.max(past)
        } else {
            hashed.bytes
        };
        Ok((hashed.sha256, bytes))
    }

    /// Reads resource `name` through `sink`, checking it as its kind asks.
    fn read_through(
        &mut self,
        name: &str,
        sink: impl FnMut(u64, &[u8]) -> bool,
    ) -> Result<(), PluginError> {
        match self.set.entries.get(name) {
            Some(Entry::Pinned(pinned)) => {
                let (sha256, bytes) = self.stream(&pinned.path, pinned.bytes, sink)?;
                if bytes != pinned.bytes || sha256 != pinned.sha256 {
                    let message = if pinned.local {
                        format!(
                            "{name} changed after the load checked it: {bytes} bytes with SHA-256 \
                             {sha256}; the manifest pins {} bytes with {}",
                            pinned.bytes, pinned.sha256
                        )
                    } else {
                        format!("{name} no longer matches the length and SHA-256 the manifest pins")
                    };
                    return Err(PluginError::new(codes::HASH_MISMATCH, message));
                }
                Ok(())
            }
            Some(Entry::Supplied(file)) => {
                let file = file.clone();
                self.set.check_size(name, &file)?;
                let path = match &self.set.root {
                    Some(root) => {
                        let resolved = canonical(&file.path)?;
                        if !resolved.starts_with(root) {
                            return Err(outside_root(&file.path));
                        }
                        resolved
                    }
                    None => file.path.clone(),
                };
                let (sha256, bytes) = self.stream(&path, file.bytes, sink)?;
                self.record(name, &file, sha256, bytes)
            }
            None => Ok(()),
        }
    }

    fn record(
        &mut self,
        name: &str,
        file: &SlotFile,
        sha256: String,
        bytes: u64,
    ) -> Result<(), PluginError> {
        let changed = || {
            PluginError::new(
                codes::FETCH_FAILED,
                format!("{} changed while the plugin ran", file.path.display()),
            )
        };
        if bytes != file.bytes {
            return Err(changed());
        }
        match self.read.get(name) {
            Some((before, _)) if *before != sha256 => Err(changed()),
            Some(_) => Ok(()),
            None => {
                self.read.insert(name.to_owned(), (sha256, bytes));
                Ok(())
            }
        }
    }

    /// The length of resource `name`; `None` for an undeclared one or after a latched failure.
    pub(super) fn size(&mut self, name: &str) -> Option<u64> {
        if self.failed() {
            return None;
        }
        match self.set.entries.get(name)? {
            Entry::Pinned(pinned) => Some(pinned.bytes),
            Entry::Supplied(file) => {
                let file = file.clone();
                match self.set.check_size(name, &file) {
                    Ok(()) => Some(file.bytes),
                    Err(error) => {
                        self.latch(error);
                        None
                    }
                }
            }
        }
    }

    /// Streams resource `name` into `target`, which is exactly its length; false for an
    /// undeclared name, another length or a failure, which is latched.
    pub(super) fn read_into(&mut self, name: &str, target: &mut [u8]) -> bool {
        if self.failed() || self.size(name) != Some(target.len() as u64) {
            return false;
        }
        let length = target.len();
        let outcome = self.read_through(name, |offset, chunk| {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            match start.checked_add(chunk.len()).filter(|end| *end <= length) {
                Some(end) => {
                    target[start..end].copy_from_slice(chunk);
                    true
                }
                None => false,
            }
        });
        match outcome {
            Ok(()) => true,
            Err(error) => {
                self.latch(error);
                false
            }
        }
    }

    /// The bytes of resource `name`, read once and kept until the reader is dropped.
    pub(super) fn bytes(&mut self, name: &str) -> Option<&[u8]> {
        if self.failed() {
            return None;
        }
        if !self.kept.contains_key(name) {
            let size = self.size(name)?;
            let mut bytes = Vec::with_capacity(usize::try_from(size).ok()?);
            let outcome = self.read_through(name, |_, chunk| {
                bytes.extend_from_slice(chunk);
                true
            });
            if let Err(error) = outcome {
                self.latch(error);
                return None;
            }
            self.kept.insert(name.to_owned(), bytes);
        }
        self.kept.get(name).map(Vec::as_slice)
    }

    /// The path of the supplied file, filled directory slot or directory under one named `name`,
    /// kept until the reader is dropped. Every file it covers is hashed the first time.
    pub(super) fn path(&mut self, name: &str) -> Option<&str> {
        if self.failed() {
            return None;
        }
        if self.paths.contains_key(name) {
            return self.paths.get(name).map(String::as_str);
        }
        let (path, files) = self.path_and_files(name)?;
        let text = path.to_str()?.to_owned();
        for file in files {
            if let Err(error) = self.read_through(&file, |_, _| true) {
                self.latch(error);
                return None;
            }
        }
        Some(self.paths.entry(name.to_owned()).or_insert(text).as_str())
    }

    /// The path `name` hands out and the supplied files under it.
    fn path_and_files(&self, name: &str) -> Option<(PathBuf, Vec<String>)> {
        if let Some(Entry::Supplied(file)) = self.set.entries.get(name) {
            return Some((file.path.clone(), vec![name.to_owned()]));
        }
        let (slot, below) = match name.split_once('/') {
            Some((slot, below)) => (slot, Some(below)),
            None => (name, None),
        };
        let filled = self.set.slots.get(slot)?;
        if filled.shape != SlotShape::Directory {
            return None;
        }
        let prefix = format!("{name}/");
        let files = filled
            .files
            .iter()
            .filter(|file| file.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        let Some(below) = below else {
            return Some((filled.root.clone(), files));
        };
        if files.is_empty() {
            return None;
        }
        Some((
            below
                .split('/')
                .fold(filled.root.clone(), |path, part| path.join(part)),
            files,
        ))
    }
}
