# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- An export result's `performance` splits `renderMs` into `setupMs` (with `encoderProbeMs`,
  `encoderOpenMs` and `loudnessMs`), `firstFrameMs` and `finishMs`, and reports `steadyFps`, the
  encode rate without start-up and finish.

### Changed

- Editor: the browser bridge's `resourceLimit` counts only imported media files. Titles and
  adopted proxies no longer count, so a project with many titles or proxies previews and exports;
  media removed from the document still counts while undo can bring it back, and the import refusal
  says so.
- Editor: a browser export that would write audio this browser cannot decode is refused before it
  renders with `unsupported_media` naming the sources (`details.sources`, `cause` a
  `DecodeUnavailableError`); option `undecodableAudio: "silence"` writes them silent, and the
  result's `report.silentAudioSources` lists them either way. `conform` for an export purpose
  decodes a trial of such audio again, so the decision uses facts taken at export time. Before, the
  file silently lacked those sources' sound.
- Mobile: `ExportProgress.onProgress` may throw (`throws` in Swift). A failure stops the export with
  nothing published, and the outcome fails with `PROGRESS_CALLBACK_FAILED`.
- Mobile: apps attach a preview window with `Alpha2Android.attach(player, surface, width, height)`
  on Android and `Player.attach(layer:width:height:)` in Swift. `Alpha2Android.windowFromSurface`
  is no longer public, so an app never holds a raw window reference.
- The CPU renderer presents quarter-turned (portrait) and downscaled video and RGBA stills about
  five times faster, with identical output.
- The CPU renderer converts HLG, PQ, non-BT.709 and alpha-plane sources through tabled transfer
  curves instead of per-pixel `pow` and `exp`. Their pictures move by under 0.01 of a half-float
  step of the brightest channel; SDR BT.709 sources without alpha are unchanged.
- The CPU renderer packs 10-bit, 4:2:2, 4:4:4 and alpha layouts on its own threads
  (`ALPHA2_RENDER_THREADS`) instead of starting up to eight more per frame. In `alpha2-render`,
  `pack_cpu_image` is now the method `CpuRenderer::pack_image`.
- `alpha2 features` lists the optional features the `alpha2` binary was built with. The release
  checks each native archive by running it before anything is published: the feature list, a
  capability report and a CPU-renderer export from the unpacked archive on all four targets, and
  the install journeys against the pinned wheels and Node package.
- Both clients run only an `alpha2-host` of their own release. A 0.5.x host (its protocol 1 ready
  line) or a host whose `ready` state names another release in the new `details.hostVersion`, or
  none, is ended at once and the request fails with `HOST_VERSION_MISMATCH` (not retried), instead
  of `HOST_START_TIMEOUT` after 30 seconds. `HOST_START_TIMEOUT` now carries what the host wrote in
  `stderr`.
- `@alpha2-sh/node` serves requests in the order they were made, as the Python client does: only
  the oldest waiting request is considered, so a request of other modes (such as the software
  rerun after `DECODE_STALLED`) no longer waits behind every younger request of the busy host's
  modes. The host protocol's "Keeping hosts" states the rule.
- `alpha2-extensions` requires the exact versions of the converters its cost budgets are written
  against (roxmltree, usvg, resvg, tiny-skia, the hayro crates, velato, exr, zip and their peers). A
  build that needs another version of one of them fails to resolve instead of running unmodeled
  code.
- `decideRetry` (`alpha2/export-retry`): the default backoff stops doubling after 17 failed
  attempts (at 65,536 s), as the native retry policy does.

### Fixed

- An SVG whose patterns, masks, clip paths or filters refer to one another in a ring, or whose
  gradients or patterns name one another by `href` in a ring, is refused as malformed instead of
  overflowing the stack or never finishing. Each such reference counts four levels against the
  nesting limit, and a mask, clip path or filter counts once per element that names it.
- Editor: an import, relink or caption import that finishes after another project was opened is
  refused with `stale_import` and changes nothing; before, a relink waiting for the engine module
  could add its asset and an undo step to the new project.
- Editor: disposing the browser bridge stops a running proxy conversion and waits for it to end;
  before, the conversion kept reading the file and writing origin storage after `dispose()`. A
  conversion that outlives a project switch keeps its proxy and adopts it only for a document that
  has that media.
