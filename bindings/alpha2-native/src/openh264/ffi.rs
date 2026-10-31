//! The `OpenH264` encoder API as Cisco's 2.7.0 binary exports it; the only
//! unsafe code in this crate.
//!
//! Declarations follow `codec_api.h`, `codec_app_def.h` and `codec_def.h` of v2.7.0. Everything is
//! called through a library loaded at run time. The structs are plain C data (`int`-sized enums,
//! one-byte `bool`s) whose sizes the tests check. [`Api`] and [`Encoder`] expose safe methods that
//! uphold each C function's requirements.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::path::Path;
use std::ptr;

use rsmpeg::avutil::AVFrame;
use rsmpeg::ffi;

/// `MAX_SPATIAL_LAYER_NUM`.
const MAX_SPATIAL_LAYER_NUM: usize = 4;
/// `MAX_LAYER_NUM_OF_FRAME`.
const MAX_LAYER_NUM_OF_FRAME: usize = 128;
/// `MAX_SLICES_NUM_TMP`: `(MAX_NAL_UNITS_IN_LAYER - SAVED_NALUNIT_NUM_TMP) / 3`.
const MAX_SLICES_NUM_TMP: usize = (128 - (4 * 4 + 1 + 4)) / 3;

/// `EVideoFormatType::videoFormatI420`.
const VIDEO_FORMAT_I420: c_int = 23;
/// `EVideoFrameType::videoFrameTypeIDR`.
pub(crate) const FRAME_TYPE_IDR: c_int = 1;
/// `EVideoFrameType::videoFrameTypeSkip`.
pub(crate) const FRAME_TYPE_SKIP: c_int = 4;
/// `CM_RETURN::cmResultSuccess`.
const RESULT_SUCCESS: c_int = 0;
/// `ENCODER_OPTION_TRACE_LEVEL`.
const ENCODER_OPTION_TRACE_LEVEL: c_int = 25;
/// `ENCODER_OPTION_TRACE_CALLBACK`.
const ENCODER_OPTION_TRACE_CALLBACK: c_int = 26;
/// `WELS_LOG_WARNING`: as a trace level, errors and warnings reach the callback.
const WELS_LOG_WARNING: c_int = 1 << 1;
/// `WELS_LOG_ERROR`.
const WELS_LOG_ERROR: c_int = 1 << 0;

/// `OpenH264Version`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Version {
    pub(crate) major: c_uint,
    pub(crate) minor: c_uint,
    pub(crate) revision: c_uint,
    pub(crate) reserved: c_uint,
}

/// `SSliceArgument`.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::struct_field_names)]
pub(crate) struct SliceArgument {
    /// `SliceModeEnum`.
    pub(crate) slice_mode: c_int,
    pub(crate) slice_num: c_uint,
    pub(crate) slice_mb_num: [c_uint; MAX_SLICES_NUM_TMP],
    pub(crate) slice_size_constraint: c_uint,
}

/// `SSpatialLayerConfig`.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SpatialLayerConfig {
    pub(crate) video_width: c_int,
    pub(crate) video_height: c_int,
    pub(crate) frame_rate: f32,
    pub(crate) spatial_bitrate: c_int,
    pub(crate) max_spatial_bitrate: c_int,
    /// `EProfileIdc`.
    pub(crate) profile_idc: c_int,
    /// `ELevelIdc`.
    pub(crate) level_idc: c_int,
    /// The QP `RC_OFF_MODE` encodes at.
    pub(crate) d_layer_qp: c_int,
    pub(crate) slice_argument: SliceArgument,
    pub(crate) video_signal_type_present: bool,
    pub(crate) video_format: u8,
    pub(crate) full_range: bool,
    pub(crate) color_description_present: bool,
    pub(crate) color_primaries: u8,
    pub(crate) transfer_characteristics: u8,
    pub(crate) color_matrix: u8,
    pub(crate) aspect_ratio_present: bool,
    /// `ESampleAspectRatio`.
    pub(crate) aspect_ratio: c_int,
    pub(crate) aspect_ratio_ext_width: u16,
    pub(crate) aspect_ratio_ext_height: u16,
}

