//! How much memory the machine has, and how much this process may use.
//!
//! The physical total, what the operating system reports as available now,
//! and the limit a container or job puts on this process, with what that
//! limit has already charged. Linux reads `/proc/meminfo` and the process's
//! cgroup (v2 `memory.max`/`memory.current`, v1
//! `memory.limit_in_bytes`/`memory.usage_in_bytes`); macOS `hw.memsize` and
//! the free, inactive and purgeable pages of `host_statistics64`; Windows
//! `GlobalMemoryStatusEx` and the job object's extended limits.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::Path;

/// What memory the machine has and what this process may use of it. A figure
/// the operating system does not report (or this build does not read on it)
/// is `None`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryCapacity {
    /// Physical memory installed.
    pub physical_bytes: Option<u64>,
    /// Memory the operating system could give a process now without
    /// paging: `MemAvailable` on Linux, free plus inactive plus purgeable
    /// pages on macOS, `ullAvailPhys` on Windows.
    pub available_bytes: Option<u64>,
    /// The most this process may use: the lowest of a cgroup or job limit
    /// and the physical total.
    pub limit_bytes: Option<u64>,
    /// What set `limit_bytes`: `cgroup-v2`, `cgroup-v1`, `job` or `physical`.
    pub limit_source: &'static str,
    /// What the limit has already charged (the cgroup's `memory.current` or
    /// usage); `None` for a physical limit or a job, whose current charge
    /// Windows does not report.
    pub used_in_limit_bytes: Option<u64>,
}

impl MemoryCapacity {
    /// The memory an export may plan with: the limit, else the physical
    /// total.
    #[must_use]
    pub fn effective_limit(&self) -> Option<u64> {
        match (self.limit_bytes, self.physical_bytes) {
            (Some(limit), Some(physical)) => Some(limit.min(physical)),
            (limit, physical) => limit.or(physical),
        }
    }

    /// What may still be allocated before the limit or the machine runs
    /// out: the lower of the available memory and the limit's headroom.
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
    #[cfg(any(target_os = "macos", windows))]
    {
        crate::platform::memory_capacity()
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        windows
    )))]
    {
        MemoryCapacity {
            limit_source: "physical",
            ..MemoryCapacity::default()
        }
    }
}

/// [`memory_capacity`] read from `root` as if it were `/`: `proc/meminfo`,
/// `proc/self/cgroup`, and the cgroup files under `sys/fs/cgroup`.
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
    let cgroup = cgroup_limit(root);
    match cgroup {
        Some((limit, used, source)) if physical.is_none_or(|physical| limit < physical) => {
            MemoryCapacity {
                physical_bytes: physical,
                available_bytes: available,
                limit_bytes: Some(limit),
                limit_source: source,
                used_in_limit_bytes: used,
            }
        }
        _ => MemoryCapacity {
            physical_bytes: physical,
            available_bytes: available,
            limit_bytes: physical,
            limit_source: "physical",
            used_in_limit_bytes: None,
        },
    }
}

/// The process's cgroup memory: `(limit, charged, source)` of the lowest
/// limit on its cgroup's path, v2 first. A level without a limit (`max`, or
/// v1's near-`u64::MAX` default) is skipped.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn cgroup_limit(root: &Path) -> Option<(u64, Option<u64>, &'static str)> {
    let membership = std::fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
    let read = |path: &Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };
    // The lowest limit from the process's own cgroup up to the hierarchy's root.
    let lowest = |mount: &Path, relative: &str, limit_file: &str, usage_file: &str| {
        let mut best: Option<(u64, Option<u64>)> = None;
        let mut path = Some(Path::new(relative.trim_start_matches('/')));
        while let Some(level) = path {
            let directory = mount.join(level);
            if let Some(limit) = read(&directory.join(limit_file))
                && limit < (1_u64 << 60)
                && best.is_none_or(|(known, _)| limit < known)
            {
                best = Some((limit, read(&directory.join(usage_file))));
            }
            path = level.parent();
        }
        best
    };
    if let Some(relative) = membership.lines().find_map(|line| line.strip_prefix("0::"))
        && let Some((limit, used)) = lowest(
            &root.join("sys/fs/cgroup"),
            relative,
            "memory.max",
            "memory.current",
        )
    {
        return Some((limit, used, "cgroup-v2"));
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
    let (limit, used) = lowest(
        &root.join("sys/fs/cgroup/memory"),
        relative,
        "memory.limit_in_bytes",
        "memory.usage_in_bytes",
    )?;
    Some((limit, used, "cgroup-v1"))
}

/// The process's cgroup v2 working set and limit, `(memory.current` less the
/// reclaimable `inactive_file` of `memory.stat`, `memory.max)`, when its
/// cgroup has a limit, for a watchdog that compares them while an export
/// runs. Page cache the export's own writes fill is reclaimed at the limit
/// rather than killing anything, so it is not counted. Only the process's own
/// cgroup is read: that is where a container's limit is.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[must_use]
pub fn cgroup_v2_usage() -> Option<(u64, u64)> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    let directory = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    let read = |name: &str| {
        std::fs::read_to_string(directory.join(name))
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };
    let inactive_file = std::fs::read_to_string(directory.join("memory.stat"))
        .ok()
        .and_then(|stat| {
            stat.lines()
                .find_map(|line| line.strip_prefix("inactive_file "))
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);
    Some((
        read("memory.current")?.saturating_sub(inactive_file),
        read("memory.max")?,
    ))
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
