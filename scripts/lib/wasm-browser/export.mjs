// Wasm export cases: the schedule, waiting frames, the working set, and every project audio rate.

// The packaged engine plays tests/audio_determinism's fixed mix to the hash native code pins, and
// every project rate (8 to 96 kHz) exports as natively. Chrome on Linux has no AAC encoder and
// Opus converts to 48 kHz itself, so the sink's conversion also runs on an explicit Opus rate.
export function exportEveryProjectRate() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { audioEncoderSampleRate, MediabunnySink } = await import("/packages/alpha2/dist/mediabunny.js");
const { renderOfflineAudio } = await import("/packages/alpha2/dist/audio.js");
const mb = await import("mediabunny");
if (typeof AudioEncoder === "undefined" || typeof AudioDecoder === "undefined") {
  report({ skipped: "this browser has no WebCodecs AudioEncoder and AudioDecoder", capability: "webcodecs-audio" });
} else {
  const AMPLITUDE = 0.25;
  // Half a second of a stereo 997 Hz tone at the project rate, exported and decoded again.
  const exportTone = async (codec, projectRate, encoderRate) => {
    const sink = new MediabunnySink({
      container: "mp4",
      audioCodec: codec,
      audioBitrate: 128000,
      audioSampleRate: encoderRate,
      audioResampler: wasm,
      destination: { kind: "memory", maxBytes: 1 << 24 },
    });
    const total = projectRate / 2;
    await renderOfflineAudio(BigInt(total), [1024, 333], async (start, frames) => {
      const left = new Float32Array(frames);
      for (let i = 0; i < frames; i += 1)
        left[i] = AMPLITUDE * Math.sin((2 * Math.PI * 997 * (Number(start) + i)) / projectRate);
      return { sourceId: "mix", startSample: start, generation: 0, sampleRate: projectRate, channels: [left, left.slice()] };
    }, sink);
    const input = new mb.Input({ source: new mb.BufferSource(sink.output.buffer), formats: mb.ALL_FORMATS });
    const track = await input.getPrimaryAudioTrack();
    const duration = await track.computeDuration();
    let sum = 0;
    let count = 0;
    for await (const sample of new mb.AudioSampleSink(track).samples(0.1, 0.4)) {
      const plane = new Float32Array(sample.numberOfFrames);
      sample.copyTo(plane, { planeIndex: 0, format: "f32-planar" });
      for (const value of plane) { sum += value * value; count += 1; }
      sample.close();
    }
    input.dispose();
    const rms = Math.sqrt(sum / Math.max(1, count));
    // The tone through a lossy codec: its level within 15 %, its priming trimmed, and its length
    // within 50 ms for Opus, and for AAC within two of its 1024-frame frames (the last, partly
    // filled, and the one the encoder flushes after it; 256 ms at 8 kHz). AAC's stream is at the
    // rate it was encoded at (Opus streams are always 48 kHz).
    const lengthSlack = codec === "aac" ? 2048 / track.sampleRate : 0.05;
    const ok = Math.abs(rms - AMPLITUDE / Math.SQRT2) < 0.15 * (AMPLITUDE / Math.SQRT2)
      && Math.abs(duration - 0.5) < lengthSlack
      && (codec !== "aac" || track.sampleRate === encoderRate);
    return { ok, fileRate: track.sampleRate, rms: Number(rms.toFixed(4)), duration: Number(duration.toFixed(4)) };
  };
  const results = {};
  const exported = new Set();
  for (const projectRate of [8000, 22050, 44100, 50000, 96000]) {
    for (const codec of ["aac", "opus"]) {
      const encoderRate = await audioEncoderSampleRate(codec, { sampleRate: projectRate, numberOfChannels: 2, bitrate: 128000 });
      const key = codec + "@" + projectRate;
      if (encoderRate === null) { results[key] = "no encoder"; continue; }
      results[key] = { encoderRate, ...(await exportTone(codec, projectRate, encoderRate)) };
      if (results[key].ok) exported.add(projectRate);
    }
  }
  // The conversion through a real encoder: Opus at 48 kHz from rates this browser's Opus would
  // also take directly.
  for (const projectRate of [22050, 50000, 96000]) {
    const key = "opus@" + projectRate + "->48000";
    results[key] = await AudioEncoder.isConfigSupported({ codec: "opus", sampleRate: 48000, numberOfChannels: 2, bitrate: 128000 }).then((r) => r.supported)
      ? await exportTone("opus", projectRate, 48000)
      : "no encoder";
  }
  const converted = Object.entries(results).filter(([key]) => key.includes("->")).map(([, value]) => value);
  const ok = exported.size === 5 && converted.every((value) => value === "no encoder" || value.ok)
    && Object.values(results).every((value) => value === "no encoder" || value.ok);
  report({ ok, results });
}
`;
}

// Each container through the browser sink, decoded back: one second of 30 fps video and 48 kHz
// stereo audio comes back as 30 frames on the frame grid (to the millisecond in Matroska, whose
// blocks are stamped in milliseconds) with the codecs asked for. MOV offers only AAC, so a browser
// without an AAC encoder (Chrome on Linux) writes it without audio and the sink must refuse Opus.
export function exportEveryContainer() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { audioEncoderSampleRate, canEncodeVideoTrack, MediabunnySink } = await import("/packages/alpha2/dist/mediabunny.js");
const { renderOfflineAudio } = await import("/packages/alpha2/dist/audio.js");
const mb = await import("mediabunny");
if (typeof VideoEncoder === "undefined" || typeof AudioEncoder === "undefined") {
  // The first capability missing names the skip.
  report(typeof VideoEncoder === "undefined"
    ? { skipped: "this browser has no WebCodecs VideoEncoder", capability: "webcodecs-video" }
    : { skipped: "this browser has no WebCodecs AudioEncoder", capability: "webcodecs-audio" });
} else {
  const WIDTH = 320;
  const HEIGHT = 240;
  const FRAMES = 30;
  const videoRateControl = { bitrate: 1000000, bitrateMode: "variable" };
  const formats = { mp4: "MP4", mov: "QuickTime File Format", mkv: "Matroska", webm: "WebM" };
  const exportOne = async (container, videoCodec, audioCodecs) => {
    const videoEncoding = await canEncodeVideoTrack(videoCodec, { width: WIDTH, height: HEIGHT, rate: videoRateControl });
    if (videoEncoding === null) return "no " + videoCodec + " encoder";
    let audioCodec;
    for (const codec of audioCodecs) {
      if ((await audioEncoderSampleRate(codec, { sampleRate: 48000, numberOfChannels: 2, bitrate: 128000 })) === 48000) {
        audioCodec = codec;
        break;
      }
    }
    const sink = new MediabunnySink({
      container,
      videoCodec,
      videoRateControl,
      ...(videoEncoding.codecString === undefined ? {} : { videoCodecString: videoEncoding.codecString }),
      frameRate: { num: 30, den: 1 },
      ...(audioCodec === undefined ? {} : { audioCodec, audioBitrate: 128000 }),
      destination: { kind: "memory", maxBytes: 1 << 24 },
    });
    const canvas = new OffscreenCanvas(WIDTH, HEIGHT);
    const context = canvas.getContext("2d");
    const writeVideo = async (track) => {
      for (let index = 0; index < FRAMES; index += 1) {
        context.fillStyle = "rgb(" + ((index * 8) & 255) + ", 64, 128)";
        context.fillRect(0, 0, WIDTH, HEIGHT);
        const frame = new VideoFrame(canvas, { timestamp: Math.round((index * 1e6) / 30), duration: Math.round(1e6 / 30) });
        await track.writeVideo({ index: BigInt(index), timeTicks: BigInt(index * 4000), frame });
      }
      await track.finish();
    };
    const writeAudio = (track) => renderOfflineAudio(48000n, [1024], async (start, frames) => {
      const left = new Float32Array(frames);
      for (let i = 0; i < frames; i += 1) left[i] = 0.25 * Math.sin((2 * Math.PI * 997 * (Number(start) + i)) / 48000);
      return { sourceId: "mix", startSample: start, generation: 0, sampleRate: 48000, channels: [left, left.slice()] };
    }, track);
    await Promise.all([writeVideo(sink.videoTrack), ...(audioCodec === undefined ? [] : [writeAudio(sink.audioTrack)])]);
    const input = new mb.Input({ source: new mb.BufferSource(sink.output.buffer), formats: mb.ALL_FORMATS });
    try {
      const format = (await input.getFormat()).name;
      const video = await input.getPrimaryVideoTrack();
      const audio = await input.getPrimaryAudioTrack();
      // Matroska stores each block's time in whole milliseconds.
      const slack = container === "mkv" || container === "webm" ? 0.0005 + 1e-6 : 1e-6;
      const times = [];
      for await (const packet of new mb.EncodedPacketSink(video).packets()) times.push(packet.timestamp);
      times.sort((a, b) => a - b);
      const onGrid = times.every((time, index) => Math.abs(time - index / 30) <= slack);
      const videoDuration = await video.computeDuration();
      const audioDuration = audio === null ? null : await audio.computeDuration();
      const ok = format === formats[container]
        && video.codec === videoCodec
        && (audio?.codec ?? undefined) === audioCodec
        && times.length === FRAMES
        && onGrid
        && Math.abs(videoDuration - 1) <= 1 / 30 + slack
        && (audioDuration === null || Math.abs(audioDuration - 1) < 0.05);
      return {
        ok, format, videoCodec: video.codec, audioCodec: audio?.codec ?? null, frames: times.length, onGrid,
        videoDuration: Number(videoDuration.toFixed(4)),
        audioDuration: audioDuration === null ? null : Number(audioDuration.toFixed(4)),
      };
    } finally {
      input.dispose();
    }
  };
  const results = {
    webm: await exportOne("webm", "vp9", ["opus"]),
    mkv: await exportOne("mkv", "avc", ["aac", "opus"]),
    mov: await exportOne("mov", "avc", ["aac"]),
    "mp4-av1": await exportOne("mp4", "av1", ["opus"]),
  };
  // A pair the container does not offer is refused before any encoder opens.
  let refused = null;
  try {
    new MediabunnySink({ container: "mov", audioCodec: "opus", audioBitrate: 128000, destination: { kind: "memory", maxBytes: 1 << 20 } });
  } catch (error) {
    refused = error instanceof RangeError ? error.message : String(error);
  }
  const ran = Object.values(results).filter((value) => typeof value !== "string");
  const ok = results.webm.ok === true && ran.every((value) => value.ok) && refused !== null;
  report({ ok, results, refused });
}
`;
}

