//! Standalone Linux Vulkan hardware-decode probe, run by the CLI to check a
//! device's decode path without an export.

use std::collections::HashSet;
use std::ffi::CString;
use std::path::Path;

use alpha2_ffmpeg_bridge::{VulkanFrameInfo, VulkanFrameLease, pixel_format_name};
use alpha2_media::DecodeError;
use alpha2_native_gpu::probe_vulkan_submission;
use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avutil::{AVFrame, AVHWDeviceContext};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use crate::convert::adapter_error;
use crate::decode::{open_input, probe_context};

/// Aggregate result from a bounded Vulkan hardware decode and surface probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanDecodeProbe {
    /// `FFmpeg` decoder implementation.
    pub decoder: String,
    /// Selected opaque hardware pixel format.
    pub hardware_format: String,
    /// Actual decoded surface layout.
    pub software_format: String,
    /// Display size reported by the decoded frames.
    pub display_size: (i32, i32),
    /// Allocated surface extent, padded to whole macroblocks by the codec.
    pub coded_size: (i32, i32),
    /// Number of hardware frames received before the bound.
    pub hardware_frames: u64,
    /// Number of distinct `VkImage` identities observed.
    pub unique_images: usize,
    /// Frames whose primary image had already appeared, proving pool reuse.
    pub reused_images: u64,
    /// Whether every decoded image included sampled-image usage.
    pub all_images_sampled: bool,
    /// Whether every image exposed a ready timeline semaphore.
    pub all_images_synchronized: bool,
    /// Number of hardware frames received after seeking back to the start.
    pub seek_frames: u64,
    /// Device identity shared by all observed surfaces.
    pub device: u64,
    /// Physical-device identity shared by all observed surfaces.
    pub physical_device: u64,
    /// Maximum number of Vulkan images backing one decoded frame.
    pub max_plane_images: u32,
    /// GPU-only synchronization/layout submissions completed by the probe.
    pub interop_submissions: u64,
    /// CPU source-plane copies performed by this probe.
    pub source_cpu_plane_copy_bytes: u64,
    /// CPU-to-GPU uploads performed by this probe.
    pub source_upload_bytes: u64,
    /// GPU-to-CPU downloads performed by this probe.
    pub source_download_bytes: u64,
}