- Editor: a preview `workerFactory` that throws (a blocked cross-origin worker script, say) leaves
  the preview on the main thread with one `preview_worker_failed` report, as documented; before,
  `bindPreview` rejected and GPU recovery ended with `preview_unavailable`.
- Editor: disposing the browser bridge or a `WorkerPreview` behind a preview worker that stopped
  answering resolves within about a second and terminates the worker; before, it never resolved
  and the worker, its GPU device and its decoders stayed alive. A worker `error` or
  `messageerror` fails every later call at once, and one reply the worker cannot post no longer
  stops it answering later requests.
- Worker requests have a deadline in active time (a hidden or frozen page does not count): 60 s
  for start, project load and recovery, 30 s for every other call, overridable with
  `requestTimeoutMs` on `WorkerRenderBackend` and `createWorkerPreview` and
  `workerRequestTimeoutMs` on the editor bridge. Past it `WorkerRenderBackend` fails with a fatal
  `RENDER_WORKER_FAILED` (also on a `messageerror`), and the editor gives the preview worker up and
  draws on the main thread; before, one unanswered request froze the preview or player with no
  error.
- Editor: waits on a media element are bounded by 10 s of active time and by the import's signal.
  A video whose element never loads imports through the decoder (or is refused), a thumbnail
  whose element never loads is left out, a cancelled import rejects at once, a fallback element
  that fails to load is made anew on the next frame instead of costing every frame a timeout, and
  a proxy staging stops when the bridge is disposed.
- Editor: a source this browser cannot decode is supplied as silence with as many channels as its
  stream, and page-made audio keeps every channel up to 24, so a clip that selects channels of it
  no longer makes the mix report the source missing (silent preview, failed export).
- Editor: preview audio failures stay with their lane. A stream this browser cannot decode, or one
  the file it reads (a proxy, say) does not have, plays silent and is reported once as
  `audio_unavailable`; a source whose sound does not arrive for a few mixes plays silent until the
  next start instead of holding every lane back; a decoder that failed to open is tried again; and
  an audio start that fails plays the picture silent with one report instead of rejecting `play()`.
- Audio sample reads in the browser race a stall limit and the abort signal, as video reads do:
  `MediabunnyAudioDecoder.refill` rejects with `DECODE_STALLED` after 5 s of active time without
  samples (option `stallTimeoutMs` on `open`) and at once on abort, so an export no longer hangs, or
  ignores a cancel, in its audio step. Decoder stall errors carry code `DECODE_STALLED` and classify
  as `GPU_TIMEOUT` in the decode phase (they were unclassified).
- The audio worklet processor stays stopped after its DSP module traps or fails: later quanta no
  longer enter the module (a node with a connected input was called again and posted an error
  each quantum), and later configuration requests are answered with the stored failure.
- Mobile: `ExportProgress.onProgress` may throw (`throws` in Swift). A failure stops the export with
  nothing published, and the outcome fails with `PROGRESS_CALLBACK_FAILED`.
- Mobile: an export, extraction or preview thread that panics no longer leaves its outcome pending.
  The outcome fails with `INTERNAL` and the panic's message, and a preview reports it in
  `lastError`. A timeline extraction whose renderer cannot start (an effect resource without an
  asset) fails with the reason instead of hanging, and `cancel()` stops a timeline extraction.
- iOS: the player retains the `CAMetalLayer` it is given before `attachWindow` returns, so an app
  that releases its layer right after attaching it no longer leaves the preview a freed layer.
- Mobile: a preview skips seeks and resizes that later ones replaced before it reached them, a new
  seek stops the frame an older one is drawing, and `end()` stops the frame or audio decode in
  progress and skips queued commands instead of carrying them out first. A preview whose audio
  needs more than a quarter of the free memory (at most 256 MiB) decoded plays silent and says so
  in `lastError`.
- Mobile: a source whose video starts after the time a clip reads from (an MP4 with an empty edit,
  a Matroska file whose first video frame is not at 0) shows its first frame until that frame is
  due, instead of every later frame early.
- Mobile: Matroska and WebM exports with more than 10 s of audio place the audio beside the video
  it plays with, so a reader seeking in the file finds both. The encoded audio is now written at
  most one second ahead of the video. MP4 and MOV bytes do not change.
- `alpha2-container`: crafted or damaged files no longer panic the readers. A Matroska block
  element whose header crosses its cluster's end ends the cluster, a DTS frame shorter than its
  header has no duration, and an H.264 sequence with huge dimensions, a `DiscardPadding` of
  `i64::MIN` and Ogg granule positions at either end of the range read. The Matroska writer refuses
  a packet whose time does not fit a millisecond timestamp.
