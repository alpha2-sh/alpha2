// Browser audio cases: the audio store, seeks and continuity, decoded windows, speaker layouts,
// loudness, and the fixed mix native code pins.

// A window decoded by a new decoder must start a tenth of a second early (Opus 400 ms), as a native
// seek does, or AAC and Opus pre-roll leaves its first samples off.
export function audioWindowPreroll() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { MediabunnyAudioDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
if (typeof AudioDecoder === "undefined") {
  report({ skipped: "this browser has no WebCodecs AudioDecoder", capability: "webcodecs-audio" });
} else {
  const ticks = (seconds) => BigInt(Math.round(seconds * 120000));
  const results = {};
  // AAC at 44.1 kHz, which the window also resamples, and Opus at 48 kHz, in containers with
  // sample-exact timestamps. Each later window starts inside a codec frame. AAC's state is one
  // frame of overlap, so its samples come out as decoding straight through gives them, to rounding;
  // Opus converges over its pre-roll (400 ms) without reaching the same bits. Then AAC in Matroska,
  // its copy in MP4 and Opus in WebM (FFmpeg's encoder and libopus), whose timestamps are rounded
  // to the millisecond: a window's first sample goes on the frame grid the first timestamps show,
  // where a window from the start holds it, not up to 48 samples off.
  for (const [name, whole, later, tolerance] of [
    ["audio-only-aac-priming.m4a", [0.05, 0.38], 0.2003, 1e-4],
    ["audio-opus.mp4", [0.05, 0.55], 0.3011, 1e-3],
    ["audio-seek-aac.mkv", [0.05, 1.5], 0.7771, 1e-4],
    ["audio-seek-aac-remux.mp4", [0.05, 1.5], 0.7771, 1e-4],
    ["audio-seek-opus.webm", [0.05, 1.5], 0.7771, 1e-3],
    ["audio-seek-libopus.webm", [0.05, 1.5], 0.7771, 1e-3],
  ]) {
    const bytes = new Uint8Array(await (await fetch("/tests/corpus/" + name)).arrayBuffer());
    const decoder = await MediabunnyAudioDecoder.open("a", bytes, wasm);
    const request = (start, end) => decoder.refill({ sourceId: "a", startMediaTimeTicks: ticks(start), endMediaTimeTicks: ticks(end), generation: 1 }, 48000);
    const straight = await request(whole[0], whole[1]);
    const windowed = await request(later, whole[1]);
    decoder.close();
    const from = Math.ceil(later * 48000);
    const to = Math.floor(whole[1] * 48000);
    let error = 0;
    let peak = 0;
    for (let sample = from; sample < to; sample += 1) {
      const expected = straight.channels[0][sample - Number(straight.startSample)];
      const got = windowed.channels[0][sample - Number(windowed.startSample)];
      error = Math.max(error, Math.abs(got - expected));
      peak = Math.max(peak, Math.abs(expected));
    }
    results[name] = { error, peak, tolerance };
  }
  const ok = Object.values(results).every(({ error, peak, tolerance }) => peak > 0.1 && error <= tolerance);
  report({ ok, ...results });
}
`;
}

// A native host subtracts a Matroska track's CodecDelay so samples sit at the same source sample as
// in MP4; mediabunny does not (AAC was 1008 samples late, Opus 144).
export function audioMatroskaCodecDelay() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { MediabunnyAudioDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
if (typeof AudioDecoder === "undefined") {
  report({ skipped: "this browser has no WebCodecs AudioDecoder", capability: "webcodecs-audio" });
} else {
  const ticks = (seconds) => BigInt(Math.round(seconds * 120000));
  const decode = async (name) => {
    const bytes = new Uint8Array(await (await fetch("/tests/corpus/" + name)).arrayBuffer());
    const decoder = await MediabunnyAudioDecoder.open("a", bytes, wasm);
    const block = await decoder.refill({ sourceId: "a", startMediaTimeTicks: ticks(0), endMediaTimeTicks: ticks(1.5), generation: 1 }, 48000);
    decoder.close();
    return block;
  };
  const results = {};
  for (const [matroska, mp4, tolerance] of [
    ["audio-seek-aac.mkv", "audio-seek-aac.mp4", 1e-4],
    ["audio-seek-opus.webm", "audio-seek-opus.mp4", 1e-3],
  ]) {
    const a = await decode(matroska);
    const b = await decode(mp4);
    // The lag that best lines the Matroska samples up with the MP4's, and the error there.
    const compare = (lag) => {
      let error = 0;
      let peak = 0;
      for (let sample = 2400; sample < 70000; sample += 1) {
        const expected = b.channels[0][sample - Number(b.startSample)];
        const got = a.channels[0][sample + lag - Number(a.startSample)];
        error = Math.max(error, Math.abs(got - expected));
        peak = Math.max(peak, Math.abs(expected));
      }
      return { error, peak };
    };
    let lag = 0;
    let best = compare(0);
    for (let candidate = -1200; candidate <= 1200; candidate += 1) {
      if (candidate === 0) continue;
      const result = compare(candidate);
      if (result.error < best.error) {
        best = result;
        lag = candidate;
      }
    }
    results[matroska] = { lag, ...best, tolerance };
  }
  const ok = Object.values(results).every(({ lag, error, peak, tolerance }) => lag === 0 && peak > 0.1 && error <= tolerance);
  report({ ok, ...results });
}
`;
}