// AAC audio starts at the output's zero, as the video does: the browser's encoder puts its
// priming (2112 frames on Chrome's macOS encoder, which WebCodecs does not report) before the
// audio, and the sink places it before zero, where an MP4 or MOV edit list, or Matroska's negative
// block times, leave it out. Chrome on Linux has no AAC encoder: the case skips there.
export function exportAacPriming() {
  return `
const { aacEncoderDelay, audioEncoderSampleRate, MediabunnySink } = await import("/packages/alpha2/dist/mediabunny.js");
const { renderOfflineAudio } = await import("/packages/alpha2/dist/audio.js");
const mb = await import("mediabunny");
const config = { sampleRate: 48000, numberOfChannels: 2, bitrate: 128000 };
const rate = typeof AudioEncoder === "undefined" ? null : await audioEncoderSampleRate("aac", config);
if (rate === null) {
  report({ skipped: "this browser has no AAC encoder", capability: "aac-encode" });
} else {
  const delay = await aacEncoderDelay({ ...config, sampleRate: rate });
  const results = {};
  for (const container of ["mp4", "mov", "mkv"]) {
    const sink = new MediabunnySink({
      container, audioCodec: "aac", audioBitrate: 128000,
      destination: { kind: "memory", maxBytes: 1 << 24 },
    });
    // Half a second of a 997 Hz tone from the first sample on.
    await renderOfflineAudio(BigInt(rate / 2), [1024], async (start, frames) => {
      const left = new Float32Array(frames);
      for (let i = 0; i < frames; i += 1) left[i] = 0.25 * Math.sin((2 * Math.PI * 997 * (Number(start) + i)) / rate);
      return { sourceId: "mix", startSample: start, generation: 0, sampleRate: rate, channels: [left, left.slice()] };
    }, sink);
    const input = new mb.Input({ source: new mb.BufferSource(sink.output.buffer), formats: mb.ALL_FORMATS });
    try {
      const track = await input.getPrimaryAudioTrack();
      let onset = null;
      for await (const sample of new mb.AudioSampleSink(track).samples()) {
        const plane = new Float32Array(sample.numberOfFrames);
        sample.copyTo(plane, { planeIndex: 0, format: "f32-planar" });
        const at = plane.findIndex((value) => Math.abs(value) > 0.05);
        sample.close();
        if (at >= 0) { onset = sample.timestamp + at / track.sampleRate; break; }
      }
      // The tone's first loud sample, a few frames in, within a millisecond of zero.
      results[container] = { ok: onset !== null && Math.abs(onset) < 0.001, onset };
    } finally {
      input.dispose();
    }
  }
  // An encoder without priming (delay 0) is allowed when no container's decoded onset shows any.
  const maxAbsOnset = Math.max(...Object.values(results).map((value) => Math.abs(value.onset ?? Infinity)));
  report({
    ok: Object.values(results).every((value) => value.ok) && (delay > 0 || maxAbsOnset < 0.001),
    rate, delay, results,
  });
}
`;
}

