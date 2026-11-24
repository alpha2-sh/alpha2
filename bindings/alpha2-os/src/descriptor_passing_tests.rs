use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use super::{DescriptorReader, write_with_descriptor};

const CHILD: &str = "ALPHA2_OS_TEST_DESCRIPTOR_CHILD";
const PROBE: &str = "ALPHA2_OS_TEST_DESCRIPTOR_PROBE";

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

#[expect(unsafe_code, reason = "fcntl on a descriptor number")]
fn descriptor_open(descriptor: i32) -> bool {
    // SAFETY: `fcntl` reads no memory; a closed number fails with `EBADF`.
    unsafe { libc::fcntl(descriptor, libc::F_GETFD) != -1 }
}

#[expect(unsafe_code, reason = "fcntl on an owned descriptor")]
fn closed_on_exec(descriptor: &OwnedFd) -> bool {
    // SAFETY: `fcntl` reads no memory; `descriptor` is open.
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
    flags != -1 && flags & libc::FD_CLOEXEC != 0
}

#[test]
#[ignore = "run by the tests of this module in a child process"]
fn descriptor_probe() {
    if let Some(descriptor) = std::env::var_os(PROBE) {
        let descriptor = descriptor.to_string_lossy().parse().expect("number");
        let open = descriptor_open(descriptor);
        println!("probe: {}", if open { "open" } else { "closed" });
    }
}

#[test]
#[ignore = "run by the tests of this module in a child process"]
fn descriptor_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let mut input = DescriptorReader::new(crate::claim_stdin().expect("stdin"));
    let mut output = crate::claim_stdout().expect("stdout");
    let mut say = |line: String| writeln!(output, "child: {line}").expect("write");
    let mut bytes = [0; 3];
    input.read_exact(&mut bytes).expect("one");
    say(format!(
        "{} {}",
        String::from_utf8_lossy(&bytes),
        input.pending()
    ));
    input.read_exact(&mut bytes).expect("two");
    say(format!(
        "{} {}",
        String::from_utf8_lossy(&bytes),
        input.pending()
    ));
    let received = input.take_descriptor().expect("a descriptor");
    say(format!("cloexec {}", closed_on_exec(&received)));
    let probe = test_child("descriptor_passing::tests::descriptor_probe")
        .env(PROBE, received.as_raw_fd().to_string())
        .stdin(Stdio::null())
        .output()
        .expect("probe");
    let probed = String::from_utf8_lossy(&probe.stdout)
        .lines()
        .find_map(|line| Some(line.split_once("probe: ")?.1.to_owned()))
        .unwrap_or_default();
    say(format!("grandchild {probed}"));
    std::fs::File::from(received)
        .write_all(b"through the descriptor")
        .expect("write through it");
}

#[test]
fn a_descriptor_arrives_with_its_bytes_closed_on_exec_and_plain_reads_are_unaffected() {
    let _fork = crate::tests::FORK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut host, worker) = UnixStream::pair().expect("socket pair");
    let mut child = test_child("descriptor_passing::tests::descriptor_child")
        .env(CHILD, "1")
        .stdin(Stdio::from(OwnedFd::from(
            worker.try_clone().expect("clone"),
        )))
        .stdout(Stdio::from(OwnedFd::from(worker)))
        .spawn()
        .expect("child");
    let (mut pipe, sent) = std::io::pipe().expect("pipe");
    let mut replies = BufReader::new(host.try_clone().expect("clone")).lines();
    let mut reply = || {
        replies
            .by_ref()
            .map_while(Result::ok)
            .find_map(|line| Some(line.split_once("child: ")?.1.to_owned()))
            .expect("a reply")
    };
    host.write_all(b"one").expect("one");
    assert_eq!(reply(), "one 0");
    write_with_descriptor(&host, b"two", sent.as_fd()).expect("two");
    drop(sent);
    assert_eq!(reply(), "two 1");
    assert_eq!(reply(), "cloexec true");
    assert_eq!(reply(), "grandchild closed");
    let mut through = String::new();
    pipe.read_to_string(&mut through).expect("read the pipe");
    assert_eq!(through, "through the descriptor");
    assert!(child.wait().expect("wait").success());
}

#[test]
#[expect(unsafe_code, reason = "sendmsg with two descriptors")]
fn a_write_that_attaches_two_descriptors_fails_the_read() {
    let (host, worker) = UnixStream::pair().expect("socket pair");
    let (_, first) = std::io::pipe().expect("pipe");
    let (_, second) = std::io::pipe().expect("pipe");
    let descriptors = [first.as_raw_fd(), second.as_raw_fd()];
    let size = libc::c_uint::try_from(size_of_val(&descriptors)).expect("size");
    let mut control = super::Control([0; 64]);
    let mut byte = *b"x";
    let mut part = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: an all-zero `msghdr` is valid; the fields the call reads are set below, and the
    // control buffer holds the header and both descriptors.
    let sent = unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut part;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(size) as _;
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size) as _;
        libc::CMSG_DATA(header)
            .cast::<[i32; 2]>()
            .write_unaligned(descriptors);
        libc::sendmsg(host.as_raw_fd(), &raw const message, 0)
    };
    assert_eq!(sent, 1);
    let mut reader = DescriptorReader::new(worker);
    let error = reader.read(&mut [0; 4]).expect_err("two descriptors");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}