// Each plane must hold its speaker's samples whatever order the browser's decoder delivers them in
// (Safari's AAC decoder uses the bitstream's C L R); the engine folds an upload by its speakers.
export function audioSpeakerLayout() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { MediabunnyAudioDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
const { decoderPlaneOrder } = await import("/packages/alpha2/dist/decoder-order.js");
const contract = await (await fetch("/tests/cases/audio-provider-contract.json")).json();
const power = (samples, frequency) => {
  const coefficient = 2 * Math.cos((2 * Math.PI * frequency) / 48000);
  let previous = 0, before = 0;
  for (const sample of samples) {
    const current = sample + coefficient * previous - before;
    before = previous;
    previous = current;
  }
  return previous * previous + before * before - coefficient * previous * before;
};
const toneOf = (samples, tones) => tones.reduce((best, tone) => (power(samples, tone) > power(samples, best) ? tone : best));
// The browser's decoder on a source's first 16 packets, without the engine's decoder: the channel
// count and rate of its first output, and per plane the RMS of 1024 frames and the tone after the
// codec settles (from frame 4800; AAC's first frames are its encoder's priming).
const trialDecode = async (bytes, tones) => {
  const mediabunny = await import("mediabunny");
  const input = new mediabunny.Input({ source: new mediabunny.BufferSource(bytes), formats: mediabunny.ALL_FORMATS });
  const track = await input.getPrimaryAudioTrack();
  const planes = [];
  let channels = 0, sampleRate = 0, failure = null;
  const decoder = new AudioDecoder({
    output: (data) => {
      if (channels === 0) [channels, sampleRate] = [data.numberOfChannels, data.sampleRate];
      for (let plane = 0; plane < data.numberOfChannels; plane++) {
        const samples = new Float32Array(data.numberOfFrames);
        data.copyTo(samples, { planeIndex: plane, format: "f32-planar" });
        (planes[plane] ??= []).push(samples);
      }
      data.close();
    },
    error: (error) => (failure = String(error?.message ?? error)),
  });
  try {
    decoder.configure(await track.getDecoderConfig());
    let count = 0;
    for await (const packet of new mediabunny.EncodedPacketSink(track).packets()) {
      decoder.decode(packet.toEncodedAudioChunk());
      if (++count === 16) break;
    }
    await decoder.flush();
  } catch (error) {
    failure ??= String(error?.message ?? error);
  } finally {
    if (decoder.state !== "closed") decoder.close();
    input.dispose();
  }
  const joined = planes.map((parts) => {
    const out = new Float32Array(parts.reduce((sum, part) => sum + part.length, 0));
    let at = 0;
    for (const part of parts) { out.set(part, at); at += part.length; }
    return out;
  });
  const rms = joined.map((plane) => {
    const head = plane.length >= 5824 ? plane.subarray(4800, 5824) : plane.subarray(0, 1024);
    return +Math.sqrt(head.reduce((sum, value) => sum + value * value, 0) / Math.max(1, head.length)).toFixed(5);
  });
  return {
    declared: track.numberOfChannels, channels, sampleRate, rms,
    tones: joined.map((plane) => (plane.length > 4800 && tones.length > 0 ? toneOf(plane.subarray(4800), tones) : null)),
    ...(failure === null ? {} : { failure }),
  };
};
const decoded = {};
if (typeof AudioDecoder === "undefined") {
  decoded.skipped = "this browser has no WebCodecs AudioDecoder";
} else {
  const mediabunny = await import("mediabunny");
  for (const layout of contract.codecLayouts) {
    const bytes = new Uint8Array(await (await fetch("/tests/" + layout.source)).arrayBuffer());
    // A browser need not decode AC-3, E-AC-3 or DTS (Chrome and Firefox on Linux do not); every
    // other codec here must decode.
    const input = new mediabunny.Input({ source: new mediabunny.BufferSource(bytes), formats: mediabunny.ALL_FORMATS });
    const track = await input.getPrimaryAudioTrack();
    const decodable = await track.canDecode();
    input.dispose();
    if (!decodable) {
      decoded[layout.source] = ["ac3", "eac3", "dts"].includes(track.codec)
        ? "skipped: this browser does not decode " + track.codec
        : { ok: false, undecodable: track.codec };
      continue;
    }
    const trial = await trialDecode(bytes, layout.tones);
    let block;
    try {
      const decoder = await MediabunnyAudioDecoder.open("codec", bytes, wasm);
      block = await decoder.refill({ sourceId: "codec", startMediaTimeTicks: 0n, endMediaTimeTicks: 60000n, generation: 1 }, 48000);
      decoder.close();
    } catch (error) {
      decoded[layout.source] = { ok: false, error: String(error?.message ?? error), code: error?.code, trial };
      continue;
    }
    // A decoder that adds a silent LFE to a source without one (Edge's E-AC-3 5.0) has that
    // layout adopted: the LFE plane must be silent and the others the source's.
    const lfe = block.speakers.length === layout.speakers.length + 1 && !layout.speakers.includes("LFE")
      ? block.speakers.indexOf("LFE") : -1;
    const planes = block.channels.filter((_, index) => index !== lfe);
    const tones = planes.map((plane) => toneOf(plane.subarray(4800), layout.tones));
    const silentLfe = lfe === -1 || block.channels[lfe].every((sample) => Math.abs(sample) < 1e-4);
    decoded[layout.source] = {
      speakers: block.speakers, tones,
      ...(lfe === -1 ? {} : { addedLfe: lfe, silentLfe }),
      ok: JSON.stringify(block.speakers.filter((_, index) => index !== lfe)) === JSON.stringify(layout.speakers) &&
        JSON.stringify(tones) === JSON.stringify(layout.tones) && silentLfe,
      // What the browser's own decoder gave, when its channel count is not the declared one.
      ...(trial.channels === track.numberOfChannels ? {} : { trial }),
    };
  }
  // Every built-in fixture's kind of stream is measured where the browser decodes its codec (AC-3
  // and E-AC-3 need not decode); the order each measured is reported, FFmpeg's being the identity.
  const orders = {};
  for (const codec of ["aac", "opus", "vorbis", "ac3", "eac3"]) {
    const { ORDER_FIXTURES } = await import("/packages/alpha2/dist/generated/" + codec + "-order-fixtures.js");
    for (const [key, fixture] of Object.entries(ORDER_FIXTURES)) {
      const order = await decoderPlaneOrder(key);
      if (order !== null) {
        orders[key] = order;
        continue;
      }
      const bytes = Uint8Array.from(atob(fixture.file), (character) => character.charCodeAt(0));
      const input = new mediabunny.Input({ source: new mediabunny.BufferSource(bytes), formats: mediabunny.ALL_FORMATS });
      const decodable = await (await input.getPrimaryAudioTrack()).canDecode();
      input.dispose();
      if (!decodable) {
        orders[key] = ["ac3", "eac3"].includes(codec) ? "skipped: this browser does not decode " + codec : null;
        continue;
      }
      // A decoder that delivers another channel count (Edge's AC-3 and E-AC-3 decoder gives 5.1
      // for every layout) has no order for the declared layout: the source opens with an added
      // silent LFE, or is refused when it opens.
      const trial = await trialDecode(bytes, []);
      if (trial.channels === trial.declared || trial.channels === 0) {
        orders[key] = null;
        continue;
      }
      try {
        const decoder = await MediabunnyAudioDecoder.open("order", bytes, wasm);
        orders[key] = decoder.speakers?.includes("LFE") && trial.channels === trial.declared + 1
          ? "added a silent LFE" : null;
        decoder.close();
      } catch (error) {
        orders[key] = error?.code === "DECODE_UNAVAILABLE"
          ? "refused at open: decodes to " + trial.channels + " channels" : null;
      }
    }
  }
  const measured = Object.values(orders).filter((order) => Array.isArray(order));
  decoded.measuredPlaneOrders = {
    orders,
    identity: measured.every((order) => order.every((plane, index) => plane === index)),
    ok: Object.values(orders).every((order) => order !== null),
  };
}
const engine = await gpuEngine(wasm.WebEffectRegistry);
const mix = (layout, planes) => {
  const clip = { id: "c", source_id: "s", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 } };
  engine.loadProject(JSON.stringify({
    schema_version: 1, id: "layout", name: "Layout",
    settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
    sources: [{ id: "s", locator: "fixture://s", kind: "audio" }],
    timeline: { tracks: [{ id: "t", name: "t", kind: "audio", clips: [clip] }] }, audio: { limiter: null },
  }));
  const frames = 48000;
  const samples = new Float32Array(planes.length * frames);
  planes.forEach((level, plane) => samples.fill(level, plane * frames, (plane + 1) * frames));
  engine.uploadAudioBlock("s", 0n, 48000, layout, samples, true);
  return Array.from(engine.renderAudio(4800n, 4, true));
};
const half = Math.fround(Math.SQRT1_2);
const lcr = mix(["FC", "FL", "FR"], [0.3, 0.1, 0.2]);
const folded = mix(["FL", "FR"], [0.1 + half * 0.3, 0.2 + half * 0.3]);
// A count is the count's default layout: three planes are 2.1, the third the LFE, left out.
const counted = mix(3, [0.3, 0.1, 0.2]);
const fronts = mix(2, [0.3, 0.1]);
let refused = null;
try { engine.uploadAudioBlock("s", 0n, 48000, ["FL", "XX"], new Float32Array(2), false); } catch (error) { refused = error.code; }
const close = (a, b) => a.length === b.length && a.every((value, index) => Math.abs(value - b[index]) < 1e-6);
const mixed = close(lcr, folded) && close(counted, fronts) && Math.abs(lcr[0] - counted[0]) > 0.01 && refused === "INVALID_INPUT";
const ok = mixed && Object.values(decoded).every((entry) => typeof entry === "string" || entry.ok);
// Everything this browser could check passed: what it could not decode names the skip. DTS is no
// capability: no browser's WebCodecs decodes it, and a browser need not.
const notes = [
  ...Object.values(decoded),
  ...Object.values(decoded.measuredPlaneOrders?.orders ?? {}),
].filter((entry) => typeof entry === "string" && /does not decode (ac3|eac3)$/.test(entry));
if (ok && decoded.skipped !== undefined)
  report({ skipped: decoded.skipped, capability: "webcodecs-audio", lcr, folded, counted, fronts, refused });