// The range, frame count and audio samples must match tests/golden/time-rules-corpus.json.
export function exportSchedule() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await gpuEngine(WebEffectRegistry);
const corpus = await (await fetch("/tests/golden/time-rules-corpus.json")).json();
const mismatches = [];
let checked = 0;
for (const sampleRate of [44100, 48000]) {
  engine.loadProject(JSON.stringify({
    schema_version: 1, id: "schedule", name: "Schedule",
    settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: sampleRate },
    sources: [], timeline: { tracks: [] },
  }));
  for (const c of corpus.exports.filter((c) => c.sampleRate === sampleRate)) {
    engine.beginExport(BigInt(c.start), BigInt(c.end), c.rate.num, c.rate.den, "h264");
    const progress = JSON.parse(engine.exportProgress());
    engine.endExport();
    checked++;
    const got = [progress.effectiveRange.startTicks, progress.effectiveRange.endTicks, progress.total, progress.audioWindow?.startSample, progress.audioWindow?.frames].join(" ");
    const want = [c.effectiveStart, c.effectiveEnd, c.total, c.audioStartSample, c.audioFrames].join(" ");
    if (got !== want) mismatches.push(c.rate.num + "/" + c.rate.den + " [" + c.start + ", " + c.end + ") at " + sampleRate + ": " + got + " != " + want);
  }
}
report({ ok: checked === corpus.exports.length && mismatches.length === 0, checked, mismatches: mismatches.slice(0, 5) });
`;
}

// A waiting export frame names source frames as a preview does: a trimmed clip at the trimmed time.
export function exportWaitingFrames() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(16, 16));
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "trim", name: "Trim",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "s", locator: "fixture://s", kind: "video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 60000, end: 180000 },
  }] }] },
}));
const expected = [{ clipId: "c", readerId: null, sourceId: "s", mediaTimeTicks: "60000" }];
const named = (ticket) => ticket.kind === "waiting"
  && JSON.stringify(ticket.missingSources) === JSON.stringify(["s"])
  && JSON.stringify(ticket.missingFrames) === JSON.stringify(expected);
const at = engine.beginExportAt(0n, "h264");
engine.beginExport(0n, 4000n, 30, 1, "h264");
const frame = engine.beginExportFrame();
const promised = await engine.exportFrame();
engine.endExport();
const planes = new Uint8Array(16 * 16 + 2 * 8 * 8).fill(128);
engine.uploadYuvFrame("s", 60000n, "I420", 16, 16, planes, Uint32Array.from([0, 16, 256, 8, 320, 8]),
  Uint32Array.from([0, 0, 16, 16]), 16, 16,
  JSON.stringify({ primaries: "bt709", transfer: "bt709", matrix: "bt709", range: "limited", dynamic_range: "sdr" }),
  "left", 16, 16, 0, false);
const after = engine.beginExportAt(0n, "h264");
const rendered = after.kind === "rendered" ? (await after.readback).kind : after.kind;
report({
  ok: named(at) && named(frame) && named(promised) && rendered === "rendered",
  at, frame, promised, rendered,
});
`;
}

