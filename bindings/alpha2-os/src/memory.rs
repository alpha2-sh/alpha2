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
    /// Usage charged against a cgroup limit; `None` for physical and job limits.
    pub used_in_limit_bytes: Option<u64>,
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

/// `(limit, charged, source)` of the lowest limit on the process's cgroup path, v2 first. Levels
/// without a limit (`max`, or v1's near-`u64::MAX` default) are skipped.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn cgroup_limit(root: &Path) -> Option<(u64, Option<u64>, &'static str)> {
    let membership = std::fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
    let read = |path: &Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
    };
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

/// The process's cgroup v2 `(working set, memory.max)` when it has a limit. The working set
/// excludes reclaimable `inactive_file` page cache, which the kernel drops at the limit instead of
/// killing anything.
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
