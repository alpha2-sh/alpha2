# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Changed

- **Breaking:** text runs carry a style (rich text). A run's `style` sets its own `font_family`,
  `font_weight`, `font_style`, `font_scale` (its size as a multiple of the text's font size, from
  0.1 to 10, keyframes in the holder's time), `letter_spacing`, `fill`, `strokes`,
  `background_color`, `underline` and `line_through` over its caption cue's, text generator's or
  layout node's style, as a CSS inline box does. Runs of another face, size or spacing are shaped
  apart with their neighbours as context and wrap with the rest; each line is as tall as its
  largest run (CSS 2.1 10.8), at least the text's own size, and a keyframed `font_scale` lays the
  text out again as it changes. Text styles and caption style patches gain `font_style`
  (`normal`, `italic`, `oblique`), `underline` and `line_through`. Faces match style before
  weight (CSS Fonts 4); a face without italics is slanted by 14 degrees, color emoji stay upright.
  Decorations take the font's own position and thickness. A text draws in at most 16
  `(family, weight, style)` selections. A layout node's `text` is a string or runs, whose runs take
  a static `style` only. Hosts and engines of 0.7.x refuse documents that use any of these.
- **Breaking:** a text holds up to 8 strokes (was 4, still at most 0.5 font sizes together) and
  every gradient up to 16 stops (was 8), in text, shapes, fills and layouts.
- **Breaking:** imported WebVTT and SRT `<b>`, `<i>` and `<u>` are drawn bold, italic and
  underlined, as WebVTT applies CSS to them; the runs keep their `b`, `i` and `u` classes.
- **Breaking:** `WasmTextRasterizer.rasterize` takes `runs` (text with per-run family, weight,
  style, pixel size, letter spacing, fill, strokes, background color and decorations) in place of
  `text`, and a `clamp`; a request with both or neither of `text` and `runs` is refused.

### Added

- `alpha2.transcribe` 1.1.0 runs whisper.cpp on the GPU on Apple silicon (Metal), about 1.2x to
  2.6x faster there by model size. A new param `device` (`auto`, the default, or `cpu`) turns it
  off. Words and times can differ from the CPU's in a fraction of a percent of words, as they do
  between CPUs. A model file whisper.cpp refuses after the plugin's own check accepted it no longer
  leaves its buffers allocated, which with Metal made the process abort when it exited.
- `ExtractResult.sourceDecode`: how the browser decoded each source of an extraction.

### Fixed

- `runExtractor` failed with a bare WebCodecs `OperationError` on a source whose software decoder
  the browser lists but cannot open, though the timeline decoded it; it now decodes such a source,
  and a codec with no software decoder, with the browser's own choice of decoder.
- `runExtractor` refuses a source no decoder reads with `DECODE_UNAVAILABLE` and its `facts`, frames
  without planes with `PLANAR_DECODE_UNAVAILABLE`, and a frame that cannot be read back with
  `FRAME_UNREADABLE`, after two retries.
- `@alpha2-sh/editor`: a decode refusal keeps its `facts` across the extract worker.
- `alpha2.transcribe` 1.0.2. Its Windows artifact was built without compiler optimization and
  transcribed 2x to 7x slower than it should; the build now restores CMake's release flags under
  MSVC. A window in which whisper.cpp advanced less than 0.16 s aborted the run (`PLUGIN_TRAPPED`,
  or the host itself under `--plugin-isolation in-process`), for any model; the build now narrows
  whisper.cpp's DTW median filter there as its development branch does. whisper.cpp's error lines
  are written as plugin diagnostics.

## [0.7.0] - 2026-11-26

### Added

- Resource slots: a plugin can load a model the user already has on disk. A request fills a slot
  with `plugin.supplied` (host protocol; `supplied` in the Node and Python plugin specs and in the
  mobile `Plugin.load`); `alpha2-host` takes such files only within `--plugin-resource-root`
  (`none` by default, `any` or a directory). A directory slot offers its files by relative name,
  through links, so a Hugging Face snapshot works as it is. Files are read and hashed only when
  the plugin asks for them, and a native plugin can ask for a path (`resource_path`) for runtimes
  that open or map a model themselves. The new code `PLUGIN_RESOURCE_INVALID` names the slot and
  the reason on every surface.
- `alpha2_host_core::plugins::Isolation::Child` and `plugins::worker::main` let another Rust
  program run native plugins in a worker process of its own.
- The Node and Python clients' `plugins` option gains `isolation` and `resourceRoot`
  (`resource_root`), and their errors gain `details`.
- A plugin manifest may carry `about` (`name`, `summary`, `author`, `homepage`, `license`) and an
  output `label` and `description`, plain text for interfaces that the engine ignores; Rust reads
  them as `PluginManifest::about` and `OutputSpec::label` / `description`, the mobile `Plugin` as
  `aboutJson()`. A loader before this release refuses a manifest that has them. `pack` writes a
  plugin crate's `about.json` as the manifest's `about`, and its notices also carry the license
  files one directory below a dependency's root.
- `docs/plugin-abi.md` lists the interface annotations a `params_schema` may carry (`title`,
  `description`, `default`, `x-alpha2-unit`, `x-alpha2-step`, `x-alpha2-display` (`percent`,
  `seconds`, `db`, and `box` or `point` for object params picked on the picture),
  `x-alpha2-order`, `x-alpha2-group`, `x-alpha2-advanced`), which every params check ignores.
- `alpha2.silence` 1.1.1 carries its `about`, an output label and annotated params, and is
  published with macOS and Windows native artifacts beside the Linux one. Its outputs are
  unchanged (`descriptor.version` 1).
- `alpha2-plugin-pack merge <pack-dir>... --out <dir>` joins packs of one plugin made on several
  machines: the first pack's manifest, wasm artifact, resources and licenses with the native
  artifacts of the others. It refuses packs of another id, version, descriptor, about, resources
  or licenses, a target two packs have, and a file that differs from what its manifest pins.
- `alpha2.scenes` 1.0.1 finds the cuts between shots as instants (`cuts`, `suggest: split_at`)
  and the stretches of black (`black`, `suggest: remove_ranges`) in a video, each with a `score`.
  It compares 4x4 grids of luma histograms by earth mover's distance against a threshold that
  adapts to recent motion and requires a cut to stand out from its neighbors, so fades,
  dissolves, flashes and camera noise do not cut. Params: `sensitivity`, `min_scene_ms`,
  `black_level`, `min_black_ms`.
- `alpha2.beats` 1.0.0 finds the beats and onsets in music as instants (`beats`, `onsets`) with a 0
  to 1 `score` (`suggest: split_at`), with an onset and loudness `envelope` (`scalars` `onset` and
  `rms`, normalized over the range, `suggest: bind`): log spectral flux, an autocorrelation tempo
  within `min_bpm`..`max_bpm` and dynamic-programming beat tracking; `sensitivity` sets the onset
  threshold.
- `alpha2.track` 1.0.0 follows a box picked on one frame (`region`) through the frames each pass
  delivers, forward and backward, from sources or the timeline: its center (`center`, points) and
  its translation, rotation and scale from the picked frame (`motion`, similarity), both suggested
  for binding, with frames whose box no longer matches the picked one by `confidence` marked
  lost.
- `alpha2.transcribe` 1.0.1 finds the words spoken in audio and their times, from a whisper.cpp
  ggml model the caller supplies in its `model` slot (none is pinned), run natively on the CPU
  with whisper.cpp linked in, in any ISO 639-1 language whisper knows or `auto`. It transcribes
  30 s windows as audio arrives and joins their overlaps without repeating them; it has no wasm
  artifact. On x86-64 it needs SSE4.2, AVX, AVX2, BMI2, FMA and F16C, and `begin` fails naming
  the missing ones on a CPU without them.
- A plugin crate opts out of wasm with `wasm = false` under `[package.metadata.alpha2-plugin]`:
  `alpha2-plugin-pack` then builds native artifacts only and refuses `--no-native`,
  `alpha2-plugin-pack builds-wasm <plugin-dir>` prints whether a crate builds wasm, and
  `scripts/pack-plugins.sh --no-native` leaves the crate out.
- `alpha2-plugin-pack` builds the same artifacts from any working directory and keeps the build
  machine's paths out of them: source, Cargo home, toolchain and target paths are remapped, for
  C and C++ too where the compiler is GCC or Clang.
- `alpha2-plugin-sdk` exports the engine's tick rate (`TICKS_PER_SECOND`) and data limits
  (`MAX_DATA_*`), and `Context::number_params` reads a table of `NumberParam`s with their
  defaults and bounds.
- `alpha2/plugins` exports `parsePluginManifest`, which checks a manifest by the engine loaders'
  rules without fetching anything, the `PluginManifest` types and the manifest limits
  (`MAX_MANIFEST_BYTES`, `MAX_PLUGIN_*`, `MAX_ABOUT_*`); `LoadedPlugin` gains `about`, and
  `loadPlugin` takes `onProgress`, called as each pinned file arrives.
- `bridge.extract` takes a plugin the app loaded with `loadPlugin`, which many runs share, or a
  `fetch` beside the manifest; its `onProgress` reports the plugin's files (`phase: "load"`).
- `bridge.extractCapabilities()` says what `bridge.extract` runs (the inputs of each media, the
  artifacts, whether it fills resource slots), and `pluginProblem(manifest, capabilities)` in
  `alpha2/plugins` says why a plugin would not run there. A host's `capabilities` result reports
  its plugin policy as `plugins` (`mode`, `slots`, `isolation`), typed in `@alpha2-sh/node`.
- `bridge.extract` reads the timeline: it draws the frames an extractor asks for on a CPU renderer
  of its own at export quality, the bytes every surface reads timeline frames as. Timeline audio
  is not supplied (`INVALID_INPUT`), as on the native hosts, and a timeline that draws runtime
  effects is refused, since the CPU renderer cannot draw them.
- The browser fills a plugin's resource slots: `loadPlugin(manifest, sha256, { supplied })` takes a
  `File` for a `file` slot and a directory handle or a directory picker's files for a `directory`
  slot, checked with the native loaders' limits and reasons, and the run records the files it
  read in `made_by.plugin.supplied`. `bridge.extract` passes `plugin.supplied` on.
- `bridge.extract` runs a plugin in a worker when `runtimeAssets.extractWorkerFactory` makes one
  running the new `@alpha2-sh/editor/extract-worker` entry, so a long extraction no longer holds
  the page's thread; a cancel terminates the worker at once. `transferablePlugin` and
  `restorePlugin` in `alpha2/plugins` hand a loaded plugin to a worker without fetching or
  compiling it again.
- `bindableProperties(project, owner)` in `alpha2/project` lists the properties of a clip, track,
  transition, composition node or the master bus that data can be bound to, in the path syntax of
  `dataReferences` and `bake`, with the data kinds each reads.
- `alpha2_engine::extract`: an extractor may offer memory for the next frame
  (`Extractor::frame_buffer`, `None` by default), which a supplier fills through
  `Supplier::frame_into` and `FramePlace` (by default it supplies as `frame` does) and the session
  through `ExtractSession::supply_placed_video` and `supply_video_into`; `supply_frame` answers a
  frame need that way, as `drive` does, and `present_for_extraction_into` presents a decoded
  frame into such memory.
- The browser bridge imports, relinks and reads image sequence frames from media references,
  `{ url, name, path? }` whose URL answers `Range` requests, in place of a `File`: every reader of
  the media (probing, hashing, preview and its workers, proxies, extraction, browser export) reads
  it by range. `alpha2/mediabunny` inputs and `alpha2/media` container readers also take a
  `RangeReader` (`size`, `read(start, end)`), and `sha256Blob` anything with `stream()`.
- A native export reads a referenced file by its `path` instead of uploading it: `nativeAssets`
  gives `{ path }` for it, and the native client sends such assets in the render request's
  `paths` (`{ [kind]: { [id]: path } }`) without an upload. `NativeAsset` has either `blob` or
  `path`.
- `bridge.putProviderSource` takes `then`: commands, or a function of the new source's ID and
  opened size returning them, committed in the same undo step as the source, such as the clip that
  places it.
- `failureClassOfHttpStatus` in `alpha2/export-failure` classifies an export service's HTTP error
  status: 413 `RESOURCE_LIMIT`, 429 and 503 `RESOURCE_BUSY`, 507 `STORAGE_QUOTA_EXCEEDED`.

### Changed

- **Breaking:** `alpha2-host` runs a native plugin in a worker process of its own,
  `alpha2-host plugin-worker`, one per run, by default (`--plugin-isolation process`): a crash, an
  abort or a hang of the plugin fails its request with `PLUGIN_TRAPPED` (`details.exit` or
  `details.signal`) or `PLUGIN_LIMIT` and the host serves on, instead of ending the host. A native
  call now ends at `--plugin-call-timeout` (300 s by default) instead of retiring the host after
  three stall limits, and a cancel kills the worker at once. `--plugin-isolation in-process` keeps
  the old behavior, for debugging a plugin under a debugger.
- **Breaking:** a plugin manifest's resource is either pinned (`url`, `sha256`, `bytes`) or a slot
  the caller fills (`supplied`, with `shape` `file` or `directory`, `required`, `max_bytes`,
  `accept`). In Rust, `PluginResource` is `{ name, source }` with `ResourceSource::Pinned` or
  `ResourceSource::Supplied`. The 4 GiB total counts pinned resources only.
- **Breaking:** `made_by.plugin` gains `manifest_sha256` (required: data a plugin made before this
  release is refused and extracted again) and `supplied`, the slot files the run read by name,
  SHA-256 and length, never by path.
- **Breaking:** `alpha2_host_core::plugins::load` takes a `LoadRequest` (manifest, its SHA-256,
  params and the supplied slot paths); `Policy` gains `resource_root` (default `ResourceRoot::None`:
  no slot may be filled) and `isolation` (default `InProcess`); `PluginExtractor::made_by` returns
  the provenance with the files read, complete once the run has ended; `PluginError` and
  `ExtractorError` gain `details`. A native artifact's `Fetch::prepare_native` is called for every
  isolated run.
- **Breaking:** `HostV1` gains `resource_path`, appended under the growth rule; the SDK takes a host
  table down to `HOST_V1_BASE_BYTES` (`ALPHA2_HOST_V1_BASE_SIZE` in the C header, which tests an
  appended field with `ALPHA2_HOST_HAS`) and its `Context` gains `supplied`. The mobile
  `Plugin.load` takes `supplied` (absolute paths; a relative one is refused with
  `INVALID_REQUEST`), and `Alpha2Error::Failed` gains `details`.
- **Breaking:** `GpuSubmissionStats` gains `over_budget` (submissions still predicted over the
  submission budget: passes that could not be cut finer, and frames spread past the per-frame
  submission cap) and `budget_ms`; the host's and the
  browser's `gpuSubmissions` add `overBudget`, and the host prints a warning when it is nonzero.
  `alpha2_gpu::FrameMeasurement` gains `device_rated_ms`.
- **Breaking:** every YUV pack in `alpha2-render` (the GPU readbacks, the NV12 pack and the CPU
  renderer's packs) refuses an output color with an HDR transfer or primaries other than BT.709
  or BT.601 with the new `RenderError::UnsupportedOutputColor`, coded `INVALID_INPUT`. The packs
  follow only the color's matrix and range, so such a tag would have described other pixels than
  the planes hold. No binding requested such a color.
- **Breaking:** Display P3 sources (H.273 primaries 12, `smpte432` in WebCodecs) are converted into
  BT.709 on import, as BT.2020 sources are, instead of being read as BT.709. `ColorPrimaries` gains
  `DisplayP3` (`display_p3` in projects). Under the BT.709 output their pictures are neither
  transcoded nor packet-copied, and the packs refuse a Display P3 output color.
- **Breaking:** effect and composite passes round their results to the nearest half float, ties to
  even, before the intermediate stores them, and full edge coverage is computed exactly (one minus
  what the edges cut off). Stored values no longer drift down with every composite, blur, transition
  or edge-sampling effect, and unsharp masks no longer drift up: a pass that computes a stored value
  to within half a step stores that value. Exported codes move by at most a few half steps per pass,
  mostly at bright levels; pictures at power-of-two canvas sizes drawn through canvas-size layers
  are unchanged. The levels, lift-gamma-gain, alpha-adjust and color-controls shaders skip `pow` at
  an exponent of one, as the CPU renderer does (alpha-adjust now does too).
- **Breaking:** GPU loss reports count submissions whose completions were seen together as one
  group, whose time is their total. `SubmissionReport` and `RenderError::GpuDeviceHung` gain
  `submissions`, and a hung group's message reads "n submissions from frame F on completed
  together in t ms". The host's `failure.diagnostics.gpuSubmission` renames `maxMs` to `gpuMs` and
  adds `submissions`; the browser's `lossReport` and `GpuLossReport` add `submissions`. Both
  describe the submission that made a loss hung, else the longest recent one.
- **Breaking:** Loss reports say whether a time was measured: `SubmissionReport` and
  `RenderError::GpuDeviceHung` gain `measured`, as do the host's `gpuSubmission`, the browser's
  `lossReport` and `GpuLossReport`. A time that ends at a completion seen late is an upper bound,
  and the hung message then says "ran up to t ms in one submission" or "completed together within
  t ms".
- **Breaking:** GPU exports to layouts other than 8-bit 4:2:0 (`yuva420p`, `yuv420p10le`,
  `p010le`, `yuv422p10le`, `yuva444p10le`) pack each frame on the GPU, in the frame's submission,
  into integer planes of the output layout, and read back only those planes (2.5 to 8 bytes a
  pixel instead of the intermediate's 8, which also shrinks `stagingBytesPerFrame`). Codes are the
  CPU pack's within one at rounding ties; the CPU renderer keeps packing on the CPU. The readback
  no longer packs on the caller's thread, so `readbackPackMs` is a plane copy; `gpuPackMsPerFrame`
  covers the pack passes and their copy-out. Rust: `layout_staging_bytes` takes the `PixelLayout`
  instead of the texture format, and `Yuv420pReadback::plane_layout` returns every plane (a `Vec`).
- **Breaking:** GPU exports to those layouts render a frame and read it back in one submission
  instead of two, sample its GPU time (`gpuRenderMsPerFrame`, `gpuPackMsPerFrame`) and report the
  submit stages (`frameQueueSubmitMs` and the other stage fields).
  `gpuTimeUnavailable` tells apart an export that drew no frame on the GPU, one that drew too few
  for one to be sampled, and one whose samples were not read back in time. Rust:
  `GpuFrameRenderer` gains the required `render_frame_layout_readback`, `Renderer` gains
  `render_graph_layout_readback`, and `FrameGpuTimes` gains `timed_frames` and `unread_frames`.
- **Breaking:** an export's in-flight plan gives up encode channel slots, down to a rendezvous,
  before it gives up a frame in flight: where the in-flight budget holds the requested readbacks
  beside the least the encoder side needs, they are kept (ProRes 4444 at 3840x2160 under the
  default budget now keeps two frames in flight and no queued frame, where it kept one and one).
  A pinned encode queue depth (`ALPHA2_ENCODE_QUEUE_DEPTH`) still keeps its slots first.
- **Breaking:** `Nv12PlaneTargets` takes default views of the NV12 plane textures instead of the
  textures, and `HardwareEncodeInput::planes` returns the views it keeps with its plane textures,
  so packing or repacking a frame into an encoder's planes creates no texture view.
- **Breaking:** `runExtractor` takes no `plugin`: a plugin's extractor records `made_by.plugin`
  itself (`Extractor.madeByPlugin`), with the slot files that run read.
- **Breaking:** the browser's `loadPlugin` checks the whole manifest as the native loaders do
  (unknown fields, URLs, limits, the descriptor's rules) before it fetches a file, and refuses a
  file as soon as more bytes arrive than the manifest pins. `LoadedPlugin.descriptor` is the
  descriptor as the engine reads it, with `max_size`, `block_frames` and `params_schema` filled
  in.
- **Breaking:** `alpha2-native-gpu`'s Metal import takes a raw `CVPixelBufferRef`
  (`SharedMetalContext::import_pixel_buffer`, `pixel_buffer_planes`, `MetalImportError`) under a
  new FFmpeg-free `iosurface-import` feature for every Apple target; `import_surface` and
  `encoder_planes` remain as the `hwdevice` adapters on macOS.
- **Breaking:** `alpha2_host_core::export::export_video` takes the renderer together with
  optional `EncoderTargets`; `VideoEncodeBackend` and `VideoSink` gain `lender`, which lends
  encoder input buffers (`LentBuffer`, `PoolLender`, `SinkLender`).
- A native plugin in a worker process reads its video frames from memory the host shares with that
  worker alone, read-only there: the host presents each delivered frame straight into that memory,
  so a 1080p RGBA frame is not copied on its way to the plugin. Where that memory cannot be made
  or mapped, the run goes through the worker's pipe and the host writes one diagnostic. On the
  shared path a plugin that writes into its input frame, which the ABI never allowed, fails its
  run with `PLUGIN_TRAPPED`. In Rust, `WorkerCommand::frames` (`FramePath::Shared` or
  `FramePath::Pipe`) chooses the path, and `alpha2-os` gains `SharedRegion`, `MappedRegion`,
  `write_with_descriptor` and `DescriptorReader`.
- A GPU frame writes its uniforms (each source's conversion, the graph's passes and the YUV or
  NV12 pack) into one buffer with one queue write, where each conversion and the pack had a buffer
  and a write of their own.
- When the cap at a quarter of the GPU memory budget lowers a texture budget the caller set (an
  export's or a preview's, not only the start-up default), `RenderDiagnostics::degradations`
  reports `CapabilityDiagnostic::TextureBudgetReduced`, and `alpha2-host` prints the requested and
  applied budgets once per change.
- The renderer keeps bind groups by what they bind and reuses them on later frames while every
  texture, upload and uniform arena they bind is its own; they leave with those resources or after
  64 frames unused. Uniform blocks are bound with dynamic offsets. A warm frame of an unchanged
  graph creates no bind group. `RenderDiagnostics::bind_groups` (`BindGroupStats`) counts created,
  reused and kept bind groups; the browser and host diagnostics report them as `bindGroups`, and
  export metrics as `bindGroupsCreated` and `bindGroupsReused`.
- Every sampling pass binds one bilinear, edge-clamped sampler in a bind group of its own
  (`@group(1) @binding(0)`), made once per renderer and per `SurfacePresenter`, so the bind groups
  made per pass hold no sampler (wgpu's D3D12 backend creates a buffer for each group that does).
  The WGSL the runtime effect wrapper generates declares `source_sampler` there and renumbers group
  0: parameters at 2, the runtime block at 3, resources from 4, named inputs from 20. Effect bodies
  name these, not numbers, and are unaffected.
- iOS: hardware-decoded frames stay on the GPU. The renderer samples `VideoToolbox` pixel buffers
  as Metal textures, as Android imports decoder buffers, in exports, stills and the `Player`; the
  export report's `surfaceFrames` counts them. A pixel buffer the GPU refuses is copied through
  memory instead, once reported as a diagnostic and counted as `surfaceFallbacks`.
- iOS: an export renders each frame straight into the H.264 or HEVC encoder's pixel buffer
  instead of reading it back; the report's `framesRenderedInPlace` counts them.
  `VideoFrameMode.cpu` turns both off.

### Removed

- **Breaking:** `PluginExtractor::is_native`; `PluginExtractor::calls_are_bounded` says whether its
  calls end at the policy's call deadline.
- **Breaking:** `alpha2-render`'s `SdrOutputTransfer` and `encode_linear_rgb`. Nothing outside
  tests called them, and the export path never applied either curve: it writes the working
  space's sRGB-coded values and tags them BT.709.
- **Breaking:** the motion blur subframe count, which no pass read. Removed from Rust:
  `QualityProfile::motion_blur_subframes` and `MAX_MOTION_BLUR_SUBFRAMES`,
  `RenderQualityControls::motion_blur_subframes`, `RenderPlan::temporal_offsets`, and the
  `motion_blur_subframes` fields of `RenderDiagnostics` and `BenchmarkMetadata`; the browser
  diagnostics drop `motionBlurSubframes`. `set_quality_controls` (on `Renderer` and in
  `RenderControls`) takes only the filter sample budget, so a quality change drops kept results
  and moves the content epoch only when that budget changes. The plan no longer depends on
  quality: `RenderPlan::filter_sample_limit` and `RenderGraph::compile_with_quality` are removed,
  and `RenderGraph::compile` is the one way to compile. Graph cache keys change value.

### Fixed

- A wait on a split frame's readback waits for each of its submissions in turn under one deadline,
  so the completions it sees are stamped as they happen and the clock times each submission
  exactly, where it waited on the last one and saw the others complete together.
- A GPU frame whose own textures need more than the GPU memory budget the adapter reports fails
  before it draws with the new `RenderError::FrameExceedsGpuMemory`, classed `OUT_OF_MEMORY`,
  instead of evicting idle textures it could not do without; the quarter-of-the-budget cap bounds
  only idle textures no recent frame used. An export that runs out of GPU memory at the smallest
  in-flight profile now moves to the CPU renderer when the client allows it, where it stopped;
  other memory failures (`resource` other than `gpu`) still stop there. In Rust,
  `export_policy::AttemptFailure` gains `resource` (`MemoryResource`).
- Layer composites, vector scenes, source conversions and packs get pass rates of their own, as
  effect passes did: a planned frame pilots each kind it draws past the planning threshold once per
  device and frame size, and plans it at what the pilot measured when that is slower than the
  device. Pilots now draw before the frame's encoder opens, so their scratch textures are no longer
  held beside the frame's until it is submitted.
- A frame that runs over its prediction raises the device rate only by what its work planned at
  the device rate took; work planned at its own measured rate no longer moves the device rate.
- A pass predicted at more than 64 submission budgets is no longer drawn in 64 bands each over the
  budget: past 64 bands it is cut across as well, into up to 16 tiles a band of at least a 2x2
  quad. A frame is at most 128 submissions: one predicted past 96 budgets, at the rates its
  passes measured, is cut into longer submissions, spread over the rest of the frame and counted in
  `overBudget`, so a frame cut finely under a short watchdog no longer outruns a backend's limit on
  outstanding command buffers (Metal refused the 4097th and the export failed as a hung device).
- A vector scene is charged for the tiles each fill covers and its paint, the layers it composites
  and its final encode, instead of a fixed charge whatever it draws, and each of its draws is a
  placed pass: a scene of many fills is cut between them (continuing with a load, not a clear) and
  a fill predicted past the submission budget is drawn in scissored bands with the same pixels.
- A layer composite is no longer one submission however many layers it draws: each batch of layers
  is a placed pass, charged for the pixels it draws and the filter samples each layer takes there,
  and a batch predicted past the submission budget is drawn in scissored bands with the same
  pixels.
- A GPU frame's source conversions (charged by the output pixels and the samples their filter
  takes), its YUV or NV12 pack and its texel copy are charged to its plan and count toward the
  planning threshold: a frame that only converts large sources is now planned, and a conversion
  or pack predicted past the submission budget is cut into bands like an effect pass.
- A pass that writes more uniform data than its frame reserved fails that frame with the new
  `RenderError::UniformArenaOverrun`, coded `INTERNAL` (a new engine code, class `DEFECT`),
  instead of panicking the rendering thread. A frame also resolves each effect's registration once,
  so another thread registering an effect while a frame encodes can no longer cause one.
- Each submission of a split frame names the effects encoded into its own command buffers in loss
  reports; the frame's first submission named every effect of the frame and the others none.
- A GPU loss after a heavy frame split into short submissions is reported `GPU_DEVICE_LOST` and
  retried, where it was reported `GPU_DEVICE_HUNG` and not retried on the GPU. Completions one wait
  sees together now implicate the export only when their total also ran more than twice its
  predicted time; a loss that recurs at the same frame still stops as hung.
- The renderer's submission clock forgets only submissions that completed. A loss again names a
  running submission however many were queued behind it, a browser's calibration and pilot draws
  are read however many submissions the frame behind them makes, and the oldest submission kept
  no longer counts its wait behind a forgotten one as GPU time.
- A GPU-resident export of a clip whose container and coded stream declare different colors no
  longer fails with "a decoded picture is ..., not the output's ...". The YUV transcode is planned
  from the color the stream itself declares (the H.264 or HEVC VUI), and a picture that still
  differs from the output is rendered instead of failing the export.
- A packet copy (`strategy` `prefer-copy` or `require-copy`) also compares the color the H.264 or
  HEVC stream itself declares with the output's, and no longer copies a stream whose VUI contradicts
  its container's `colr` box: such a copy kept the stream's own tags under a BT.709 container.
- Exports through the readback path carry their color in the coded stream as well as in the
  container: H.264, HEVC and AV1 packets pass through FFmpeg's metadata bitstream filter, as on the
  GPU-resident path, so encoders that write no color description (Media Foundation's H.264 on
  Windows) are tagged too, and frames reach the encoders tagged, so ProRes frame headers say BT.709
  instead of unspecified.
- Exports whose encoder keeps a frame after taking it (ProRes) no longer allocate, zero and fill a
  new encoder frame with the previous picture for every output frame; the frame pool grows to the
  encoder's delay instead, and `encoderFramesAllocated` counts every frame it allocated.
- The renderer keeps, per texture size and format, as many idle intermediate textures as the
  last frames had in use at once, whatever the texture budget; the budget now bounds only idle
  textures no recent frame used. Exports whose frames outgrow the budget (7680x4320 and larger
  under the default export quality, or many stacked clips) no longer destroy and recreate textures
  every frame, and no longer hold a destroyed texture beside its replacement until the frame is
  submitted.
- The CPU renderer's frame buffer pool follows the same rule: per buffer length it keeps as many
  buffers as the last frames took at once, so a 7680x4320 export no longer frees and allocates a
  frame buffer every frame.
- A GPU watchdog under 8 ms (`ALPHA2_GPU_WATCHDOG_MS`) no longer rounds the submission budget down
  to 0 ms, which cut every planned pass into its most pieces; `alpha2_gpu::submission_budget_ms`
  returns an `f64`.
- The `directional` transition no longer blends frame-edge pixels on the axis it does not move
  along with the opposite edge on devices whose interpolation rounds inward: it finds its seam by
  distance, as `directional-easing` and `directional-scaled` do.
- The Python client's params check words an `enum` or `const` refusal as `schemaProblem` does.
- macOS: the Metal interop helpers drain an autorelease pool, so the Objective-C objects they
  autoreleased no longer leak on the host's long-lived worker threads.
- The native client treats a 404 to an asset upload or a render request as a job the service lost,
  so the next attempt allocates and uploads it again instead of failing three times, and no longer
  retries an upload or a project the service refuses as too large (413): the export fails at once
  with `RESOURCE_LIMIT` and the service's `limitBytes` in `diagnostics`. Its job failures are
  `ExportFailureError`s with the service's message.
- `bake` of a linear source-anchored data set read through a clip no longer stops the motion at
  the clip's last sample: the baked keys follow the live value toward the next sample up to the
  clip's end, at loop pass boundaries, and over the frames a reversed pass holds.

## [0.6.2] - 2026-11-20

### Fixed

- The GPU planner learns its device rate only from frames it measured: a frame counts when its
  start is known (the queue was idle at its first submit, or the submission before it completed
  inside a wait), so frames whose completions arrived together no longer measure zero and lower
  the rate. Effects with scattered reads ran 4 to 190 times past their prediction and
  past the GPU watchdog on several device classes; the first planned frame that draws an effect
  pass heavy enough to be planned alone now draws one band of it first and plans the pass at the
  rate it measured when that is slower than the device rate.
- A pilot draws the rows predicted at 4 ms at the device rate, at least a sixty-fourth and at most
  a quarter of the frame, so a pilot of a small pass on a phone GPU no longer measures mostly
  submit latency and cuts the pass in two, and a pilot of a pass predicted at seconds no longer
  reaches the watchdog itself. A failed pilot is not drawn again on every planned frame.
- Calibration keeps the fastest of three waited draws, and a draw that confirms the old
  calibration keeps the rate learned since, so a 4K export of built-in effects is no longer cut
  into twice the submissions it needs.
- A browser keeps its pilot and calibration draws in the submission clock (256 entries instead of
  32), so a frame cut into many submissions no longer pushes them out before they are read.
- The Python client's error for a host that did not start in time keeps what the host wrote to
  standard output before it failed.
- `wasmtime` 50.0.3 for RUSTSEC-2026-0566 to RUSTSEC-2026-0572.

## [0.6.1] - 2026-11-18

### Fixed

- The Windows `alpha2-host` wheel leaves out the debug symbols (`.pdb`), which stay in the release
  archive, so it fits PyPI's file size limit. The `alpha2` 0.6.0 wheels never reached PyPI, so
  0.6.1 is the first release there with the 0.6.0 changes.

## [0.6.0] - 2026-11-18

### Added

- Native mobile targets, as a preview: the `alpha2-mobile` crate with a uniffi API for iOS and
  Android apps, and the scripts that build an Android library with Kotlin bindings and a Swift
  package over a static XCFramework. No Android library or Swift package is published with 0.6.0.
- Mobile export and preview over platform codecs (MediaCodec, VideoToolbox, AudioToolbox).
  Hardware-decoded frames stay on the GPU on Android; iOS copies decoded and encoded frames through
  memory. Apps read the library's diagnostics through a listener (the system log under `alpha2`
  on Android by default), and a decoder or encoder session the system takes away in the background
  is replaced. Known limits: the player always draws on a GPU device that presents to its window
  (`RendererMode` governs exports and stills), `conform` reports only whether each source's file
  opens and ignores `purpose`, Android codecs are opened by MIME type or a fixed component name,
  without `MediaCodecList` discovery, and Android decodes 10-bit video (H.264, HEVC, VP9, AV1)
  only from Android 13 on. A forced keyframe lands on its frame on Android, so a repeated export is
  byte-identical.
- `alpha2-container`: MP4/MOV, Matroska/WebM, WAV, MP3, ADTS, FLAC and Ogg demuxers, and MP4 and
  Matroska/WebM muxers, with FFmpeg packet parity and memory and work bounded per file.
- `alpha2-host-core`: the policies, audio timelines, cursor rules, providers, plugin runtime,
  request parsing and protocol frames every native host shares, and the FFmpeg-free decode, export
  and still pipeline (PNG, JPEG and WebP with Exif orientation) the mobile hosts run.
- Compose templates that bind typed parameters into plain specs, with a data schema, lint and `$if`
  on any value. The title, card, lower third, logo, reframe and comparison grid nodes are now
  first-party templates under their old names, beside new circle and wipe reveals.
- Compose nodes: `box` layout, `shape`, `group`, `matte` and `mask`; animated values (constant,
  ramp, keys); theme lengths and colors.
- Text fits its font size to its box and clamps lines with an ellipsis.
- Compose everywhere: `alpha2/compose` in the browser (with its wasm module `alpha2/compose-wasm`),
  a `compose` operation in `serve`, and compose on mobile behind a default `compose` feature.
- `python -m alpha2 mcp`: an MCP server in the Python package that lists, binds, checks and renders
  templates and specs. It reads and writes only under its roots (the working directory or each
  `--root`), and `--read-only`, `--max-seconds` and `--max-pixels` bound what a call may do.
- The compose schema lists every built-in effect with its parameter types and ranges. Schemas carry
  their alpha2.sh ids, and a core compose schema leaves out engine nodes.
- `serve` reports its state: `ready`, or `draining` once a new process would serve better (memory,
  memory growth, requests served, open handles, a lost device or encoder path, a caught panic, idle
  time), with soft limits set by `serve` flags. Requests it did not start are answered
  `HOST_RETIRING`, and a `status` operation reports the state at once. A request that stays in one
  step for three times the stall limit is answered with that step's code and the host retires.
- `@alpha2-sh/node` and the Python client keep their hosts between requests, up to `concurrency`,
  one request at a time each, in the order requests were made, and replace a host that drains or
  exits. Hosts are kept apart by the environment they start with, and a host of another release is
  refused at once with `HOST_VERSION_MISMATCH`. `Alpha2NodeClient.close()` and `HostClient.close()`
  end them; idle hosts keep no Node or Python process alive. The MCP server reuses hosts across tool
  calls.
- A `still` without an `output` returns its PNG in the response instead of a file.
- `serve` runs up to `--max-running` requests at once on its one GPU device, 1 by default. With a
  higher count, requests share the device: budgets and codec threads are not divided, and an
  export's GPU counters cover the requests that ran with it.
- Data sets in the project: `points`, `scalars`, `similarity`, `words` and `intervals`, anchored to
  a source or the timeline. Animated values can follow a data set, a clip can take a similarity
  correction from one, and a caption track can take its cues from words. A data set with times past
  its source's recorded duration loads, and `lint()` reports it as stale.
- Extractors: code outside the engine is supplied a range's frames or audio and returns data sets.
  The host has an `extract` operation (`client.extract` in Node, `HostClient.extract` in Python),
  the browser has `runExtractor` in `alpha2/extract`, and mobile apps have `Alpha2.extract`.
- Extractor plugins under the `alpha2-plugin/1` ABI, as wasm or native artifacts pinned by SHA-256
  in a manifest. The browser loads the wasm artifact with `loadPlugin` from `alpha2/plugins`, and
  mobile apps load bundled ones when the library is built with its `plugins` feature. The host
  fetches them into a private, bounded cache and runs them under a policy its operator sets at
  start: `--plugins off|wasm|native` (default `wasm`), `--plugin-origins` and `--plugin-root`,
  refused with `PLUGIN_REFUSED`; the clients take a `plugins` option for these flags. A wasm plugin
  has one memory of at most 2 GiB, and in the host cancel and the call deadline reach inside its
  calls. The SDK, the pack tool and a first-party silence plugin live in `plugins/`; no release
  contains a plugin.
- Edit operations `remove_ranges`, `split_at` and `bake`, with a report of what moved and of each
  new clip with the clip it was cut from (`created`: `{id, from}`): an `edit` host operation
  (`client.edit` in Node), `alpha2.edit.apply` in Python, `applyEdit` in `alpha2/apply-edit` and
  `applyEdit` on mobile. Kept moments render as before, including track and master automation,
  timeline-anchored data and transitions with their keyed parameters. A cut that cannot be applied
  exactly is refused (`matte_source`, `read_through`, `in_transition`, `in_fade`), as is a result
  past a limit (`too_many_clips`, `too_many_cues`), or, in `split_at`, left out and listed in
  `report.skipped`, and `bake` clamps each value to its property's range. Known limits: parts of a
  reversed clip whose edges are off the frame grid, and sound at a loop wrap a cut turns into a part
  boundary, are not exact.
- Conform reports data sets made from an original that has since changed.
- `@alpha2-sh/editor` edits data: `data.put` and `data.remove` commands (removal refused while
  anything reads the set), `dataValue` and `isDataBound`, `editProject` and `applyEdit` to commit an
  edit operation's result as one undo step, and `extractionProject` and `applyExtraction` to commit
  an extraction's data sets. Both refuse a result computed from a document that has since changed
  in what they read. The browser runtime runs edits and extractions itself (`applyEdit`,
  `extract`). Undo steps can carry a label (`undoLabel`, `redoLabel`). Clips may share a
  composition; an effect command on one of them copies it first.
- `dataReferences` in `alpha2/project` lists every place that reads a data set.
- An export result's `performance` splits the render time into set-up (encoder probe and open,
  loudness), the first frame and the finish, and reports `steadyFps`. Progress reports carry
  `restarted` on the first report after an export started over.
- `alpha2 features` lists the optional features the `alpha2` binary was built with.
- `SECURITY.md` and `docs/security.md`: how to report a vulnerability, and what the engine bounds
  when it reads untrusted projects, media, fonts, extension sources and plugins.

### Changed

- Host protocol version 2: every message is a length-prefixed frame, a JSON header and binary
  attachments, instead of a JSON line. `serve` owns standard output: it writes a magic before its
  first frame, and anything else the process prints goes to standard error. Headers are UTF-8, and
  an answer whose header would pass 256 MiB is answered `RESOURCE_LIMIT`.
- Every host request refuses a field it does not know with `INVALID_REQUEST` naming the field.
- Both clients send request paths absolute, resolving a relative one against the working directory
  at the time of the call.
- `conform`'s `purpose` takes the conform report's spelling (`export_original` and so on) on the
  wire and on the command line; a compose `compile` answer's `loudness` is the object `export`
  takes, whose `truePeakCeilingDbtp` defaults to -1 dBTP.
- A new failure class `DEFECT` for an engine fault (a caught panic, a wasm trap), retried once on a
  fresh host or worker. A host whose request panicked answers it `INTERNAL` and drains.
- Native sources decode only with a fixed list of 48 FFmpeg decoders (the documented codecs and
  mainstream production codecs, reported by `capabilities`); `--allow-every-decoder` lifts the list
  for the process.
- Project documents bound their sequences while they are parsed (the schema states every limit as
  `maxItems`), and layout and text values are bounded in magnitude, not only finite. New limits:
  100,000 keyframes per animated value, 10,000 effect resources, 65,536 passes per looping clip and
  canvas-space follows at most 8 links deep.
- An export whose range shows a composition that cannot draw fails before its first frame.
- The Python package no longer installs an `alpha2` command, which shared its name with the native
  archive's media program; run `python -m alpha2 <command>` instead.
- The published crates require their alpha2 siblings at exactly their own version (`=x.y.z`).
- Compose facts say whether a video has sound (`has_audio`) when a spec plays it unmuted; facts
  without it compiled the video silently.
- Editor ripple edits that reach every track cut or open timeline-anchored data sets, track and
  master automation and markers as the engine's `remove_ranges` does.
- Built-in effects and transitions are validated against their published definitions. Sharpen and
  film grain are sized in project pixels, so a preview at a lower scale matches the export.
- Mattes and masks spread over transitions that commute with coverage, such as wipes.
- The CPU renderer presents portrait and downscaled video and RGBA stills about five times faster,
  and HDR, non-BT.709 and alpha-plane sources through tabled curves (pictures move by under 0.05 of
  a half-float step of the brightest channel).
- Builds link the pinned FFmpeg ahead of system library directories. A bundled archive's FFmpeg
  libraries may be replaced only by a build of the same 8.1.x series and configuration, and
  hardware interop is off when the loaded libavutil is another minor release.
- An editor import refused because this browser decodes none of a video is `unsupported_media`, as
  an HDR import or an export refused for the same reason is, and its `cause` is a
  `DecodeUnavailableError` (`DECODE_UNAVAILABLE`, with the track's `facts`); it was
  `unreadable_media` with the browser's error.

### Fixed

- Decoding video or images whose pixels are 8-bit 4:2:0 with alpha (VP9 with alpha in WebM or
  Matroska, lossy WebP with alpha) wrote up to 60 bytes past a heap buffer on x86-64 at any width
  that is not a multiple of 16, or left the rightmost 1 to 7 columns transparent; on arm64 the last
  column of an odd width stayed transparent. A host could abort with a heap-corruption message or
  crash later, or show a transparent strip at the right edge of the layer, in preview and export.
  Every earlier release is affected (0.5.0 to 0.5.3 for VP9 alpha, 0.1.0 to 0.5.3 for WebP alpha);
  hardware decode does not avoid it, because alpha sources always decode in software. This is a
  memory-safety fix and updating is the only remedy. Odd-width alpha sources change in color by
  about one code value. Encoding RGBA frames of 6 pixels or less across through `alpha2 transcode`
  or the crate's encode sinks wrote past a buffer the same way.
- Video stored as red, green and blue planes at 9 to 15 bits (for example FFV1 `gbrp10le`) is
  drawn at its full level on both renderers; it rendered almost black.
- Animated captions and titles that differ only in an animator or in their background box no
  longer draw with each other's pixels, in one frame or after an edit, in preview and export.
- The GPU composite draws a layer shown 1:1 on a canvas wider than 1920 pixels exactly, as the CPU
  renderer does, and leaves the picture below a transparent layer unchanged.
- 4:2:2 and 4:1:1 sources that declare no chroma siting (ProRes 422, DNxHD, DNxHR) read their chroma
  co-sited with even luma columns, as BT.601 and BT.709 site it; before, they read it centred over
  the luma columns it covers.
- Three or more clips that overlap on one track draw every transition between them in a defined
  order.
- A position keyframed along a curved path moves the distance its timing asks.
- Opening audio whose first packet timestamps are far apart no longer hangs, and timestamps on a
  coarse grid count as rounded only when one constant period explains them.
- A layout grid line of -32768 is refused instead of panicking the validator, and `validateProject`
  no longer throws on deeply nested objects or array holes.
- A video that loops in compose refuses a `speed` other than 1 instead of ignoring it.
- The sound of the two sides of a compose sequence join crosses over the transition; both played
  at full level over the overlap. A compose spec's `duration` is the project's length.
- A terminated host exits within the stall limit plus 5 seconds even when its request cannot stop or
  nobody reads its output.
- A 10-bit export on a host whose renderer stores 8 bits is refused with `INVALID_INPUT`, a code the
  protocol lists.
- Under a cgroup memory limit, file page cache no longer counts as used, so a container whose cache
  filled its limit no longer refuses every export; the memory watchdog keeps watching after it
  stopped an export.
- An export whose file is in place answers `ok` even when the encoder teardown afterwards fails,
  and a cancelled export stops copying its staging file.
- The render cache keeps baking after one failed write and bounds what one damaged file can cost.
- A readback whose mapping result never arrives fails with `GPU_TIMEOUT` instead of waiting forever.
- Crafted input no longer panics, hangs or exhausts memory: the browser's container readers refuse
  fields of impossible size, native hosts refuse pictures past 16384 pixels per side or 2^26 pixels
  before FFmpeg allocates them, caption import is linear and bounded per cue, SVG, PDF, Lottie and
  OpenEXR sources are charged for what drawing them costs (reference rings refused), runtime effect
  loops, `else if` chains and arrays are bounded, and fonts are checked when they are registered.
- Browser: a frame split into several GPU submissions fails when one was rejected; a lost GPU
  device or WebGL context is refused with `GPU_DEVICE_LOST` instead of drawing nothing; and an
  export cancelled while finishing is discarded.
- Editor: an export whose audio this browser cannot decode is refused instead of written silent
  (`undecodableAudio: "silence"` keeps the old result); one kept proxy that cannot be staged no
  longer blocks the whole project, waits on workers and media elements are bounded, and preview
  audio failures stay with their lane.
- H.264 levels account for macroblocks per second, not only frame size.
- Python functional `TypedDict` fields name their types as strings, so later types resolve.
- `sha256Blob` hashes in pieces sized to about 2 ms at the measured rate instead of fixed 1 MiB
  pieces, so the page stays responsive on slow machines.
- OpenEXR sources are read with exr 1.74.3, which no longer panics on damaged B44 blocks or reserves
  unbounded memory for damaged DWA data.
- Windows: an export to an output whose previous host was just killed waits up to 2 seconds for
  Windows to let go of the old partial file, instead of failing with `OUTPUT_IN_USE` or a sharing
  violation (`os error 32`).

## [0.5.3] - 2026-11-04

### Added

- LaTeX math sources: a provider source `{ format: "latex", latex, font_size }`, with optional
  `color` and `display`, typeset with a bundled STIX Two Math font. It reads no file, so it has no
  locator or fingerprint.
- PDF page sources (`format: "pdf"`, with `page` and `background`), drawn through the SVG provider.
  Decoded streams and image pixels are bounded before a page converts.
- LaTeX and PDF sources draw in the native host, in browser preview and export, and in the editor,
  where `putProviderSource` adds or edits a file-less source in one undoable step.
- PDF support ships as its own wasm module (`alpha2/extensions-pdf-wasm`,
  `alpha2_extensions_pdf_bg.wasm`), loaded only when a PDF source opens. `loadExtensions` also
  takes module URLs by name (`{ core, pdf }`); the editor takes `extensionModuleUrls`.
- `alpha2/wasm-module`: `WasmModule` loads a wasm-bindgen module once per realm and restarts it
  asynchronously.
- An extension module restarts asynchronously after a trap. Until `ready()` resolves, calls throw
  `ExtensionModuleRestartingError`.

### Changed

- **Breaking:** the minimum supported Rust version is 1.99 (was 1.88).

### Fixed

- The TypeScript project validator now checks clip audio `stream` and `channels` selections, as
  the Rust validator does.

## [0.5.2] - 2026-11-04

### Changed

- The CPU renderer reuses its band scratch and frame buffers between frames. This removes the
  slowdown that the 0.5.1 `serve` allocator arena cap added on the CPU path. The buffers are freed
  when each request ends, so an idle host holds none.

### Fixed

- At startup the host hashes its own executable in chunks. It used to read the whole file into
  memory, which raised glibc's mmap threshold for the rest of a `serve` process.

## [0.5.1] - 2026-11-03

### Added

- Clips can choose an audio stream (`stream`, counted over the file's audio streams) and up to two
  source channels (`channels`: one plays as mono, two as a stereo pair). Supported in the project
  and compose schemas and in the native and browser hosts. Conform reports each original's audio
  streams.
- Compose motion presets:
  - `enter` and `exit` (`fade`, `rise`, `slide`, `scale`) on visual nodes and containers.
  - `stagger` on stacks and grids.
  - Word or line `reveal` on text.
  - A `counter` node that counts between two numbers, with prefix, suffix, decimals and thousands
    separators.
  - Easings by CSS name.
- JS helpers `frames(rate)` (frame-exact snapping on the engine's tick grid), `linearGradient`
  and `radialGradient`. The Python package adds `frames`, `cubic_bezier`, `EASING_PRESETS`,
  `linear_gradient`, `radial_gradient`, and the `Fade`, `Rise`, `Slide`, `Scale`, `Reveal` and
  `Counter` compose builders.

### Changed

- Native builds pin FFmpeg SDK build5, which fixes Vulkan Video encoder memory leaks.
- On Linux with glibc, `serve` caps the C allocator at two arenas unless `MALLOC_ARENA_MAX` is set,
  so idle memory stays flat across mixed requests.

### Fixed

- The browser CPU renderer accepts registered runtime effects. It refuses only the effects a project
  actually draws, so a browser or editor export no longer fails just because an effect is
  registered.
- `Canonical<MergeLayer>` keeps its `input` and `blend_mode` fields. Special cases now match by
  type identity, not mutual assignability.
- Preview playback follows a smoothed audio position, so frames are no longer dropped when the
  audio clock advances in bursts.
- A long-lived host stops re-probing hardware encoders that keep failing. On Windows, each failed
  HEVC Media Foundation probe used to leak handles.
- Per-frame vector buffers are destroyed after submission, so GPU memory no longer grows over
  long renders.
- When conform refuses an audio selection, it names the clip and the reason.
- Editor: a proxy conversion that stops making progress now fails instead of hanging. The preview
  worker keeps its first message handler when its entry runs again.

## [0.5.0] - 2026-11-01

### Added

- Text: a text style's `fill` takes a color or a linear or radial gradient over the text's layout
  box. Stacked `strokes` and CSS-style `shadows` (offset, blur, color) work on captions, text
  generators and layout text.
- Text runs: a text can be a list of runs, each with `class` names and an optional spoken `time`.
- Per-character text animators: Range, Timing and Class selectors over characters, words or lines.
  They animate offset, scale, rotation, opacity, tracking, fill and stroke. Layout boxes take Range
  animators as `text_animators`.
- Caption styles:
  - Tracks hold named `styles` (style patches).
  - Each cue can name a `style` and a `speaker`, and a speaker's style applies by name.
  - WebVTT import maps voices, classes and inline timestamps to speakers, run classes and run
    times.
- Compose captions add the `bold`, `karaoke` and `pop` presets, a highlight color and per-speaker
  looks (`speakers`).
- Pitch-preserving speed changes: `preserve_pitch` on a clip time-stretches its audio instead of
  resampling it. Compose video nodes default to `true`.
- `plus_lighter` blend mode (Compositing Level 2) on the GPU and CPU renderers.
- Alpha output: with `alpha: true`, an export writes VP9 with alpha in WebM or Matroska, or ProRes
  4444 in MOV. A still with `alpha: true` is drawn without the project background.
- 10-bit output: `bitDepth: 10` for HEVC Main 10, AV1, VP9 profile 2 and ProRes. ProRes (422 HQ,
  or 4444 with alpha) is a new MOV codec. `capabilities` reports `encode.output`.
- Browser export supports alpha and 10 bits too (`videoAlpha`, `videoBitDepth`).
  `canEncodePixels` checks whether the browser really encodes them.
- VP9 sources with alpha decode with their alpha through libvpx. Hardware decode is refused for
  them, and conform reports when alpha is dropped.
- GPU-resident HEVC encode on Linux through Vulkan Video.
- Host protocol:
  - The host writes a `ready` line on start.
  - An in-band `cancel` operation stops or pre-cancels a request by id.
  - The host exits when the process that started it dies.
  - New error codes: `GPU_DEVICE_HUNG`, `ENCODER_BUSY` (with `retryAfterMs`) and
    `OUTPUT_PUBLISH_FAILED`.
- Node and Python clients:
  - A start timeout fails with `HOST_START_TIMEOUT` (`hostStartTimeoutMs` / `host_start_timeout`,
    30 s by default).
  - New typed refusals: `HOST_QUARANTINED` (macOS quarantine), `HOST_BLOCKED` (Windows App
    Control) and `HOST_OOM_KILLED` (Linux OOM killer).
- GPU adapter policy: `ALPHA2_GPU_ADAPTER` takes `auto`, `performance`, an index or a name.
  - `auto` (the default on Linux and macOS) prefers adapters that encode on the GPU.
  - `capabilities` reports the choice and every candidate as `adapters`.
- Memory:
  - `capabilities` reports the device memory budget (`gpu.memory.budgetBytes`), and idle texture
    pools are held to a quarter of it.
  - An export whose smallest working set exceeds available machine memory fails up front with
    `RESOURCE_LIMIT`.
  - On Linux, exports are also stopped when they near the cgroup memory limit.
- Export results report:
  - Each encoder fallback, classified as `absent`, `busy`, `unsuitable` or `unknown`.
  - The audio encoder used.
  - A restart on the readback path when the GPU-resident encoder rejects a frame
    (`gpuOutputPathFallback`).
- Browser GPU recovery (`alpha2/gpu-recovery`):
  - After a first external GPU loss, the GPU is tried again.
  - Recovery moves to the CPU renderer when the loss came from the page's own work or the browser
    withdrew GPU access.
  - `Alpha2Player` and the React component can swap in a new canvas (`replaceSurface`).
- `alpha2/renderer-choice` (`createRenderer`). `"auto"` now draws on a hardware GPU, else on the
  CPU renderer. `createForCanvas` takes `{ softwareAdapter: "refuse" }`.
- The renderer splits GPU work into submissions sized under the GPU watchdog, based on per-effect
  cost estimates.
- New `alpha2` subpath exports:
  - `export-metrics`: encoder latency and AAC priming probes.
  - `worker-error`: errors that keep their codes across a worker boundary.
  - `abort`: AbortSignal helpers.
  - `active-clock`: timeouts that ignore time a page spends frozen or asleep.
- Browser media:
  - Typed refusals `DECODE_UNAVAILABLE` and `PLANAR_DECODE_UNAVAILABLE` for tracks this browser
    cannot decode into readable frames, such as some HDR.
  - The editor can preview such video degraded (`hdrWithoutPlanes`).
  - Trial audio decodes at import, and retried frame readbacks.
- Installers check the downloaded archive against SHA-256 pins shipped in the npm package and the
  wheel. Results report `verifiedBy` / `verified_by`.
- Release archives and wheels have GitHub build provenance attestations.
- Installers honour `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`. The Python client verifies TLS with
  the system store (`truststore`), or with `certifi` on Linux systems without one.

### Changed

- **Breaking:** text style `color` is now `fill`, and `outline` is now `strokes` (a list), in
  project and compose schemas.
- **Breaking:** `@alpha2-sh/node` requires Node.js 22.21 or later (24.5 or later on Node 24). It
  previously required Node.js 20.
- **Breaking:** the browser packages require Safari 26 or later (was 16.4).
- **Breaking:** Python requires 3.11.4 or later.
- **Breaking:** the `Alpha2Player` constructor takes an options object (`{ onRefill,
  replaceSurface }`) instead of a refill callback.
- **Breaking:** in the Rust facade, `GpuContext::request_high_performance` is replaced by
  `GpuContext::request_product` (one product adapter policy for every OS).
- **Breaking:** partial and kept outputs are named `<stem>.a2partial.<ext>` and
  `<stem>.a2kept.<ext>`. Partial files were previously `<stem>.alpha2-partial.<ext>`.
- **Breaking:** cancelling from `@alpha2-sh/node` or Python now sends an in-band `cancel` line
  instead of a signal (a 5 s grace period, then the host is ended). On Windows, cancel no longer
  leaves a partial file.
- Without `overwrite`, an output is moved into place by a single rename that never replaces a file.
  - A finished render that cannot be moved into place is kept beside the output, and its path is
    reported, never deleted.
  - Outputs on folders that only write files in order (FUSE mounts without a cache) are staged in
    the temporary directory.
  - On Windows, an output held open by another program is waited for, up to 10 s.
- The default video decode is `auto` on macOS (was `software`). Linux stays `auto`, Windows stays
  `software`.
- Windows release archives ship DXC, and the host compiles its DX12 shaders with it instead of FXC.
  The host refuses to start without DXC; a source build points `ALPHA2_DXC_PATH` at it.
- Host protocol output is ASCII: non-ASCII characters in strings are `\uXXXX` escapes.
- Project, effect and `.cube` files may be UTF-16 with a byte order mark. A legacy code page fails
  with a message naming the file and the first bad byte.
- Decoder session estimates count 10-bit streams at twice the size of 8-bit ones. A session too
  large for the budget runs with fewer decoder threads.
- Linux installers refuse musl systems and glibc older than 2.35 before downloading. On macOS,
  an x64 Node or Python under Rosetta installs the arm64 host.
- On macOS, OpenH264 is found only at Homebrew and MacPorts absolute paths, never in the working
  directory.

### Removed

- **Breaking:** `thin` release archives (host linked against a system FFmpeg 8) and the
  installers' `--thin` option / `kind` argument. To use a system FFmpeg, build the host from source
  (`cargo install alpha2-host`).

### Fixed

- Windows D3D12VA encode copy targets are created in zeroed memory, so hardware-encoded exports no
  longer pick up stale picture data.
- Encoding on single-CPU hosts no longer stalls for about 250 ms on every frame handed to the
  encoder.
- A D3D12VA export whose GPU-resident encoder rejects a frame restarts on the readback path instead
  of failing.
- Still outputs that differ only by Unicode normalization or letter case are refused as one file.
- Conform's `offline` message names the path it looked up.
- The editor copies the main-thread preview frame before presenting it.

## [0.4.6] - 2026-10-29

### Added

- Host protocol `still` operation: renders 1 to 256 frames of a project to 8-bit RGBA PNG files,
  each the frame an export draws at that time, decoding once per request; optional `size`.
- `@alpha2-sh/node`: `still()` and `stills()` (client methods and module functions) for PNG frames.
- Python: `alpha2.still()` and `alpha2.stills()`, and `python -m alpha2 still project.json out.png
  --at SECONDS`.
- Opt-in `overwrite` for exports and stills (`--overwrite` on `alpha2-host export`, `overwrite`
  in the protocol, Node and Python): replaces an existing regular file only once the output is
  whole.
- Export results report `output`, the absolute path of the file written.
- Text styles, text generators and layout boxes take `font_weight` (1 to 1000, matched as CSS
  Fonts 4 matches `font-weight`, variable fonts drawn at the weight) and `letter_spacing` (CSS
  `letter-spacing` in font sizes, -0.5 to 1; cursive scripts take none).
- Hosts register every face of a font collection (`.ttc`); `addFontFile` on the wasm text
  measurer and `WasmTextRasterizer` does the same in the browser.
- `measureLayout` reports each text node's first and last baselines (`baselines`).
- New crate `alpha2-layout`: text and layout placement measured without a GPU.
- Python authoring helpers: `constant`, `key`, `keyframes`, `point_key`, `point_keyframes`,
  `lift`, `visual()`, `effect()` and `alpha2.fx` for typed effect parameters, `alpha2.layout`
  (the `alpha2/layout` builder: `box`, `text`, `shape`) and `alpha2.shapes`.
- Python `alpha2.TextMeasurer(fonts)`: measures text and layout generators with the engine's
  placement code, without a GPU or a host.
- Python `alpha2.lint_project()`, and typed `ExportResult` and `StillResult`.
- Project lint (`lintProject`, `Project::lint`) also reports unreached keyframes of composition
  node settings (composition time) and of shapes and layout generators (picture time).
- Native exports size the number of open video decoder sessions to the export and a memory
  budget (`memory.decoderSessionBytes`, `ALPHA2_DECODER_SESSION_BYTES`, `ALPHA2_DECODER_SESSIONS`);
  when sessions run short, videos take turns instead of failing. Results report
  `performance.decoderSessions`, `decoderSessionLimit`, `decoderSessionPeak` and
  `decoderSessionReopens`, plus a `decoderSessionsShort` decode diagnostic.
- `@alpha2-sh/editor`: `videoConversion` (`engine` or `browser`) on the bridge and per export,
  choosing who converts decoded SDR frames; the export report says which ran.

### Changed

- **Breaking:** the export result field `videoOutputPath` is renamed `videoPipeline` (`readback`,
  `gpu-resident` or `packet-copy`), in the host protocol, Node types and metrics files.
- **Breaking:** text generator and caption `font_size` is a fraction of the canvas's shorter side
  instead of its height, like every other length; portrait canvases render text differently.
- **Breaking:** the Gaussian blur `radius` (and glow, unsharp mask and the other built-ins sized in
  output heights) is a fraction of the shorter side; portrait output changes. Compose's `blur`
  now matches its documentation on portrait canvases.
- Text lines center each line's em box in the line (CSS half-leading), so text with
  `line_height` above 1 sits lower than before.
- Every field of a clip's `visual` is optional and defaults to centered, unscaled, unrotated and
  opaque; `canonicalProject` fills in the defaults.
- An export or still whose output is the same file as one of its inputs (asset, proxy, frame,
  resource, effect asset or font, hard links included) is refused with `INVALID_REQUEST`.
- Hardware decode sessions open with two FFmpeg workers, making exports of many short cuts from
  many sources markedly faster.
- `@alpha2-sh/editor` imports stage up to four files of one import at once and hash each file
  while it is probed, so large imports finish sooner.

### Fixed

- Browser exports with AAC audio compensate for the encoder's priming delay, so audio is no longer
  offset, and encode AAC-LC at every sample rate (no HE-AAC at 24 kHz and below).
- Browser exports no longer fail on opaque hardware frames (such as HDR) that the engine must
  convert: those frames are decoded again in software.
- `@alpha2-sh/editor` imports videos that WebCodecs decodes even when the browser's `<video>`
  element cannot play them, refuses those WebCodecs cannot decode, and bounds and cancels their
  thumbnail decode.

## [0.4.5] - 2026-10-28

### Added

- 51 new built-in effects (62 in all) across the Color, Keying, Blur & Sharpen, Stylize and
  Distort groups, among them levels, lift/gamma/gain, luma key, spill suppress, directional
  and radial blur, lens blur, tilt-shift, glitch, CRT, VHS, emboss, edge detect, oil paint, swirl,
  ripple, lens distortion, kaleidoscope and tile. Each also runs on the CPU renderer.
- 92 new built-in transitions (122 in all), including ports of further gl-transitions and new
  cover, uncover, shape wipe, gradient wipe, zoom blur, whip pan, glitch and page peel
  transitions; the catalog gains Shapes, 3D, Zoom & Blur and Glitch groups.
- The `pixelate` effect takes a `shape` parameter (`square` or `hexagon`).

### Changed

- **Breaking:** Rust `Renderer::register_builtins` is synchronous; built-in effects and
  transitions are compiled the first time a frame draws them (or `warmup` names them), so start-up
  no longer compiles every shader.

### Fixed

- Windows: a hang between closing one Media Foundation encoder (such as `h264_mf`) and opening
  another; opens and closes of Media Foundation encoders are now serialized.

## [0.4.4] - 2026-10-27

### Added

- CPU renderer: `alpha2-host` renders without a GPU, drawing the same render graph pass for pass
  with byte-identical output on every machine and thread count. Chosen with `--renderer
  auto|gpu|cpu` or `ALPHA2_RENDERER`; threads with `ALPHA2_RENDER_THREADS`.
- `renderer` option in `@alpha2-sh/node` and the Python client (with `video_decode` in Python);
  results and capabilities report `renderer`, `rendererReason`, `rendererThreads` and
  `cpuRenderMs`, and capabilities list `renderer.gpu`, `renderer.cpu` and `effects.runtime`.
- Browser CPU fallback: `WebEffectRegistry.createCpu(canvas)` and `renderer: "auto" | "gpu" |
  "cpu"` on `createWasmRenderWorkerHost`, used when neither WebGPU nor WebGL2 is available;
  `WorkerRenderBackend.rendererChoice` and `detectBrowserCapabilities().renderer` say which is in
  use.
- Software H.264 through Cisco's OpenH264 2.7.0, downloaded only on request with
  `npx @alpha2-sh/node install --openh264`, `python -m alpha2 install --openh264`,
  `installOpenH264()` or `alpha2.install_openh264()`; pinned with `--video-encoder openh264`,
  configurable with `ALPHA2_OPENH264_LIBRARY`, `ALPHA2_OPENH264=0` and a download mirror variable.
- NVENC H.264, HEVC and AV1 encoding on Linux and Windows NVIDIA GPUs with an encoder, offered only
  after a test encode succeeds on the machine.
- Capabilities report `encode.h264` (`available`, `gpu-only` or `unavailable`, with `reason` and
  `remedy`) and `encode.openh264`; exports that used OpenH264 report `videoEncoderLibrary`.
- Browser H.264 exports fall back from High to Main and Constrained Baseline profiles when the
  browser's encoder refuses High; the editor export report includes `videoCodecString`,
  `videoCodecProfile` and `videoEncoderFallbacks`.
- `alpha2/mediabunny`: `videoEncoderProfiles()` and the `AvcProfile`, `VideoEncoderProfile`,
  `VideoEncoderFallback` and `VideoTrackEncoding` types.

### Changed

- **Breaking:** `canEncodeVideoTrack` in `alpha2/mediabunny` resolves to a `VideoTrackEncoding` or
  `null` instead of a boolean.
- `alpha2-host` in `auto` mode never uses a software GPU adapter (lavapipe, llvmpipe, WARP,
  SwiftShader): a machine without a hardware GPU renders on the CPU renderer instead, and
  `GPU_INIT_FAILED` is raised only with `renderer: "gpu"`.
- An export with no usable H.264 encoder fails with a message naming why and how to fix it.
- The bundled FFmpeg runtime moves to the 8.1.3 build that enables NVENC; Linux and Windows
  archives carry the nv-codec-headers notice, and the npm package ships `THIRD_PARTY_NOTICES.md`.

## [0.4.3] - 2026-10-26

### Added

- New crate `alpha2-compose`: compiles compose specs (sequences, stacks, grids, media, text,
  captions, layouts, audio, and components such as slideshows, title cards, lower thirds, end
  cards, comparison grids, reframes, logos and music beds) into ordinary project documents.
- `schema/compose.v1.schema.json`, the JSON Schema of the compose spec.
- Python `alpha2.compose`: `Video`, node and component constructors, `clean-dark` and
  `clean-light` themes or custom ones, compile with media facts gathered through the host's
  `conform` (no GPU) or given as `facts=`; errors raise `alpha2.Alpha2ComposeError` with a path.
- Python `export` accepts a compiled compose result and uses its assets, fonts and loudness.
- `ProjectBuilder.assets()` in Python and in `alpha2/builder`: each media source's id mapped to
  its locator, ready to pass as an export's `assets`.

## [0.4.2] - 2026-10-26

### Added

- Python package `alpha2` (Python 3.11 or later): `ProjectBuilder`, in-process project validation
  with the engine's own validator, generated `alpha2.types`, and `HostClient` with `export`,
  `conform` and `capabilities`, cancellation, progress callbacks and classified errors.
- `alpha2-host` platform wheels for Linux x86_64 and aarch64, macOS arm64 and Windows x64,
  installed with `pip install alpha2`.
- `python -m alpha2` (and the `alpha2` command): `install` (into the cache shared with
  `@alpha2-sh/node`), `export` and `capabilities`.
- `ExportOrigin` in `alpha2/export-failure` gains `python`.

## [0.4.1] - 2026-10-25

### Added

- `npx @alpha2-sh/node install` (`--thin`, `--force`) and `installHost()`: download the
  `alpha2-host` archive of the package's version, verify it against the release checksums and
  unpack it into a shared cache (`ALPHA2_HOST_CACHE_DIR`, mirror via `ALPHA2_HOST_DOWNLOAD_URL`).
- `@alpha2-sh/node` finds the cached host automatically (new `cacheDirectory` option), and exports
  `hostCacheDirectory`, `hostTarget`, `HOST_DOWNLOAD_URL` and `Alpha2HostInstallError`.

## [0.4.0] - 2026-10-25

### Added

- Layout generators: boxes laid out by CSS flexbox and grid around text and shapes, with CSS-style
  lengths (numbers, `px` of the project canvas, percent), padding and margin shorthands, inherited
  text style, borders, radii, overflow and post-layout transforms. See `docs/layout.md`.
- `alpha2/layout`: `measureLayout` (box positions, line counts and overflow, measured with the
  engine's fonts) and a builder (`layout`, `box`, `text`, `shape`) from CSS-like props.
- `BrowserBridge.measureLayout()` in `@alpha2-sh/editor`, and `measure_layout` in the Rust engine.
- Colors anywhere in a project may be CSS hex strings, and a color may stand for a solid paint.
- `canonicalProject`, `canonicalRgba` and `canonicalPaint` in `alpha2/project`, returning
  documents in the form the engine writes back (`CanonicalProject`).

### Changed

- `@alpha2-sh/editor` holds documents in canonical form (`EditorDocument.project` is a
  `CanonicalProject`), converting on open, create and every command; its composition helpers
  return results in the form of their input.
- **Breaking:** Rust `Generator::validate` returns a `GeneratorIssue` instead of
  `Option<ShapeIssue>`; `validate_on` checks a generator against a canvas size.

## [0.3.1] - 2026-10-24

### Added

- Browser exports take `videoDecode: "auto" | "software"` (default `auto`); `software` decodes
  source video on the CPU, falling back to `auto` per source where the browser has no software
  decoder (reported in `report.decodeFallbacks` as `kind: "software-unavailable"`). Another value
  is refused with `EditorError` `invalid_video_decode`. The export report now includes
  `videoDecode`.
- `MediabunnyVideoDecoder.open` with `hardwareAcceleration: "prefer-software"` rejects with
  `code: "software_decode_unavailable"` when the browser has no software decoder for the track.
- `FrameSupply` takes `unreadableFrames: "redecode" | "present"` and reports recovery counts in
  `readbackRecovery`.

### Fixed

- Chrome exports and previews no longer fail when the browser intermittently cannot read back a
  hardware-decoded frame (`copyTo` `InvalidStateError`): the read is retried, then the frame is
  decoded again in software, which yields the same pixels. The preview instead shows the frame
  through the browser's own conversion, except for HDR or color-declared sources.

## [0.3.0] - 2026-10-24

### Added

- Shape layers: `Generator::Shapes`, an ordered content tree of groups, rectangles, ellipses,
  polystars, Bezier paths, fills, strokes (with dashes, caps and joins) and trims, in Lottie paint
  order. Every value can be keyframed and positions can follow spatial Bezier curves.
- Deterministic in-house vector rasterizer (new `alpha2-geometry` crate, `alpha2::geometry` in the
  facade) that produces the same bytes natively and in wasm.
- `alpha2/geometry` npm export with the engine's geometry for editors: `shapeBounds`, `hitTest`,
  `nearestPoint`, `pathLength`, `trimPath`, `resamplePath`, `toPath`, `motionPath`.
- Shape errors (`InvalidShape`) report the exact path of the bad item; the project linter flags
  empty subpaths and fills or strokes with no geometry before them.
- Shape masks take any shape geometry (rectangle, ellipse, polystar, path) and a `fill_rule`.
- Effects with multiple inputs: an `EffectDefinition` can declare up to 8 named `inputs`, which an
  effect node connects under `inputs`; WGSL reads them with `sample_<name>(uv)` and
  `size_<name>()`. Multi-pass effects can be built from group compositions.
- `BrowserBridge.registerEffects(definitions)` registers runtime effects and transitions on every
  renderer the bridge creates (preview, worker, after GPU loss, each export attempt); errors are
  `invalid_effect` and `effect_conflict`.
- MOV, Matroska (MKV) and WebM export besides MP4, on the native host, the host protocol,
  `@alpha2-sh/node` and the browser exporter. The container comes from the output extension or
  an explicit `container` field (`--container` on the CLI); codec defaults follow the container
  (H.264 + AAC, or VP9 + Opus for WebM).
- Opus audio: `audioCodec: "opus"` on the host protocol, `--audio-codec opus`, `@alpha2-sh/node`
  and browser `startExport`.
- Software VP9 (libvpx), AV1 (SVT-AV1) and Opus encoders in the pinned FFmpeg SDK, so hosts
  without a GPU encoder can write VP9 or AV1.
- `alpha2/containers` npm export with the shared container/codec table; the host reports it as
  `capabilities.encode.containers`, and export results report the `container` written.
- `alpha2 transcode` writes the container its output extension names.
- Preview resolution scale: `previewScale` bridge option (default 0.5, above 0 up to 8) and
  `BrowserBridge.setPreviewScale(scale)`; at 1 the preview matches export pixel for pixel.
- `BrowserBridge.setPassTiming(enabled)` for per-pass GPU and CPU timing; `diagnostics.host`
  reports render, unchanged-frame, skipped-request and bitmap-transfer counters, render size,
  preview scale and backend; worker-mode diagnostics are forwarded to the main thread.

### Changed

- **Breaking:** `Generator::Shape` (the `shape` generator) is removed; use `shapes`.
- **Breaking:** shape masks have a new layout: `shape` is a shape geometry that holds its own
  center, size and rotation.
- **Breaking:** mask feather is a Gaussian blur (sigma = 0.3125 x feather), so feathered masks
  render differently than in 0.2.x.
- **Breaking:** `MediabunnyMp4Sink` in `alpha2/mediabunny` is renamed `MediabunnySink`; it writes
  MP4, MOV, MKV and WebM.
- **Breaking:** the bound preview canvas's backing store is the size frames render at, not the
  document size; size the canvas on the page with CSS (`EditorPreview` already does).
- **Breaking:** export output paths must end in `.mp4`, `.m4v`, `.mov`, `.mkv` or `.webm` and
  agree with `container`; a mismatch, or a codec the container does not offer, is refused with
  `INVALID_INPUT` before anything opens.
- **Breaking:** host protocol effect requests carry each definition's `inputs`; 0.2.x hosts cannot
  run them.
- The clip transform `position` is an animated point with optional spatial tangents; the old JSON
  form is still accepted.
- MKV and WebM are written bit-exact, so repeated exports are byte-identical.
- Preview playback advances by frame start times and requests each frame once; a frame equal to
  the one on screen is not rendered, presented or copied again, cutting preview CPU use on static
  and generated content.
- Text shaping and rasterization are faster (text crates built at opt-level 3).
- The `alpha2` wasm module is about 540 KB larger for the vector renderer.

### Fixed

- Building the wasm package from a clone on Windows (wasm-opt step).

## [0.2.2] - 2026-10-23

### Added

- `BrowserBridge.measureText(generator)` returns where a text generator draws its text
  (`layout`, `content` and `ink` rectangles in canvas fractions, and `lineCount`) without
  rendering; wasm `WasmTextMeasurer` and Rust `alpha2_engine::measure_text` expose the same.
- `Generator::is_valid` is public in the Rust model.
- `MediabunnyMp4Sink` options `videoLatencyMode` and `stallTimeoutMs` (default 10 s), the
  `videoEncoderLatencyMode` helper, and a `videoLatencyMode` getter; the browser export report
  includes the latency mode used.

### Fixed

- Safari exports no longer stall: the H.264 encoder is opened in realtime mode where quality mode
  produces no output, and an encoder that holds frames without output fails with
  `ENCODE_STALLED` instead of hanging.
- Firefox, where WebGPU refuses `VideoFrame` uploads: presented frames go through
  `createImageBitmap` instead of a 2D canvas draw on the calling thread (new
  `uploadPresentedImageBitmap` engine method).

## [0.2.1] - 2026-10-22

### Added

- Proxy-first preview: `BrowserBridge.setPreviewMedia("original" | "proxies")` and
  `previewMedia` make the preview read fresh proxies wherever they exist; exports are unaffected.
- `preview-proxy` media purpose (`preview_proxy` in `alpha2/assets` and wasm `conformProject`)
  on `alpha2-host conform --purpose` and the host protocol `conform` request.

### Changed

- `BrowserBridge.getAsset` returns the source's original while it is online, even when the preview
  reads a proxy.

## [0.2.0] - 2026-10-22

### Added

- Color emoji and color glyphs in the portable text host: COLR v0/v1 paintings and CBDT/sbix
  bitmaps are drawn identically natively and in wasm; outlines surround color glyphs.
- `EmojiPolicy::Color` (`emojiPolicy: "color"`); emoji graphemes try color fonts first, U+FE0E and
  `monochrome` try the other fonts first.

### Changed

- **Breaking:** `color` is the new default emoji policy, so project text and captions with emoji
  now render in color instead of as monochrome outlines.
- **Breaking:** `OffscreenTextRasterizer`'s `emojiPolicy: "native-color"` is renamed `"color"`.
- The `alpha2` wasm module is larger for the color glyph painter and PNG decoder.

## [0.1.0] - 2026-10-20

### Added

- First release of alpha2, a GPU media engine that renders a timeline project (JSON, schema
  version 1, 120,000 ticks per second) identically in the browser, in native exports and in Rust.
- Project model: video, audio and caption tracks; clips with keyframed transforms, blend modes,
  time remapping and looping; composition node graphs (effects, transitions, transforms, merges,
  mattes, shape masks, groups); fill, shape and text generators; captions with styled text.
- Built-in effects and transitions (blur, glow, chroma key, LUTs and more) plus runtime WGSL
  effects; HDR sources and a configurable color pipeline.
- Audio mixing with EQ, compressor, reverb, ducking, a master limiter and loudness normalization.
- Extension providers for SVG, animated images, Lottie, OpenEXR and image sequences, natively and
  in the browser.
- `alpha2` npm package: wasm engine, player, browser exporter (WebCodecs/mediabunny), frame
  supply, audio worklet, captions, waveform and asset/proxy conform helpers; WebGPU with a WebGL2
  canvas fallback (Chrome 96, Firefox 89, Safari 16.4 or later).
- `@alpha2-sh/editor`: documents, commands, undo, preview workers and a preview/export bridge,
  with React bindings and a native-host client.
- `@alpha2-sh/react`: a React canvas over the browser player.
- `@alpha2-sh/node`: native MP4 export from Node.js 20+ or Electron via `alpha2-host`.
- `alpha2-host` (`export`, `conform`, `serve` with a JSON-lines stdio protocol) and the `alpha2`
  media command (`probe`, `thumbnail`, `transcode`, `export-audio`, `waveform`), built on FFmpeg 8,
  shipped as bundled and thin archives for Linux x64/arm64, macOS arm64 and Windows x64.
- Native GPU rendering on Vulkan, Metal and Direct3D 12 with hardware decode and encode (Vulkan
  Video, VideoToolbox, D3D12VA), proxies, render cache and memory budgets.
- `alpha2` Rust crate (facade over the engine crates) for programs that supply decoded frames and
  take rendered frames or YUV planes; Rust 1.88 or later.

[0.7.0]: https://github.com/alpha2-sh/alpha2/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/alpha2-sh/alpha2/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/alpha2-sh/alpha2/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/alpha2-sh/alpha2/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/alpha2-sh/alpha2/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/alpha2-sh/alpha2/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/alpha2-sh/alpha2/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/alpha2-sh/alpha2/compare/v0.4.6...v0.5.0
[0.4.6]: https://github.com/alpha2-sh/alpha2/compare/v0.4.5...v0.4.6
[0.4.5]: https://github.com/alpha2-sh/alpha2/compare/v0.4.4...v0.4.5
[0.4.4]: https://github.com/alpha2-sh/alpha2/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/alpha2-sh/alpha2/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/alpha2-sh/alpha2/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/alpha2-sh/alpha2/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/alpha2-sh/alpha2/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/alpha2-sh/alpha2/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/alpha2-sh/alpha2/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/alpha2-sh/alpha2/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/alpha2-sh/alpha2/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/alpha2-sh/alpha2/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alpha2-sh/alpha2/releases/tag/v0.1.0
