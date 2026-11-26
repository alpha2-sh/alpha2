//! Memory a host writes and a worker process it started reads, never named on disk.

use std::io;
use std::ptr::NonNull;

#[derive(Debug)]
struct Mapping {
    start: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; access goes through `&` or `&mut`.
unsafe impl Send for Mapping {}
// SAFETY: as above; shared references only read.
unsafe impl Sync for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        platform::unmap(self.start, self.len);
    }
}

/// The host's side of a region, mapped read-write: a sealed `memfd` on Linux, a POSIX shared
/// memory object unlinked at once on macOS, an unnamed section on Windows.
#[derive(Debug)]
pub struct SharedRegion {
    mapping: Mapping,
    share: platform::Share,
}

impl SharedRegion {
    /// Makes a region of `len` bytes, zeroed.
    ///
    /// # Errors
    /// When `len` is zero or the operating system refuses the memory, and on platforms without
    /// shared regions (`Unsupported`).
    pub fn create(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a region of no bytes",
            ));
        }
        let (mapping, share) = platform::create(len)?;
        Ok(Self { mapping, share })
    }

    /// The region's length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.mapping.len
    }

    /// Whether the region holds no bytes, which [`Self::create`] refuses.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mapping.len == 0
    }

    /// The region's bytes.
    pub const fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the mapping is `len` readable and writable bytes, borrowed mutably with `self`.
        unsafe { std::slice::from_raw_parts_mut(self.mapping.start.as_ptr(), self.mapping.len) }
    }

    /// The descriptor to send a worker, which maps it with [`MappedRegion::open`]. It cannot
    /// resize the region or map it writable, except on Linux before 5.1 (no
    /// `F_SEAL_FUTURE_WRITE`), where only the worker's read-only mapping keeps it from writing.
    #[cfg(unix)]
    #[must_use]
    pub fn descriptor(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.share.reader.as_fd()
    }

    /// Duplicates a read-only handle of the region into `process`, for it to map with
    /// [`MappedRegion::open`]; returns the handle's value there. The handle closes with that
    /// process unless it maps it first.
    ///
    /// # Errors
    /// When the handle cannot be duplicated into `process`.
    #[cfg(windows)]
    pub fn share_with(&self, process: &std::process::Child) -> io::Result<u64> {
        platform::share_with(&self.share, process)
    }
}

/// A worker's side of a region: mapped read-only.
#[derive(Debug)]
pub struct MappedRegion {
    mapping: Mapping,
}

impl MappedRegion {
    /// Maps the first `len` bytes of the region `descriptor` names, read-only, and closes the
    /// descriptor; the mapping keeps the region.
    ///
    /// # Errors
    /// When the region is shorter than `len` or cannot be mapped.
    #[cfg(unix)]
    pub fn open(descriptor: std::os::fd::OwnedFd, len: usize) -> io::Result<Self> {
        let mapping = platform::open(&descriptor, len)?;
        drop(descriptor);
        Ok(Self { mapping })
    }

    /// Maps the first `len` bytes of the region whose handle [`SharedRegion::share_with`] gave
    /// this process, read-only, and closes the handle, which this call owns.
    ///
    /// # Errors
    /// When `handle` names no region of at least `len` bytes or it cannot be mapped.
    #[cfg(windows)]
    pub fn open(handle: u64, len: usize) -> io::Result<Self> {
        Ok(Self {
            mapping: platform::open(handle, len)?,
        })
    }

    /// The region's length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.mapping.len
    }

    /// Whether the region holds no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mapping.len == 0
    }

    /// The region's bytes.
    #[must_use]
    pub const fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is `len` readable bytes; the host writes them only while no call
        // reads them.
        unsafe { std::slice::from_raw_parts(self.mapping.start.as_ptr(), self.mapping.len) }
    }
}

