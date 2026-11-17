//! The classes an export failure is sorted into, which retry policies read.

/// An export failure's class, spelled on the wire as [`FailureClass::as_str`] gives it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FailureClass {
    /// Memory ran out.
    OutOfMemory,
    /// One allocation was refused.
    AllocationFailed,
    /// The GPU device was lost.
    DeviceLost,
    /// The export's own GPU work stops the GPU.
    DeviceHung,
    /// Reading rendered pixels back failed.
    ReadbackFailed,
    /// GPU work did not finish in time.
    GpuTimeout,
    /// The GPU refused the work as invalid.
    ValidationFailed,
    /// The request or its media is wrong.
    InvalidInput,
    /// The host process exited.
    HostExited,
    /// Writing the output failed.
    OutputIoFailed,
    /// The output's storage is full.
    StorageQuotaExceeded,
    /// The export stopped before exceeding a budget.
    ResourceLimit,
    /// A device is held by other work and may free up later.
    ResourceBusy,
    /// The caller cancelled.
    Cancelled,
    /// The engine faulted (a panic or a wasm trap).
    Defect,
    /// Nothing in the evidence names a class.
    Unknown,
}

impl FailureClass {
    /// Every class.
    pub const ALL: [Self; 16] = [
        Self::OutOfMemory,
        Self::AllocationFailed,
        Self::DeviceLost,
        Self::DeviceHung,
        Self::ReadbackFailed,
        Self::GpuTimeout,
        Self::ValidationFailed,
        Self::InvalidInput,
        Self::HostExited,
        Self::OutputIoFailed,
        Self::StorageQuotaExceeded,
        Self::ResourceLimit,
        Self::ResourceBusy,
        Self::Cancelled,
        Self::Defect,
        Self::Unknown,
    ];

    /// The wire spelling (`OUT_OF_MEMORY`, ...).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OutOfMemory => "OUT_OF_MEMORY",
            Self::AllocationFailed => "ALLOCATION_FAILED",
            Self::DeviceLost => "DEVICE_LOST",
            Self::DeviceHung => "DEVICE_HUNG",
            Self::ReadbackFailed => "READBACK_FAILED",
            Self::GpuTimeout => "GPU_TIMEOUT",
            Self::ValidationFailed => "VALIDATION_FAILED",
            Self::InvalidInput => "INVALID_INPUT",
            Self::HostExited => "HOST_EXITED",
            Self::OutputIoFailed => "OUTPUT_IO_FAILED",
            Self::StorageQuotaExceeded => "STORAGE_QUOTA_EXCEEDED",
            Self::ResourceLimit => "RESOURCE_LIMIT",
            Self::ResourceBusy => "RESOURCE_BUSY",
            Self::Cancelled => "CANCELLED",
            Self::Defect => "DEFECT",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// The class spelled `code`; `None` for anything else, a host error code included.
    #[must_use]
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == code)
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