/// Decodes Vulkan frames without mapping, downloading, or uploading their planes.
///
/// The probe verifies the codec hardware configuration, receives actual
/// `AV_PIX_FMT_VULKAN` frames, reads their image/layout/timeline state through
/// the platform shim, observes pool reuse, and repeats decode after a seek.
/// It opens its own device and decoder rather than going through
/// [`crate::NativeExportDecoder`], so it observes `FFmpeg`'s surfaces directly.
///
/// # Errors
/// Returns an error if Vulkan decode is unavailable or any received frame does
/// not satisfy the resident-surface contract.
#[allow(clippy::too_many_lines)]
pub fn probe_vulkan_decode(
    path: impl AsRef<Path>,
    frame_limit: usize,
) -> Result<VulkanDecodeProbe, DecodeError> {
    if frame_limit < 2 {
        return Err(DecodeError::Unsupported(
            "Vulkan decode probe requires at least two frames".into(),
        ));
    }
    let mut input = open_input(path.as_ref())?;
    let selected = probe_context(&input, path.as_ref())?
        .selected_video
        .ok_or_else(|| DecodeError::Unsupported("source has no video stream".into()))?;
    input.discard_all_but(selected);
    let parameters = input
        .streams()
        .get(selected)
        .ok_or(DecodeError::OutOfRange)?
        .codecpar();
    let preferred = AVCodec::find_decoder(parameters.codec_id)
        .ok_or_else(|| DecodeError::Unsupported("selected video codec has no decoder".into()))?;
    // The preferred decoder may be a software one (libdav1d for AV1); the
    // Vulkan configuration lives on `FFmpeg`'s own decoder for the codec.
    let (codec, config) = crate::decode::registered_video_decoders(parameters.codec_id)
        .find_map(|codec| {
            let config = (0..)
                .map_while(|index| codec.hw_config(index))
                .find(|config| {
                    config.device_type == ffi::AV_HWDEVICE_TYPE_VULKAN
                        && config.pix_fmt == ffi::AV_PIX_FMT_VULKAN
                        && crate::decode::has_hw_config_method(
                            config.methods,
                            ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX,
                        )
                })?;
            Some((codec, config))
        })
        .ok_or_else(|| {
            DecodeError::Unsupported(format!(
                "decoder {} has no Vulkan HW_DEVICE_CTX configuration",
                preferred.name().to_string_lossy()
            ))
        })?;
    debug_assert_eq!(config.pix_fmt, ffi::AV_PIX_FMT_VULKAN);

    // The device an export decodes on, as the host chooses it, passed by its index: `FFmpeg`
    // reads a name that starts with a digit as an index, so passing `ALPHA2_VULKAN_DEVICE` on
    // unchanged would pick another device or none.
    let requested_device = CString::new(
        alpha2_native_gpu::shared_device_index(
            std::env::var("ALPHA2_VULKAN_DEVICE").ok().as_deref(),
        )
        .map_err(|error| DecodeError::Unsupported(error.to_string()))?
        .to_string(),
    )
    .map_err(|_| DecodeError::Unsupported("a device index held a NUL byte".into()))?;
    let device = AVHWDeviceContext::create(
        ffi::AV_HWDEVICE_TYPE_VULKAN,
        Some(&requested_device),
        None,
        0,
    )
    .map_err(adapter_error)?;
    let mut decoder = AVCodecContext::new(&codec);
    decoder.apply_codecpar(&parameters).map_err(adapter_error)?;
    drop(parameters);
    decoder.set_pkt_timebase(input.streams()[selected].time_base);
    decoder.set_get_format(Some(alpha2_ffmpeg_bridge::vulkan_get_format_callback()));
    decoder.set_hw_device_ctx(device);
    decoder.open(None).map_err(adapter_error)?;

    let mut demux_eof = false;
    let mut flushed = false;
    let mut images = HashSet::new();
    let mut reuse = 0_u64;
    let mut first_info = None;
    let mut all_sampled = true;
    let mut all_synchronized = true;
    let mut max_plane_images = 0;
    let mut frames = 0_u64;
    while frames < frame_limit as u64 {
        let Some(frame) = decode_next_raw(
            &mut input,
            &mut decoder,
            selected,
            &mut demux_eof,
            &mut flushed,
        )?
        else {
            break;
        };
        let lease = VulkanFrameLease::new(frame);
        let info = validate_frame(&lease, first_info.as_ref())?;
        probe_vulkan_submission(&lease).map_err(adapter_error)?;
        let primary = info
            .images
            .iter()
            .copied()
            .find(|image| *image != 0)
            .ok_or_else(|| DecodeError::Adapter("Vulkan frame has no image".into()))?;
        if !images.insert(primary) {
            reuse += 1;
        }
        all_sampled &= info.sampled;
        all_synchronized &= synchronized(&info);
        max_plane_images = max_plane_images.max(info.plane_count);
        first_info.get_or_insert(info);
        frames += 1;
    }
    let first_info = first_info.ok_or_else(|| {
        DecodeError::Unsupported("decoder returned no Vulkan hardware frame".into())
    })?;
    if !all_sampled {
        return Err(DecodeError::Unsupported(
            "decoded Vulkan image is not sampleable".into(),
        ));
    }
    if !all_synchronized {
        return Err(DecodeError::Adapter(
            "decoded Vulkan image has no ready timeline semaphore".into(),
        ));
    }

    input
        .seek(
            i32::try_from(selected).map_err(|_| DecodeError::OutOfRange)?,
            0,
            ffi::AVSEEK_FLAG_BACKWARD.cast_signed(),
        )
        .map_err(adapter_error)?;
    decoder.flush_buffers();
    demux_eof = false;
    flushed = false;
    let seek_frame = decode_next_raw(
        &mut input,
        &mut decoder,
        selected,
        &mut demux_eof,
        &mut flushed,
    )?
    .ok_or_else(|| DecodeError::Adapter("no Vulkan frame after seek".into()))?;
    let seek_lease = VulkanFrameLease::new(seek_frame);
    validate_frame(&seek_lease, Some(&first_info))?;
    probe_vulkan_submission(&seek_lease).map_err(adapter_error)?;

    Ok(VulkanDecodeProbe {
        decoder: codec.name().to_string_lossy().into_owned(),
        hardware_format: format_name(first_info.hardware_format),
        software_format: format_name(first_info.software_format),
        display_size: (first_info.width, first_info.height),
        coded_size: (first_info.coded_width, first_info.coded_height),
        hardware_frames: frames,
        unique_images: images.len(),
        reused_images: reuse,
        all_images_sampled: all_sampled,
        all_images_synchronized: all_synchronized,
        seek_frames: 1,
        device: first_info.device,
        physical_device: first_info.physical_device,
        max_plane_images,
        interop_submissions: frames + 1,
        source_cpu_plane_copy_bytes: 0,
        source_upload_bytes: 0,
        source_download_bytes: 0,
    })
}

