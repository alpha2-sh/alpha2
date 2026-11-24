# Security

What alpha2 treats as hostile, what it bounds, and what a deployment must contain itself. alpha2 has no security layer of its own: what a caller can reach is what each entry point takes, and the rights it runs with are those of the process it runs in. [SECURITY.md](../SECURITY.md) says how to report a vulnerability.

## Who decides what

| Party | Examples | Trusted with |
| --- | --- | --- |
| Operator | whoever starts `alpha2-host` (a service's start command, or an app through a Node or Python client), the page that loads the browser package, the app that links the mobile library | everything the process can do: its files, its network, its environment, its flags |
| Request sender | the program writing protocol requests, a caller of a client's API | paths on the host's file system, inside what the operator's flags allow |
| Content | media files, project documents, compose specs and template data, fonts, SVG, PDF, Lottie, OpenEXR, LaTeX and animated images, caption files, runtime effects, plugins | nothing: alpha2 parses and renders it under the limits below |

A project document names no file and no URL: sources are ids, and a request's `assets` and other maps say which file each id reads. That is the boundary every surface keeps. The rest of this page is what lies beside it.

## `alpha2-host`

### Paths are the request's

Every path in a request is a path on the host's file system, opened with the host's rights: `assets`, `proxies`, `frames`, `resources`, `effectAssets`, `fonts`, `output` and each still's `output`, `renderCache.directory`, and a plugin's `manifest`. There is no root directory and no allow-list for these; the files a request supplies for a plugin's resource slots (`plugin.supplied`) are the exception, refused unless the operator names where they may lie (below). A request can read any file the host can read by naming it as media (the rendered output and the `conform` report show what it holds: whether it exists, its size, modification time, stream facts, and its SHA-256 when the project records one), and can replace any regular file the host can write with `overwrite`.

A compose spec or its template data chooses media and caption paths, and the clients send a compiled spec's `assets` to the host as they are. A service that takes paths, specs or template data from someone it does not trust checks every path against its own rules before it sends the request. The [MCP server](#the-mcp-server) does this for an agent.

Outputs are written beside their path and moved into place when complete; an existing file is replaced only with `overwrite` and only if it is a regular file, and an output that is one of the request's inputs is refused.

### Network and native code

The host opens no listening socket; it talks to its parent over standard input and output. It reaches the network only to fetch [plugins](host-protocol.md#plugins), and loads code other than its own and FFmpeg's only as a native plugin (in a worker process of its own by default) or as a library its environment names (below). The operator decides these when it starts `serve`, and no request can widen them:

- `--plugins off|wasm|native` (default `wasm`). A request's `allow_native` takes effect only under `native`; a native artifact then runs with the host's rights, in a worker process per run (`--plugin-isolation process`, the default) or inside the host process (`--plugin-isolation in-process`).
- `--plugin-origins none|<https origins>` (default: a manifest from any `https:` origin, with its files and redirects on that origin).
- `--plugin-root any|none|<dir>` (default `any`) for manifests given as local paths.
- `--plugin-resource-root none|any|<dir>` (default `none`) for the files a request supplies for a plugin's resource slots: under `none` no request fills a slot; under a directory every supplied file, links resolved, must lie under it. With `any`, a sender can hand a wasm plugin of its own choice the bytes of any file the host can read, and the plugin's data carries them back; it is for an operator who trusts its senders with every path.

With the defaults, a request sender can make the host fetch an `https:` URL and learn whether it answered, and learn the SHA-256 of a local file of at most 1 MiB by naming it as a manifest. A host that serves senders it does not trust sets `--plugins off`, or `--plugin-origins` and `--plugin-root`.

A wasm plugin cannot open a file, a connection or read a clock. It sees the run's frames or audio, its `params`, the resources its manifest pins and the files the request supplies for its slots within `--plugin-resource-root`; its memory and tables stay under their limits, what it emits is refused at the data limits of its outputs, and each call ends at its deadline. It can use the CPU for as long as its calls may run and return any data the limits allow, so its output is as trustworthy as the plugin.

Runtime effects are WGSL that runs on the GPU. The engine checks a definition (size, the code rules, the calls it makes, a cost model) but not who sent it, and the driver's shader compiler runs on it; a host that takes requests from other users decides who may send effect definitions.

### Requests share the process

The host runs one request at a time by default (`--max-running 1`) and keeps serving: up to 1,000 requests (`--max-requests`) until it drains. Requests that one host runs share its process: its memory, its GPU device and driver, its caches on disk and, under `--plugin-isolation in-process`, the native plugin libraries it loaded. A native plugin in a worker process shares nothing with other requests but the machine and the user's rights: a crash or hang ends its worker and its request, not the host. A request's rendered frames are its own, but a fault that one request's input triggers in a decoder, a driver, a document parser or a native plugin can reach the data of the requests that run after it, or beside it with a higher `--max-running`.

- Requests from senders who must not see each other's data go to separate hosts: one host per tenant.
- Outside input goes to a disposable process: `alpha2-host serve --max-running 1 --max-requests 1`, so each request gets a fresh one. For PDF pages and for FFmpeg's decoders this is the only containment there is.
- The Node and Python clients keep hosts for many requests and pass only the plugin flags. A service that wants a process per request or per tenant makes a client per tenant and closes it (`close()`) after each job, or runs `serve` itself.

### Run it contained

Decoders, document parsers, font shaping and the GPU driver run in the host process with its rights. For input it does not trust:

- Run the host as a user of its own, or in a container, with the inputs read-only and one writable directory for outputs (plus the render cache directory and `ALPHA2_PLUGIN_CACHE` where they are used, and the temporary directory). Keep both caches where only that user can write: whoever can write there can plant entries the host reads.
- Give it no network unless it fetches plugins, and then only the origins `--plugin-origins` lists.
- Set a memory limit on the process (a cgroup on Linux, a job object on Windows). The host reads it: it sizes its budgets from the memory it may use, stops an export that runs out, and drains when its footprint passes `--max-memory-share`. An address-space limit (`ulimit -v`) turns exhaustion into an abort instead, which is still an abort.
- Keep a deadline of your own for every request and cancel it then (`cancel`, or a client's `signal` or `cancel`). The host has no wall-clock limit per request: it answers a request whose thread stays in one step for three stall limits (120 seconds by default) and retires, but a step that keeps making progress runs as long as it takes.
- The GPU path needs the device in the container: on Linux the DRM render node (`/dev/dri/renderD*`) for Mesa's Vulkan and GL drivers, or the NVIDIA container runtime's device and driver libraries; hardware decoding and encoding use the same devices. Without a GPU, `ALPHA2_RENDERER=cpu` (`--renderer cpu` for `export`) renders on the CPU and opens no graphics driver.
- Start it with a clean environment that holds only what you set. Environment variables choose libraries it loads (`ALPHA2_OPENH264_LIBRARY`, and with `ALPHA2_OPENH264_UNVERIFIED` one whose hash is not Cisco's; `ALPHA2_DXC_PATH`), caches (`ALPHA2_PLUGIN_CACHE`, `ALPHA2_HOST_CACHE_DIR`), the renderer, adapter and decode modes, budgets, and test hooks that inject faults or write captures of every exported frame (`ALPHA2_EXPORT_FRAMES_Y4M`, `ALPHA2_EXPORT_TRACE`).

### Media decoders

A source opens only with the containers alpha2 reads and decodes only with a fixed list of FFmpeg decoders (`capabilities` reports it as `decode.sourceDecoders`; see [`docs/native.md`](native.md#which-decoders-a-source-may-use)); a video of another codec conforms as `unsupported` and an audio track of another codec is left out. FFmpeg opens files only through its `file` protocol and the listed demuxers, none of which opens another file. `alpha2-host --allow-every-decoder` lifts the decoder list for the whole process; no request can. FFmpeg's parsers still run by codec id inside the demuxer, whatever the list says, and every decoder runs in C on the file's bytes.

### What is bounded

Each of these refuses past its limit with a typed error, before or while it does the work:

- Protocol frames: a request header of 128 MiB, a project of 64 MiB, attachments of 256 MiB ([Messages](host-protocol.md#messages)).
- Project documents: count limits on every list (stated as `maxItems` in [`schema/project.v1.schema.json`](../schema/project.v1.schema.json); the constants are in `schema/project-limits.v1.json`), bounded layout and text values, and a validator whose work is bounded by those counts.
- Extension sources (SVG, PDF, Lottie, OpenEXR, LaTeX, animated images, image sequences): the cost budgets in [`extensions/README.md`](../extensions/README.md#source-limits), the same in native hosts and the browser module.
- Fonts are untrusted input: a font is checked when it is registered (at most 256 faces in a collection, glyphs that expand past a budget or form a ring, color glyph paint graphs past a budget) and the outline, painting and bitmap sinks stop at fixed budgets.
- Caption files: cue and word counts, and a file no larger than a project.
- Runtime effects: the definition's size and code rules, its calls and a cost model.
- wasm plugins: one memory of at most `--plugin-memory-limit` (1 GiB by default, at most 2 GiB), 1,048,576 table elements, `--plugin-call-timeout` per call, and the data limits on what it emits; plugin files are checked against their pinned sizes and hashes.
- Native plugins in a worker process: `--plugin-call-timeout` per call, enforced by killing the worker and what it started, and the data limits on what it emits. Not their memory: a worker's memory is outside the host's limits (on Linux it stays in the host's cgroup).
- The render cache: its quota (4 GiB by default).

Not bounded, and the operator's to contain with the process limits above: the wall-clock time of a request, the size of its output and the disk it fills, what FFmpeg's decoders and parsers do with a file's bytes, the PDF interpreter (font programs, shadings, functions) and its image codecs, a GPU driver's shader compile, font table parsing and shaping, and a native plugin's memory, or all of a native plugin that runs inside the host. An extension provider call that runs long cannot be cancelled; only the stall report sees it.

### Answers are for the operator

Error messages, `failure.diagnostics`, a response's `details`, the `capabilities` report (GPU adapter and driver, OS version) and a client's standard error tail carry host paths and system details. URLs in plugin messages are shown without user info, query or fragment, but file paths stay. Show them to the operator, not to whoever sent the request.

## The MCP server

`python -m alpha2 mcp` reads and writes only under its roots: the working directory, or each `--root DIR`. That covers spec, template and project files, the media and caption files a spec names, and outputs, with links resolved; a link as an output is refused, and `render_frame` writes only `.png` files. A file argument must be a regular file no larger than a project document may be. `--read-only` leaves out the tools that write, and `--max-seconds` and `--max-pixels` bound what one render call does. The `--templates` directories and `--font` files are read at start without a root: the operator chose them. The server runs `alpha2-host` with the host's defaults, so the rest of this page applies to it.

## Clients and installers

- `npx @alpha2-sh/node install` and `python -m alpha2 install` download the release archive and check it against the SHA-256 the published package pins for it, so a mirror cannot substitute one; a source checkout, which has no pin, checks against the release's `checksums.sha256`. Extraction refuses absolute names, `..` and links that leave the archive's directory.
- The OpenH264 install checks Cisco's download and the library in it against pinned SHA-256s.
- The Python package's in-process calls (validation, compose, edit operations, text measuring) run the engine's code in the Python process. A panic there raises `Alpha2InternalError`; an abort (allocation failure, stack overflow) ends the process.

## Browsers

- The browser package fetches nothing by itself. It instantiates wasm from the URLs the page gives it; only a plugin's files are checked against pinned hashes (`loadPlugin`), and `loadPlugin`'s `fetch` option decides where they come from.
- An SVG, a Lottie animation or an image sequence names other files by locator, and the page's `ResourceReader` decides what a locator loads. A reader that fetches any URL lets a document reach any URL the page can.
- A trap or a stack overflow in an extension provider restarts its wasm module, and every source opened from it before is stale. Run provider calls in a worker with a deadline, as the editor's import does.
- A wasm plugin's memory is capped (`memoryBytes`, 1 GiB by default); the loader puts no deadline on its calls, so run extractions in a worker the page can end.
- Runtime effects compile and run on the user's GPU through the browser's own checks and the engine's.
- `@alpha2-sh/editor` keeps proxies in the origin's private file system (`alpha2-proxies`), keyed by the original's SHA-256 and shared by every project and every user of the origin. An app where several accounts share one origin removes them on sign-out (`removeKeptProxies` from `@alpha2-sh/editor/browser`, or the bridge's `removeProxies`). Export files written to the origin's private storage are deleted by `release()`.

## Mobile apps

The mobile library (a preview in this release) runs everything in the app's process: decoders, document parsers, fonts, effects and plugins. An abort there ends the app, and there is no containment to offer. Every path is one the app passes, inside its sandbox. Plugins, in a library built with the `plugins` feature, are bundled with the app: a bundled manifest's `https:` URLs are refused, and a native artifact runs, with the app's rights, only when the app passes `allowNative`.