else if (ok && notes.length > 0)
  report({ skipped: "this browser does not decode AC-3 or E-AC-3", capability: "ac3-decode", decoded, lcr, folded, counted, fronts, refused });
else report({ ok, decoded, lcr, folded, counted, fronts, refused });
`;
}

// A browser whose decoder rejects an AAC layout its canDecode() accepts (Firefox: channel
// configuration 4 in MP4 and two program config element layouts) has the source refused when it
// opens, with DECODE_UNAVAILABLE, never at a refill with the decoder's own error; every source
// that opens decodes.
export function aacLayoutRefusal() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { MediabunnyAudioDecoder } = await import("/packages/alpha2/dist/mediabunny.js");
const { ORDER_FIXTURES } = await import("/packages/alpha2/dist/generated/aac-order-fixtures.js");
if (typeof AudioDecoder === "undefined") skip("webcodecs-audio", "this browser has no WebCodecs AudioDecoder");
const results = {};
for (const key of ["aac:4", "aac:pce:CF LL", "aac:pce:SF CF SB", "aac:adts:4", "aac:7"]) {
  const bytes = Uint8Array.from(atob(ORDER_FIXTURES[key].file), (character) => character.charCodeAt(0));
  let decoder;
  try {
    decoder = await MediabunnyAudioDecoder.open("s", bytes, wasm);
  } catch (error) {
    results[key] = error?.code === "DECODE_UNAVAILABLE"
      ? { refused: true, channels: error.facts?.channels, ok: true }
      : { ok: false, open: String(error?.message ?? error) };
    continue;
  }
  try {
    const block = await decoder.refill({ sourceId: "s", startMediaTimeTicks: 0n, endMediaTimeTicks: 40000n, generation: 1 }, 48000);
    results[key] = { planes: block.channels.length, speakers: block.speakers, ok: block.channels.length > 0 };
  } catch (error) {
    results[key] = { ok: false, refill: String(error?.message ?? error), name: error?.name, code: error?.code };
  } finally {
    decoder.close();
  }
}
const refused = Object.keys(results).filter((key) => results[key].refused);
report({ ok: Object.values(results).every((result) => result.ok), refused, results });
`;
}

