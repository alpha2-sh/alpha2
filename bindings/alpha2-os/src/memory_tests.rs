#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::{Path, PathBuf};

use super::*;

/// Every supported OS reports its physical memory and something available,
/// a limit no larger than the machine, and a footprint for this process.
#[test]
fn memory_capacity_reports_physical_memory() {
    let capacity = memory_capacity();
    let physical = capacity.physical_bytes.expect("physical memory");
    assert!(physical > 0, "{capacity:?}");
    assert!(
        capacity
            .available_bytes
            .is_some_and(|available| available > 0 && available <= physical),
        "{capacity:?}"
    );
    assert!(
        capacity
            .effective_limit()
            .is_some_and(|limit| limit <= physical),
        "{capacity:?}"
    );
    assert!(
        ["cgroup-v2", "cgroup-v1", "job", "physical"].contains(&capacity.limit_source),
        "{capacity:?}"
    );
    let process = crate::process_memory();
    assert!(
        process.footprint_bytes.is_some_and(|bytes| bytes > 0),
        "{process:?}"
    );
    assert!(
        process
            .peak_footprint_bytes
            .zip(process.footprint_bytes)
            .is_some_and(|(peak, now)| peak >= now),
        "{process:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "android"))]
/// A directory laid out like `/` for [`memory_capacity_at`]: `files` are
/// paths under it and their contents.
fn root(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("alpha2-os-memory-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
        std::fs::write(path, contents).expect("file");
    }
    root
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const MEMINFO: &str =
    "MemTotal:       65536000 kB\nMemFree:         1000 kB\nMemAvailable:   32768000 kB\n";

#[cfg(any(target_os = "linux", target_os = "android"))]
/// The limit is read from the process's own cgroup and the levels above it,
/// the lowest winning; a level without one (`max`) is skipped, and what the
/// limiting level has charged comes with it.
#[test]
fn cgroup_v2_limit_is_read_from_the_process_cgroup() {
    let dir = root(
        "v2",
        &[
            ("proc/meminfo", MEMINFO),
            ("proc/self/cgroup", "0::/docker/abc\n"),
            ("sys/fs/cgroup/docker/abc/memory.max", "max\n"),
            ("sys/fs/cgroup/docker/abc/memory.current", "5\n"),
            ("sys/fs/cgroup/docker/memory.max", "1073741824\n"),
            ("sys/fs/cgroup/docker/memory.current", "268435456\n"),
        ],
    );
    let capacity = memory_capacity_at(&dir);
    assert_eq!(capacity.physical_bytes, Some(65_536_000 * 1_024));
    assert_eq!(capacity.available_bytes, Some(32_768_000 * 1_024));
    assert_eq!(capacity.limit_bytes, Some(1 << 30));
    assert_eq!(capacity.limit_source, "cgroup-v2");
    assert_eq!(capacity.used_in_limit_bytes, Some(1 << 28));
    assert_eq!(capacity.effective_limit(), Some(1 << 30));
    assert_eq!(capacity.headroom(), Some((1 << 30) - (1 << 28)));
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
/// A v1 hierarchy is read where there is no v2 limit, and its "unlimited"
/// default is no limit: the physical total is.
#[test]
fn cgroup_v1_limit_and_its_unlimited_default() {
    let dir = root(
        "v1",
        &[
            ("proc/meminfo", MEMINFO),
            (
                "proc/self/cgroup",
                "12:cpu,cpuacct:/x\n4:memory:/job\n0::/\n",
            ),
            (
                "sys/fs/cgroup/memory/job/memory.limit_in_bytes",
                "2147483648\n",
            ),
            ("sys/fs/cgroup/memory/job/memory.usage_in_bytes", "1024\n"),
        ],
    );
    let capacity = memory_capacity_at(&dir);
    assert_eq!(capacity.limit_bytes, Some(2 << 30));
    assert_eq!(capacity.limit_source, "cgroup-v1");
    assert_eq!(capacity.used_in_limit_bytes, Some(1_024));
    std::fs::write(
        dir.join("sys/fs/cgroup/memory/job/memory.limit_in_bytes"),
        "9223372036854771712\n",
    )
    .expect("file");
    let unlimited = memory_capacity_at(&dir);
    assert_eq!(unlimited.limit_source, "physical");
    assert_eq!(unlimited.limit_bytes, unlimited.physical_bytes);
    assert_eq!(unlimited.used_in_limit_bytes, None);
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
/// Nothing to read is nothing reported, not a guess.
#[test]
fn a_missing_root_reports_nothing() {
    let capacity = memory_capacity_at(Path::new("/nonexistent/alpha2-os-memory"));
    assert_eq!(capacity.physical_bytes, None);
    assert_eq!(capacity.limit_source, "physical");
    assert_eq!(capacity.effective_limit(), None);
}