- `alpha2-host-core`: an audio timestamp more than 2^31 seconds from zero is damaged input on the
  built-in decode path too, as it is on the FFmpeg path: `strict` fails and `tolerant` drops the
  chunk, where before it was placed at time 0 or overflowed.
- Native hosts refuse a picture larger than 16384 pixels per side or 2^26 pixels before FFmpeg
  allocates it: a PNG, JPEG or WebP whose header states such a size fails `probe` and open with
  `Unsupported`, stream probing and the decoders take FFmpeg's `max_pixels`, and decoded YCbCr
  frames are checked like converted ones.
- The CPU renderer no longer panics drawing a composition-graph transition whose kernel divides by
  a tiny authored progress (`fold`, `cube`, `slides`); its sampler clamps a coordinate before
  turning it into a texel index.
- `validateProject` no longer throws on a project object nested deeper than 127 levels or on an
  array with holes; it reports the depth problem of the text path, and a hole fails its item's
  shape as `undefined` does.
- `serve` compares its memory, memory-growth and handle limits only after an answer that leaves no
  request running, and takes the memory-growth baseline again after it makes its renderer, so a
  running export no longer drains the host and a first `compose` no longer counts the device as
  growth.
- A readback whose mapping result never arrives fails with `GPU_TIMEOUT` instead of waiting forever.
- An export whose file is in place answers `ok`: an encoder teardown that fails afterwards is
  reported in `encoderTeardownFailure` (and retires the host if it leaves a thread stuck), and the
  `hardware-required` check runs before the output is finished, on both render paths.
- The render cache keeps baking after a write that fails once, stops only on failures that repeat
  or eight in a row, and reads at most what a frame of its size can take.
- A cancelled export stops copying its staging file into an output folder that cannot seek, and
  publishes nothing.
- A text raster larger than the largest frame (67,108,864 pixels) or whose planes would take over
  1 GiB is refused as too large before anything is allocated. Before, only each side was limited,
  and the browser module aborted on the allocation.
- Both clients start their hosts with the client's cache directory (`cacheDirectory`,
  `cache_directory`) as `ALPHA2_HOST_CACHE_DIR`, in absolute form, also when it comes relative from
  the environment, so a host finds an OpenH264 installed there. An OpenH264 install from this
  process starts new hosts and a new capability probe, so the next export can use it.
- The Python client keeps no thread or pipe of a host that exited during a request (one of each
  leaked per exit, until host starts failed for want of descriptors), and a Ctrl-C while a host
  starts ends that host and frees its place in the pool.
- `@alpha2-sh/node` takes a host whose process ended out of its pool within 5 seconds even while a
  process the host started keeps its output open; until then later requests and `close()` waited
  for that process.
- Both clients send a request to another host when the host they wrote it to exited before taking
  all of it (a host that ended while idle), instead of failing it with `HOST_EXITED`; such resends
  share the bound of three that `HOST_RETIRING` resends have.
- The `alpha2` Python wheels carry `THIRD_PARTY_NOTICES.md` and the license texts of the Rust
  crates compiled into them; the `alpha2-host` wheels list every license file of their archive and
  state `License-Expression`. The Android library and the Swift package carry both notice files.
- An SVG whose entity references expand past its 32 MiB text budget, in text or in attribute
  values, is refused before it is parsed, and many entity declarations are scanned in linear time.
- Reading an SVG counts the layout of its text, so one `<text>` of more than about 60,000
  characters is refused, with or without a registered font. The count stops as soon as a budget is
  passed: many `use` copies of a large group, or a shape with many markers, are refused at once.
- An SVG source counts the raster images it holds: each copy usvg makes of an embedded image counts
  toward 256 MiB of image bytes and four canvases of declared pixels, and a registered image is
  read once however often it is drawn. The provider budget sees the images an SVG or PDF source
  holds and the composition a Lottie source holds, so it can close them when they are off screen.
- A PDF page is checked for what drawing it costs before it is converted: every invocation of a
  form, tiling pattern, soft mask or Type 3 glyph counts, a page may draw 262,144 paths, glyphs and
  images, decode four canvases of image pixels and write 256 MiB of image data, its content streams
  may decode to 32 MiB together, a form that invokes itself is refused, and resources a page
  inherits are checked too.