// Loudness normalization under a master limiter below the target's peaks: renderAudio's output
// must match the plan rather than the target the limiter would cut back.
export function loudnessUnderLimiter() {
  return `
const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await wasm.default();
const { planLoudnessNormalization } = await import("/packages/alpha2/dist/loudness.js");
const engine = await gpuEngine(wasm.WebEffectRegistry);
const rate = 48000, frames = rate * 4;
const clip = { id: "c", source_id: "s", timeline_range: { start: 0, end: 480000 }, source_range: { start: 0, end: 480000 } };
engine.loadProject(JSON.stringify({
  schema_version: 1, id: "loudness", name: "Loudness",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: rate },
  sources: [{ id: "s", locator: "fixture://s", kind: "audio" }],
  timeline: { tracks: [{ id: "t", name: "t", kind: "audio", clips: [clip] }] },
  audio: { limiter: { threshold: 0.1, release_ms: 50 } },
}));
const tone = Float32Array.from({ length: frames }, (_, index) => Math.fround(10 ** (-30 / 20) * Math.sin(2 * Math.PI * 1000 * index / rate)));
engine.uploadAudioBlock("s", 0n, rate, 1, tone, true);
const play = (meter) => {
  for (let start = 0; start < frames; start += 4096) {
    const count = Math.min(4096, frames - start);
    const block = engine.renderAudio(BigInt(start), count, true);
    if (meter) meter.push(block);
  }
};
engine.setLoudnessMetering(true);
play(null);
const measured = JSON.parse(engine.loudnessReport());
engine.setLoudnessMetering(false);
const plan = planLoudnessNormalization(wasm, measured, -16, -1);
engine.setNormalizationGainDb(plan.gainDb);
const meter = new wasm.LoudnessMeter(rate, 2);
// A seek back to zero resets the mix, as a fresh export pass does.
play(meter);
const output = JSON.parse(meter.report());
report({
  ok: Math.abs(measured.limiterThreshold - Math.fround(0.1)) < 1e-12 && plan.limiterLimited && plan.peakLimited &&
    Math.abs(output.integratedLufs - plan.integratedLufs) < 0.05 && output.samplePeakDbfs < -20,
  limiterThreshold: measured.limiterThreshold, plan, output: output.integratedLufs, peak: output.samplePeakDbfs,
});
`;
}