/// `SEncParamExt`.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct EncParamExt {
    /// `EUsageType`.
    pub(crate) usage_type: c_int,
    pub(crate) pic_width: c_int,
    pub(crate) pic_height: c_int,
    pub(crate) target_bitrate: c_int,
    /// `RC_MODES`.
    pub(crate) rc_mode: c_int,
    pub(crate) max_frame_rate: f32,
    pub(crate) temporal_layer_num: c_int,
    pub(crate) spatial_layer_num: c_int,
    pub(crate) spatial_layers: [SpatialLayerConfig; MAX_SPATIAL_LAYER_NUM],
    /// `ECOMPLEXITY_MODE`.
    pub(crate) complexity_mode: c_int,
    pub(crate) intra_period: c_uint,
    pub(crate) num_ref_frame: c_int,
    /// `EParameterSetStrategy`.
    pub(crate) sps_pps_id_strategy: c_int,
    pub(crate) prefix_nal_adding_ctrl: bool,
    pub(crate) enable_ssei: bool,
    pub(crate) simulcast_avc: bool,
    pub(crate) padding_flag: c_int,
    pub(crate) entropy_coding_mode_flag: c_int,
    pub(crate) enable_frame_skip: bool,
    pub(crate) max_bitrate: c_int,
    pub(crate) max_qp: c_int,
    pub(crate) min_qp: c_int,
    pub(crate) max_nal_size: c_uint,
    pub(crate) enable_long_term_reference: bool,
    pub(crate) ltr_ref_num: c_int,
    pub(crate) ltr_mark_period: c_uint,
    pub(crate) multiple_thread_idc: u16,
    pub(crate) use_load_balancing: bool,
    pub(crate) loop_filter_disable_idc: c_int,
    pub(crate) loop_filter_alpha_c0_offset: c_int,
    pub(crate) loop_filter_beta_offset: c_int,
    pub(crate) enable_denoise: bool,
    pub(crate) enable_background_detection: bool,
    pub(crate) enable_adaptive_quant: bool,
    pub(crate) enable_frame_cropping_flag: bool,
    pub(crate) enable_scene_change_detect: bool,
    pub(crate) is_lossless_link: bool,
    pub(crate) fix_rc_over_shoot: bool,
    pub(crate) idr_bitrate_ratio: c_int,
    pub(crate) psnr_y: bool,
    pub(crate) psnr_u: bool,
    pub(crate) psnr_v: bool,
}

impl EncParamExt {
    /// All zero, which `GetDefaultParams` then fills in.
    pub(crate) const fn zeroed() -> Self {
        let slice = SliceArgument {
            slice_mode: 0,
            slice_num: 0,
            slice_mb_num: [0; MAX_SLICES_NUM_TMP],
            slice_size_constraint: 0,
        };
        let layer = SpatialLayerConfig {
            video_width: 0,
            video_height: 0,
            frame_rate: 0.0,
            spatial_bitrate: 0,
            max_spatial_bitrate: 0,
            profile_idc: 0,
            level_idc: 0,
            d_layer_qp: 0,
            slice_argument: slice,
            video_signal_type_present: false,
            video_format: 0,
            full_range: false,
            color_description_present: false,
            color_primaries: 0,
            transfer_characteristics: 0,
            color_matrix: 0,
            aspect_ratio_present: false,
            aspect_ratio: 0,
            aspect_ratio_ext_width: 0,
            aspect_ratio_ext_height: 0,
        };
        Self {
            usage_type: 0,
            pic_width: 0,
            pic_height: 0,
            target_bitrate: 0,
            rc_mode: 0,
            max_frame_rate: 0.0,
            temporal_layer_num: 0,
            spatial_layer_num: 0,
            spatial_layers: [layer; MAX_SPATIAL_LAYER_NUM],
            complexity_mode: 0,
            intra_period: 0,
            num_ref_frame: 0,
            sps_pps_id_strategy: 0,
            prefix_nal_adding_ctrl: false,
            enable_ssei: false,
            simulcast_avc: false,
            padding_flag: 0,
            entropy_coding_mode_flag: 0,
            enable_frame_skip: false,
            max_bitrate: 0,
            max_qp: 0,
            min_qp: 0,
            max_nal_size: 0,
            enable_long_term_reference: false,
            ltr_ref_num: 0,
            ltr_mark_period: 0,
            multiple_thread_idc: 0,
            use_load_balancing: false,
            loop_filter_disable_idc: 0,
            loop_filter_alpha_c0_offset: 0,
            loop_filter_beta_offset: 0,
            enable_denoise: false,
            enable_background_detection: false,
            enable_adaptive_quant: false,
            enable_frame_cropping_flag: false,
            enable_scene_change_detect: false,
            is_lossless_link: false,
            fix_rc_over_shoot: false,
            idr_bitrate_ratio: 0,
            psnr_y: false,
            psnr_u: false,
            psnr_v: false,
        }
    }
}

