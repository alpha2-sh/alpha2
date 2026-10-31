/**
 * What an alpha2 runtime can do, in the one shape the native host's `capabilities` result and the
 * browser bridge both report (`alpha2-capabilities`, version 2).
 */

/** Which frame real-time playback shows, and whether sound plays. */
export type PlaybackPolicy =
  /** Sound leads at 1x; late frames are dropped, a missing one holds the last. */
  | "drop_video_keep_audio"
  /** Every frame in order, paced by the rate but never skipped; silent. */
  | "preserve_every_video_frame"
  /** The wall clock at any rate, holding the last frame while one is missing; silent. */
  | "silent_scrub"
  /** Every frame in order as fast as it renders, as an export does; silent. */
  | "offline_exact";

/** One feature: whether it works here, and why not. */
export interface CapabilityFeature {
  readonly available: boolean;
  /** Why the feature is unavailable; may also qualify an available one. */
  readonly reason?: string;
}

/** The GPU a runtime renders with. */
export interface GpuCapability {
  readonly adapter: string;
  readonly backend: string;
  readonly driver: string;
  readonly driverInfo: string;
  /** The DX12 shader compiler (`FXC`, `DXC <path>`); empty on other backends and in browsers. */
  readonly shaderCompiler: string;
  /** `a`: an adapter meeting all of WebGPU; `b`: a downlevel one (GL, WebGL2). */
  readonly tier: "a" | "b" | "unknown";
  /**
   * What pictures are stored in between passes: half float wherever the adapter renders, filters,
   * and copies it (every tier-A adapter, and tier B with a float colour buffer), 8-bit elsewhere.
   * The tier does not decide it.
   */
  readonly intermediateFormat: "rgba16float" | "rgba8unorm";
  readonly maxTextureDimension2d: number;
  readonly timestampQueries: boolean;
  /** The idle intermediate texture pool's byte budget. */
  readonly textureBudgetBytes: number;
  readonly memory: {
    /**
     * The device memory budget the adapter's graphics API reports (DXGI's local segment budget,
     * Vulkan's `VK_EXT_memory_budget` device-local heaps, Metal's recommended working set);
     * `null` where it reports none, in a browser and on the CPU renderer. A native host holds its
     * idle texture pools and a default in-flight budget to a quarter of it.
     */
    readonly budgetBytes: number | null;
  };
}

/** How a runtime handles color. */
export interface ColorCapability {
  readonly workingSpace: "linear_bt709_sdr";
  /** Whether PQ and HLG sources are tone-mapped into the working space. */
  readonly hdrSources: boolean;
  /** Whether output can be HDR. No runtime encodes HDR output yet, so this is `false`. */
  readonly hdrOutput: boolean;
  /** Whether linear float sources keep values beyond SDR until compositing. */
  readonly extendedRangeSources: boolean;
}

/** How a runtime's real-time playback behaves. */
export interface PlaybackCapability {
  readonly policies: readonly PlaybackPolicy[];
  readonly defaultPolicy: PlaybackPolicy;
  /** Whether sound plays at rates other than 1x. */
  readonly audioAtNonUnitRate: boolean;
}

/** Why a native host renders on the adapter it chose. */
export type AdapterChoiceReason =
  /** It encodes more codecs on the GPU than the runner-up. */
  | "most-gpu-encoders"
  /** It encodes as many codecs and decodes on the GPU where the runner-up does not. */
  | "decodes"
  /** Nothing else tells it from the runner-up: it comes first in the high-performance order. */
  | "high-performance-order"
  /** `ALPHA2_GPU_ADAPTER=performance`: the first hardware adapter in the high-performance order. */
  | "performance-policy"
  /** `ALPHA2_GPU_ADAPTER` names it. */
  | "pinned"
  /** It is the only hardware adapter that renders. */
  | "only-adapter"
  /** No adapter could share a device with the video decoder and encoder. */
  | "no-shared-device";

/** One adapter a native host considered, with what it does or why not. */
export interface AdapterCandidate {
  /** Its position in the backend's enumeration, which an `ALPHA2_GPU_ADAPTER` number names. */
  readonly index: number;
  readonly name: string;
  readonly backend: "dx12" | "vulkan" | "metal";
  /** The DXGI adapter LUID as hex (`0x...`); `null` off Windows. */
  readonly luid: string | null;
  readonly software: boolean;
  readonly renders: boolean;
  readonly rendersReason: string | null;
  /** Whether hardware decode imports into its device. */
  readonly decodes: boolean;
  readonly decodesReason: string | null;
  /** The codecs it encodes on the GPU (`h264`, `hevc`, `av1`). */
  readonly gpuEncodes: readonly string[];
  /** Why it encodes none, when it encodes none. */
  readonly gpuEncodeReason: string | null;
}

/** Which adapter a native host renders on, and why. */
export interface AdapterChoice {
  /**
   * The `ALPHA2_GPU_ADAPTER` policy: `auto`, `performance`, `index:<n>` or `name:<text>`. Unset,
   * it is `auto` on Linux and macOS and `performance` on Windows.
   */
  readonly policy: string;
  /** The `index` of the candidate the renderer is on; `null` for a device none of them gave. */
  readonly chosen: number | null;
  readonly reason: AdapterChoiceReason;
  /** One sentence naming the chosen adapter, and the runner-up where the reason compares them. */
  readonly explanation: string;
  readonly candidates: readonly AdapterCandidate[];
}

/** What a runtime can do. `Decode` and `Encode` are the runtime's own descriptions. */
export interface CapabilityReport<Decode = unknown, Encode = unknown> {
  readonly schema: "alpha2-capabilities";
  readonly version: 2;
  readonly runtime: "native" | "browser";
  /** The GPU, or `null` before a browser's engine has started. */
  readonly gpu: GpuCapability | null;
  readonly color: ColorCapability | null;
  readonly playback: PlaybackCapability;
  readonly decode: Decode;
  readonly encode: Encode;
  readonly features: Readonly<Record<string, CapabilityFeature>>;
  /** Which adapter a native GPU host renders on; `null` in a browser and on the CPU renderer. */
  readonly adapters: AdapterChoice | null;
  /** The machine's memory and the host process's use of it; `null` in a browser. */
  readonly memory: MemoryCapability | null;
}

/** What memory a native host's machine has, and what the host process uses of it. */
export interface MemoryCapability {
  readonly capacity: {
    /** Physical memory installed. */
    readonly physicalBytes: number | null;
    /** What the operating system could give a process now without paging. */
    readonly availableBytes: number | null;
    /** The most the host may use: the lower of its cgroup or job limit and the physical memory. */
    readonly limitBytes: number | null;
    readonly limitSource: "cgroup-v2" | "cgroup-v1" | "job" | "physical";
    /** What the cgroup limit has already charged; `null` for a job or physical limit. */
    readonly usedInLimitBytes: number | null;
  };
  /**
   * The host process now: resident bytes (in RAM, shared pages included) and its footprint, what
   * the system charges it for (Linux anonymous and shared resident pages, the macOS physical
   * footprint, the Windows private commit), each with its peak.
   */
  readonly process: {
    readonly residentBytes: number | null;
    readonly peakResidentBytes: number | null;
    readonly footprintBytes: number | null;
    readonly peakFootprintBytes: number | null;
  };
}
