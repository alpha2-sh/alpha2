import {
  createEditor,
  createEmptyDocument,
  graphIssues,
} from "@alpha2-sh/editor";
import {
  createBrowserBridge,
  createWorkerPreview,
} from "@alpha2-sh/editor/browser";
import EditorWorker from "@alpha2-sh/editor/preview-worker?worker";
import HashWorker from "@alpha2-sh/editor/hash-worker?worker";
import ExtensionsWorker from "@alpha2-sh/editor/extensions-worker?worker";
import extensionsWasmUrl from "alpha2/wasm/alpha2_extensions_bg.wasm?url";
import wasmUrl from "alpha2/wasm/alpha2_wasm_bg.wasm?url";
import audioWasmUrl from "alpha2/wasm/alpha2_audio_worklet.wasm?url";
import workletUrl from "alpha2/audio-worklet-processor?worker&url";

// The title tracks the stage so a run that loses the page can say where it was.
let stage;
Object.defineProperty(globalThis, "editorTestStage", {
  get: () => stage,
  set: (value) => {
    stage = value;
    if (!document.title.startsWith("%7B")) document.title = "stage " + value;
  },
});
// Keep module evaluation synchronous: lazy engine chunks may share imports with the entry module.
// Initialization runs after those modules can evaluate.
async function main() {
  const params = new URLSearchParams(location.search);
  // Draws through WebGL2 on the main thread so the page can force a context loss.
  const contextLoss = params.has("context-loss");
  const webgl2Contexts = [];
  if (contextLoss) {
    Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
    const getContext = OffscreenCanvas.prototype.getContext;
    OffscreenCanvas.prototype.getContext = function (kind, ...rest) {
      const context = getContext.call(this, kind, ...rest);
      if (kind === "webgl2" && context && this.width > 1)
        webgl2Contexts.push(context);
      return context;
    };
  }
  // Neither WebGPU nor WebGL: preview and export use the CPU renderer.
  const noGpu = params.get("gpu") === "none";
  if (noGpu) {
    Object.defineProperty(Navigator.prototype, "gpu", { get: () => undefined });
    for (const Canvas of [HTMLCanvasElement, OffscreenCanvas]) {
      const getContext = Canvas.prototype.getContext;
      Canvas.prototype.getContext = function (kind, ...rest) {
        return kind === "webgl2" || kind === "webgl"
          ? null
          : getContext.call(this, kind, ...rest);
      };
    }
  }
  const editor = createEditor({
    document: createEmptyDocument({ width: 320, height: 180 }),
  });
  const status = document.getElementById("status");
  const reported = [];
  const error = (e) => {
    reported.push(String(e?.message ?? e));
    status.textContent = e.message;
    console.error(e);
  };
  const workerFactory = () => {
    const worker = new EditorWorker();
    // The package harness also exercises a worker failure and surface fallback.
    if (new URLSearchParams(location.search).has("fallback")) {
      const post = worker.postMessage.bind(worker);
      worker.postMessage = (message, transfer) => {
        if (message.type === "seek" && message.revision > 0) {
          queueMicrotask(() =>
            worker.onmessage({
              data: {
                ...message,
                ok: false,
                error: {
                  code: "unsupported_codec",
                  message: "Injected fallback contract case",
                },
              },
            }),
          );
        } else post(message, transfer);
      };
    }
    return worker;
  };
  const bridge = createBrowserBridge({
    editor,
    runtimeAssets: {
      wasmUrl,
      audioWasmUrl,
      workletUrl,
      hashWorkerFactory: () => new HashWorker(),
      extensionsWasmUrl,
      extensionsWorkerFactory: () => new ExtensionsWorker(),
      ...(contextLoss ? {} : { workerFactory }),
    },
    ...(contextLoss ? { renderer: "gpu" } : {}),
    onError: error,
  });
  globalThis.editorTestStage = "binding";
  const surface = await bridge.bindPreview(document.getElementById("preview"));
  const dispatch = (command) => {
    const result = editor.dispatch(command);
    if (!result.ok) throw result.error;
  };
  const insert = (id, kind = "video") => {
    const trackId = crypto.randomUUID();
    dispatch({
      type: "transaction",
      commands: [
        {
          type: "track.add",
          track: { id: trackId, kind, name: "Track", clips: [] },
        },
        {
          type: "clip.insert",
          trackId,
          clip: {
            id: crypto.randomUUID(),
            source_id: id,
            timeline_range: { start: 0, end: 120000 },
            source_range: { start: 0, end: 120000 },
            time_map: { type: "forward" },
          },
        },
      ],
    });
  };
  function addColor() {
    const id = crypto.randomUUID();
    dispatch({
      type: "asset.put",
      asset: { id, kind: "generated", name: "Color", contentRevision: 0 },
      source: {
        id,
        kind: "generated",
        generator: {
          type: "fill",
          paint: {
            type: "solid",
            color: { red: 0.8, green: 0.3, blue: 0.2, alpha: 1 },
          },
        },
      },
    });
    insert(id);
    return bridge.seek(0);
  }
  document.getElementById("shape").onclick = () => addColor().catch(error);
  document.getElementById("undo").onclick = () => editor.undo();
  document.getElementById("play").onclick = () => bridge.play().catch(error);
  document.getElementById("import").onchange = async (event) => {
    try {
      const result = await bridge.importFiles([...event.target.files]);
      result.assetIds.forEach((id) =>
        insert(
          id,
          editor.document.assets[id].kind === "audio" ? "audio" : "video",
        ),
      );
      await bridge.seek(0);
    } catch (e) {
      error(e);
    }
  };
  document.getElementById("export").onclick = async () => {
    try {
      const artifact = await bridge.startExport({
        videoCodec: "avc",
        includeAudio: false,
      }).result;
      await download(artifact, params.has("test") ? 0 : 30000);
      status.textContent = "Downloaded";
    } catch (e) {
      error(e);
    }
  };
  window.addEventListener(
    "pagehide",
    () => {
      void bridge.dispose().then(() => editor.dispose());
    },
    { once: true },
  );
  status.textContent = "Ready";
  if (params.has("test") && contextLoss) {
    try {
      globalThis.editorTestStage = "add-color";
      await addColor();
      if (bridge.previewBackend !== "main-thread")
        throw Error("Unexpected preview adapter: " + bridge.previewBackend);
      globalThis.editorTestStage = "lose-context";
      const lose = webgl2Contexts.at(-1)?.getExtension("WEBGL_lose_context");
      if (!lose) throw Error("this browser offers no WEBGL_lose_context");
      lose.loseContext();
      setTimeout(() => lose.restoreContext(), 300);
      await new Promise((resolve) => setTimeout(resolve, 50));
      reported.length = 0;
      globalThis.editorTestStage = "seek-after-loss";
      const rendered = await bridge.seek(0);
      globalThis.editorTestStage = "seek-again";
      const again = await bridge.seek(0);
      const result = {
        ok:
          rendered === true &&
          again === true &&
          reported.length === 0 &&
          bridge.previewRecoveries === 1,
        rendered,
        again,
        reported,
        previewRecoveries: bridge.previewRecoveries,
      };
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
          reported,
        }),
      );
    }
  } else if (params.has("test") && noGpu) {
    try {
      globalThis.editorTestStage = "add-color";
      const rendered = await addColor();
      const renderer = bridge.capabilities.renderer;
      globalThis.editorTestStage = "export";
      const job = bridge.startExport({
        videoCodec: "avc",
        includeAudio: false,
      });
      const artifact = await job.result.catch(() => null);
      const report = artifact?.report;
      const exported =
        rendered === true &&
        renderer?.renderer === "cpu" &&
        renderer.runtimeEffects === false &&
        bridge.capabilities.features.preview.available === true &&
        artifact !== null &&
        (artifact.file?.size ?? artifact.bytes?.byteLength ?? 0) > 0 &&
        report?.renderer?.renderer === "cpu" &&
        reported.length === 0;
      const result = {
        ok: exported && job.attempts.length === 1,
        exported,
        // The runner forgives retries after encoder stalls on a virtual machine.
        retriedAfter: job.attempts.slice(0, -1).map((attempt) => ({
          code: attempt.failure?.code ?? null,
          stage: attempt.failure?.stage ?? null,
          message: attempt.failure?.message ?? null,
        })),
        rendered,
        renderer,
        previewBackend: bridge.previewBackend,
        exportRenderer: report?.renderer ?? null,
        attempts: job.attempts.length,
        failure: job.failure ?? null,
        reported,
      };
      await artifact?.release?.();
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
          reported,
        }),
      );
    }
  } else if (params.has("test") && params.has("incomplete-graph")) {
    try {
      globalThis.editorTestStage = "add-color";
      await addColor();
      const clipId = editor.document.project.timeline.tracks[0].clips[0].id;
      dispatch({ type: "graph.create", clipId });
      const look =
        editor.document.project.timeline.tracks[0].clips[0].composition.id;
      await bridge.seek(0);
      const blur = { id: "alpha2.effect.gaussian-blur", version: 1 };
      const cases = {
        no_output: [
          { type: "graph.output", compositionId: look, nodeId: null },
        ],
        missing_input: [
          {
            type: "graph.node.add",
            compositionId: look,
            node: {
              id: "device-lost-blur",
              op: { type: "effect", input: null, effect: blur },
            },
          },
          {
            type: "graph.output",
            compositionId: look,
            nodeId: "device-lost-blur",
          },
        ],
        unbound_group_input: [
          { type: "graph.port.add", compositionId: look, name: "In" },
          {
            type: "graph.node.add",
            compositionId: look,
            node: { id: "missing-in", op: { type: "group_input", index: 0 } },
          },
          { type: "graph.output", compositionId: look, nodeId: "missing-in" },
        ],
        clip_without_clip: [
          {
            type: "composition.create",
            compositionId: "title",
            sourceId: "titled",
            name: "Title",
          },
          {
            type: "graph.node.add",
            compositionId: "title",
            node: { id: "clip", op: { type: "clip" } },
          },
          { type: "graph.output", compositionId: "title", nodeId: "clip" },
          {
            type: "graph.node.add",
            compositionId: look,
            node: {
              id: "shader-title",
              op: { type: "media", source_id: "titled" },
            },
          },
          { type: "graph.output", compositionId: look, nodeId: "shader-title" },
        ],
      };
      const results = {};
      for (const [kind, commands] of Object.entries(cases)) {
        globalThis.editorTestStage = kind;
        reported.length = 0;
        dispatch({ type: "transaction", commands });
        const issues = graphIssues(
          editor.document.project.compositions.find((c) => c.id === look),
          "clip",
          editor.document.project,
        ).map((issue) => issue.kind);
        const rendered = await bridge.seek(0);
        const issue = bridge.previewIssue;
        editor.undo();
        const recovered = await bridge.seek(0);
        results[kind] = {
          issues,
          rendered,
          issue,
          reported: [...reported],
          cleared: bridge.previewIssue === null && recovered === true,
        };
      }
      const result = {
        ok: Object.entries(results).every(
          ([kind, r]) =>
            r.issues.length === 1 &&
            r.issues[0] === kind &&
            typeof r.issue === "string" &&
            r.reported.length === 0 &&
            r.cleared,
        ),
        results,
      };
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
          reported,
        }),
      );
    }
  } else if (params.has("test") && params.get("export") === "button") {
    try {
      globalThis.editorTestStage = "add-color";
      await addColor();
      globalThis.editorTestStage = "export";
      document.getElementById("export").click();
      const deadline = performance.now() + 60000;
      while (
        status.textContent !== "Downloaded" &&
        !reported.length &&
        performance.now() < deadline
      )
        await new Promise((resolve) => setTimeout(resolve, 50));
      const directory = await (
        await navigator.storage.getDirectory()
      ).getDirectoryHandle("alpha2-exports", { create: true });
      const left = [];
      for await (const name of directory.keys()) left.push(name);
      const result = {
        ok:
          status.textContent === "Downloaded" &&
          reported.length === 0 &&
          left.length === 0,
        status: status.textContent,
        reported,
        left,
      };
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
          reported,
        }),
      );
    }
  } else if (params.has("test") && params.get("export") === "exact-rate") {
    try {
      globalThis.editorTestStage = "add-color";
      await addColor();
      // Frame 30 at 29.97 fps starts before 1.01 s, so 31 frames.
      dispatch({
        type: "metadata.update",
        patch: { exportRange: { start: 0, end: 121_200 } },
      });
      globalThis.editorTestStage = "export";
      const job = bridge.startExport({
        videoCodec: "avc",
        includeAudio: false,
        frameRate: { num: 30_000, den: 1_001 },
      });
      const frames = [];
      job.subscribe((progress) => {
        if (progress.frames) frames.push(progress.frames);
      });
      const artifact = await job.result.catch(() => null);
      let refused = null;
      try {
        bridge.startExport({ frameRate: 29.97 });
      } catch (e) {
        refused = e.code ?? String(e);
      }
      globalThis.editorTestStage = "read-output";
      const { Input, BlobSource, ALL_FORMATS } = await import("mediabunny");
      const file = artifact?.file ?? artifact?.blob;
      const track = file
        ? await new Input({
            source: new BlobSource(file),
            formats: ALL_FORMATS,
          }).getPrimaryVideoTrack()
        : null;
      const stats = await track?.computePacketStats();
      const last = frames.at(-1);
      const result = {
        ok:
          refused === "invalid_frame_rate" &&
          stats?.packetCount === 31 &&
          Math.abs(stats.averagePacketRate - 30_000 / 1_001) < 0.01 &&
          frames.every((f) => f.completed <= f.total) &&
          last?.total === 31 &&
          last?.completed === 31,
        refused,
        packets: stats?.packetCount ?? null,
        packetRate: stats?.averagePacketRate ?? null,
        lastProgress: last ?? null,
        overrun: frames.filter((f) => f.completed > f.total).length,
        failure: job.failure ?? null,
      };
      await artifact?.release?.();
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
        }),
      );
    }
  } else if (
    params.has("test") &&
    params.get("import") === "no-audio-decoder"
  ) {
    try {
      globalThis.editorTestStage = "import-audio";
      delete globalThis.AudioDecoder;
      const audioFile = new File(
        [
          await fetch(`${import.meta.env.BASE_URL}test-audio.m4a`).then((r) =>
            r.blob(),
          ),
        ],
        "audio.m4a",
        { type: "audio/mp4" },
      );
      const { assetIds } = await bridge.importFiles([audioFile]);
      const asset = bridge.getAsset(assetIds[0]);
      const result = {
        ok:
          asset?.audioDecodable === false &&
          asset.audioDecodeFacts?.kind === "audio" &&
          editor.document.assets[assetIds[0]]?.kind === "audio" &&
          reported.length === 0,
        audioDecodable: asset?.audioDecodable,
        facts: asset?.audioDecodeFacts ?? null,
        reported,
      };
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
        }),
      );
    }
  } else if (
    params.has("test") &&
    params.get("proxy") === "no-create-writable"
  ) {
    try {
      delete FileSystemFileHandle.prototype.createWritable;
      globalThis.editorTestStage = "import-video";
      const videoFile = new File(
        [
          await fetch(`${import.meta.env.BASE_URL}test-video.mp4`).then((r) =>
            r.blob(),
          ),
        ],
        "video.mp4",
        { type: "video/mp4" },
      );
      const { assetIds } = await bridge.importFiles([videoFile]);
      insert(assetIds[0]);
      globalThis.editorTestStage = "proxy";
      await bridge.generateProxy(assetIds[0]);
      const status = bridge.proxyStatus(assetIds[0]);
      const result = {
        ok: status.available === true && reported.length === 0,
        status,
        reported,
      };
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
          reported,
        }),
      );
    }
  } else if (params.has("test") && params.has("export")) {
    // Simulates WebKit conditions: nearly full storage, no AudioEncoder, or no createWritable.
    const mode = params.get("export");
    try {
      globalThis.editorTestStage = "add-color";
      await addColor();
      globalThis.editorTestStage = "import-audio";
      const { assetIds } = await bridge.importFiles([toneWav(1, 48000)]);
      insert(assetIds[0], "audio");
      if (mode === "no-audio-encoder") delete globalThis.AudioEncoder;
      if (mode === "no-create-writable")
        delete FileSystemFileHandle.prototype.createWritable;
      if (mode === "webkit-storage")
        StorageManager.prototype.estimate = async () => ({
          quota: 150e9,
          usage: 150e9 - 1e6,
        });
      globalThis.editorTestStage = "export";
      const job = bridge.startExport({ videoCodec: "avc", includeAudio: true });
      const artifact = await job.result.catch(() => null);
      const report = artifact?.report;
      const result =
        mode === "no-audio-encoder"
          ? {
              ok:
                artifact === null &&
                job.failure?.code === "INVALID_INPUT" &&
                job.failure.phase === "prepare" &&
                job.attempts.length === 1,
              failure: job.failure ?? null,
              attempts: job.attempts.length,
            }
          : mode === "no-create-writable"
            ? {
                ok:
                  artifact?.kind === "blob" &&
                  (artifact.blob?.size ?? 0) > 0 &&
                  (report.audioCodec === "aac" || report.audioCodec === "opus"),
                kind: artifact?.kind ?? null,
                bytes: artifact?.blob?.size ?? null,
                failure: job.failure ?? null,
              }
            : {
                ok:
                  artifact?.kind === "file" &&
                  artifact.file.size > 0 &&
                  (report.audioCodec === "aac" ||
                    report.audioCodec === "opus") &&
                  report.performance?.output?.storageEstimate?.belowEstimate ===
                    true,
                kind: artifact?.kind ?? null,
                bytes: artifact?.file?.size ?? null,
                audioCodec: report?.audioCodec ?? null,
                storageEstimate: report?.performance?.output?.storageEstimate,
                failure: job.failure ?? null,
              };
      await artifact?.release?.();
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
        }),
      );
    }
  } else if (params.has("test") && params.has("extensions")) {
    try {
      const longTasks = [];
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries())
          longTasks.push({ start: entry.startTime, duration: entry.duration });
      }).observe({ type: "longtask" });
      const fetched = async (name) =>
        new File([await (await fetch(name)).arrayBuffer()], name);
      const longestDuring = async (run) => {
        const start = performance.now();
        const result = await run();
        await new Promise((resolve) => setTimeout(resolve, 50));
        const end = performance.now();
        const during = longTasks.filter(
          (task) => task.start + task.duration >= start && task.start <= end,
        );
        return {
          result,
          ms: Math.round(end - start - 50),
          longestTaskMs: Math.round(
            Math.max(0, ...during.map((task) => task.duration)),
          ),
        };
      };
      const imported = {};
      for (const name of ["clip.gif", "squares.json", "pages.pdf"]) {
        globalThis.editorTestStage = `import ${name}`;
        const file = await fetched(name);
        const { result, ms, longestTaskMs } = await longestDuring(() =>
          bridge.importFiles([file]),
        );
        const asset = editor.document.assets[result.assetIds[0]];
        imported[name] = {
          ms,
          longestTaskMs,
          kind: asset.kind,
          duration: asset.duration,
          thumbnail: Boolean(bridge.getAsset(asset.id)?.thumbnail?.dataUrl),
        };
      }
      globalThis.editorTestStage = "cancel";
      const gif = await fetched("clip.gif");
      const controller = new AbortController();
      const cancelled = bridge.importFiles([gif], {
        signal: controller.signal,
      });
      // A page thread busy in the provider would fire this timer late.
      const due = performance.now() + 300;
      await new Promise((resolve) => setTimeout(resolve, 300));
      const timerLateMs = Math.round(performance.now() - due);
      controller.abort();
      const cancel = await cancelled.then(
        () => ({ outcome: "imported" }),
        (e) => ({ outcome: e?.name ?? String(e) }),
      );
      cancel.ms = Math.round(performance.now() - due);
      cancel.timerLateMs = timerLateMs;
      const refused = {};
      for (const name of ["morphology.svg", "painted.json"]) {
        globalThis.editorTestStage = `refuse ${name}`;
        const file = await fetched(name);
        const { result, ms, longestTaskMs } = await longestDuring(() =>
          bridge.importFiles([file]).then(
            () => "imported",
            (e) => `${e?.code ?? e?.name}: ${e?.message ?? e}`,
          ),
        );
        refused[name] = { result, ms, longestTaskMs };
      }
      const pageLoadedProviderModule = performance
        .getEntriesByType("resource")
        .some((entry) => entry.name.includes("alpha2_extensions_bg"));
      const result = {
        ok:
          !pageLoadedProviderModule &&
          Object.entries(imported).every(
            ([name, entry]) =>
              entry.kind === (name.endsWith(".pdf") ? "image" : "video") &&
              entry.thumbnail &&
              entry.longestTaskMs < 200,
          ) &&
          cancel.outcome === "AbortError" &&
          cancel.ms < 200 &&
          Object.values(refused).every(
            (entry) =>
              /^unreadable_media: .*too large/.test(entry.result) &&
              entry.longestTaskMs < 200,
          ),
        pageLoadedProviderModule,
        imported,
        cancel,
        refused,
        assets: Object.keys(editor.document.assets).length,
      };
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(JSON.stringify(result));
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
        }),
      );
    }
  } else if (params.has("test")) {
    try {
      globalThis.editorTestStage = "add-color";
      await addColor();
      const previewBackend = bridge.previewBackend;
      if (
        previewBackend !==
        (new URLSearchParams(location.search).has("fallback")
          ? "main-thread"
          : "worker")
      )
        throw Error("Unexpected preview adapter: " + previewBackend);
      globalThis.editorTestStage = "worker-port";
      const worker = createWorkerPreview(new EditorWorker(), {
        runtimeAssets: { wasmUrl },
      });
      await worker.replaceProject(editor.document.project, 1);
      const first = await worker.seek(0);
      const videoFile = new File(
        [
          await fetch(`${import.meta.env.BASE_URL}test-video.mp4`).then((r) =>
            r.blob(),
          ),
        ],
        "video.mp4",
        { type: "video/mp4" },
      );
      const audioFile = new File(
        [
          await fetch(`${import.meta.env.BASE_URL}test-audio.m4a`).then((r) =>
            r.blob(),
          ),
        ],
        "audio.m4a",
        { type: "audio/mp4" },
      );
      const project = {
        ...editor.document.project,
        sources: [
          { id: "v", kind: "video", locator: "worker://video" },
          { id: "a", kind: "audio", locator: "worker://audio" },
        ],
        timeline: {
          tracks: [
            {
              id: "vt",
              name: "Video",
              kind: "video",
              clips: [
                {
                  id: "vc",
                  source_id: "v",
                  timeline_range: { start: 0, end: 120000 },
                  source_range: { start: 0, end: 120000 },
                  time_map: { type: "forward" },
                },
              ],
            },
            {
              id: "at",
              name: "Audio",
              kind: "audio",
              clips: [
                {
                  id: "ac",
                  source_id: "a",
                  timeline_range: { start: 0, end: 120000 },
                  source_range: { start: 0, end: 120000 },
                  time_map: { type: "forward" },
                },
              ],
            },
          ],
        },
      };
      await worker.replaceProject(project, 2, [
        {
          id: "v",
          kind: "video",
          file: videoFile,
          duration: 1,
          width: 96,
          height: 64,
        },
        {
          id: "a",
          kind: "audio",
          file: audioFile,
          duration: 1,
          hasAudio: true,
        },
      ]);
      const decoded = await worker.seek(0);
      const planes = await worker.mix(0, 1024);
      if (
        !first.rendered ||
        !decoded.rendered ||
        planes.length !== 2 ||
        planes[0].length !== 1024
      )
        throw Error("Worker decode/audio failed");
      await worker.dispose();
      globalThis.editorTestStage = "play";
      await bridge.play();
      bridge.pause();
      // An AudioContext that never resumes (Firefox without sound output) must not block Play.
      globalThis.editorTestStage = "play-without-audio";
      const { resume } = AudioContext.prototype;
      Object.defineProperty(AudioContext.prototype, "state", {
        configurable: true,
        get: () => "suspended",
      });
      AudioContext.prototype.resume = () => new Promise(() => {});
      const recoveriesBefore = bridge.previewRecoveries;
      let playWithoutAudioRecoveries = 0;
      try {
        await bridge.play();
        if (!bridge.playing) throw Error("Play did not start without audio");
        const from = bridge.tick;
        await new Promise((resolve) => setTimeout(resolve, 300));
        if (!status.textContent.includes("without sound"))
          throw Error(`No audio notice: ${status.textContent}`);
        // SwiftShader loses its device every few seconds; recovery takes a moment.
        for (let wait = 0; wait < 50 && bridge.tick <= from; wait++)
          await new Promise((resolve) => setTimeout(resolve, 100));
        if (bridge.tick <= from)
          throw Error(
            `Playback without audio did not advance: ${from} -> ${bridge.tick}, playing ${bridge.playing}, hidden ${document.hidden}, status ${status.textContent}, recoveries ${bridge.previewRecoveries - recoveriesBefore}`,
          );
        playWithoutAudioRecoveries =
          bridge.previewRecoveries - recoveriesBefore;
        bridge.pause();
      } finally {
        delete AudioContext.prototype.state;
        AudioContext.prototype.resume = resume;
      }
      const previewRecoveries = bridge.previewRecoveries;
      let rendererBackend = bridge.diagnostics?.adapter?.backend ?? null;
      globalThis.editorTestStage = "decode-1080p";
      const { MediabunnyVideoDecoder } = await import("alpha2/mediabunny");
      const fullHd = await MediabunnyVideoDecoder.open(
        "full-hd",
        await fetch(`${import.meta.env.BASE_URL}test-video-1080p.mp4`).then(
          (r) => r.blob(),
        ),
      );
      let fullHdDecode;
      try {
        const decoded = await fullHd.refill({
          sourceId: "full-hd",
          mediaTimeTicks: 0n,
          reason: "seek",
          generation: 0,
        });
        fullHdDecode = {
          size: [decoded.frame.displayWidth, decoded.frame.displayHeight],
          softwareFallback: fullHd.softwareFallback ?? null,
        };
        decoded.frame.close();
      } finally {
        fullHd.close();
      }
      globalThis.editorTestStage = "surface-dispose";
      await surface.dispose();
      globalThis.editorTestStage = "rebind";
      await bridge.bindPreview(document.createElement("canvas"));
      await bridge.seek(0);
      rendererBackend ??= bridge.diagnostics?.adapter?.backend ?? null;
      await bridge.dispose();
      editor.dispose();
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: true,
          isolated: crossOriginIsolated,
          resources: bridge.resourceCount,
          previewBackend,
          workerDecode: true,
          workerAudioFrames: 1024,
          previewRecoveries,
          playWithoutAudioRecoveries,
          rendererBackend,
          fullHdDecode,
        }),
      );
    } catch (e) {
      document.title = encodeURIComponent(
        JSON.stringify({
          ok: false,
          error: String(e),
          stack: e.stack,
          phase: globalThis.editorTestStage,
        }),
      );
    }
  }
}
void main().catch((error) => {
  document.getElementById("status").textContent = error.message;
  if (new URLSearchParams(location.search).has("test"))
    document.title = encodeURIComponent(
      JSON.stringify({ ok: false, error: String(error), stack: error.stack }),
    );
});