/// `SLayerBSInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
struct LayerBsInfo {
    temporal_id: u8,
    spatial_id: u8,
    quality_id: u8,
    frame_type: c_int,
    layer_type: u8,
    sub_seq_id: c_int,
    nal_count: c_int,
    nal_length_in_byte: *mut c_int,
    bs_buf: *mut u8,
    psnr: [f32; 3],
}

/// `SFrameBSInfo`.
#[repr(C)]
struct FrameBsInfo {
    layer_num: c_int,
    layer_info: [LayerBsInfo; MAX_LAYER_NUM_OF_FRAME],
    frame_type: c_int,
    frame_size_in_bytes: c_int,
    time_stamp: i64,
}

impl FrameBsInfo {
    fn zeroed() -> Box<Self> {
        let layer = LayerBsInfo {
            temporal_id: 0,
            spatial_id: 0,
            quality_id: 0,
            frame_type: 0,
            layer_type: 0,
            sub_seq_id: 0,
            nal_count: 0,
            nal_length_in_byte: ptr::null_mut(),
            bs_buf: ptr::null_mut(),
            psnr: [0.0; 3],
        };
        Box::new(Self {
            layer_num: 0,
            layer_info: [layer; MAX_LAYER_NUM_OF_FRAME],
            frame_type: 0,
            frame_size_in_bytes: 0,
            time_stamp: 0,
        })
    }
}

/// `SSourcePicture`.
#[repr(C)]
struct SourcePicture {
    color_format: c_int,
    stride: [c_int; 4],
    data: [*mut u8; 4],
    pic_width: c_int,
    pic_height: c_int,
    time_stamp: i64,
    psnr_y: bool,
    psnr_u: bool,
    psnr_v: bool,
}

/// `ISVCEncoderVtbl`, the encoder's virtual functions in declaration order.
#[repr(C)]
struct EncoderVtbl {
    initialize: unsafe extern "C" fn(*mut EncoderObject, *const c_void) -> c_int,
    initialize_ext: unsafe extern "C" fn(*mut EncoderObject, *const EncParamExt) -> c_int,
    get_default_params: unsafe extern "C" fn(*mut EncoderObject, *mut EncParamExt) -> c_int,
    uninitialize: unsafe extern "C" fn(*mut EncoderObject) -> c_int,
    encode_frame:
        unsafe extern "C" fn(*mut EncoderObject, *const SourcePicture, *mut FrameBsInfo) -> c_int,
    encode_parameter_sets: unsafe extern "C" fn(*mut EncoderObject, *mut FrameBsInfo) -> c_int,
    // The C++ method's second argument (`iLayerId = -1`) is passed too, as its
    // ABI expects; never called.
    force_intra_frame: unsafe extern "C" fn(*mut EncoderObject, bool, c_int) -> c_int,
    set_option: unsafe extern "C" fn(*mut EncoderObject, c_int, *mut c_void) -> c_int,
    get_option: unsafe extern "C" fn(*mut EncoderObject, c_int, *mut c_void) -> c_int,
}

