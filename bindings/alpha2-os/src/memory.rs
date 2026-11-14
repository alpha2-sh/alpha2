//! How much memory the machine has, and how much this process may use under a container or job
//! limit.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::Path;

/// The machine's memory and this process's limit. Unavailable figures are `None`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryCapacity {
    /// Physical memory installed.
    pub physical_bytes: Option<u64>,
    /// Memory available without paging: `MemAvailable` on Linux, free plus inactive plus
    /// purgeable pages on macOS, `ullAvailPhys` on Windows, what the app may still allocate on iOS.
    pub available_bytes: Option<u64>,
    /// The lowest of a cgroup or job limit and the physical total.
    pub limit_bytes: Option<u64>,
    /// What set `limit_bytes`: `cgroup-v2`, `cgroup-v1`, `job`, `app` (the per-app limit on
    /// iOS) or `physical`.
    pub limit_source: &'static str,
    /// The working set under a cgroup limit ([`CgroupMemory::working_set_bytes`]); `None` for
    /// physical and job limits.
    pub used_in_limit_bytes: Option<u64>,
    /// What the kernel charges against a cgroup limit, file page cache included.
    pub charged_in_limit_bytes: Option<u64>,
}

impl MemoryCapacity {
    /// The limit, else the physical total.
    #[must_use]
    pub fn effective_limit(&self) -> Option<u64> {
        match (self.limit_bytes, self.physical_bytes) {
            (Some(limit), Some(physical)) => Some(limit.min(physical)),
            (limit, physical) => limit.or(physical),
        }
    }

    /// The lower of available memory and the headroom left under the limit.
    #[must_use]
    pub fn headroom(&self) -> Option<u64> {
        let in_limit = self
            .limit_bytes
            .zip(self.used_in_limit_bytes)
            .map(|(limit, used)| limit.saturating_sub(used));
        match (self.available_bytes, in_limit) {
            (Some(available), Some(in_limit)) => Some(available.min(in_limit)),
            (available, in_limit) => available.or(in_limit),
        }
    }
}

/// The machine's memory and this process's limit, read now.
#[must_use]
pub fn memory_capacity() -> MemoryCapacity {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        memory_capacity_at(Path::new("/"))
    }
    #[cfg(any(target_vendor = "apple", windows))]
    {
        crate::platform::memory_capacity()
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    )))]
    {
        MemoryCapacity {
            limit_source: "physical",
            ..MemoryCapacity::default()
        }
    }
}

/// [`memory_capacity`] with `root` standing in for `/`.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[must_use]
pub fn memory_capacity_at(root: &Path) -> MemoryCapacity {
    let meminfo = std::fs::read_to_string(root.join("proc/meminfo")).unwrap_or_default();
    let kib = |key: &str| {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.trim().strip_suffix("kB"))
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|value| value.saturating_mul(1_024))
    };
    let physical = kib("MemTotal:");
    let available = kib("MemAvailable:");
    match cgroup_memory_at(root) {
        Some(cgroup) if physical.is_none_or(|physical| cgroup.limit_bytes < physical) => {
            MemoryCapacity {
                physical_bytes: physical,
                available_bytes: available,
                limit_bytes: Some(cgroup.limit_bytes),
                limit_source: cgroup.source,
                used_in_limit_bytes: cgroup.working_set_bytes,
                charged_in_limit_bytes: cgroup.charged_bytes,
            }
        }
        _ => MemoryCapacity {
            physical_bytes: physical,
            available_bytes: available,
            limit_bytes: physical,
            limit_source: "physical",
            used_in_limit_bytes: None,
            charged_in_limit_bytes: None,
        },
    }
}

/// The lowest memory limit on this process's cgroup path and what is charged against it.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CgroupMemory {
    /// The limit, `memory.max` or `memory.limit_in_bytes`.
    pub limit_bytes: u64,
    /// What the kernel charges at that level, file page cache included.
    pub charged_bytes: Option<u64>,
    /// The charge less the page cache the kernel reclaims before it kills (`inactive_file` and
    /// `active_file`); the charge itself when `memory.stat` cannot be read.
    pub working_set_bytes: Option<u64>,
    /// `cgroup-v2` or `cgroup-v1`.
    pub source: &'static str,
}

/// [`CgroupMemory`] of this process, read now; `None` without a cgroup memory limit.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[must_use]
pub fn cgroup_memory() -> Option<CgroupMemory> {
    cgroup_memory_at(Path::new("/"))
}

/// [`cgroup_memory`] with `root` standing in for `/`. Levels without a limit (`max`, or v1's
/// near-`u64::MAX` default) are skipped; v2 is read first.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[must_use]
pub fn cgroup_memory_at(root: &Path) -> Option<CgroupMemory> {
    let level = Level::lowest(root)?;
    level.read(0)
}