- The SVG drawing estimate counts a path's geometry, not only its area: each edge, edges that share
  device rows (which the rasterizer sorts against each other), and the dashes of a dashed stroke,
  which are made for the whole path before clipping. A dense zig-zag or many copies of a finely
  dashed line are refused before they are drawn; PDF pages are covered by the same estimate.
- A Lottie frame is charged for the vertices and dashes of its paths, through repeaters and
  precompositions, and for the canvas-sized clip drawn for every image layer; a render is refused
  at a size where its nested offscreen layers would hold more than 1 GiB.
- A Lottie animation reads each image asset file once, however many assets name it, decodes the
  assets that name one file once, and checks the pixel budget as each asset arrives, so it never
  holds more than one encoded asset at a time.
- A PDF image is sized from its own data as its decoder sizes it: JPEG 2000 from the codestream,
  JBIG2 from the embedded page, CCITT from `/Columns`, so a small dictionary size no longer hides a
  multi-gigabyte decode. A CCITT image without a row count is refused.
- A PDF is probed by opening its first page, so import refuses a page that opening would refuse.
- Matching an SVG's style rules, which usvg repeats, may spend half of the reading budget, so a
  document whose rules backtrack through deep nesting is refused in about half a second instead of
  taking two.
- An OpenEXR file of B44 blocks whose deltas wrap below zero no longer panics the release build:
  the exr decoder is built with the wrapping arithmetic of the reference implementation.
- Compose refuses a time past the latest a project holds, and a sum of times that runs past it,
  at the field or node that holds it, and refuses an invalid canvas before lowering anything. Such
  specs panicked the compiler or compiled to a project no host loads.
- An edit whose result would be longer than the 64 MiB a project document may be (a `bake` of long
  data on a large project) is refused with `INVALID_INPUT`, refusal `result_too_large`, instead
  of returning a project no host loads; compose refuses such a compiled project at `spec`.
- Template binding makes at most 100,000 values (a string counts one per 64 bytes) and fails at the
  path where the budget runs out, `$each` no longer copies its scope per item, and a counter counts
  over at most 100,000 frames. Nested `$each`, large declared minimums in lint and long counters
  hung or exhausted memory. Lint's sample data now meets a `min_chars` above 4.
- A compose sequence checks a transition against the clips it joins: a crossfade into or out of a
  part shorter than it (a held last frame, a short video) fades the incoming node in on a lane of
  its own, another transition is refused at the child whose part is short, and a middle child or
  slide shorter than its two transitions is refused at its path. Such specs failed with "the
  compiled project is not valid" and no path.