/// An encoder object: its first word is the virtual function table.
#[repr(C)]
struct EncoderObject {
    vtbl: *const EncoderVtbl,
}

type CreateEncoder = unsafe extern "C" fn(*mut *mut EncoderObject) -> c_int;
type DestroyEncoder = unsafe extern "C" fn(*mut EncoderObject);
type GetCodecVersionEx = unsafe extern "C" fn(*mut Version);
type TraceCallback = unsafe extern "C" fn(*mut c_void, c_int, *const c_char);

/// A loaded `OpenH264` library and the three entry points alpha2 calls.
pub(crate) struct Api {
    create: CreateEncoder,
    destroy: DestroyEncoder,
    version: Version,
    /// Kept loaded while any encoder from it exists; each [`Encoder`] holds the [`Api`] by `Arc`.
    _library: libloading::Library,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Api")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Why a library did not load as an `OpenH264` encoder.
#[derive(Debug)]
pub(crate) enum LoadError {
    /// The loader refused it (missing, wrong architecture, or a missing dependency such as
    /// `libstdc++.so.6`), with the loader's message.
    Open(String),
    /// It loaded but lacks an entry point, so it is not `OpenH264`.
    Symbol(&'static str, String),
}

impl Api {
    /// Loads `path` and resolves the encoder entry points and its version.
    ///
    /// Loading runs the library's initializers, so only an already accepted library (by hash or by
    /// explicit user choice) may be passed.
    pub(crate) fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: loading runs the library's initializers. Only Cisco's hash-verified binary or one
        // the user explicitly chose as OpenH264 is passed.
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|error| LoadError::Open(error.to_string()))?;
        // SAFETY: each symbol uses the signature `codec_api.h` declares, and the pointers are used
        // only while `library` stays loaded inside the `Api`.
        let (create, destroy, version) = unsafe {
            let create = *library
                .get::<CreateEncoder>(b"WelsCreateSVCEncoder\0")
                .map_err(|error| LoadError::Symbol("WelsCreateSVCEncoder", error.to_string()))?;
            let destroy = *library
                .get::<DestroyEncoder>(b"WelsDestroySVCEncoder\0")
                .map_err(|error| LoadError::Symbol("WelsDestroySVCEncoder", error.to_string()))?;
            let version_ex = *library
                .get::<GetCodecVersionEx>(b"WelsGetCodecVersionEx\0")
                .map_err(|error| LoadError::Symbol("WelsGetCodecVersionEx", error.to_string()))?;
            let mut version = Version::default();
            version_ex(&raw mut version);
            (create, destroy, version)
        };
        Ok(Self {
            create,
            destroy,
            version,
            _library: library,
        })
    }

    /// The version the library reports.
    pub(crate) const fn version(&self) -> Version {
        self.version
    }
}

/// One `OpenH264` encoder, destroyed when dropped.
pub(crate) struct Encoder {
    api: std::sync::Arc<Api>,
    object: *mut EncoderObject,
    initialized: bool,
    /// Reused for every call: it is 7 KiB, and `OpenH264` only writes pointers to its own buffers.
    output: Box<FrameBsInfo>,
}

// SAFETY: an OpenH264 encoder has no thread affinity; `&mut self` on every call
// ensures one thread at a time.
unsafe impl Send for Encoder {}

/// One encoded access unit: Annex B NAL units with four-byte start codes.
pub(crate) struct EncodedFrame {
    /// `EVideoFrameType`.
    pub(crate) frame_type: c_int,
    /// Every layer's bytes, in order.
    pub(crate) annex_b: Vec<u8>,
}

