/**
 * Runtime capability report (`alpha2-capabilities`, version 2), shared by the native host and the
 * browser bridge.
 */

/** How real-time playback picks frames and whether sound plays. */
export type PlaybackPolicy =
  /** Sound leads at 1x; late frames are dropped, a missing one holds the last. */
  | "drop_video_keep_audio"
  /** Every frame in order, paced by the rate but never skipped; silent. */
  | "preserve_every_video_frame"
  /** Follows the wall clock at any rate, holding the last frame while one is missing; silent. */
  | "silent_scrub"
  /** Every frame in order as fast as it renders, as an export does; silent. */
  | "offline_exact";

export interface CapabilityFeature {
  readonly available: boolean;
  /** Why the feature is unavailable, or a caveat on an available one. */
  readonly reason?: string;
}

export interface GpuCapability {
  readonly adapter: string;
  readonly backend: string;
  readonly driver: string;
  readonly driverInfo: string;
  /** DX12 shader compiler (`FXC`, `DXC <path>`); empty on other backends and in browsers. */
  readonly shaderCompiler: string;
  /** `a`: full `WebGPU` adapter; `b`: downlevel adapter (GL, WebGL2). */
  readonly tier: "a" | "b" | "unknown";
  /**
   * Format between render passes: half float where the adapter can render, filter and copy it,
   * 8-bit elsewhere. Independent of `tier`.
   */
  readonly intermediateFormat: "rgba16float" | "rgba8unorm";
  readonly maxTextureDimension2d: number;
  readonly timestampQueries: boolean;
  /** Byte budget of the idle intermediate texture pool. */
  readonly textureBudgetBytes: number;
  readonly memory: {
    /**
     * Device memory budget reported by the graphics API; `null` when unreported, in a browser and
     * on the CPU renderer. A native host caps idle texture pools and the default in-flight budget
     * at a quarter of it.
     */
    readonly budgetBytes: number | null;
  };
}

/** How a runtime handles color. */
export interface ColorCapability {
  readonly workingSpace: "linear_bt709_sdr";
  /** Whether PQ and HLG sources are tone-mapped into the working space. */
  readonly hdrSources: boolean;
  /** Always `false`; no runtime encodes HDR output yet. */
  readonly hdrOutput: boolean;
  /** Whether linear float sources keep values beyond SDR until compositing. */
  readonly extendedRangeSources: boolean;
}

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
  /** Tie with the runner-up, broken by high-performance order. */
  | "high-performance-order"
  /** `ALPHA2_GPU_ADAPTER=performance`: the first hardware adapter in the high-performance order. */
  | "performance-policy"
  /** `ALPHA2_GPU_ADAPTER` names it. */
  | "pinned"
  /** It is the only hardware adapter that renders. */
  | "only-adapter"
  /** No adapter could share a device with the video decoder and encoder. */
  | "no-shared-device";

/** An adapter a native host considered. */
export interface AdapterCandidate {
  /** Position in the backend's enumeration, as used by `ALPHA2_GPU_ADAPTER=index:<n>`. */
  readonly index: number;
  readonly name: string;
  readonly backend: "dx12" | "vulkan" | "metal";
  /** DXGI adapter LUID as hex (`0x...`); `null` off Windows. */
  readonly luid: string | null;
  readonly software: boolean;
  readonly renders: boolean;
  readonly rendersReason: string | null;
  /** Whether hardware decode imports into its device. */
  readonly decodes: boolean;
  readonly decodesReason: string | null;
  /** Codecs it encodes on the GPU (`h264`, `hevc`, `av1`). */
  readonly gpuEncodes: readonly string[];
  /** Set when `gpuEncodes` is empty. */
  readonly gpuEncodeReason: string | null;
}

export interface AdapterChoice {
  /**
   * `ALPHA2_GPU_ADAPTER` policy: `auto`, `performance`, `index:<n>` or `name:<text>`. Defaults to
   * `auto` on Linux and macOS and `performance` on Windows.
   */
  readonly policy: string;
  /** `index` of the chosen candidate; `null` when the device came from none of them. */
  readonly chosen: number | null;
  readonly reason: AdapterChoiceReason;
  /** One sentence naming the chosen adapter, and the runner-up when compared. */
  readonly explanation: string;
  readonly candidates: readonly AdapterCandidate[];
}

/** A runtime's capabilities. `Decode` and `Encode` are runtime-specific. */
export interface CapabilityReport<Decode = unknown, Encode = unknown> {
  readonly schema: "alpha2-capabilities";
  readonly version: 2;
  readonly runtime: "native" | "browser";
  /** `null` before a browser's engine has started. */
  readonly gpu: GpuCapability | null;
  readonly color: ColorCapability | null;
  readonly playback: PlaybackCapability;
  readonly decode: Decode;
  readonly encode: Encode;
  readonly features: Readonly<Record<string, CapabilityFeature>>;
  /** `null` in a browser and on the CPU renderer. */
  readonly adapters: AdapterChoice | null;
  /** `null` in a browser. */
  readonly memory: MemoryCapability | null;
}

/** Machine memory and the native host process's use of it. */
export interface MemoryCapability {
  readonly capacity: {
    readonly physicalBytes: number | null;
    /** Memory available now without paging. */
    readonly availableBytes: number | null;
    /** Lower of the cgroup or job limit and physical memory. */
    readonly limitBytes: number | null;
    readonly limitSource: "cgroup-v2" | "cgroup-v1" | "job" | "physical";
    /** Usage charged against the cgroup limit; `null` for a job or physical limit. */
    readonly usedInLimitBytes: number | null;
  };
  /**
   * Current and peak process memory. Resident includes shared pages; footprint is what the
   * OS charges (Linux anonymous plus shared resident, macOS physical footprint, Windows private
   * commit).
   */
  readonly process: {
    readonly residentBytes: number | null;
    readonly peakResidentBytes: number | null;
    readonly footprintBytes: number | null;
    readonly peakFootprintBytes: number | null;
  };
}
