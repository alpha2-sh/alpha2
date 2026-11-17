//! Core Foundation helpers and framework status codes shared by the sessions.

// Every call into the media frameworks is FFI.
#![allow(unsafe_code)]

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};

/// `kVTInvalidSessionErr`: the system invalidated the session (the app went to the background).
pub(crate) const INVALID_SESSION: i32 = -12_903;
/// `kVTVideoDecoderNotAvailableNowErr`: the hardware decoder is busy elsewhere for now.
pub(crate) const DECODER_NOT_AVAILABLE_NOW: i32 = -12_913;
/// `kVTCouldNotFindVideoDecoderErr`.
pub(crate) const NO_DECODER: i32 = -12_906;
/// `kVTVideoDecoderUnsupportedDataFormatErr`.
pub(crate) const UNSUPPORTED_DATA_FORMAT: i32 = -12_910;
/// `kVTVideoDecoderBadDataErr`.
pub(crate) const BAD_DATA: i32 = -12_909;
/// `kVTVideoDecoderReferenceMissingErr`.
pub(crate) const REFERENCE_MISSING: i32 = -17_694;

/// A four-character code as text, or the number when it is not printable.
pub(crate) fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        bytes.iter().map(|byte| char::from(*byte)).collect()
    } else {
        code.cast_signed().to_string()
    }
}

/// The four-character code `code` spells.
pub(crate) const fn code(code: [u8; 4]) -> u32 {
    u32::from_be_bytes(code)
}

/// A status as text: its four characters when it spells some, else the number.
pub(crate) fn status(status: i32) -> String {
    let bytes = status.to_be_bytes();
    if bytes.iter().all(u8::is_ascii_alphanumeric) {
        format!("'{}'", fourcc(status.cast_unsigned()))
    } else {
        status.to_string()
    }
}

/// A dictionary of `pairs`.
pub(crate) fn dictionary(
    pairs: &[(&CFString, &CFType)],
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys = pairs.iter().map(|(key, _)| *key).collect::<Vec<_>>();
    let values = pairs.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    CFDictionary::from_slices(&keys, &values)
}

/// A boolean as a property value.
pub(crate) fn boolean(value: bool) -> &'static CFType {
    CFBoolean::new(value)
}

/// An integer as a property value.
pub(crate) fn int(value: i64) -> CFRetained<CFNumber> {
    CFNumber::new_i64(value)
}

/// Looks up a framework function by name, for functions newer than the minimum OS: a direct
/// reference would keep the library from loading on older systems.
pub(crate) fn symbol(name: &std::ffi::CStr) -> Option<std::ptr::NonNull<std::ffi::c_void>> {
    unsafe extern "C" {
        fn dlsym(
            handle: *mut std::ffi::c_void,
            symbol: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_void;
    }
    // `RTLD_DEFAULT` on Apple platforms is `(void *)-2`.
    let default = std::ptr::without_provenance_mut((-2_isize).cast_unsigned());
    // SAFETY: `name` is a NUL-terminated string and `RTLD_DEFAULT` searches every loaded image.
    std::ptr::NonNull::new(unsafe { dlsym(default, name.as_ptr()) })
}