// An export pins the sources of the frame it waits for; the upload that would outgrow the budget
// is refused at once with RESOURCE_LIMIT, on both export paths.
export function exportWorkingSet() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 48));
const pixels = new Uint8Array(64 * 48 * 4).fill(255);
const frameBytes = engine.uploadRgbaFrame("probe", 0n, 64, 48, pixels).retainedBytes;
engine.releaseSourceFrames("probe");
engine.setUploadedFrameBudget(frameBytes * 2.5);
const clip = (id, source) => ({ id, name: id, kind: "video", clips: [{
  id: "clip-" + id, source_id: source,
  timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
}] });
const load = (sources) => engine.loadProject(JSON.stringify({
  schema_version: 1, id: "set", name: "Set",
  settings: { width: 64, height: 48, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: sources.map(([id, kind]) => ({ id, locator: "fixture://" + id, kind })),
  timeline: { tracks: sources.map(([id]) => clip(id, id)) },
}));
const refusal = (upload) => {
  try { upload(); return null; } catch (error) {
    return { code: error.code, stage: error.failure?.stage, phase: error.failure?.phase, requestedBytes: error.failure?.requestedBytes };
  }
};
const expected = { code: "RESOURCE_LIMIT", stage: "working-set-over-budget", phase: "upload", requestedBytes: 3 * frameBytes };
// Three layers, two and a half frames of budget.
load([["a", "video"], ["b", "video"], ["c", "video"]]);
engine.beginExport(0n, 4000n, 30, 1, "h264");
const waiting = engine.beginExportFrame();
const fill = (frames) => frames.map((need) => refusal(() => engine.uploadRgbaFrame(need.sourceId, BigInt(need.mediaTimeTicks), 64, 48, pixels)));
const stateful = fill(waiting.missingFrames);
const heldStateful = ["a", "b"].every((id) => engine.uploadedFrameBytes >= BigInt(2 * frameBytes));
engine.endExport();
engine.clearFrames();
const at = engine.beginExportAt(0n, "h264");
const stateless = fill(at.missingFrames);
const sameKey = (refused) => JSON.stringify(refused) === JSON.stringify([null, null, expected]);
// A still under a video, one video frame per export frame: the still stays.
engine.clearFrames();
load([["logo", "image"], ["v", "video"]]);
const logo = engine.uploadRgbaImage("logo", 64, 48, pixels).generation;
engine.beginExport(0n, 20000n, 30, 1, "h264");
const kept = [];
for (let frame = 0; frame < 5; frame++) {
  let ticket = engine.beginExportFrame();
  for (const need of ticket.kind === "waiting" ? ticket.missingFrames : [])
    engine.uploadRgbaFrame(need.sourceId, BigInt(need.mediaTimeTicks), 64, 48, pixels);
  if (ticket.kind === "waiting") ticket = engine.beginExportFrame();
  if (ticket.kind === "rendered") await ticket.readback;
  kept.push(ticket.kind === "rendered" && engine.holdsImage("logo", logo));
}
engine.endExport();
report({
  ok: waiting.kind === "waiting" && sameKey(stateful) && heldStateful && at.kind === "waiting" && sameKey(stateless) && kept.every(Boolean),
  frameBytes, stateful, stateless, kept,
});
`;
}

// Alpha and 10-bit export pictures: the engine draws `yuva420p` frames without the background and
// `yuv420p10le` frames as 10-bit codes; a 10-bit VP9 file is encoded and read back where this
// browser encodes it, and alpha is offered only where a real one-frame encode keeps it.
export function exportPixels() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const { canEncodePixels, MediabunnySink } = await import("/packages/alpha2/dist/mediabunny.js");
const mb = await import("mediabunny");
const engine = await WebEffectRegistry.createForCanvas(new OffscreenCanvas(64, 32));
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "pixels", name: "Pixels",
  settings: { width: 64, height: 32, frame_rate: { num: 30, den: 1 }, sample_rate: 48000,
    background: { red: 0.2, green: 0.4, blue: 0.8 } },
  sources: [{ id: "red", kind: "generated", generator: { type: "fill", paint: { type: "solid",
    color: { red: 1, green: 0.1, blue: 0.1, alpha: 1 } } } }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{ id: "c", source_id: "red",
    timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 },
    visual: { scale: { mode: "constant", value: { x: 0.5, y: 1 } } } }] }] },
}));
const frameOf = async (pixels) => {
  const ticket = engine.beginExportAt(0n, "vp9", pixels);
  if (ticket.kind !== "rendered") throw new Error("the frame waits: " + JSON.stringify(ticket));
  return await ticket.readback;
};
const alpha = await frameOf("yuva420p");
const ten = await frameOf("yuv420p10le");
const at = (frame, plane, x, y, bytes) => {
  const { offset, stride } = frame.layout[plane];
  const index = offset + y * stride + x * bytes;
  return bytes === 1 ? frame.pixels[index] : frame.pixels[index] | (frame.pixels[index + 1] << 8);
};
// The corner is outside the half-width clip: transparent with alpha, the blue background without.
const alphaOk = alpha.format === "I420A" && alpha.layout.length === 4
  && at(alpha, 3, 2, 16, 1) === 0 && at(alpha, 3, 32, 16, 1) === 255 && at(alpha, 0, 2, 16, 1) === 16;
const background = Math.round(64 + 876 * (0.2126 * 0.2 + 0.7152 * 0.4 + 0.0722 * 0.8));
const tenOk = ten.format === "I420P10" && Math.abs(at(ten, 0, 2, 16, 2) - background) <= 1;
const probes = {
  vp9Alpha: await canEncodePixels("vp9", { bitDepth: 8, alpha: true }),
  vp9Ten: await canEncodePixels("vp9", { bitDepth: 10, alpha: false }),
  av1Ten: await canEncodePixels("av1", { bitDepth: 10, alpha: false }),
};
let encoded = null;
if (probes.vp9Ten) {
  const sink = new MediabunnySink({
    container: "webm", videoCodec: "vp9", videoBitDepth: 10, frameRate: { num: 30, den: 1 },
    videoRateControl: { bitrate: 1000000, bitrateMode: "variable" },
    destination: { kind: "memory", maxBytes: 1 << 22 },
  });
  for (let index = 0; index < 3; index += 1) {
    const plane = await frameOf("yuv420p10le");
    const frame = new VideoFrame(plane.pixels, { format: plane.format, codedWidth: 64, codedHeight: 32,
      layout: plane.layout, timestamp: Math.round((index * 1e6) / 30), duration: Math.round(1e6 / 30),
      colorSpace: plane.colorSpace });
    await sink.videoTrack.writeVideo({ index: BigInt(index), timeTicks: BigInt(index * 4000), frame });
  }
  await sink.videoTrack.finish();
  const input = new mb.Input({ source: new mb.BufferSource(sink.output.buffer), formats: mb.ALL_FORMATS });
  try {
    const video = await input.getPrimaryVideoTrack();
    encoded = { codec: video.codec, codecString: await video.getCodecParameterString() };
  } finally {
    input.dispose();
  }
}
const encodedOk = encoded === null || (encoded.codec === "vp9" && encoded.codecString.startsWith("vp09.02.10"));
report({ ok: alphaOk && tenOk && encodedOk, alphaOk, tenOk, probes, encoded,
  alphaFormat: alpha.format, tenFormat: ten.format });
`;
}

