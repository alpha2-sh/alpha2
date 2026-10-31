//! The operating system's version, for reports: which release a run was on
//! matters as much as which driver when a GPU or codec misbehaves.

use std::sync::OnceLock;

/// The operating system's version, read once: on macOS the product version
/// and build (`27.0.1 (26A434)`), on Linux the distribution's name and the
/// kernel release (`Ubuntu 22.04.5 LTS, kernel 6.8.0-136-generic`), on
/// Windows `major.minor.build.revision` (`10.0.26300.9711`). Whatever part
/// the system does not answer is left out; `unknown` when none is.
#[must_use]
pub fn os_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let version = read();
        if version.is_empty() {
            "unknown".to_owned()
        } else {
            version
        }
    })
}

#[cfg(target_os = "macos")]
fn read() -> String {
    let product = sysctl_string(c"kern.osproductversion");
    let build = sysctl_string(c"kern.osversion");
    match (product, build) {
        (Some(product), Some(build)) => format!("{product} ({build})"),
        (Some(product), None) => product,
        (None, Some(build)) => build,
        (None, None) => String::new(),
    }
}

/// A string `sysctl` value by name.
#[cfg(target_os = "macos")]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut length = 0_usize;
    // SAFETY: `name` is NUL-terminated; a null buffer asks only for the length.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || length == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; length];
    // SAFETY: `buffer` holds the `length` bytes the call may write.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(length);
    let text = String::from_utf8_lossy(&buffer);
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn read() -> String {
    let distribution = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|release| {
            release.lines().find_map(|line| {
                let value = line.strip_prefix("PRETTY_NAME=")?.trim();
                let value = value.trim_matches('"').trim_matches('\'');
                (!value.is_empty()).then(|| value.to_owned())
            })
        });
    let kernel = kernel_release();
    match (distribution, kernel) {
        (Some(distribution), Some(kernel)) => format!("{distribution}, kernel {kernel}"),
        (Some(distribution), None) => distribution,
        (None, Some(kernel)) => format!("kernel {kernel}"),
        (None, None) => String::new(),
    }
}

/// `uname`'s release: the kernel's version.
#[cfg(all(unix, not(target_os = "macos")))]
fn kernel_release() -> Option<String> {
    // SAFETY: `utsname` is plain old data; all zeros is a valid value.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a writable `utsname`.
    if unsafe { libc::uname(&raw mut name) } != 0 {
        return None;
    }
    // SAFETY: `uname` filled `release` with a NUL-terminated string.
    let release = unsafe { std::ffi::CStr::from_ptr(name.release.as_ptr()) };
    let release = release.to_string_lossy().trim().to_owned();
    (!release.is_empty()).then_some(release)
}

#[cfg(windows)]
fn read() -> String {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

    // `GetVersionEx` answers what the executable's manifest claims to
    // support; `RtlGetVersion` answers the running system.
    // SAFETY: `OSVERSIONINFOW` is plain old data; all zeros is a valid value.
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).unwrap_or(0);
    // SAFETY: `info` is a writable `OSVERSIONINFOW` whose size field is set.
    if unsafe { RtlGetVersion(&raw mut info) } != 0 {
        return String::new();
    }
    let version = format!(
        "{}.{}.{}",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    );
    match update_build_revision() {
        Some(revision) => format!("{version}.{revision}"),
        None => version,
    }
}

/// The update build revision (the fourth part of the version Windows shows),
/// which only the registry holds.
#[cfg(windows)]
fn update_build_revision() -> Option<u32> {
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW,
    };

    let key: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "UBR\0".encode_utf16().collect();
    let mut revision = 0_u32;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).ok()?;
    // SAFETY: both names are NUL-terminated; `revision` holds the `size` bytes a DWORD takes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut revision).cast(),
            &raw mut size,
        )
    };
    (status == 0).then_some(revision)
}

#[cfg(not(any(unix, windows)))]
fn read() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_os_version_is_not_empty() {
        let version = super::os_version();
        eprintln!("os version: {version}");
        assert_ne!(version, "", "the version is never empty");
        assert_ne!(version, "unknown", "the system answered nothing");
    }
}