fn decode_next_raw(
    input: &mut alpha2_ffmpeg_bridge::DemuxInput,
    decoder: &mut AVCodecContext,
    stream_index: usize,
    demux_eof: &mut bool,
    flushed: &mut bool,
) -> Result<Option<AVFrame>, DecodeError> {
    loop {
        match decoder.receive_frame() {
            Ok(frame) => return Ok(Some(frame)),
            Err(RsmpegError::DecoderDrainError) => {}
            Err(RsmpegError::DecoderFlushedError) => return Ok(None),
            Err(error) => return Err(adapter_error(error)),
        }
        if *demux_eof {
            if !*flushed {
                decoder.send_packet(None).map_err(adapter_error)?;
                *flushed = true;
                continue;
            }
            return Ok(None);
        }
        let Some(packet) = input.read_packet().map_err(adapter_error)? else {
            *demux_eof = true;
            continue;
        };
        if usize::try_from(packet.stream_index).ok() == Some(stream_index) {
            decoder.send_packet(Some(&packet)).map_err(adapter_error)?;
        }
    }
}

fn validate_frame(
    frame: &VulkanFrameLease,
    expected: Option<&VulkanFrameInfo>,
) -> Result<VulkanFrameInfo, DecodeError> {
    let info = frame.info().map_err(adapter_error)?;
    if info.hardware_format != ffi::AV_PIX_FMT_VULKAN {
        return Err(DecodeError::Adapter(format!(
            "decoder returned {} instead of vulkan",
            format_name(info.hardware_format)
        )));
    }
    if let Some(expected) = expected {
        if info.device != expected.device || info.physical_device != expected.physical_device {
            return Err(DecodeError::Adapter(
                "Vulkan decoder changed device while probing".into(),
            ));
        }
        if info.software_format != expected.software_format {
            return Err(DecodeError::Adapter(
                "Vulkan decoder changed surface format while probing".into(),
            ));
        }
        if info.coded_width != expected.coded_width || info.coded_height != expected.coded_height {
            return Err(DecodeError::Adapter(
                "Vulkan decoder changed surface extent while probing".into(),
            ));
        }
    }
    // Presentation normalizes against the allocated extent, so a surface smaller than the picture
    // it carries would rescale every sample.
    if info.coded_width < info.width || info.coded_height < info.height {
        return Err(DecodeError::Adapter(
            "Vulkan surface is smaller than the frame it carries".into(),
        ));
    }
    Ok(info)
}

fn synchronized(info: &VulkanFrameInfo) -> bool {
    info.images
        .iter()
        .enumerate()
        .filter(|(_, image)| **image != 0)
        .all(|(index, _)| info.semaphores[index] != 0)
}

fn format_name(format: ffi::AVPixelFormat) -> String {
    pixel_format_name(format).unwrap_or_else(|| format!("unknown({format})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronization_ignores_unused_plane_slots() {
        let info = VulkanFrameInfo {
            hardware_format: ffi::AV_PIX_FMT_VULKAN,
            software_format: ffi::AV_PIX_FMT_NV12,
            width: 16,
            height: 16,
            coded_width: 16,
            coded_height: 16,
            plane_count: 1,
            sampled: true,
            instance: 1,
            physical_device: 2,
            device: 3,
            images: [4, 0, 0, 0, 0, 0, 0, 0],
            formats: [1; 8],
            layouts: [0; 8],
            access: [0; 8],
            semaphores: [5, 0, 0, 0, 0, 0, 0, 0],
            semaphore_values: [0; 8],
            queue_families: [0; 8],
        };
        assert!(synchronized(&info));
    }
}