// Downloads a temporary origin-private file or memory blob; a user-picked file needs nothing.
async function download(artifact, keepMs) {
  if (artifact.kind === "saved") return;
  const url = URL.createObjectURL(
    artifact.kind === "file" ? artifact.file : artifact.blob,
  );
  const a = document.createElement("a");
  a.href = url;
  a.download = artifact.name;
  a.click();
  await new Promise((resolve) => setTimeout(resolve, keepMs));
  URL.revokeObjectURL(url);
  await artifact.release?.();
}

// PCM WAV, which mediabunny decodes itself, so it imports without AudioDecoder.
function toneWav(seconds, sampleRate) {
  const frames = Math.round(seconds * sampleRate);
  const bytes = new DataView(new ArrayBuffer(44 + frames * 4));
  const text = (offset, value) =>
    [...value].forEach((c, i) => bytes.setUint8(offset + i, c.charCodeAt(0)));
  text(0, "RIFF");
  bytes.setUint32(4, 36 + frames * 4, true);
  text(8, "WAVEfmt ");
  bytes.setUint32(16, 16, true);
  bytes.setUint16(20, 1, true);
  bytes.setUint16(22, 2, true);
  bytes.setUint32(24, sampleRate, true);
  bytes.setUint32(28, sampleRate * 4, true);
  bytes.setUint16(32, 4, true);
  bytes.setUint16(34, 16, true);
  text(36, "data");
  bytes.setUint32(40, frames * 4, true);
  for (let i = 0; i < frames; i++) {
    const sample = Math.round(
      8000 * Math.sin((2 * Math.PI * 440 * i) / sampleRate),
    );
    bytes.setInt16(44 + i * 4, sample, true);
    bytes.setInt16(46 + i * 4, sample, true);
  }
  return new File([bytes.buffer], "tone.wav", { type: "audio/wav" });
}
