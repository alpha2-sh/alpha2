/**
 * What an alpha2 runtime can do, in the one shape the native host's `capabilities` result and the
 * browser bridge both report (`alpha2-capabilities`, version 1).
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
  readonly textureBudgetBytes: number;
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

/** What a runtime can do. `Decode` and `Encode` are the runtime's own descriptions. */
export interface CapabilityReport<Decode = unknown, Encode = unknown> {
  readonly schema: "alpha2-capabilities";
  readonly version: 1;
  readonly runtime: "native" | "browser";
  /** The GPU, or `null` before a browser's engine has started. */
  readonly gpu: GpuCapability | null;
  readonly color: ColorCapability | null;
  readonly playback: PlaybackCapability;
  readonly decode: Decode;
  readonly encode: Encode;
  readonly features: Readonly<Record<string, CapabilityFeature>>;
}