fn empty_mapping(len: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("a mapping of {len} bytes"),
    )
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::ptr::NonNull;

    use super::Mapping;

    pub(super) fn map(
        descriptor: BorrowedFd<'_>,
        len: usize,
        writable: bool,
    ) -> io::Result<Mapping> {
        if len == 0 {
            return Err(super::empty_mapping(len));
        }
        let protection = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        // SAFETY: a new shared mapping of an open descriptor; no existing memory is touched.
        let start = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                protection,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        if start == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let start = NonNull::new(start.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
        Ok(Mapping { start, len })
    }

    pub(super) fn unmap(start: NonNull<u8>, len: usize) {
        // SAFETY: `start` and `len` are a mapping this process made and nothing else unmaps.
        unsafe { libc::munmap(start.as_ptr().cast(), len) };
    }

    pub(super) fn set_len(descriptor: BorrowedFd<'_>, len: usize) -> io::Result<()> {
        let len = libc::off_t::try_from(len).map_err(io::Error::other)?;
        // SAFETY: `ftruncate` reads no memory.
        if unsafe { libc::ftruncate(descriptor.as_raw_fd(), len) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn open(descriptor: &OwnedFd, len: usize) -> io::Result<Mapping> {
        // SAFETY: an all-zero `stat` is a valid value for the call to fill.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `status` outlives the call.
        if unsafe { libc::fstat(descriptor.as_raw_fd(), &raw mut status) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if u64::try_from(status.st_size).unwrap_or(0) < u64::try_from(len).unwrap_or(u64::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "a region of {} bytes, not the {len} it was sent for",
                    status.st_size
                ),
            ));
        }
        map(descriptor.as_fd(), len, false)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use std::io;
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};

    pub(super) use super::unix::{open, unmap};
    use super::{Mapping, unix};

    #[derive(Debug)]
    pub(super) struct Share {
        pub(super) reader: OwnedFd,
    }

    const SEALS: libc::c_int = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;

    pub(super) fn create(len: usize) -> io::Result<(Mapping, Share)> {
        // SAFETY: the name is a valid C string; the call returns a new descriptor or fails.
        let descriptor = unsafe {
            libc::memfd_create(
                c"alpha2-frames".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is new and owned by nothing else.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        unix::set_len(descriptor.as_fd(), len)?;
        let mapping = unix::map(descriptor.as_fd(), len, true)?;
        seal(&descriptor)?;
        Ok((mapping, Share { reader: descriptor }))
    }

    fn seal(descriptor: &OwnedFd) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let add = |seals: libc::c_int| {
            // SAFETY: `fcntl` reads no memory; `descriptor` is open.
            if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_ADD_SEALS, seals) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        // Linux 5.1 adds `F_SEAL_FUTURE_WRITE`; older kernels refuse it with `EINVAL`.
        match add(SEALS | libc::F_SEAL_FUTURE_WRITE) {
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => add(SEALS),
            sealed => sealed,
        }
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use std::ffi::CStr;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

    pub(super) use super::unix::{open, unmap};
    use super::{Mapping, unix};

    #[derive(Debug)]
    pub(super) struct Share {
        pub(super) reader: OwnedFd,
    }

    const NAME_ATTEMPTS: usize = 8;

    pub(super) fn create(len: usize) -> io::Result<(Mapping, Share)> {
        let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
        for _ in 0..NAME_ATTEMPTS {
            let mut random = [0_u8; 8];
            // SAFETY: `random` is writable for its length.
            unsafe { libc::arc4random_buf(random.as_mut_ptr().cast(), random.len()) };
            let name = format!("/a2f-{:016x}\0", u64::from_ne_bytes(random));
            let name = CStr::from_bytes_with_nul(name.as_bytes()).map_err(io::Error::other)?;
            match create_named(name, len) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = error,
                made => return made,
            }
        }
        Err(last)
    }

    fn shm_open(name: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
        let mode = libc::c_uint::from(0o600_u16);
        // SAFETY: `name` is a valid C string; the call returns a new descriptor or fails.
        let descriptor = unsafe { libc::shm_open(name.as_ptr(), flags, mode) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor` is new and owned by nothing else.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        // SAFETY: `fcntl` reads no memory; `descriptor` is open.
        if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor)
    }

    pub(super) fn create_named(name: &CStr, len: usize) -> io::Result<(Mapping, Share)> {
        let writer = shm_open(name, libc::O_CREAT | libc::O_EXCL | libc::O_RDWR)?;
        let made = (|| {
            let reader = shm_open(name, libc::O_RDONLY)?;
            unix::set_len(writer.as_fd(), len)?;
            let mapping = unix::map(writer.as_fd(), len, true)?;
            Ok((mapping, Share { reader }))
        })();
        // SAFETY: `name` is a valid C string.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        made
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
mod platform {
    use std::io;
    use std::os::fd::OwnedFd;

    use super::Mapping;
    pub(super) use super::unix::{open, unmap};

    #[derive(Debug)]
    pub(super) struct Share {
        pub(super) reader: OwnedFd,
    }

    pub(super) fn create(_len: usize) -> io::Result<(Mapping, Share)> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::NonNull;

    use windows_sys::Win32::Foundation::{
        CloseHandle, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
        MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::Mapping;

    #[derive(Debug)]
    pub(super) struct Share {
        section: OwnedHandle,
    }

    fn view(section: HANDLE, access: u32, len: usize) -> io::Result<Mapping> {
        if len == 0 {
            return Err(super::empty_mapping(len));
        }
        // SAFETY: a new view of a section handle; no existing memory is touched.
        let view = unsafe { MapViewOfFile(section, access, 0, 0, len) };
        let start = NonNull::new(view.Value.cast::<u8>()).ok_or_else(io::Error::last_os_error)?;
        Ok(Mapping { start, len })
    }

    pub(super) fn create(len: usize) -> io::Result<(Mapping, Share)> {
        let size = u64::try_from(len).map_err(io::Error::other)?;
        let high = u32::try_from(size >> 32).map_err(io::Error::other)?;
        let low = u32::try_from(size & u64::from(u32::MAX)).map_err(io::Error::other)?;
        // SAFETY: an unnamed section backed by the paging file; the call returns a new handle or
        // null.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                high,
                low,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `section` is new and owned by nothing else.
        let section = unsafe { OwnedHandle::from_raw_handle(section) };
        let mapping = view(section.as_raw_handle(), FILE_MAP_WRITE, len)?;
        Ok((mapping, Share { section }))
    }

    pub(super) fn share_with(share: &Share, process: &std::process::Child) -> io::Result<u64> {
        let mut duplicated: HANDLE = std::ptr::null_mut();
        // SAFETY: both process handles and the section are open; `duplicated` outlives the call.
        let done = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                share.section.as_raw_handle(),
                process.as_raw_handle(),
                &raw mut duplicated,
                FILE_MAP_READ,
                0,
                0,
            )
        };
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        u64::try_from(duplicated.addr()).map_err(io::Error::other)
    }

    pub(super) fn open(handle: u64, len: usize) -> io::Result<Mapping> {
        let handle: HANDLE =
            std::ptr::without_provenance_mut(usize::try_from(handle).map_err(io::Error::other)?);
        let mapped = view(handle, FILE_MAP_READ, len);
        // SAFETY: the caller hands this call the handle it owns.
        unsafe { CloseHandle(handle) };
        mapped
    }

    pub(super) fn unmap(start: NonNull<u8>, _len: usize) {
        // SAFETY: `start` is a view this process mapped and nothing else unmaps.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: start.as_ptr().cast(),
            })
        };
    }
}

#[cfg(test)]
#[path = "shared_region_tests.rs"]
mod tests;