impl Encoder {
    /// Creates an encoder, sending its log to `alpha2_os::diagnostic!`.
    pub(crate) fn create(api: std::sync::Arc<Api>) -> Result<Self, String> {
        let mut object = ptr::null_mut();
        // SAFETY: `WelsCreateSVCEncoder` writes a new encoder or leaves the pointer null.
        let status = unsafe { (api.create)(&raw mut object) };
        if status != RESULT_SUCCESS || object.is_null() {
            return Err(format!("WelsCreateSVCEncoder failed ({status})"));
        }
        let mut encoder = Self {
            api,
            object,
            initialized: false,
            output: FrameBsInfo::zeroed(),
        };
        // OpenH264 prints to stderr itself unless given a callback; only
        // errors and warnings are wanted.
        let mut callback: TraceCallback = trace;
        encoder.set_option(
            ENCODER_OPTION_TRACE_CALLBACK,
            (&raw mut callback).cast::<c_void>(),
        )?;
        let mut level = WELS_LOG_WARNING;
        encoder.set_option(
            ENCODER_OPTION_TRACE_LEVEL,
            (&raw mut level).cast::<c_void>(),
        )?;
        Ok(encoder)
    }

    fn vtbl(&self) -> &EncoderVtbl {
        // SAFETY: `object` is a live encoder from `WelsCreateSVCEncoder`, whose first word is its
        // vtable, valid for the encoder's lifetime.
        unsafe { &*(*self.object).vtbl }
    }

    fn set_option(&mut self, option: c_int, value: *mut c_void) -> Result<(), String> {
        // SAFETY: a live encoder; `value` points at the type `option` takes, alive for the call.
        let status = unsafe { (self.vtbl().set_option)(self.object, option, value) };
        if status == RESULT_SUCCESS {
            Ok(())
        } else {
            Err(format!("SetOption({option}) failed ({status})"))
        }
    }

    /// `OpenH264`'s defaults for every parameter.
    pub(crate) fn default_params(&mut self) -> Result<EncParamExt, String> {
        let mut params = EncParamExt::zeroed();
        // SAFETY: a live encoder; `params` is a writable `SEncParamExt`.
        let status = unsafe { (self.vtbl().get_default_params)(self.object, &raw mut params) };
        if status == RESULT_SUCCESS {
            Ok(params)
        } else {
            Err(format!("GetDefaultParams failed ({status})"))
        }
    }

    /// Initializes the encoder with `params`.
    pub(crate) fn initialize(&mut self, params: &EncParamExt) -> Result<(), String> {
        // SAFETY: a live encoder; `params` is read for the call only.
        let status = unsafe { (self.vtbl().initialize_ext)(self.object, params) };
        if status == RESULT_SUCCESS {
            self.initialized = true;
            Ok(())
        } else {
            Err(format!(
                "InitializeExt refused the parameters ({status}; OpenH264 logs why above)"
            ))
        }
    }

    /// The sequence and picture parameter sets, without encoding a picture.
    pub(crate) fn parameter_sets(&mut self) -> Result<Vec<u8>, String> {
        // SAFETY: an initialized encoder writes the parameter sets into its own buffers
        // and points `output` at them.
        let status =
            unsafe { (self.vtbl().encode_parameter_sets)(self.object, &raw mut *self.output) };
        if status != RESULT_SUCCESS {
            return Err(format!("EncodeParameterSets failed ({status})"));
        }
        self.collect_output()
    }

