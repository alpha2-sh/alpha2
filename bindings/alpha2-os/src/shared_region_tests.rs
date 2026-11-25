use std::io::{BufRead, BufReader};
use std::process::{Command, ExitStatus, Stdio};

use super::{MappedRegion, SharedRegion};

const CHILD: &str = "ALPHA2_OS_TEST_REGION_CHILD";
const PROBE: &str = "ALPHA2_OS_TEST_REGION_PROBE";
const LEN: usize = (1 << 20) + 3;

fn test_child(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--exact",
        test,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    command
}

fn fnv(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

fn pattern(at: usize) -> u8 {
    u8::try_from(at % 251).expect("byte")
}

fn written(len: usize) -> SharedRegion {
    let mut region = SharedRegion::create(len).expect("region");
    for (at, byte) in region.as_mut_slice().iter_mut().enumerate() {
        *byte = pattern(at);
    }
    region
}

fn said<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|line| Some(line.split_once(key)?.1.trim()))
}

#[cfg(unix)]
fn received() -> (std::os::fd::OwnedFd, usize, String) {
    use std::os::fd::AsRawFd;
    let mut input = BufReader::new(crate::DescriptorReader::new(
        crate::claim_stdin().expect("stdin"),
    ));
    let mut line = String::new();
    input.read_line(&mut line).expect("line");
    let len = line.trim().parse().expect("length");
    let descriptor = input.get_mut().take_descriptor().expect("a descriptor");
    let name = descriptor.as_raw_fd().to_string();
    (descriptor, len, name)
}

#[cfg(windows)]
fn received() -> (u64, usize, String) {
    let mut line = String::new();
    BufReader::new(std::io::stdin())
        .read_line(&mut line)
        .expect("line");
    let (len, handle) = line.trim().split_once(' ').expect("two numbers");
    let handle: u64 = handle.parse().expect("handle");
    (handle, len.parse().expect("length"), handle.to_string())
}

#[cfg(unix)]
#[expect(unsafe_code, reason = "a descriptor number from the parent")]
fn map_named(name: &str, len: usize) -> std::io::Result<MappedRegion> {
    let number: i32 = name.parse().map_err(std::io::Error::other)?;
    // SAFETY: `fcntl` reads no memory; a closed number fails with `EBADF`.
    if unsafe { libc::fcntl(number, libc::F_GETFD) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the number is open, and this probe owns nothing else under it.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(number) };
    MappedRegion::open(borrowed.try_clone_to_owned()?, len)
}

#[cfg(windows)]
fn map_named(name: &str, len: usize) -> std::io::Result<MappedRegion> {
    MappedRegion::open(name.parse().map_err(std::io::Error::other)?, len)
}

#[test]
#[ignore = "run by the tests of this module in a child process"]
fn region_probe() {
    if let Some(probe) = std::env::var_os(PROBE) {
        let probe = probe.to_string_lossy().into_owned();
        let (name, len) = probe.split_once(' ').expect("two values");
        let mapped = map_named(name, len.parse().expect("length"));
        println!(
            "probe: {}",
            if mapped.is_ok() { "mapped" } else { "refused" }
        );
    }
}

#[test]
#[ignore = "run by the tests of this module in a child process"]
#[expect(unsafe_code, reason = "a write through the read-only mapping")]
fn region_child() {
    let Some(mode) = std::env::var_os(CHILD) else {
        return;
    };
    crate::disable_crash_dumps();
    let (region, len, name) = received();
    if mode == "spawn" {
        let probe = test_child("shared_region::tests::region_probe")
            .env(PROBE, format!("{name} {len}"))
            .stdin(Stdio::null())
            .output()
            .expect("probe");
        let output = String::from_utf8_lossy(&probe.stdout);
        println!("child: {}", said(&output, "probe: ").unwrap_or_default());
        return;
    }
    let mapped = MappedRegion::open(region, len).expect("map");
    println!("child: {:08x} {}", fnv(mapped.as_slice()), mapped.len());
    if mode == "write" {
        // SAFETY: none holds; the write is meant to fault.
        unsafe { mapped.as_slice().as_ptr().cast_mut().write_volatile(1) };
        println!("child: wrote");
    }
}

