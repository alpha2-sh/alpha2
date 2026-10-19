//! `alpha2-host probe-gpu-encode [width] [height] [slice-path]` on Windows:
//! what every DX12 adapter's D3D12 Video Encode does, one row per adapter.
//!
//! Each adapter is opened, its device shared with `FFmpeg`
//! ([`SharedD3D12Context::from_gpu`]), and asked what it encodes for every
//! codec (`encode_support`). Each codec it reports is held to one encoded
//! frame at the probe's size, in a child process
//! ([`crate::d3d12va_probe::probe_child`]): a driver that crashes inside
//! its encode submission (an Intel Iris Xe on driver 30.0.101.2079 does,
//! about once in a few thousand frames) takes that child with it, and every
//! other row is still reported. A row that stops names the step it stopped
//! at and the reason, with the `HRESULT` or `FFmpeg` error where there is
//! one.
//!
//! With a slice path, the first adapter, in the host's order, whose
//! capability the GPU-resident path would offer also renders eight frames
//! through the hardware-encode contract into an MP4, as on Linux and macOS;
//! without such an adapter the report says why for each one.

use std::time::Duration;

use alpha2_gpu::GpuContext;
use alpha2_native::{D3D12EncodeProbe, HardwareEncodeDevice, NativeVideoCodec};
use alpha2_native_gpu::{D3D12EncodeExtent, SharedD3D12Context};
use serde_json::{Value, json};

use crate::protocol::HostError;

/// `wgpu` opened no device on the adapter.
pub const OPEN_DEVICE: &str = "open-device";
/// `FFmpeg`'s D3D12VA context did not bind to the device.
pub const D3D12VA_DEVICE: &str = "d3d12va-device";
/// This `FFmpeg` runtime registers no D3D12VA encoder for the codec.
pub const ENCODER: &str = "encoder";
/// D3D12 Video Encode reports no encoder for the codec, with the check that
/// refused and its `HRESULT`.
pub const ENCODE_SUPPORT: &str = "encode-support";
/// The probe's size is outside the codec's coded extents on the device.
pub const EXTENT: &str = "extent";
/// The encoder session did not open on the device.
pub const SESSION_OPEN: &str = "session-open";
/// The session opened and the frame did not encode, or the child crashed,
/// hung, or exited without an answer.
pub const TEST_ENCODE: &str = "test-encode";
/// The frame encoded into a first packet no decoder can start from.
pub const FIRST_PACKET: &str = "first-packet";

/// Where a row stopped, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub step: &'static str,
    pub reason: String,
    /// The last line the test encode's child logged, where `FFmpeg` writes
    /// the `HRESULT` behind a generic error.
    pub log: Option<String>,
}

impl Refusal {
    fn at(step: &'static str, reason: impl Into<String>) -> Self {
        Self {
            step,
            reason: reason.into(),
            log: None,
        }
    }

    fn json(&self) -> Value {
        json!({"step": self.step, "reason": self.reason, "log": self.log})
    }
}

/// One DX12 adapter and what its D3D12 Video Encode did.
#[derive(Clone, Debug)]
pub struct AdapterRow {
    /// Its position in the backend's own enumeration, as
    /// `ALPHA2_GPU_ADAPTER` counts.
    pub index: usize,
    pub name: String,
    /// `DXGI_ADAPTER_DESC1::AdapterLuid`; `None` when DXGI does not describe
    /// it.
    pub luid: Option<u64>,
    pub vendor: u32,
    pub device: u32,
    pub driver: String,
    pub device_type: String,
    pub software: bool,
    /// Why the adapter has no shared device, when it has none.
    pub refused: Option<Refusal>,
    pub codecs: Vec<CodecRow>,
}

/// One codec on one adapter.
#[derive(Clone, Debug)]
pub struct CodecRow {
    pub codec: NativeVideoCodec,
    /// The D3D12VA encoder this runtime registers for it.
    pub encoder: Option<&'static str>,
    /// What the device reports for it.
    pub support: Option<D3D12EncodeExtent>,
    /// What the test encode produced.
    pub probe: Option<D3D12EncodeProbe>,
    pub refused: Option<Refusal>,
}

