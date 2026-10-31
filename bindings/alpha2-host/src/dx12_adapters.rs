//! The Windows renderer's adapter: every DX12 adapter is opened and asked
//! what it renders, decodes and encodes, and [`alpha2_gpu::rank_adapters`]
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

use crate::adapter_choice::{AdapterChoiceReport, AdapterPolicy};
use alpha2_gpu::AdapterRecord;

/// What the scan found, and the device the renderer takes.
pub struct Dx12Choice {
    pub gpu: GpuContext,
    /// The renderer's device shared with `FFmpeg`, or why there is none;
    /// `None` when neither decode nor encode wanted one.
    pub shared: Option<Result<SharedD3D12Context, String>>,
    /// Why the renderer's adapter does not decode, when decode wanted the
    /// GPU and it does not: each adapter considered, with its reason.
    pub decode_refused: Option<String>,
    /// Every adapter the scan found, what each does, and which one the
    /// renderer is on and why; `None` for a device no scan found.
    pub report: Option<AdapterChoiceReport>,
}

impl Dx12Choice {
    /// The renderer on `gpu`, a device no scan found.
    pub const fn without_scan(gpu: GpuContext) -> Self {
        Self {
            gpu,
            shared: None,
            decode_refused: None,
            report: None,
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

/// The DX12 adapters in preference order. None at all is a backend that did
/// not start (its shader compiler), not a pin that matches nothing.
async fn listed_adapters() -> Result<Vec<(usize, wgpu::Adapter)>, ScanError> {
    let adapters = GpuContext::adapters_by_preference_on(wgpu::Backends::DX12).await;
    if adapters.is_empty() {
        return Err(ScanError::Unavailable(
            alpha2_gpu::GpuError::no_dx12_adapter(),
        ));
    }
    Ok(adapters)
}

/// Opens the DX12 adapters and takes one under `policy`.
///
/// `decode` is whether the decode policy allows decoding on the GPU, and
/// `encode` whether this host may encode on it. Under `performance` without
/// encode the scan stops at the first hardware adapter that renders, which
/// that policy takes whatever else there is. Otherwise every hardware
/// adapter is opened and asked, since `auto` ranks them by what each
/// encodes and decodes, encode's reason names an adapter that could encode,
/// and the start diagnostic lists each one. A software adapter is opened
/// only when nothing else renders, or a pin names it.
///
/// # Errors
/// [`ScanError::Pin`] when the pin takes nothing; [`ScanError::Unavailable`]
/// when no adapter renders.
pub async fn scan(
    policy: &AdapterPolicy,
    decode: bool,
    encode: bool,
) -> Result<Dx12Choice, ScanError> {
    let exhaustive = encode || *policy != AdapterPolicy::Performance;
    let mut records = Vec::new();
    let mut opened: Vec<Option<Opened>> = Vec::new();
    let adapters = listed_adapters().await?;
    let identities = row_identities(&adapters);
    let duplicates = crate::adapter_choice::duplicate_rows(&identities);
    for ((index, adapter), identity) in adapters.into_iter().zip(&identities) {
        let info = adapter.get_info();
        let software = identity.software;
        let mut record = AdapterRecord::unasked(index, info.name.clone(), "dx12", software);
        record.luid = identity.luid;
        record.decodes = Err(if decode {
            "not opened".to_owned()
        } else {
            "decode is in software".to_owned()
        });
        let pinned = policy.names(record.index, &record.name);
        // A second row of a GPU another row is: never opened, and named only by its index.
        if let Some(&(_, same)) = duplicates.iter().find(|(row, _)| *row == index)
            && !matches!(policy, AdapterPolicy::Index(pinned) if *pinned == index)
        {
            record.renders = Err(format!("same GPU as [{same}], not opened"));
            records.push(record);
            opened.push(None);
            continue;
        }
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
                    // The record names the adapter; the reason is the step that failed.
                    SharedD3D12Context::from_gpu(gpu.clone())
                        .map_err(|error| error.step().to_owned())
                });
                ask(&mut record, shared.as_ref(), decode, encode);
                Some(Opened { gpu, shared })
            }
            Err(error) => {
                record.renders = Err(error.to_string());
                None
            }
        };
        // Enough for `performance` without encode: the first one it takes ends the scan.
        let taken = !exhaustive && !record.software && record.renders.is_ok();
        records.push(record);
        opened.push(device);
        if taken {
            break;
        }
    }
    let order = match alpha2_gpu::rank_adapters(&records, policy, decode) {
        Ok(order) => order,
        Err(reason) if policy.is_pin() => return Err(ScanError::Pin(reason)),
        // A plain request answers with the reason no adapter renders, as a request without a
        // scan would.
        Err(_) => {
            return GpuContext::request_product_with(&alpha2_gpu::ProductRequest::default())
                .await
                .map(Dx12Choice::without_scan)
                .map_err(ScanError::Unavailable);
        }
    };
    let chosen = order.order[0];
    let Some(Opened { gpu, shared }) = opened.get_mut(chosen).and_then(Option::take) else {
        return Err(ScanError::Pin(format!(
            "{} was chosen without a device",
            records[chosen].name
        )));
    };
    let decode_refused = (decode && records[chosen].decodes.is_err())
        .then(|| alpha2_gpu::decode_refusals(&records, chosen, policy.is_pin()));
    // The other adapters' devices close here, before the renderer starts on its own.
    drop(opened);
    Ok(Dx12Choice {
        gpu,
        shared,
        decode_refused,
        report: Some(AdapterChoiceReport::ranked(policy.clone(), records, &order)),
    })
}

/// What tells each adapter's GPU from the others', for [`crate::adapter_choice::duplicate_rows`].
fn row_identities(adapters: &[(usize, wgpu::Adapter)]) -> Vec<crate::adapter_choice::RowIdentity> {
    adapters
        .iter()
        .map(|(index, adapter)| {
            let info = adapter.get_info();
            let identity = alpha2_native_gpu::d3d12_adapter_identity(adapter);
            crate::adapter_choice::RowIdentity {
                index: *index,
                vendor_device: identity.map_or((info.vendor, info.device), |identity| {
                    identity.vendor_device
                }),
                pci_address: identity.and_then(|identity| identity.pci_address),
                luid: identity.map(|identity| identity.luid),
                software: identity.map_or(info.device_type == wgpu::DeviceType::Cpu, |identity| {
                    identity.software
                }),
            }
        })
        .collect()
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
        record.gpu_encodes = crate::adapter_choice::ENCODE_CODECS
            .iter()
            .map(|&codec| {
                let answer = match shared {
                    Ok(context) => crate::d3d12va_probe::device_encodes(context, codec),
                    Err(reason) => Err(format!("{}: {reason}", record.name)),
                };
                (codec.as_str(), answer)
            })
            .collect();
    }
}