export function fixedAudioMix(cpu = false) {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const fixture = await (await fetch("/tests/golden/audio_mix.json")).json();
const frames = fixture.sourceFrames;
const generate = {
  noise(channels) {
    let state = 0x12345678;
    return Array.from({ length: channels }, () => Float32Array.from({ length: frames }, () => {
      state = (Math.imul(state, 1664525) + 1013904223) >>> 0;
      return Math.fround(Math.fround((state >>> 16) / 65535) - 0.5);
    }));
  },
  triangle(channels) {
    return Array.from({ length: channels }, () => Float32Array.from({ length: frames }, (_, index) => {
      const phase = index % 200;
      return Math.fround(Math.fround((phase < 100 ? phase : 200 - phase) / 100) - 0.5);
    }));
  },
  saw(channels) {
    return Array.from({ length: channels }, (_, plane) => Float32Array.from({ length: frames }, (_, index) =>
      Math.fround(Math.fround(((index * (2 * plane + 3)) % 400) / 400) - 0.5)));
  },
};
// The CPU renderer mixes as the GPU one does: the mix needs no GPU.
const engine = ${cpu ? "WebEffectRegistry.createCpu()" : "await gpuEngine(WebEffectRegistry)"};
engine.loadProject(JSON.stringify(fixture.project));
for (const source of fixture.sources) {
  const planes = generate[source.generator](source.channels);
  const samples = new Float32Array(planes.length * frames);
  planes.forEach((plane, index) => samples.set(plane, index * frames));
  engine.uploadAudioBlock(source.id, 0n, fixture.sampleRate, source.channels, samples, true);
}
engine.setNormalizationGainDb(fixture.normalizationGainDb);
// FNV-1a 64 over the bytes of every block, as the native hash runs, in two 32-bit halves.
let hi = 0xcbf29ce4, lo = 0x84222325, blocks = 0;
const missing = [];
for (const [from, to] of fixture.playback) {
  for (let start = from; start < to; start += fixture.block) {
    const count = Math.min(fixture.block, to - start);
    const block = engine.renderAudio(BigInt(start), count, true);
    const absent = JSON.parse(engine.audioMissingSources());
    if (absent.length) missing.push(start + ": " + absent.join(","));
    for (const byte of new Uint8Array(block.buffer, block.byteOffset, block.byteLength)) {
      lo ^= byte;
      const low = (lo >>> 0) * 0x1b3;
      hi = (Math.imul(hi, 0x1b3) + Math.floor(low / 4294967296) + (lo << 8)) >>> 0;
      lo = low >>> 0;
    }
    blocks += 1;
  }
}
const hash = hi.toString(16).padStart(8, "0") + (lo >>> 0).toString(16).padStart(8, "0");
report({ ok: hash === fixture.expected && missing.length === 0, hash, expected: fixture.expected, blocks, missing: missing.slice(0, 5) });
`;
}

// After a seek, a compressor on the clip, track or master renders what playing straight through does.
export function audioSeekState() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await gpuEngine(WebEffectRegistry);
const results = [];
for (const place of ["clip", "track", "master"]) {
  const effect = { type: "compressor", threshold_db: -18, ratio: 4, attack_ms: 50, release_ms: 100, lookahead_ms: 10 };
  const clip = { id: "c", source_id: "tone", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 }, audio: { effects: place === "clip" ? [effect] : [] } };
  const track = { id: "t", name: "t", kind: "audio", clips: [clip], audio: { effects: place === "track" ? [effect] : [] } };
  engine.loadProject(JSON.stringify({
    schema_version: 1, id: "seek", name: "Seek",
    settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
    sources: [{ id: "tone", locator: "fixture://tone", kind: "audio" }],
    timeline: { tracks: [track] }, audio: { limiter: null, effects: place === "master" ? [effect] : [] },
  }));
  engine.uploadAudioBlock("tone", 0n, 48000, 2, new Float32Array(96000).fill(0.5));
  let linear;
  for (let sample = 0; sample <= 8192; sample += 512) linear = engine.renderAudio(BigInt(sample), 512, true);
  engine.resetAudio();
  const needs = JSON.parse(engine.audioNeeds(8192n, 512));
  const seek = engine.renderAudio(8192n, 512, true);
  const error = Math.max(...seek.map((value, index) => Math.abs(value - linear[index])));
  results.push({ place, error, linearFirst: linear[0], seekFirst: seek[0], historyStart: needs[0]?.startTicks });
}
report({ ok: results.every(r => r.error < 1e-6 && r.linearFirst < 0.25 && r.historyStart === "0"), results });
`;
}