impl CodecRow {
    /// Whether the GPU-resident path could take this codec here: it encoded
    /// a first packet a decoder starts from.
    pub fn usable(&self) -> bool {
        self.probe.is_some() && self.refused.is_none()
    }
}

/// The probe: every adapter's rows, the slice when asked for, as one JSON
/// line on standard output, and one diagnostic line per adapter.
///
/// # Errors
/// Arguments it cannot read, or a report it cannot write.
pub fn probe(args: Vec<String>) -> Result<(), HostError> {
    let mut args = args.into_iter();
    let width = args
        .next()
        .map_or(Ok(1_920), |value| value.parse::<u32>())
        .map_err(|_| HostError::invalid_input("width must be a positive integer"))?;
    let height = args
        .next()
        .map_or(Ok(1_080), |value| value.parse::<u32>())
        .map_err(|_| HostError::invalid_input("height must be a positive integer"))?;
    let slice_path = args.next();
    if args.next().is_some() {
        return Err(HostError::invalid_input(crate::cli::usage()));
    }
    // The slice's capability holds its device to the test encode, which runs in a child too.
    crate::d3d12va_probe::install();
    let timeout = crate::config::configured_gpu_wait_timeout();
    alpha2_native_gpu::set_gpu_wait_timeout(timeout);
    let mut rows = Vec::new();
    let mut devices = Vec::new();
    for (index, adapter) in
        pollster::block_on(GpuContext::adapters_by_preference_on(wgpu::Backends::DX12))
    {
        let info = adapter.get_info();
        let identity = alpha2_native_gpu::d3d12_adapter_identity(&adapter);
        let mut row = AdapterRow {
            index,
            name: info.name.clone(),
            luid: identity.map(|identity| identity.luid),
            vendor: info.vendor,
            device: info.device,
            driver: format!("{} {}", info.driver, info.driver_info)
                .trim()
                .to_owned(),
            device_type: format!("{:?}", info.device_type),
            software: identity.map_or(info.device_type == wgpu::DeviceType::Cpu, |identity| {
                identity.software
            }),
            refused: None,
            codecs: Vec::new(),
        };
        let shared = match pollster::block_on(GpuContext::open_adapter(adapter)) {
            Err(error) => {
                row.refused = Some(Refusal::at(OPEN_DEVICE, error.to_string()));
                None
            }
            Ok(gpu) => match SharedD3D12Context::from_gpu(gpu) {
                Err(error) => {
                    row.refused = Some(Refusal::at(D3D12VA_DEVICE, error.to_string()));
                    None
                }
                Ok(shared) => Some(shared),
            },
        };
        if let Some(shared) = &shared {
            row.codecs = codec_rows(shared, row.luid, (width, height), timeout);
        }
        alpha2_os::diagnostic!("alpha2-host probe-gpu-encode: {}", summary(&row));
        rows.push(row);
        devices.push(shared);
    }
    let rendered = match slice_path {
        None => Value::Null,
        Some(path) => slice(&rows, &devices, (width, height), &path),
    };
    crate::write_stdout(&report_json((width, height), &rows, &rendered))
}

