#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::{Path, PathBuf};

use super::*;

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
        ["cgroup-v2", "cgroup-v1", "job", "app", "physical"].contains(&capacity.limit_source),
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
/// A fake `/` for [`memory_capacity_at`] holding `files`.
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
/// The lowest limit on the cgroup path wins, with that level's usage.
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
#[test]
fn a_missing_root_reports_nothing() {
    let capacity = memory_capacity_at(Path::new("/nonexistent/alpha2-os-memory"));
    assert_eq!(capacity.physical_bytes, None);
    assert_eq!(capacity.limit_source, "physical");
    assert_eq!(capacity.effective_limit(), None);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const PAGE_CACHE_STAT: &str =
    "anon 761856\nfile 2146484224\nactive_file 8228864\ninactive_file 2138255360\n";

#[cfg(any(target_os = "linux", target_os = "android"))]
/// A limit full of file page cache leaves the limit less the working set as headroom.
#[test]
fn page_cache_under_a_cgroup_v2_limit_is_not_counted_as_used() {
    let dir = root(
        "v2-cache",
        &[
            ("proc/meminfo", MEMINFO),
            ("proc/self/cgroup", "0::/app\n"),
            ("sys/fs/cgroup/app/memory.max", "2147483648\n"),
            ("sys/fs/cgroup/app/memory.current", "2147250176\n"),
            ("sys/fs/cgroup/app/memory.stat", PAGE_CACHE_STAT),
        ],
    );
    let working_set = 2_147_250_176 - 2_138_255_360 - 8_228_864;
    let capacity = memory_capacity_at(&dir);
    assert_eq!(capacity.charged_in_limit_bytes, Some(2_147_250_176));
    assert_eq!(capacity.used_in_limit_bytes, Some(working_set));
    assert_eq!(capacity.headroom(), Some((2 << 30) - working_set));
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn page_cache_under_a_cgroup_v1_limit_is_not_counted_as_used() {
    let dir = root(
        "v1-cache",
        &[
            ("proc/meminfo", MEMINFO),
            ("proc/self/cgroup", "4:memory:/job\n"),
            (
                "sys/fs/cgroup/memory/job/memory.limit_in_bytes",
                "2147483648\n",
            ),
            (
                "sys/fs/cgroup/memory/job/memory.usage_in_bytes",
                "2000000000\n",
            ),
            (
                "sys/fs/cgroup/memory/job/memory.stat",
                "cache 1900000000\ntotal_inactive_file 1500000000\ntotal_active_file 400000000\n",
            ),
        ],
    );
    let capacity = memory_capacity_at(&dir);
    assert_eq!(capacity.used_in_limit_bytes, Some(100_000_000));
    assert_eq!(capacity.charged_in_limit_bytes, Some(2_000_000_000));
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
/// A limit one level up is what admission and the watchdog both read.
#[test]
fn a_limit_on_an_ancestor_gives_the_same_working_set_to_admission_and_the_watchdog() {
    let dir = root(
        "v2-ancestor",
        &[
            ("proc/meminfo", MEMINFO),
            ("proc/self/cgroup", "0::/scope/leaf\n"),
            ("sys/fs/cgroup/scope/leaf/memory.max", "max\n"),
            ("sys/fs/cgroup/scope/leaf/memory.current", "5\n"),
            ("sys/fs/cgroup/scope/memory.max", "2147483648\n"),
            ("sys/fs/cgroup/scope/memory.current", "2147250176\n"),
            ("sys/fs/cgroup/scope/memory.stat", PAGE_CACHE_STAT),
        ],
    );
    let memory = cgroup_memory_at(&dir).expect("a limit");
    assert_eq!(memory.limit_bytes, 2 << 30);
    assert_eq!(memory.source, "cgroup-v2");
    assert_eq!(
        memory.working_set_bytes,
        memory_capacity_at(&dir).used_in_limit_bytes
    );
    assert_eq!(
        memory.working_set_bytes,
        Some(2_147_250_176 - 2_138_255_360 - 8_228_864)
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn without_memory_stat_the_working_set_is_the_charge() {
    let dir = root(
        "v2-no-stat",
        &[
            ("proc/meminfo", MEMINFO),
            ("proc/self/cgroup", "0::/app\n"),
            ("sys/fs/cgroup/app/memory.max", "2147483648\n"),
            ("sys/fs/cgroup/app/memory.current", "1073741824\n"),
        ],
    );
    let capacity = memory_capacity_at(&dir);
    assert_eq!(capacity.used_in_limit_bytes, Some(1 << 30));
    assert_eq!(capacity.charged_in_limit_bytes, Some(1 << 30));
    std::fs::remove_dir_all(dir).expect("cleanup");
}