// How long the driver keeps the page frozen: longer than any of the SDK's stall timeouts (the
// decoder's 5 s, the encoder's 10 s), so a timer counting wall-clock time would fire.
export const FREEZE_MS = 20_000;

// The driver half of `export-survives-frozen-page`: once the page says it is mid-export (its title
// is "freeze-now"), it is frozen for FREEZE_MS, as Chrome freezes a background tab, then resumed.
export async function freezeMidExport(cdp, sessionId) {
  for (;;) {
    const { result } = await cdp.send(
      "Runtime.evaluate",
      { expression: "document.title", returnByValue: true },
      sessionId,
    );
    const title = result?.value ?? "";
    // A page that reported before it asked (a skip, an error) is not frozen.
    if (title.startsWith("%7B") || title.startsWith("%7b")) return;
    if (title === "freeze-now") break;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  await cdp.send("Page.setWebLifecycleState", { state: "frozen" }, sessionId);
  await new Promise((resolve) => setTimeout(resolve, FREEZE_MS));
  await cdp.send("Page.setWebLifecycleState", { state: "active" }, sessionId);
}

// A browser export of a decoded video, frozen mid-export for 20 s (a background tab, a laptop lid
// closed) with a decode and encoder frames in flight: the export completes, and nothing it was
// waiting on reports a stall, since the SDK's timeouts count active time.
export function exportSurvivesFrozenPage() {
  return `
const project = {
  schema_version: 1, id: "frozen", name: "Frozen page",
  settings: { width: 128, height: 96, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: [{ id: "video", kind: "video", locator: "fixture://video" }],
  timeline: { tracks: [{ id: "v", name: "V", kind: "video", clips: [{
    id: "clip", source_id: "video",
    timeline_range: { start: 0, end: 240000 }, source_range: { start: 0, end: 240000 },
  }] }] },
};
const source = await (await fetch("/tests/corpus/h264-bt709-copy.mp4")).blob();
const { WorkerRenderBackend, WorkerExportBackend } = await import("/packages/alpha2/dist/worker.js");
const { Alpha2Exporter, exportToSink } = await import("/packages/alpha2/dist/exporter.js");
const { classifyBrowserFailure } = await import("/packages/alpha2/dist/export-failure.js");
const { canEncodeVideoTrack, MediabunnySink, MediabunnyVideoDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
const { decoderConsumerId } = await import("/packages/alpha2/dist/media.js");
const rateControl = { bitrate: 1_000_000, bitrateMode: "variable" };
const encoding = await canEncodeVideoTrack("avc", { width: 128, height: 96, rate: rateControl });
if (encoding === null) skip("webcodecs-video", "this browser has no H.264 encoder");
const origin = location.origin;
const worker = new Worker(URL.createObjectURL(new Blob([\`
import init, { WebEffectRegistry } from "\${origin}/packages/alpha2/wasm/alpha2_wasm.js";
import { createWasmRenderWorkerHost, installRenderWorker } from "\${origin}/packages/alpha2/dist/worker.js";
await init();
installRenderWorker(self, createWasmRenderWorkerHost(WebEffectRegistry, \${JSON.stringify(JSON.stringify(project))}));
self.postMessage({ type: "test-ready" });
\`], { type: "text/javascript" })), { type: "module" });
await new Promise((resolve, reject) => {
  worker.addEventListener("message", function ready(event) {
    if (event.data?.type === "test-ready") { worker.removeEventListener("message", ready); resolve(); }
  });
  worker.addEventListener("error", (event) => reject(new Error(event.message)), { once: true });
});
const backend = new WorkerRenderBackend(worker);
await backend.initialize(new OffscreenCanvas(64, 48));
const decoder = await MediabunnyVideoDecoder.open("video", source);
let frozen = false;
document.addEventListener("freeze", () => (frozen = true));
const resumed = () =>
  new Promise((resolve) => {
    document.addEventListener("resume", resolve, { once: true });
    // A driver that cannot freeze the page: go on, and say so.
    setTimeout(resolve, 30_000);
  });
let refills = 0;
const refill = async (waiting) => {
  for (const { clipId, readerId, sourceId, mediaTimeTicks } of waiting.missingFrames) {
    refills += 1;
    const decoding = decoder.refill({
      sourceId, mediaTimeTicks, reason: "prefetch", generation: 0, consumerId: decoderConsumerId(clipId, readerId),
    });
    // The tenth decode is in flight, and earlier frames are in the encoder, when the page freezes.
    if (refills === 10) {
      document.title = "freeze-now";
      await resumed();
    }
    await backend.uploadVideoFrame(await decoding);
  }
};
const exporter = new Alpha2Exporter(new WorkerExportBackend(backend, "h264"), { num: 30n, den: 1n }, 0n, 240000n);
const sink = new MediabunnySink({
  container: "mp4",
  videoCodec: "avc",
  videoRateControl: rateControl,
  ...(encoding.codecString === undefined ? {} : { videoCodecString: encoding.codecString }),
  frameRate: project.settings.frame_rate,
  destination: { kind: "memory", maxBytes: 1 << 26 },
});
const started = performance.now();
let result = null;
let failure = null;
try {
  result = await exportToSink(exporter, sink, refill);
} catch (error) {
  failure = classifyBrowserFailure(error, { phase: "render" });
} finally {
  decoder.close();
  backend.dispose();
}
if (!frozen) skip("page-freeze", "the driver did not freeze the page");
// A decode read as stalled would have been decoded again in software; none was.
const softwareFallback = decoder.softwareFallback ?? null;
report({
  ok: failure === null && result?.frames === 60 && softwareFallback === null,
  frames: result?.frames ?? null,
  softwareFallback,
  wallMs: Math.round(performance.now() - started),
  failure: failure && { code: failure.code, stage: failure.stage ?? null, message: failure.message.slice(0, 300) },
});
`;
}