/// Each codec's row on `shared`'s device: the device's own answers, then a
/// test encode at `size` in a child for every codec they let through, all at
/// once, each bounded by `timeout`.
fn codec_rows(
    shared: &SharedD3D12Context,
    luid: Option<u64>,
    size: (u32, u32),
    timeout: Duration,
) -> Vec<CodecRow> {
    let mut rows: Vec<CodecRow> = crate::adapter_choice::ENCODE_CODECS
        .into_iter()
        .map(|codec| {
            let mut row = CodecRow {
                codec,
                encoder: None,
                support: None,
                probe: None,
                refused: None,
            };
            match alpha2_native::d3d12va_encoder(codec) {
                Err(reason) => row.refused = Some(Refusal::at(ENCODER, reason)),
                Ok(encoder) => row.encoder = Some(encoder),
            }
            if row.refused.is_none() {
                let support = crate::d3d12va_probe::d3d12_codec(codec)
                    .ok_or_else(|| format!("D3D12 Video Encode does not encode {}", codec.as_str()))
                    .and_then(|asked| shared.encode_support(asked));
                match support {
                    Err(reason) => row.refused = Some(Refusal::at(ENCODE_SUPPORT, reason)),
                    Ok(extent) => {
                        if !extent.accepts(size.0, size.1) {
                            row.refused = Some(Refusal::at(EXTENT, outside(&extent, size)));
                        }
                        row.support = Some(extent);
                    }
                }
            }
            if row.refused.is_none() && luid.is_none() {
                row.refused = Some(Refusal::at(
                    TEST_ENCODE,
                    "the adapter has no DXGI identity, so no child can find it",
                ));
            }
            row
        })
        .collect();
    let answers: Vec<_> = std::thread::scope(|scope| {
        let children: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.refused.is_none())
            .filter_map(|(at, row)| {
                let (luid, codec) = (luid?, row.codec);
                Some((
                    at,
                    scope.spawn(move || {
                        crate::d3d12va_probe::probe_child(luid, codec, size, timeout)
                    }),
                ))
            })
            .collect();
        children
            .into_iter()
            .map(|(at, child)| {
                let answer = child.join().unwrap_or_else(|_| {
                    (
                        Err("the thread that ran the child panicked".to_owned()),
                        None,
                    )
                });
                (at, answer)
            })
            .collect()
    });
    for (at, (answer, log)) in answers {
        let row = &mut rows[at];
        match answer {
            Ok(probe) => {
                if let Err(reason) = probe.first_packet_decodes() {
                    row.refused = Some(Refusal::at(FIRST_PACKET, reason));
                }
                row.probe = Some(probe);
            }
            Err(reason) => {
                row.refused = Some(Refusal {
                    step: test_encode_step(&reason),
                    reason,
                    log,
                });
            }
        }
    }
    rows
}

/// Why `size` does not fit `extent`.
fn outside(extent: &D3D12EncodeExtent, size: (u32, u32)) -> String {
    let coded = extent.coded(size.0, size.1);
    format!(
        "{}x{} codes as {}x{}, outside the device's {}x{} to {}x{} (alignment {}x{})",
        size.0,
        size.1,
        coded.0,
        coded.1,
        extent.minimum.0,
        extent.minimum.1,
        extent.maximum.0,
        extent.maximum.1,
        extent.granularity.0,
        extent.granularity.1
    )
}

/// The step a test encode's refusal is at: the session's open, whose error
/// the D3D12VA backend names, or anything after it, the child's own end
/// included.
pub fn test_encode_step(reason: &str) -> &'static str {
    if reason.contains("session could not open") {
        SESSION_OPEN
    } else {
        TEST_ENCODE
    }
}

/// The names of a first packet's unit types, for a reader: NAL unit types
/// for H.264 and HEVC, OBU types for AV1, and the number for a type
/// without a name here.
pub fn unit_names(codec: NativeVideoCodec, units: &[u8]) -> Vec<String> {
    units
        .iter()
        .map(|&unit| {
            // H.264 and HEVC share their parameter-set names, at different numbers.
            let parameter_set = match (codec, unit) {
                (NativeVideoCodec::H264, 7) | (NativeVideoCodec::Hevc, 33) => Some("SPS"),
                (NativeVideoCodec::H264, 8) | (NativeVideoCodec::Hevc, 34) => Some("PPS"),
                (NativeVideoCodec::H264, 9) | (NativeVideoCodec::Hevc, 35) => Some("AUD"),
                _ => None,
            };
            let name = parameter_set.or(match (codec, unit) {
                (NativeVideoCodec::H264, 1) => Some("slice"),
                (NativeVideoCodec::H264, 5) => Some("IDR"),
                (NativeVideoCodec::H264, 6) => Some("SEI"),
                (NativeVideoCodec::Hevc, 0 | 1) => Some("TRAIL"),
                (NativeVideoCodec::Hevc, 19) => Some("IDR_W_RADL"),
                (NativeVideoCodec::Hevc, 20) => Some("IDR_N_LP"),
                (NativeVideoCodec::Hevc, 21) => Some("CRA"),
                (NativeVideoCodec::Hevc, 32) => Some("VPS"),
                (NativeVideoCodec::Hevc, 39) => Some("SEI_PREFIX"),
                (NativeVideoCodec::Hevc, 40) => Some("SEI_SUFFIX"),
                (NativeVideoCodec::Av1, 1) => Some("SEQUENCE_HEADER"),
                (NativeVideoCodec::Av1, 2) => Some("TEMPORAL_DELIMITER"),
                (NativeVideoCodec::Av1, 3) => Some("FRAME_HEADER"),
                (NativeVideoCodec::Av1, 4) => Some("TILE_GROUP"),
                (NativeVideoCodec::Av1, 5) => Some("METADATA"),
                (NativeVideoCodec::Av1, 6) => Some("FRAME"),
                (NativeVideoCodec::Av1, 15) => Some("PADDING"),
                _ => None,
            });
            name.map_or_else(|| unit.to_string(), str::to_owned)
        })
        .collect()
}

