//! The Windows renderer's adapter: every DX12 adapter is opened and asked
//! what it renders, decodes and encodes, and [`crate::adapter_choice`]
//! takes one.
//!
//! D3D12VA decodes, and D3D12 Video Encode encodes, on the renderer's own
//! device, so the adapter the renderer is on decides both. Each check is
//! the device's own answer: whether a device opens, whether `FFmpeg`'s
//! D3D12VA context binds to it and `wgpu` shows its NV12 planes, and what
//! D3D12 Video Encode reports for each codec. None of it is a test encode,
//! which only the chosen adapter is held to, later.

use alpha2_gpu::GpuContext;
use alpha2_native_gpu::SharedD3D12Context;

use crate::adapter_choice::{AdapterPolicy, AdapterRecord, choose_adapter, decode_refusals};

/// What the scan found, and the device the renderer takes.
pub struct Dx12Choice {
    pub gpu: GpuContext,
    /// The renderer's device shared with `FFmpeg`, or why there is none;
    /// `None` when neither decode nor encode wanted one.
    pub shared: Option<Result<SharedD3D12Context, String>>,
    /// Why the renderer's adapter does not decode, when decode wanted the
    /// GPU and it does not: each adapter considered, with its reason.
    pub decode_refused: Option<String>,
    pub records: Vec<AdapterRecord>,
    /// The position in `records` of the renderer's adapter; `None` for a
    /// device none of them gave.
    pub chosen: Option<usize>,
}

impl Dx12Choice {
    /// The renderer on `gpu`, a device no scan found.
    pub const fn without_scan(gpu: GpuContext) -> Self {
        Self {
            gpu,
            shared: None,
            decode_refused: None,
            records: Vec::new(),
            chosen: None,
        }
    }
}

/// Why the scan took no adapter.
pub enum ScanError {
    /// No DX12 adapter renders: what a request for one answers.
    Unavailable(alpha2_gpu::GpuError),
    /// `ALPHA2_GPU_ADAPTER` names no adapter that renders.
    Pin(String),
}

impl std::fmt::Debug for ScanError {
    /// The reason alone, as the request's own error read before the scan.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "{error:?}"),
            Self::Pin(reason) => formatter.write_str(reason),
        }
    }
}

/// One adapter's device, while the scan decides.
struct Opened {
    gpu: GpuContext,
    shared: Option<Result<SharedD3D12Context, String>>,
}

/// Opens the DX12 adapters and takes one under `policy`.
///
/// `decode` is whether the decode policy allows decoding on the GPU, and
/// `encode` whether this host may encode on it. A scan that wants neither,
/// or only decode under `default`, stops at the first adapter `default`
/// would take, so it opens no more devices than that choice needs.
/// Otherwise every hardware adapter is opened and asked:
/// encode's reason names an adapter that could encode, and the start
/// diagnostic lists each one. A software adapter is opened only when
/// nothing else renders, or a pin names it.
///
/// # Errors
/// [`ScanError::Pin`] when the pin takes nothing; [`ScanError::Unavailable`]
/// when no adapter renders.
pub async fn scan(
    policy: &AdapterPolicy,
    decode: bool,
    encode: bool,
) -> Result<Dx12Choice, ScanError> {
    let exhaustive = encode || *policy != AdapterPolicy::Default;
    let mut records = Vec::new();
    let mut opened: Vec<Option<Opened>> = Vec::new();
    for (index, adapter) in GpuContext::adapters_by_preference_on(wgpu::Backends::DX12).await {
        let info = adapter.get_info();
        let software = alpha2_native_gpu::d3d12_adapter_identity(&adapter)
            .map_or(info.device_type == wgpu::DeviceType::Cpu, |identity| {
                identity.software
            });
        let mut record = AdapterRecord {
            index,
            name: info.name.clone(),
            software,
            renders: Err("not opened".to_owned()),
            decodes: Err(if decode {
                "not opened".to_owned()
            } else {
                "decode is in software".to_owned()
            }),
            encodes: Vec::new(),
        };
        let pinned = policy.names(&record);
        let hardware_renders = records
            .iter()
            .any(|other: &AdapterRecord| !other.software && other.renders.is_ok());
        if software && hardware_renders && !pinned {
            record.renders =
                Err("a software adapter, not opened while a hardware one renders".to_owned());
            records.push(record);
            opened.push(None);
            continue;
        }
        let device = match GpuContext::open_adapter(adapter).await {
            Ok(gpu) => {
                record.renders = Ok(());
                let shared = (decode || encode).then(|| {
                    SharedD3D12Context::from_gpu(gpu.clone()).map_err(|error| error.to_string())
                });
                ask(&mut record, shared.as_ref(), decode, encode);
                Some(Opened { gpu, shared })
            }
            Err(error) => {
                record.renders = Err(error.to_string());
                None
            }
        };
        // Enough for `default` without encode: the first one it takes ends the scan.
        let taken = !exhaustive
            && !record.software
            && record.renders.is_ok()
            && (!decode || record.decodes.is_ok());
        records.push(record);
        opened.push(device);
        if taken {
            break;
        }
    }
    let chosen = match choose_adapter(
        &records,
        policy,
        decode,
        crate::adapter_choice::DEFAULT_ENCODE_CODEC,
    ) {
        Ok(chosen) => chosen,
        Err(reason) if policy.is_pin() => return Err(ScanError::Pin(reason)),
        // A plain request answers with the reason no adapter renders, as a request without a
        // scan would.
        Err(_) => {
            return GpuContext::request_high_performance_on(wgpu::Backends::DX12)
                .await
                .map(Dx12Choice::without_scan)
                .map_err(ScanError::Unavailable);
        }
    };
    let Some(Opened { gpu, shared }) = opened.get_mut(chosen).and_then(Option::take) else {
        return Err(ScanError::Pin(format!(
            "{} was chosen without a device",
            records[chosen].name
        )));
    };
    let decode_refused = (decode && records[chosen].decodes.is_err())
        .then(|| decode_refusals(&records, chosen, policy.is_pin()));
    // The other adapters' devices close here, before the renderer starts on its own.
    drop(opened);
    Ok(Dx12Choice {
        gpu,
        shared,
        decode_refused,
        records,
        chosen: Some(chosen),
    })
}

/// Fills in what `record`'s shared device (`shared`, when one was made)
/// decodes and encodes.
#[cfg_attr(not(feature = "hwencode"), allow(unused_variables))]
fn ask(
    record: &mut AdapterRecord,
    shared: Option<&Result<SharedD3D12Context, String>>,
    decode: bool,
    encode: bool,
) {
    let Some(shared) = shared else {
        return;
    };
    if decode {
        record.decodes = match shared {
            Ok(context) if context.decode_planes() => Ok(()),
            Ok(_) => Err("D3D12 adapter does not expose NV12 plane views".to_owned()),
            Err(reason) => Err(reason.clone()),
        };
    }
    #[cfg(feature = "hwencode")]
    if encode {
        record.encodes = crate::adapter_choice::ENCODE_CODECS
            .iter()
            .map(|&codec| {
                let answer = match shared {
                    Ok(context) => crate::d3d12va_probe::device_encodes(context, codec),
                    Err(reason) => Err(format!("{}: {reason}", record.name)),
                };
                (codec, answer)
            })
            .collect();
    }
}