// "continue-small-gaps" skips settling history after a step of a block or less; an export and
// "exact" always replay. The choice survives a load and an export; an unknown name is refused.
export function audioContinuity() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await gpuEngine(WebEffectRegistry);
const load = () => {
  const effect = { type: "compressor", threshold_db: -18, ratio: 4, attack_ms: 50, release_ms: 100, lookahead_ms: 10 };
  const clip = { id: "c", source_id: "tone", timeline_range: { start: 0, end: 120000 }, source_range: { start: 0, end: 120000 }, audio: { effects: [] } };
  const track = { id: "t", name: "t", kind: "audio", clips: [clip], audio: { effects: [effect] } };
  engine.loadProject(JSON.stringify({
    schema_version: 1, id: "continuity", name: "Continuity",
    settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
    sources: [{ id: "tone", locator: "fixture://tone", kind: "audio" }],
    timeline: { tracks: [track] }, audio: { limiter: null, effects: [] },
  }));
  engine.uploadAudioBlock("tone", 0n, 48000, 2, new Float32Array(96000).fill(0.5));
};
// Where the source reads for the block at 'next + step' begin, in ticks, after a block ending at
// 'next'.
const readsFrom = (next, step) => {
  engine.renderAudio(BigInt(next - 512), 512, true);
  return JSON.parse(engine.audioNeeds(BigInt(next + step), 512))[0]?.startTicks;
};
load();
const max = engine.audioMaxBlock;
const exact = [readsFrom(8192, 1), readsFrom(8192, -1)];
engine.setAudioContinuity("continue-small-gaps");
const preview = [readsFrom(8192, 1), readsFrom(8192, -1), readsFrom(8192, max), readsFrom(16384, -max)];
const jump = [readsFrom(16384, max + 1), readsFrom(16384, -max - 1)];
load();
const reloaded = readsFrom(8192, 1);
engine.beginExport(0n, 120000n, 30, 1, "h264");
const exporting = readsFrom(8192, 1);
engine.endExport();
const afterExport = readsFrom(8192, 1);
let refused = null;
try { engine.setAudioContinuity("loose"); } catch (error) { refused = { code: error.code, fatal: error.fatal }; }
const kept = readsFrom(8192, 1);
const history = (ticks) => ticks === "0";
const noHistory = (ticks) => ticks !== undefined && BigInt(ticks) > 0n;
report({
  ok: exact.every(history) && preview.every(noHistory) && jump.every(history) && noHistory(reloaded) &&
    history(exporting) && noHistory(afterExport) && refused?.code === "INVALID_INPUT" && refused.fatal === false &&
    noHistory(kept),
  exact, preview, jump, reloaded, exporting, afterExport, refused, kept,
});
`;
}

// The audio store holds what one render reads; only an upload no window can hold is refused, as a
// resource limit.
export function audioStoreCapacity() {
  return `