/// The report: the size, every adapter's row, and the slice.
pub fn report_json(size: (u32, u32), rows: &[AdapterRow], rendered: &Value) -> Value {
    json!({
        "ok": true,
        "result": {
            "size": [size.0, size.1],
            "adapters": rows.iter().map(adapter_json).collect::<Vec<_>>(),
            "rendered": rendered,
        },
    })
}

/// One adapter's row, as the report has it.
pub fn adapter_json(row: &AdapterRow) -> Value {
    json!({
        "index": row.index,
        "name": row.name,
        "luid": row.luid.map(|luid| format!("{luid:016X}")),
        "vendorDevice": format!("{:04x}:{:04x}", row.vendor, row.device),
        "driver": row.driver,
        "deviceType": row.device_type,
        "software": row.software,
        "refused": row.refused.as_ref().map(Refusal::json),
        "codecs": row.codecs.iter().map(codec_json).collect::<Vec<_>>(),
    })
}

fn codec_json(row: &CodecRow) -> Value {
    let support = row.support.as_ref().map(|extent| {
        json!({
            "profile": extent.profile,
            "minimum": [extent.minimum.0, extent.minimum.1],
            "maximum": [extent.maximum.0, extent.maximum.1],
            "alignment": [extent.granularity.0, extent.granularity.1],
            "maxPPictureL0References": extent.max_p_picture_l0_references,
            "maxBPictureL0References": extent.max_b_picture_l0_references,
            "maxL1References": extent.max_l1_references,
            "rateControlModes": extent.rate_control_modes,
            "refusedRateControlModes": extent
                .refused_rate_control_modes
                .iter()
                .map(|(mode, status)| json!({
                    "mode": mode,
                    "hresult": format!("{:#010X}", status.cast_unsigned()),
                }))
                .collect::<Vec<_>>(),
        })
    });
    let result = row.probe.as_ref().map(|probe| {
        let mut result = crate::d3d12va_probe::probe_json(probe);
        result["firstPacketUnitNames"] = json!(unit_names(probe.codec, &probe.first_packet_units));
        result["firstPacketDecodes"] = json!(probe.first_packet_decodes().is_ok());
        result
    });
    json!({
        "videoCodec": row.codec.as_str(),
        "encoder": row.encoder,
        "usable": row.usable(),
        "support": support,
        "result": result,
        "refused": row.refused.as_ref().map(Refusal::json),
    })
}