    /// Encodes one 8-bit 4:2:0 frame of the initialized size.
    pub(crate) fn encode(
        &mut self,
        frame: &AVFrame,
        width: c_int,
        height: c_int,
    ) -> Result<EncodedFrame, String> {
        if !self.initialized {
            return Err("the encoder is not initialized".into());
        }
        if frame.format != ffi::AV_PIX_FMT_YUV420P || frame.width != width || frame.height != height
        {
            return Err(format!(
                "OpenH264 takes {width}x{height} YUV420P frames; this one is {}x{} format {}",
                frame.width, frame.height, frame.format
            ));
        }
        let chroma_width = (width + 1) / 2;
        for (plane, minimum) in [(0, width), (1, chroma_width), (2, chroma_width)] {
            if frame.data[plane].is_null() || frame.linesize[plane] < minimum {
                return Err(format!(
                    "plane {plane} of the frame is missing or narrower than its width"
                ));
            }
        }
        let picture = SourcePicture {
            color_format: VIDEO_FORMAT_I420,
            stride: [frame.linesize[0], frame.linesize[1], frame.linesize[2], 0],
            data: [frame.data[0], frame.data[1], frame.data[2], ptr::null_mut()],
            pic_width: width,
            pic_height: height,
            time_stamp: 0,
            psnr_y: false,
            psnr_u: false,
            psnr_v: false,
        };
        // SAFETY: an initialized encoder of this size, and `frame` is a live YUV420P frame
        // of `width`x`height` (checked above) whose planes have `linesize` bytes per row
        // for every row. OpenH264 reads only visible samples, copies the picture, and keeps
        // no pointer after the call.
        let status = unsafe {
            (self.vtbl().encode_frame)(self.object, &raw const picture, &raw mut *self.output)
        };
        if status != RESULT_SUCCESS {
            return Err(format!("EncodeFrame failed ({status})"));
        }
        let frame_type = self.output.frame_type;
        let annex_b = if frame_type == FRAME_TYPE_SKIP {
            Vec::new()
        } else {
            self.collect_output()?
        };
        Ok(EncodedFrame {
            frame_type,
            annex_b,
        })
    }

    /// Copies every layer's NAL units out of the encoder's buffers.
    fn collect_output(&self) -> Result<Vec<u8>, String> {
        let output = &*self.output;
        let layers = usize::try_from(output.layer_num)
            .ok()
            .filter(|layers| *layers <= MAX_LAYER_NUM_OF_FRAME)
            .ok_or("the encoder reported an impossible layer count")?;
        let mut bytes = Vec::new();
        for layer in &output.layer_info[..layers] {
            let nals = usize::try_from(layer.nal_count)
                .map_err(|_| "the encoder reported a negative NAL count")?;
            if nals == 0 {
                continue;
            }
            if layer.nal_length_in_byte.is_null() || layer.bs_buf.is_null() {
                return Err("the encoder reported NAL units without a buffer".into());
            }
            // SAFETY: after a successful call each layer points at `nal_count` lengths in the
            // encoder's memory, valid until the next call, which `&self` rules out.
            let lengths = unsafe { std::slice::from_raw_parts(layer.nal_length_in_byte, nals) };
            let mut size = 0_usize;
            for length in lengths {
                size = size
                    .checked_add(
                        usize::try_from(*length)
                            .map_err(|_| "the encoder reported a negative NAL length")?,
                    )
                    .ok_or("the encoder reported an impossible layer size")?;
            }
            // SAFETY: as above; the layer's NAL units are `size` contiguous bytes at `bs_buf`.
            bytes.extend_from_slice(unsafe { std::slice::from_raw_parts(layer.bs_buf, size) });
        }
        Ok(bytes)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: a live encoder, uninitialized at most once and destroyed exactly once, here.
        unsafe {
            if self.initialized {
                (self.vtbl().uninitialize)(self.object);
            }
            (self.api.destroy)(self.object);
        }
    }
}