const { default: init, WebEffectRegistry } = await import("/packages/alpha2/wasm/alpha2_wasm.js");
await init();
const engine = await gpuEngine(WebEffectRegistry);
const SECOND = 120000;
const load = (sources, clips, audio) => engine.loadProject(JSON.stringify({
  schema_version: 1, id: "store", name: "Store",
  settings: { width: 16, height: 16, frame_rate: { num: 30, den: 1 }, sample_rate: 48000 },
  sources: sources.map((id) => ({ id, locator: "fixture://" + id, kind: "audio" })),
  timeline: { tracks: [{ id: "t", name: "t", kind: "audio", clips: clips.map((clip, index) => ({ id: "c" + index, ...clip })) }] },
  audio,
}));
const ramp = (from, to) => Float32Array.from({ length: to - from }, (_, index) => ((from + index) % 997) / 997 - 0.5);
const missing = () => JSON.parse(engine.audioMissingSources());

// Twelve 0.1 s cuts from one recording, three seconds apart, inside the default limiter's history:
// a seek past them reads all twelve.
load(["rec"], Array.from({ length: 12 }, (_, index) => ({
  source_id: "rec",
  timeline_range: { start: index * SECOND / 10, end: (index + 1) * SECOND / 10 },
  source_range: { start: index * 3 * SECOND, end: index * 3 * SECOND + SECOND / 10 },
})));
const needs = JSON.parse(engine.audioNeeds(57600n, 4096));
for (const need of needs) {
  const from = Math.max(0, Math.floor((Number(need.startTicks) * 2) / 5) - 2);
  const to = Math.ceil((Number(need.endTicks) * 2) / 5) + 2;
  engine.uploadAudioBlock("rec", BigInt(from), 48000, 1, ramp(from, to));
}
engine.renderAudio(57600n, 4096, true);
const replayMissing = missing();