/// One adapter's row in a line: its identity, then per codec what it
/// encoded or where it stopped.
pub fn summary(row: &AdapterRow) -> String {
    let identity = format!(
        "[{}] {}, {:04x}:{:04x}, LUID {}, driver {}{}",
        row.index,
        row.name,
        row.vendor,
        row.device,
        row.luid
            .map_or_else(|| "none".to_owned(), |luid| format!("{luid:016X}")),
        if row.driver.is_empty() {
            "unknown"
        } else {
            &row.driver
        },
        if row.software {
            " (a software adapter)"
        } else {
            ""
        }
    );
    if let Some(refused) = &row.refused {
        return format!(
            "{identity}: refused at {}: {}",
            refused.step, refused.reason
        );
    }
    let codecs = row
        .codecs
        .iter()
        .map(|codec| {
            let encoded = codec.probe.as_ref().map(|probe| {
                format!(
                    "{} {}x{} {} packet(s), {} bytes, first packet [{}]",
                    probe.encoder,
                    probe.coded_size.0,
                    probe.coded_size.1,
                    probe.packets,
                    probe.packet_bytes,
                    unit_names(probe.codec, &probe.first_packet_units).join(" ")
                )
            });
            match (&codec.refused, encoded) {
                (None, Some(encoded)) => format!("{}: {encoded}", codec.codec.as_str()),
                (Some(refused), encoded) => format!(
                    "{}: refused at {}: {}{}",
                    codec.codec.as_str(),
                    refused.step,
                    refused.reason,
                    refused
                        .log
                        .as_ref()
                        .map(|log| format!(" (it last logged: {log})"))
                        .or_else(|| encoded.map(|encoded| format!(" ({encoded})")))
                        .unwrap_or_default()
                ),
                (None, None) => format!("{}: not asked", codec.codec.as_str()),
            }
        })
        .collect::<Vec<_>>();
    format!("{identity}: {}", codecs.join("; "))
}

/// Why an adapter is not where the slice renders.
fn unavailable(row: &AdapterRow) -> String {
    if let Some(refused) = &row.refused {
        return format!(
            "{}: refused at {}: {}",
            row.name, refused.step, refused.reason
        );
    }
    let codecs = row
        .codecs
        .iter()
        .filter_map(|codec| {
            codec.refused.as_ref().map(|refused| {
                format!(
                    "{} refused at {}: {}",
                    codec.codec.as_str(),
                    refused.step,
                    refused.reason
                )
            })
        })
        .collect::<Vec<_>>();
    format!("{}: {}", row.name, codecs.join("; "))
}

/// Renders the slice on the first adapter, in order, whose capability the
/// GPU-resident path would offer; or why none was, adapter by adapter.
fn slice(
    rows: &[AdapterRow],
    devices: &[Option<SharedD3D12Context>],
    size: (u32, u32),
    path: &str,
) -> Value {
    let mut reasons = Vec::new();
    for (row, shared) in rows.iter().zip(devices) {
        let Some(shared) = shared
            .as_ref()
            .filter(|_| row.codecs.iter().any(CodecRow::usable))
        else {
            reasons.push(unavailable(row));
            continue;
        };
        let device = HardwareEncodeDevice::d3d12(shared.clone());
        if let Err(reason) = device.preferred_capability() {
            reasons.push(format!("{}: {reason}", row.name));
            continue;
        }
        return match crate::render_gpu_encode_slice(&shared.gpu(), &device, size.0, size.1, path) {
            Ok(mut rendered) => {
                rendered["adapter"] = json!(row.name);
                rendered
            }
            Err(error) => json!({
                "adapter": row.name,
                "failed": {"code": error.code, "message": error.message},
            }),
        };
    }
    json!({"skipped": skipped(&reasons)})
}