fn share(region: &SharedRegion, mode: &str) -> (ExitStatus, String) {
    let _fork = crate::tests::FORK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut command = test_child("shared_region::tests::region_child");
    command.env(CHILD, mode).stdout(Stdio::piped());
    #[cfg(unix)]
    let child = {
        let (host, worker) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let child = command
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(worker)))
            .spawn()
            .expect("child");
        crate::write_with_descriptor(
            &host,
            format!("{}\n", region.len()).as_bytes(),
            region.descriptor(),
        )
        .expect("send");
        child
    };
    #[cfg(windows)]
    let child = {
        use std::io::Write;
        let mut child = command.stdin(Stdio::piped()).spawn().expect("child");
        let handle = region.share_with(&child).expect("share");
        let mut input = child.stdin.take().expect("stdin");
        writeln!(input, "{} {handle}", region.len()).expect("send");
        child
    };
    let output = child.wait_with_output().expect("wait");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn a_worker_reads_what_its_host_wrote() {
    let region = written(LEN);
    let (status, output) = share(&region, "read");
    assert!(status.success(), "{status}: {output}");
    let expected = format!(
        "{:08x} {LEN}",
        fnv(&(0..LEN).map(pattern).collect::<Vec<_>>())
    );
    assert_eq!(
        said(&output, "child: "),
        Some(expected.as_str()),
        "{output}"
    );
}

#[test]
fn a_write_through_the_worker_mapping_faults() {
    let region = written(LEN);
    let (status, output) = share(&region, "write");
    assert!(
        !status.success() && !output.contains("wrote"),
        "{status}: {output}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            matches!(status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
            "{status}"
        );
    }
    #[cfg(windows)]
    assert_eq!(status.code().map(i32::cast_unsigned), Some(0xC000_0005));
}

#[test]
fn what_the_worker_received_does_not_reach_a_process_it_starts() {
    let region = written(LEN);
    let (status, output) = share(&region, "spawn");
    assert!(status.success(), "{status}: {output}");
    assert_eq!(said(&output, "child: "), Some("refused"), "{output}");
}

#[test]
fn a_region_of_no_bytes_is_refused() {
    let refused = SharedRegion::create(0).expect_err("no bytes");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(unix)]
#[test]
fn a_region_shorter_than_the_length_sent_is_refused() {
    let region = written(LEN);
    let descriptor = region.descriptor().try_clone_to_owned().expect("clone");
    let refused = MappedRegion::open(descriptor, LEN + (1 << 20)).expect_err("too short");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
}

#[cfg(target_os = "linux")]
#[test]
#[expect(unsafe_code, reason = "mmap and ftruncate on the shared descriptor")]
fn the_sealed_descriptor_refuses_a_writable_mapping_and_a_resize() {
    use std::os::fd::AsRawFd;
    let region = written(LEN);
    let descriptor = region.descriptor().as_raw_fd();
    // SAFETY: a new mapping of an open descriptor, unmapped below if it is made.
    let writable = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            LEN,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            descriptor,
            0,
        )
    };
    let error = std::io::Error::last_os_error();
    if writable != libc::MAP_FAILED {
        // SAFETY: the mapping made above.
        unsafe { libc::munmap(writable, LEN) };
    }
    assert_eq!(writable, libc::MAP_FAILED);
    assert_eq!(error.raw_os_error(), Some(libc::EPERM));
    for len in [0, 2 * LEN] {
        let len = libc::off_t::try_from(len).expect("length");
        // SAFETY: `ftruncate` reads no memory.
        assert_eq!(unsafe { libc::ftruncate(descriptor, len) }, -1, "{len}");
    }
    let clone = region.descriptor().try_clone_to_owned().expect("clone");
    let mapped = MappedRegion::open(clone, LEN).expect("a read-only mapping");
    assert_eq!(mapped.as_slice()[LEN - 1], pattern(LEN - 1));
}

#[cfg(target_vendor = "apple")]
#[test]
#[expect(unsafe_code, reason = "shm_open of the name the region had")]
fn a_region_leaves_no_name_behind() {
    let name = format!("/a2f-test-{:08x}\0", std::process::id());
    let name = std::ffi::CStr::from_bytes_with_nul(name.as_bytes()).expect("name");
    let made = super::platform::create_named(name, LEN).expect("region");
    // SAFETY: `name` is a valid C string.
    let opened = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    let error = std::io::Error::last_os_error();
    assert_eq!(opened, -1);
    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    drop(made);
    drop(super::platform::create_named(name, LEN).expect("the name is free again"));
}