// The later half decoded first, then the half before it: a backward seek.
load(["seam"], [{ source_id: "seam", timeline_range: { start: 0, end: SECOND }, source_range: { start: 0, end: SECOND } }], { limiter: null });
engine.uploadAudioBlock("seam", 24000n, 48000, 1, ramp(24000, 48002));
engine.uploadAudioBlock("seam", 0n, 48000, 1, ramp(0, 24000));
engine.renderAudio(22000n, 4096, true);
const seamMissing = missing();
const seamWindows = JSON.parse(engine.audioRetainedWindows("seam"));

const sources = Array.from({ length: 200 }, (_, index) => "s" + index);
load(sources, []);
const twoSeconds = new Float32Array(2 * 96000).fill(0.1);
let refusedSources = 0;
for (const source of sources) {
  try { engine.uploadAudioBlock(source, 0n, 48000, 2, twoSeconds); } catch { refusedSources++; }
}
const heldSources = sources.filter((source) => JSON.parse(engine.audioRetainedWindows(source)).length === 1).length;

const refusal = (upload) => { try { upload(); return null; } catch (error) { return { code: error.code, fatal: error.fatal }; } };
const tooLong = refusal(() => engine.uploadAudioBlock("s0", 0n, 48000, 1, new Float32Array(600 * 48000 + 1)));
const wrongRate = refusal(() => engine.uploadAudioBlock("s0", 0n, 44100, 1, new Float32Array(4)));
const keptAfterRefusal = JSON.parse(engine.audioRetainedWindows("s0")).length === 1;
// A NaN or infinite sample does not refuse the upload: as on a native host, the mixer reads it as
// silence.
const nonFinite = refusal(() => engine.uploadAudioBlock("s1", 0n, 48000, 1, Float32Array.of(0.5, NaN, Infinity, -Infinity, 0.25)));
// The binding mixes at most its own block per call, exposed as audioMaxBlock; a larger request is
// refused, naming the limit.
const maxBlock = engine.audioMaxBlock;
const atBlock = engine.renderAudio(0n, maxBlock, true).length === maxBlock * 2;
let overBlock = null;
try { engine.renderAudio(0n, maxBlock + 1, true); } catch (error) { overBlock = { code: error.code, fatal: error.fatal, namesLimit: String(error.message).includes(String(maxBlock)) }; }

report({
  ok: needs.length > 8 && replayMissing.length === 0 && seamMissing.length === 0 && seamWindows.length === 1
    && refusedSources === 0 && heldSources === 200
    && tooLong?.code === "RESOURCE_LIMIT" && tooLong.fatal === false
    && wrongRate?.code === "INVALID_INPUT" && keptAfterRefusal
    && nonFinite === null && maxBlock === 4096 && atBlock
    && overBlock?.code === "INVALID_INPUT" && overBlock.fatal === false && overBlock.namesLimit,
  needs: needs.length, replayMissing, seamMissing, seamWindows, refusedSources, heldSources, tooLong, wrongRate, keptAfterRefusal,
  nonFinite, maxBlock, atBlock, overBlock,
});
`;
}