/// Reads [`CgroupMemory`] again and again for a watch. The level of the lowest limit is found
/// once, and again after a read at it fails; `memory.stat` is parsed only from
/// [`Self::STAT_FROM_PERCENT`] of the limit, below which the working set is the charge.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Debug)]
pub struct CgroupMemoryWatch {
    root: std::path::PathBuf,
    level: Option<Level>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl CgroupMemoryWatch {
    /// Below this share of the limit the page cache is not worth parsing.
    pub const STAT_FROM_PERCENT: u64 = 90;

    /// This process's cgroup path.
    #[must_use]
    pub fn new() -> Self {
        Self::at(Path::new("/"))
    }

    /// With `root` standing in for `/`.
    #[must_use]
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            level: None,
        }
    }

    /// `None` without a limit, or when the level just stopped answering.
    pub fn read(&mut self) -> Option<CgroupMemory> {
        if self.level.is_none() {
            self.level = Level::lowest(&self.root);
        }
        let memory = self
            .level
            .as_ref()?
            .read(Self::STAT_FROM_PERCENT)
            .filter(|memory| memory.charged_bytes.is_some());
        if memory.is_none() {
            self.level = None;
        }
        memory
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Default for CgroupMemoryWatch {
    fn default() -> Self {
        Self::new()
    }
}

/// The cgroup directory whose memory limit is the lowest on the process's path.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Debug)]
struct Level {
    directory: std::path::PathBuf,
    /// The limit file, then the charge file.
    files: [&'static str; 2],
    /// The `memory.stat` keys of reclaimable page cache.
    cache: [&'static str; 2],
    source: &'static str,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl Level {
    const V2: ([&'static str; 2], [&'static str; 2], &'static str) = (
        ["memory.max", "memory.current"],
        ["inactive_file", "active_file"],
        "cgroup-v2",
    );
    const V1: ([&'static str; 2], [&'static str; 2], &'static str) = (
        ["memory.limit_in_bytes", "memory.usage_in_bytes"],
        ["total_inactive_file", "total_active_file"],
        "cgroup-v1",
    );

    fn lowest(root: &Path) -> Option<Self> {
        let membership = std::fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
        if let Some(relative) = membership.lines().find_map(|line| line.strip_prefix("0::"))
            && let Some(level) = Self::lowest_under(&root.join("sys/fs/cgroup"), relative, Self::V2)
        {
            return Some(level);
        }
        let relative = membership.lines().find_map(|line| {
            let mut fields = line.splitn(3, ':');
            let _ = fields.next()?;
            let controllers = fields.next()?;
            controllers
                .split(',')
                .any(|controller| controller == "memory")
                .then(|| fields.next())
                .flatten()
        })?;
        Self::lowest_under(&root.join("sys/fs/cgroup/memory"), relative, Self::V1)
    }

    fn lowest_under(
        mount: &Path,
        relative: &str,
        (files, cache, source): ([&'static str; 2], [&'static str; 2], &'static str),
    ) -> Option<Self> {
        let mut best: Option<(u64, std::path::PathBuf)> = None;
        let mut path = Some(Path::new(relative.trim_start_matches('/')));
        while let Some(level) = path {
            let directory = mount.join(level);
            if let Some(limit) = read_limit(&directory.join(files[0]))
                && best.as_ref().is_none_or(|(known, _)| limit < *known)
            {
                best = Some((limit, directory));
            }
            path = level.parent();
        }
        best.map(|(_, directory)| Self {
            directory,
            files,
            cache,
            source,
        })
    }

    /// `None` when the limit is gone. `memory.stat` is parsed from `stat_from_percent` of the
    /// limit.
    fn read(&self, stat_from_percent: u64) -> Option<CgroupMemory> {
        let limit = read_limit(&self.directory.join(self.files[0]))?;
        let charged = read_number(&self.directory.join(self.files[1]));
        let cached = charged
            .filter(|charged| {
                charged.saturating_mul(100) >= limit.saturating_mul(stat_from_percent)
            })
            .and_then(|_| self.cached());
        Some(CgroupMemory {
            limit_bytes: limit,
            charged_bytes: charged,
            working_set_bytes: charged
                .map(|charged| cached.map_or(charged, |cached| charged.saturating_sub(cached))),
            source: self.source,
        })
    }

    fn cached(&self) -> Option<u64> {
        let stat = std::fs::read_to_string(self.directory.join("memory.stat")).ok()?;
        self.cache.iter().try_fold(0_u64, |sum, key| {
            let value = stat.lines().find_map(|line| {
                line.strip_prefix(key)?
                    .strip_prefix(' ')?
                    .trim()
                    .parse::<u64>()
                    .ok()
            })?;
            Some(sum.saturating_add(value))
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
}

/// A limit, or `None` for `max` and v1's near-`u64::MAX` default.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_limit(path: &Path) -> Option<u64> {
    read_number(path).filter(|limit| *limit < (1_u64 << 60))
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
