//! Descriptors carried with the bytes of a Unix stream socket (`SCM_RIGHTS`).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

#[repr(C, align(8))]
struct Control([u8; 64]);

#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: libc::c_int = 0;

#[cfg(any(target_os = "linux", target_os = "android"))]
const RECEIVE_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECEIVE_FLAGS: libc::c_int = 0;

fn one_descriptor() -> (usize, usize) {
    let size = libc::c_uint::try_from(size_of::<RawFd>()).unwrap_or(4);
    // SAFETY: both are arithmetic on their argument.
    let (space, length) = unsafe { (libc::CMSG_SPACE(size), libc::CMSG_LEN(size)) };
    let space = usize::try_from(space).unwrap_or(usize::MAX);
    assert!(space <= size_of::<Control>());
    (space, usize::try_from(length).unwrap_or(usize::MAX))
}

/// Writes all of `bytes` to `socket`, with `descriptor` attached to the first of them, so that
/// the reader receives a copy of it with those bytes ([`DescriptorReader`]).
///
/// # Errors
/// When `bytes` is empty, or the socket refuses the write.
pub fn write_with_descriptor(
    socket: &UnixStream,
    bytes: &[u8],
    descriptor: BorrowedFd<'_>,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a descriptor needs at least one byte to travel with",
        ));
    }
    let (space, length) = one_descriptor();
    let mut control = Control([0; 64]);
    let mut part = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: an all-zero `msghdr` is valid; the fields the call reads are set below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &raw mut part;
    message.msg_iovlen = 1;
    message.msg_control = control.0.as_mut_ptr().cast();
    #[allow(
        clippy::useless_conversion,
        clippy::unnecessary_fallible_conversions,
        reason = "the field's type differs between platforms"
    )]
    {
        message.msg_controllen = space.try_into().map_err(io::Error::other)?;
    }
    // SAFETY: `message` names a control buffer of `space` bytes, room for one descriptor's
    // header and data, which `CMSG_FIRSTHDR` and `CMSG_DATA` point into.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        #[allow(
            clippy::useless_conversion,
            clippy::unnecessary_fallible_conversions,
            reason = "the field's type differs between platforms"
        )]
        {
            (*header).cmsg_len = length.try_into().map_err(io::Error::other)?;
        }
        libc::CMSG_DATA(header)
            .cast::<RawFd>()
            .write_unaligned(descriptor.as_raw_fd());
    }
    let sent = loop {
        // SAFETY: `message` and the buffers it names live for the call.
        let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &raw const message, SEND_FLAGS) };
        if let Ok(sent) = usize::try_from(sent) {
            break sent;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    let mut socket = socket;
    socket.write_all(&bytes[sent..])
}

/// Reads a Unix stream socket and keeps, in arrival order, the descriptors that come with its
/// bytes, each closed on `exec`. A read that receives more than one descriptor at once closes
/// them and fails with `InvalidData`.
#[derive(Debug)]
pub struct DescriptorReader {
    socket: OwnedFd,
    received: VecDeque<OwnedFd>,
}

impl DescriptorReader {
    /// A reader of `socket`.
    pub fn new(socket: impl Into<OwnedFd>) -> Self {
        Self {
            socket: socket.into(),
            received: VecDeque::new(),
        }
    }

    /// The oldest received descriptor not yet taken.
    pub fn take_descriptor(&mut self) -> Option<OwnedFd> {
        self.received.pop_front()
    }

    /// How many received descriptors were not taken.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.received.len()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn close_on_exec(descriptor: &OwnedFd) -> io::Result<()> {
    // SAFETY: `fcntl` reads no memory; `descriptor` is open.
    if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Read for DescriptorReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (space, _) = one_descriptor();
        let mut control = Control([0; 64]);
        let mut part = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: an all-zero `msghdr` is valid; the fields the call reads are set below.
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &raw mut part;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        #[allow(
            clippy::useless_conversion,
            clippy::unnecessary_fallible_conversions,
            reason = "the field's type differs between platforms"
        )]
        {
            message.msg_controllen = space.try_into().map_err(io::Error::other)?;
        }
        let read = loop {
            // SAFETY: `message` and the buffers it names live for the call.
            let read =
                unsafe { libc::recvmsg(self.socket.as_raw_fd(), &raw mut message, RECEIVE_FLAGS) };
            if let Ok(read) = usize::try_from(read) {
                break read;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        let mut descriptors = Vec::new();
        // SAFETY: `recvmsg` filled the control buffer and set its length; the macros walk it, and
        // each descriptor it holds was installed in this process for this message alone.
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&raw const message);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let data = libc::CMSG_DATA(header);
                    let start = data.offset_from(header.cast::<u8>()).cast_unsigned();
                    #[allow(
                        clippy::useless_conversion,
                        clippy::unnecessary_fallible_conversions,
                        reason = "the field's type differs between platforms"
                    )]
                    let length = usize::try_from((*header).cmsg_len).unwrap_or(0);
                    let count = length.saturating_sub(start) / size_of::<RawFd>();
                    for index in 0..count {
                        let descriptor = data.cast::<RawFd>().add(index).read_unaligned();
                        descriptors.push(OwnedFd::from_raw_fd(descriptor));
                    }
                }
                header = libc::CMSG_NXTHDR(&raw const message, header);
            }
        }
        if descriptors.len() > 1 || message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "more than one descriptor came with one write",
            ));
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        for descriptor in &descriptors {
            close_on_exec(descriptor)?;
        }
        self.received.extend(descriptors);
        Ok(read)
    }
}

#[cfg(test)]
#[path = "descriptor_passing_tests.rs"]
mod tests;