/// Where `OpenH264`'s own log lines go.
unsafe extern "C" fn trace(_context: *mut c_void, level: c_int, message: *const c_char) {
    if message.is_null() {
        return;
    }
    // SAFETY: OpenH264 passes a NUL-terminated string valid for the call.
    let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    let severity = if level & WELS_LOG_ERROR != 0 {
        "error"
    } else {
        "warning"
    };
    alpha2_os::diagnostic!("alpha2 native: OpenH264 {severity}: {}", message.trim_end());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Struct sizes and offsets from v2.7.0's `codec_app_def.h` on every shipped 64-bit target.
    #[test]
    fn the_structs_are_laid_out_as_the_pinned_header_declares_them() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<SliceArgument>(), 152);
        assert_eq!(size_of::<SpatialLayerConfig>(), 200);
        assert_eq!(offset_of!(SpatialLayerConfig, d_layer_qp), 28);
        assert_eq!(offset_of!(SpatialLayerConfig, slice_argument), 32);
        assert_eq!(
            offset_of!(SpatialLayerConfig, video_signal_type_present),
            184
        );
        assert_eq!(offset_of!(SpatialLayerConfig, color_matrix), 190);
        assert_eq!(offset_of!(SpatialLayerConfig, aspect_ratio_present), 191);
        assert_eq!(offset_of!(SpatialLayerConfig, aspect_ratio), 192);
        assert_eq!(offset_of!(SpatialLayerConfig, aspect_ratio_ext_height), 198);
        assert_eq!(size_of::<EncParamExt>(), 924);
        assert_eq!(offset_of!(EncParamExt, spatial_layers), 32);
        assert_eq!(offset_of!(EncParamExt, complexity_mode), 832);
        assert_eq!(offset_of!(EncParamExt, sps_pps_id_strategy), 844);
        assert_eq!(offset_of!(EncParamExt, prefix_nal_adding_ctrl), 848);
        assert_eq!(offset_of!(EncParamExt, padding_flag), 852);
        assert_eq!(offset_of!(EncParamExt, entropy_coding_mode_flag), 856);
        assert_eq!(offset_of!(EncParamExt, enable_frame_skip), 860);
        assert_eq!(offset_of!(EncParamExt, max_bitrate), 864);
        assert_eq!(offset_of!(EncParamExt, max_nal_size), 876);
        assert_eq!(offset_of!(EncParamExt, enable_long_term_reference), 880);
        assert_eq!(offset_of!(EncParamExt, ltr_ref_num), 884);
        assert_eq!(offset_of!(EncParamExt, multiple_thread_idc), 892);
        assert_eq!(offset_of!(EncParamExt, use_load_balancing), 894);
        assert_eq!(offset_of!(EncParamExt, loop_filter_disable_idc), 896);
        assert_eq!(offset_of!(EncParamExt, enable_denoise), 908);
        assert_eq!(offset_of!(EncParamExt, fix_rc_over_shoot), 914);
        assert_eq!(offset_of!(EncParamExt, idr_bitrate_ratio), 916);
        assert_eq!(offset_of!(EncParamExt, psnr_v), 922);
        assert_eq!(size_of::<LayerBsInfo>(), 56);
        assert_eq!(offset_of!(LayerBsInfo, frame_type), 4);
        assert_eq!(offset_of!(LayerBsInfo, layer_type), 8);
        assert_eq!(offset_of!(LayerBsInfo, sub_seq_id), 12);
        assert_eq!(offset_of!(LayerBsInfo, nal_length_in_byte), 24);
        assert_eq!(offset_of!(LayerBsInfo, bs_buf), 32);
        assert_eq!(offset_of!(LayerBsInfo, psnr), 40);
        assert_eq!(size_of::<FrameBsInfo>(), 7192);
        assert_eq!(offset_of!(FrameBsInfo, layer_info), 8);
        assert_eq!(offset_of!(FrameBsInfo, frame_type), 7176);
        assert_eq!(offset_of!(FrameBsInfo, time_stamp), 7184);
        assert_eq!(size_of::<SourcePicture>(), 80);
        assert_eq!(offset_of!(SourcePicture, stride), 4);
        assert_eq!(offset_of!(SourcePicture, data), 24);
        assert_eq!(offset_of!(SourcePicture, pic_width), 56);
        assert_eq!(offset_of!(SourcePicture, time_stamp), 64);
        assert_eq!(offset_of!(SourcePicture, psnr_v), 74);
        assert_eq!(size_of::<Version>(), 16);
    }
}