/// Why the slice did not render, from each adapter's reason.
fn skipped(reasons: &[String]) -> String {
    if reasons.is_empty() {
        "no DX12 adapter".to_owned()
    } else {
        format!(
            "no adapter's GPU-resident path is available: {}",
            reasons.join(" | ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iris_xe() -> AdapterRow {
        AdapterRow {
            index: 0,
            name: "Intel(R) Iris(R) Xe Graphics".to_owned(),
            luid: Some(0xAB12C),
            vendor: 0x8086,
            device: 0x9a49,
            driver: "30.0.101.2079".to_owned(),
            device_type: "IntegratedGpu".to_owned(),
            software: false,
            refused: None,
            codecs: vec![
                CodecRow {
                    codec: NativeVideoCodec::H264,
                    encoder: Some("h264_d3d12va"),
                    support: Some(D3D12EncodeExtent {
                        codec: alpha2_native_gpu::D3D12EncodeCodec::H264,
                        profile: "main",
                        minimum: (64, 64),
                        maximum: (4096, 4096),
                        granularity: (16, 16),
                        max_p_picture_l0_references: 2,
                        max_b_picture_l0_references: 0,
                        max_l1_references: 0,
                        rate_control_modes: vec!["disabled", "qvbr"],
                        refused_rate_control_modes: vec![("cbr", 0x887A_0020_u32.cast_signed())],
                    }),
                    probe: Some(D3D12EncodeProbe {
                        encoder: "h264_d3d12va".to_owned(),
                        codec: NativeVideoCodec::H264,
                        rate_control: "disabled",
                        coded_size: (1920, 1088),
                        opened_profile: 77,
                        extradata_size: 0,
                        profile: None,
                        first_packet_units: vec![5],
                        packets: 1,
                        packet_bytes: 2_048,
                        decode_failure: None,
                    }),
                    refused: Some(Refusal::at(FIRST_PACKET, "lacks its parameter sets")),
                },
                CodecRow {
                    codec: NativeVideoCodec::Hevc,
                    encoder: Some("hevc_d3d12va"),
                    support: None,
                    probe: Some(D3D12EncodeProbe {
                        encoder: "hevc_d3d12va".to_owned(),
                        codec: NativeVideoCodec::Hevc,
                        rate_control: "disabled",
                        coded_size: (1920, 1088),
                        opened_profile: 1,
                        extradata_size: 0,
                        profile: Some(1),
                        first_packet_units: vec![32, 33, 34, 19],
                        packets: 1,
                        packet_bytes: 4_321,
                        decode_failure: None,
                    }),
                    refused: None,
                },
                CodecRow {
                    codec: NativeVideoCodec::Av1,
                    encoder: Some("av1_d3d12va"),
                    support: None,
                    probe: None,
                    refused: Some(Refusal {
                        step: SESSION_OPEN,
                        reason: "av1_d3d12va session could not open: Generic error".to_owned(),
                        log: Some("[av1_d3d12va @ 0] Failed: 0x887A0020".to_owned()),
                    }),
                },
            ],
        }
    }

    fn mx450() -> AdapterRow {
        AdapterRow {
            index: 1,
            name: "NVIDIA GeForce MX450".to_owned(),
            luid: None,
            vendor: 0x10de,
            device: 0x1f97,
            driver: String::new(),
            device_type: "DiscreteGpu".to_owned(),
            software: false,
            refused: Some(Refusal::at(
                D3D12VA_DEVICE,
                "AVERROR(-22): `Invalid argument`",
            )),
            codecs: Vec::new(),
        }
    }

    /// Each row says what it is, and for each codec what it encoded or the
    /// step it stopped at and why.
    #[test]
    fn a_row_reads_as_its_adapter_and_each_codec_s_answer() {
        let report = report_json((1920, 1080), &[iris_xe(), mx450()], &Value::Null);
        let adapters = &report["result"]["adapters"];
        let iris = &adapters[0];
        assert_eq!(iris["luid"], "00000000000AB12C");
        assert_eq!(iris["vendorDevice"], "8086:9a49");
        assert_eq!(iris["refused"], Value::Null);
        let h264 = &iris["codecs"][0];
        assert_eq!(h264["usable"], false);
        assert_eq!(h264["refused"]["step"], "first-packet");
        assert_eq!(h264["result"]["firstPacketUnitNames"], json!(["IDR"]));
        assert_eq!(h264["result"]["firstPacketDecodes"], false);
        assert_eq!(h264["support"]["alignment"], json!([16, 16]));
        assert_eq!(
            h264["support"]["refusedRateControlModes"],
            json!([{"mode": "cbr", "hresult": "0x887A0020"}])
        );
        let hevc = &iris["codecs"][1];
        assert_eq!(hevc["usable"], true);
        assert_eq!(hevc["refused"], Value::Null);
        assert_eq!(
            hevc["result"]["firstPacketUnitNames"],
            json!(["VPS", "SPS", "PPS", "IDR_W_RADL"])
        );
        assert_eq!(hevc["result"]["extradataSize"], 0);
        assert_eq!(hevc["result"]["codedSize"], json!([1920, 1088]));
        assert_eq!(hevc["result"]["firstPacketDecodes"], true);
        let av1 = &iris["codecs"][2];
        assert_eq!(av1["refused"]["step"], "session-open");
        assert_eq!(
            av1["refused"]["log"],
            "[av1_d3d12va @ 0] Failed: 0x887A0020"
        );
        assert_eq!(av1["result"], Value::Null);
        let mx = &adapters[1];
        assert_eq!(mx["luid"], Value::Null);
        assert_eq!(mx["refused"]["step"], "d3d12va-device");
        assert_eq!(mx["codecs"], json!([]));

        assert_eq!(
            summary(&iris_xe()),
            "[0] Intel(R) Iris(R) Xe Graphics, 8086:9a49, LUID 00000000000AB12C, driver \
             30.0.101.2079: h264: refused at first-packet: lacks its parameter sets \
             (h264_d3d12va 1920x1088 1 packet(s), 2048 bytes, first packet [IDR]); hevc: \
             hevc_d3d12va 1920x1088 1 packet(s), 4321 bytes, first packet [VPS SPS PPS \
             IDR_W_RADL]; av1: refused at session-open: av1_d3d12va session could not open: \
             Generic error (it last logged: [av1_d3d12va @ 0] Failed: 0x887A0020)"
        );
        assert_eq!(
            summary(&mx450()),
            "[1] NVIDIA GeForce MX450, 10de:1f97, LUID none, driver unknown: refused at \
             d3d12va-device: AVERROR(-22): `Invalid argument`"
        );
    }

    #[test]
    fn a_refused_test_encode_names_its_step() {
        assert_eq!(
            test_encode_step("hevc_d3d12va session could not open: Generic error"),
            SESSION_OPEN
        );
        for reason in [
            "the D3D12VA test encode crashed (exit 0xC0000005)",
            "the D3D12VA test encode did not finish within 30s and was killed",
            "hevc_d3d12va opened but produced no packet",
        ] {
            assert_eq!(test_encode_step(reason), TEST_ENCODE, "{reason}");
        }
    }

    #[test]
    fn unit_types_have_their_names() {
        assert_eq!(
            unit_names(NativeVideoCodec::H264, &[9, 7, 8, 6, 5, 1, 12]),
            ["AUD", "SPS", "PPS", "SEI", "IDR", "slice", "12"]
        );
        assert_eq!(
            unit_names(NativeVideoCodec::Av1, &[2, 1, 6, 3, 4]),
            [
                "TEMPORAL_DELIMITER",
                "SEQUENCE_HEADER",
                "FRAME",
                "FRAME_HEADER",
                "TILE_GROUP"
            ]
        );
    }

    /// The slice is skipped on every adapter with nothing usable, and the
    /// reason names each one and where it stopped.
    #[test]
    fn a_skipped_slice_says_why_for_each_adapter() {
        let mut iris = iris_xe();
        iris.codecs[1].refused = Some(Refusal::at(FIRST_PACKET, "lacks a coded picture"));
        let rendered = slice(&[iris, mx450()], &[None, None], (1920, 1080), "unused.mp4");
        assert_eq!(
            rendered,
            json!({
                "skipped": "no adapter's GPU-resident path is available: Intel(R) Iris(R) Xe \
                            Graphics: h264 refused at first-packet: lacks its parameter sets; \
                            hevc refused at first-packet: lacks a coded picture; av1 refused at \
                            session-open: av1_d3d12va session could not open: Generic error | \
                            NVIDIA GeForce MX450: refused at d3d12va-device: AVERROR(-22): \
                            `Invalid argument`"
            })
        );
        assert_eq!(
            slice(&[], &[], (2, 2), "unused.mp4"),
            json!({"skipped": "no DX12 adapter"})
        );
    }

    #[test]
    fn a_size_outside_the_extent_says_where_it_lands() {
        let extent = iris_xe().codecs[0].support.clone().expect("extent");
        assert_eq!(
            outside(&extent, (8_190, 4_320)),
            "8190x4320 codes as 8192x4320, outside the device's 64x64 to 4096x4096 (alignment \
             16x16)"
        );
    }
}