- A compose zoom (including a slideshow's `ken_burns`) on a video shorter than its node runs once
  over the node; it no longer restarts on the held last frame.
- The sound of the two sides of a compose sequence join crosses over the transition (equal-power
  fades, or a clip's own longer fade); both sounds played at full level over the overlap and the
  outgoing one stopped without a fade.
- A compiled compose project the model refuses is reported at the node that made the refused clip,
  source, track or composition instead of at `spec`.
- Compose checks the project limits it can count where the node is: tracks (named at the first
  node that does not fit), clips, transitions and cues per track in a sequence, layers and nodes of
  a group, a box's or `layout` node's layout tree (a box with a picture hides at most 15 leaves),
  text length, overlapping caption rows and transition names, each with the node's path and the
  limit.
- A compose spec's `duration` is the project's length: when the content ends earlier, a
  transparent clip fills the rest, so the export lasts as long as asked.
- Compose facts must say whether a video has sound (`has_audio`) when a spec plays it unmuted;
  facts without it compiled the video silently. `requirements` lists a file named first by an
  `audio` node and then by a `media` node once, as the picture.
- Mobile: a video source whose decoder session the system takes away (a trip to the background)
  fails only after three losses in a row with no frame decoded between them, instead of at the
  fourth loss in its life. Catching up to where a lost session stopped gets the 10 s decode budget
  per frame, and a seek after a failed catch-up shows the frame at its time instead of a later one.
  Under the tolerant decode policy a picture a hardware decoder cannot decode is skipped and
  reported, as on the desktop.
- Mobile: an export whose encoder stops taking frames fails after 10 s without progress, and
  `cancel()` stops it while it waits, instead of the export hanging. A process frozen while it
  waits no longer counts the frozen time against that bound.
- Mobile: an export is run again on the software video encoder only when the hardware video encoder
  is what failed, after the app is back in the foreground. A full disk, a failed write, an audio
  encoder failure or a failed publish fail once with their own code; a failed publish keeps its
  `OUTPUT_PUBLISH_FAILED` error and the path of the kept file.
- Mobile: an export closes a clip's decoder once the clip is past, so a project of many clips cut
  from one recording holds as many decoder sessions as clips play at once instead of one per clip
  until the end. A preview keeps at most two more sessions open than its timeline reads at once,
  closing the one read longest ago, and every clip of one image shares one decoded picture. The
  decoded frames sources hold count against the frame budget.
- A browser export cancelled while its sink is finishing rejects at once with `cancelled` and
  commits nothing, also when `finish` never settles; before, the export waited for `finish` and
  then reported a committed file as cancelled. `MediabunnySink.finish` rejects with
  `ENCODE_STALLED` (stage `finalize-stalled`) when no packet or write arrives for the stall time,
  and refuses to commit once `abort` was called. A cancel that lands after `finish` resolved no
  longer fails a finished export or offline audio render.
- Browser video decoders no longer stay in software for good: after the software retry fails
  too, the next request starts from the browser's own choice again, a `device-recovery` request
  returns a decoder that fell back to its original acceleration, and `DecoderRefillCoordinator`
  closes a cursor whose refill failed in its decoder and opens a new one on the next request. One
  damaged packet no longer fails every later read of that clip.
- A frame of a reversed clip that the browser refuses to copy out is read again and, if it still
  fails, decoded on the forward path; before, it was drawn through the browser's own RGB
  conversion and could reach an export with colors that differ from the decoded frame.
- `Alpha2Player` keeps at most one presentation on the backend and one waiting: a `present` or
  `seek` that arrives while another waits replaces it (the replaced call resolves `undefined`),
  so presenting on every animation frame no longer builds a backlog in the render worker. A
  presentation superseded by a later `present` still shows the frames it refilled, so the picture
  advances when frames arrive slower than presentations. `seek` recovers a lost device first, as
  `present` does, and after `dispose()` calls in flight resolve `undefined` instead of rejecting.
- The browser's container side readers resist damaged and crafted files: a Matroska integer or
  float field of an impossible size is refused without reading it, an MP4 whose `moov` follows an
  `mdat` larger than 64 MiB has its rotation and mirror read (it was shown unmirrored), and a
  truncated `stsd` entry or `tkhd` no longer makes the audio layout reader throw `RangeError`.
- `DecoderRefillCoordinator` bounds its decoders across sources: option `maxCursors` (default 16)
  replaces `maxCursorsPerSource`, and past it the least recently used cursor that is neither
  refilling nor retained closes; `retain(cursors)` declares the current round's cursors; a cursor
  without a request for `idleMs` (default 2000, active time) releases its decoded frames and keeps
  the file open; `diagnostics` counts open cursors, evictions, idle flushes and budget overruns.
  Before, a timeline with many source files kept a configured decoder and its queued frames per
  file until the coordinator closed, and several consumers of one source reopened decoders every
  frame. `BrowserRobustnessCoordinator` and `BrowserResourceBudget` are removed (nothing used
  them), with the diagnostics only they reported; `detectBrowserCapabilities` now reports
  `audioDecoder` and `audioEncoder`.
- The browser GPU renderer refuses every call that would make or use a GPU object once its
  device or WebGL context is lost, with a fatal `GPU_DEVICE_LOST`, also right after a WebGL
  context loss whose event has not arrived yet; before, such a call could reach the WebGL
  backend's unchecked object creation, or on WebGPU draw nothing without an error.
  `Alpha2Player` recovers and presents again when the renderer refuses a presentation this way.
- A browser exception while the WebGL2 renderer imports a frame (the 2D canvas draw or read) is
  refused as `INVALID_INPUT` (`INTERNAL` for the upload probe) and the renderer keeps working;
  before it left the binding uncoded, which cost the editor its preview worker. An export frame
  the browser cannot copy out of memory rejects with a coded failure (`OUT_OF_MEMORY`, stage
  `copy-out`) instead of leaving the readback unsettled, on the GPU and the CPU renderer.
- The browser CPU renderer refuses a `VideoFrame` whose own color space is HDR (PQ or HLG) with
  `INVALID_INPUT`, as the GPU renderer does, instead of reading it through a 2D canvas with the
  browser's tone mapping; such frames go through `uploadYuvFrame`.
- `readRgbaAt` on the browser CPU renderer draws at export quality every time, so the timeline
  frames an extraction reads (and its digest) no longer depend on whether a preview was drawn
  before; it matches the native host. The `renderAt` documentation of the CPU renderer is back
  on `renderAt`.

## [0.6.0] - 2026-11-08

### Added

- Native mobile targets, as a preview: the `alpha2-mobile` crate with a uniffi API for iOS and
  Android apps, and the scripts that build an Android library with Kotlin bindings and a Swift
  package over a static XCFramework. No Android library or Swift package is published with 0.6.0.
- Mobile export and preview over platform codecs (MediaCodec, VideoToolbox, AudioToolbox).
  Hardware-decoded frames stay on the GPU on Android; iOS copies decoded and encoded frames through
  memory. Known limits: the player always draws on a GPU device that presents to its window
  (`RendererMode` governs exports and stills), `conform` reports only whether each source's file
  opens and ignores `purpose`, and Android codecs are opened by MIME type or a fixed component name,
  without `MediaCodecList` discovery.
- `alpha2-container`: MP4/MOV, Matroska/WebM, WAV, MP3, ADTS, FLAC and Ogg demuxers, and MP4 and
  Matroska/WebM muxers, with FFmpeg packet parity.
- `alpha2-host-core`: the policies, audio timelines, cursor rules, providers, plugin runtime and
  protocol frames every native host shares, and the FFmpeg-free decode, export and still pipeline
  (PNG, JPEG and WebP with Exif orientation) the mobile hosts run.
- Compose templates that bind typed parameters into plain specs, with a data schema and lint.
  Titles, cards, lower thirds, logos, reframes, reveals and comparison grids ship as first-party
  templates.
- Compose nodes: `box` layout, `shape`, `group`, `matte` and `mask`; animated values (constant,
  ramp, keys); theme lengths and colors; `$if` on any value.
- Text fits its font size to its box and clamps lines with an ellipsis.
- Compose everywhere: `alpha2/compose-wasm` in the browser, a `compose` operation in `serve`, and
  compose on mobile behind a default `compose` feature.
- `python -m alpha2 mcp`: an MCP server in the Python package that lists, binds, checks and renders templates
  and specs.
- The compose schema lists every built-in effect with its parameter types and ranges. Schemas carry
  their alpha2.sh ids, and a core compose schema leaves out engine nodes.
- `serve` reports its state: `ready`, or `draining` once a new process would serve better (memory,
  memory growth, requests served, open handles, a lost device or encoder path, idle time), with
  soft limits set by `serve` flags. Requests it did not start are answered `HOST_RETIRING`, and a
  `status` operation reports the state at once.
- `@alpha2-sh/node` and the Python client keep their hosts between requests, up to `concurrency`,
  one request at a time each, and replace a host that drains or exits. Hosts are kept apart by the
  environment they start with. `Alpha2NodeClient.close()` and `HostClient.close()` end them; idle
  hosts keep no Node or Python process alive. The MCP server reuses hosts across tool calls.
- A `still` without an `output` returns its PNG in the response instead of a file.
- `serve` runs up to `--max-running` requests at once on its one GPU device, 1 by default. With a
  higher count, requests share the device: budgets and codec threads are not divided, and an
  export's GPU counters cover the requests that ran with it. Requests hold the runtime effects and
  effect resources they define, so two that define one id differently take turns, and the memory
  watchdog stops the newest export first.
- Data sets in the project: `points`, `scalars`, `similarity`, `words` and `intervals`, anchored to
  a source or the timeline. Animated values can follow a data set, a clip can take a similarity
  correction from one, and a caption track can take its cues from words.
- Extractors: code outside the engine is supplied a range's frames or audio and returns data sets.
  The host has an `extract` operation (`client.extract` in Node, `HostClient.extract` in Python),
  the browser has `runExtractor` in `alpha2/extract`, and mobile apps have `Alpha2.extract`.
- Extractor plugins under the `alpha2-plugin/1` ABI, as wasm or native artifacts pinned by SHA-256
  in a manifest. The host fetches, caches and runs them (wasm sandboxed, native only when allowed),
  the browser loads the wasm artifact with `loadPlugin` from `alpha2/plugins`, and mobile apps load
  bundled ones. The SDK, the pack tool and a first-party silence plugin live in `plugins/`; no
  release contains a plugin.
- Edit operations `remove_ranges`, `split_at` and `bake`, with a report of what moved: an `edit`
  host operation (`client.edit` in Node), `alpha2.edit.apply` in Python, `applyEdit` in
  `alpha2/apply-edit` and `applyEdit` on mobile. Known limits: track and master automation with hold
  or eased keys across a removed range, a linear timeline-anchored sample set just before a cut,
  keyed parameters of a transition a ripple moves, parts of a reversed clip whose edges are off the
  frame grid, and a fade under a cut are not exact; `split_at` inside a transition, more than 4,096
  parts or cues on a track, and a bake of data outside a property's range still end as `INTERNAL`.
- Conform reports data sets made from an original that has since changed.
- `@alpha2-sh/editor` edits data: `data.put` and `data.remove` commands (removal refused while
  anything reads the set), `dataValue` and `isDataBound`, `editProject` and `applyEdit` to commit an
  edit operation's result as one undo step, and `extractionProject` and `applyExtraction` to commit
  an extraction's data sets. Both refuse a result computed from a document that has since changed
  in what they read. The browser runtime runs edits and extractions itself (`applyEdit`,
  `extract`). Undo steps can carry a label (`undoLabel`, `redoLabel`).
- `dataReferences` in `alpha2/project` lists every place that reads a data set.

### Changed

- An edit report's `created` lists `{id, from}`: each new clip and the clip it was cut from.
- Editor ripple edits that reach every track cut or open timeline-anchored data sets as the
  engine's `remove_ranges` does, a split's right part reads its bindings through itself, and a
  pasted clip's bindings through itself read through its new id. `alpha2/edit` refuses removing a
  clip another clip's binding reads through (`read_through`).
- Editor commands no longer revalidate unchanged parts of a document, so a document holding a large
  data set stays fast to edit.
- Host protocol version 2: every message is a length-prefixed frame, a JSON header and binary
  attachments, instead of a JSON line. `serve` owns standard output: it writes a magic before its
  first frame, and anything else the process prints goes to standard error.
- Built-in effects and transitions are validated against their published definitions.
- Mattes and masks spread over transitions that commute with coverage, such as wipes.
- Builds link the pinned FFmpeg ahead of system library directories.
- A bundled archive's FFmpeg libraries may be replaced only by a build of the same 8.1.x series and
  configuration; another series can lay out the same structures differently.

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
- A compose spec without a template header keeps `{{` in its text, paths and data as written;
  only its template nodes are expanded, and a `$if`, `$switch`, `$each` or `$param` object outside
  a template is refused with its path. Data bound into a template is no longer interpolated again
  when the spec is compiled.
- A video that loops in compose (`loop`, `when_short: "loop"` or a `longest_loop` grid cell)
  refuses a `speed` other than 1; the speed was silently ignored.
- Animated captions and titles that differ only in an animator or in their background box no
  longer draw with each other's pixels, in one frame or after an edit, in preview and export.
- Opening audio whose first packet timestamps are far apart no longer hangs on every host and in
  the browser: the frame-grid search tries at most 4,096 periods, and arithmetic that would
  overflow gives no grid instead of a panic.
- A layout grid line of -32768 is refused instead of panicking the validator.
- Both clients send request paths absolute, resolving a relative one against the working
  directory at the time of the call. A kept host resolved it against the directory it started in,
  so after a `chdir` a request could read another directory's media or, with `overwrite`, replace
  another directory's output.
- A Python process forked after using `alpha2` (including `multiprocessing` with the `fork` start
  method) no longer hangs on its first request: the child starts its own hosts, and the parent's
  hosts see the end of their input when the parent closes its client.
- A `compose` or `edit` request that panics inside the host is answered `INTERNAL`, and the host
  counts it as ended and serves the next request; it was never answered and the host stayed busy.
- A 10-bit export on a host whose renderer stores 8 bits is refused with `INVALID_INPUT`, a code the
  protocol lists.
- Every host request refuses a field it does not know with `INVALID_REQUEST` naming the field; a
  misspelled option such as `includeaudio` silently changed an export.
- Under a cgroup memory limit, file page cache no longer counts as used: a container whose page
  cache filled its limit refused every export until the cache was dropped. Admission and the
  memory watchdog read one working set (the charge less reclaimable file cache) of the lowest limit
  on the cgroup path, so a limit set on a parent cgroup also arms the watchdog.
- A terminated host whose request cannot stop, or whose output nobody reads, exits anyway after
  the stall limit plus 5 seconds (5 seconds after its parent's exit) instead of living on with the
  GPU device and the partial file. An extraction waiting for an acknowledgement when the input ends
  is answered `CANCELLED`.
- Runtime effect code outside comments must be ASCII; non-ASCII identifiers and Unicode
  blankspace, which slipped past the effect's cost analysis, are refused at registration.
- Layout value strings are trimmed in linear time; a long inner run of spaces no longer stalls
  `validateProject` and the Node client for seconds.
- An SVG whose entities expand past 4,000,000 XML nodes is refused while it is parsed instead of
  exhausting memory, and once one nested SVG image is refused no further one is parsed. An OpenEXR
  file whose largest block in any part decompresses to more than 256 MiB (every channel counted,
  shown or not) is refused before any chunk is read.
- The silence plugin no longer traps on a range whose last audio block has 1 to 63 samples, keeps
  a leading silence inside a range that starts off its 16 kHz grid, refuses unknown, mistyped or
  out-of-range params naming the key, and no longer offers timeline input.
- A plugin manifest's artifact and resource `url` must be a path under the manifest's directory or
  an absolute `https:` URL; `file:` URLs, other schemes, absolute or drive paths, backslashes and
  `..` above the manifest are refused with `PLUGIN_LOAD_FAILED`, and a downloaded file's hash
  mismatch no longer states the length or digest found.
- A wasm plugin has one 32-bit memory of at most 2 GiB and tables of at most 1,048,576 elements
  together; several memories or 64-bit memory are refused with `PLUGIN_UNSUPPORTED_FEATURE`, and
  `--plugin-memory-limit` takes at most 2147483648. Extraction data limits are checked as values
  are emitted, so an emit that takes a requested output past its limit fails at once with
  `INVALID_EMIT`.
- The `alpha2-plugin/1` native tables (`HostV1`, `PluginV1`, `alpha2_plugin.h`) carry `size` after
  `abi`; hosts refuse a plugin table shorter than their own or with a null entry. Native plugins
  must be rebuilt.
- The MCP server answers every request and keeps serving after a malformed tool call, an
  out-of-range number, an invalid project document or an internal error, including a panic in the
  native module; `render_frame` and `render_video` are annotated as destructive.
- `@alpha2-sh/node` no longer fails at import when optional dependencies are not installed
  (`npm install --omit=optional`); it never loads mediabunny.
- In the Python client, a request abandoned by an exception other than Ctrl-C (a signal handler, a
  task queue's time limit) is cancelled, or its host ended, before the exception propagates; the
  host had gone on running it while the pool handed it to the next caller.
- A client-side extractor that emits a value the wire cannot carry (NaN, Infinity, a non-integer or
  `bigint` time) fails the request with `EXTRACTOR_FAILED` naming the emit; a `bigint` ended the Node
  process, a NaN from JavaScript became a lost channel, and Python left the request running. Python
  extractors accept any value with `__float__` and any time with `__index__` (such as NumPy scalars).
- In the browser, a frame split into several GPU submissions fails with the reported error when one
  of them was rejected, instead of delivering wrong pixels into the export.
- `remove_ranges` and `split_at` keep a track matte, and a binding that reads through another clip
  (`via`), on the part of a cut clip beside it; a cut that no single part can follow is refused with
  the new refusals `matte_source` or `read_through`, instead of drawing the matte, misplacing a
  follower or failing with `INTERNAL`.
- `rippleTrim` at a clip's start, and the editor's start-edge ripple trim, close or open time at the
  clip's start on sync tracks, in timeline-anchored data and for timeline markers; they edited at the
  clip's old end. A start trim that would close time where a sync track has content is refused
  (`sync_conflict`).
- In the editor, a kept proxy that cannot be staged or has no room in the runtime no longer makes
  every preview load, `conform()` and `nativeAssets()` reject; it is reported once to `onError`
  (`proxy_unavailable`) and its source reads the original.
- The GPU composite draws a layer shown 1:1 on a canvas wider than 1920 pixels exactly, as the CPU
  renderer does; up to a few hundred columns of a 3840-wide clip were softened, differently on each
  GPU.
- A timeline-anchored sample set without samples no longer makes `remove_ranges` fail.
- H.264 levels account for macroblocks per second, not only frame size.
- Python functional `TypedDict` fields name their types as strings, so later types resolve.

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
