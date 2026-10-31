// The worklet plays every render quantum size a context chooses (renderSizeHint).
export function audioWorkletQuanta() {
  return `
const { Alpha2AudioWorkletHost, loadAlpha2AudioWorkletWasm, configureMasterAudio } =
  await import("/packages/alpha2/dist/audio-worklet.js");
const wasm = await loadAlpha2AudioWorkletWasm();
const results = {};
let skipped;
for (const hint of [undefined, 256, 1024, 8192]) {
  const options = { numberOfChannels: 2, length: 24_000, sampleRate: 48_000 };
  if (hint !== undefined) options.renderSizeHint = hint;
  const context = new OfflineAudioContext(options);
  if (hint !== undefined && context.renderQuantumSize !== hint) {
    skipped = "this browser does not render the quantum it is asked for (renderSizeHint " + hint + ", renderQuantumSize " + context.renderQuantumSize + ")";
    break;
  }
  const node = await Alpha2AudioWorkletHost.create(context, wasm, { outputChannelCount: [2] });
  node.channelCount = 2;
  node.channelCountMode = "explicit";
  const errors = [];
  node.port.addEventListener("message", (event) => {
    if (event.data?.type === "alpha2-error") errors.push(event.data.message);
  });
  await configureMasterAudio(node, { gain: { mode: "constant", value: 0.5 } });
  const source = new ConstantSourceNode(context, { offset: 0.5 });
  source.connect(node).connect(context.destination);
  source.start();
  const rendered = await context.startRendering();
  const wrong = [0, 1].map((channel) => rendered.getChannelData(channel).filter((sample) => Math.abs(sample - 0.25) > 1e-6).length);
  results[hint ?? "default"] = { quantum: context.renderQuantumSize ?? 128, wrong, errors };
}
if (skipped !== undefined) report({ skipped, capability: "audio-render-quantum", results });
else report({
  ok: Object.values(results).every((result) => result.wrong.every((count) => count === 0) && result.errors.length === 0),
  results,
});
`;
}

// Low master EQ bands must filter at up to 768 kHz as at 48 kHz, where f32 precision is marginal.
export function audioWorkletHighRate() {
  return `
const { Alpha2AudioWorkletHost, loadAlpha2AudioWorkletWasm, configureMasterAudio } =
  await import("/packages/alpha2/dist/audio-worklet.js");
const wasm = await loadAlpha2AudioWorkletWasm();
const results = {};
const render = async (sampleRate, kind, makeSource) => {
  let context;
  try {
    context = new OfflineAudioContext({ numberOfChannels: 1, length: Math.round(sampleRate * 1.5), sampleRate });
  } catch (error) {
    return { refused: String(error) };
  }
  const node = await Alpha2AudioWorkletHost.create(context, wasm, { outputChannelCount: [1] });
  node.channelCount = 1;
  node.channelCountMode = "explicit";
  await configureMasterAudio(node, {
    effects: [{ type: "eq", bands: [{ kind, frequency_hz: 10, q: 0.707, gain_db: 0 }] }],
  });
  makeSource(context).connect(node).connect(context.destination);
  const tail = (await context.startRendering()).getChannelData(0).subarray(sampleRate);
  let peak = 0;
  let energy = 0;
  let sum = 0;
  for (const sample of tail) {
    peak = Math.max(peak, Math.abs(sample));
    energy += sample * sample;
    sum += sample;
  }
  return { peak: +peak.toPrecision(5), rms: +Math.sqrt(energy / tail.length).toPrecision(5), mean: +(sum / tail.length).toPrecision(5) };
};
let ok = true;
for (const rate of [48_000, 384_000, 768_000]) {
  const highPass = await render(rate, "high_pass", (context) => {
    const tone = new OscillatorNode(context, { frequency: 1_000 });
    tone.start();
    return tone.connect(new GainNode(context, { gain: 0.25 }));
  });
  const lowPass = await render(rate, "low_pass", (context) => {
    const level = new ConstantSourceNode(context, { offset: 0.25 });
    level.start();
    return level;
  });
  results[rate] = { highPass, lowPass };
  if (highPass.refused || lowPass.refused) {
    // Only Chrome renders above 384 kHz.
    if (rate === 48_000) ok = false;
    continue;
  }
  ok &&=
    highPass.peak <= 0.2502 &&
    Math.abs(highPass.rms - 0.25 / Math.SQRT2) < 0.002 &&
    Math.abs(lowPass.mean - 0.25) < 0.25 * 0.012;
}
report({ ok, results });
`;
}

// The playback clock against getOutputTimestamp().contextTime: spread and drift within one frame
// over `?seconds` (default 20).
//
// Lip-sync offset is the clock minus (contextTime * contextRate - firstFrame) * rate / contextRate,
// and must stay within base latency, one render quantum and 2 ms (capability
// audio-output-timestamp).
export function audioClockTracking(contextRate) {
  return `
const seconds = Number(new URL(location.href).searchParams.get("seconds") ?? 20);
const { AudioPump, AudioRingBuffer, AudioPlaybackClock } = await import("/packages/alpha2/dist/audio-timeline.js");
const { Alpha2AudioWorkletHost, loadAlpha2AudioWorkletWasm, useAudioRing, configureEngineMixedAudio } =
  await import("/packages/alpha2/dist/audio-worklet.js");
const rate = 48000;
const context = new AudioContext({ sampleRate: ${contextRate} });
const resumed = await Promise.race([
  context.resume().then(() => true),
  new Promise((resolve) => setTimeout(() => resolve(false), 5000)),
]);
if (!resumed) {
  void context.close();
  skip("audio-autostart", "AudioContext did not start without a user gesture");
}
const node = await Alpha2AudioWorkletHost.create(context, await loadAlpha2AudioWorkletWasm(), {
  numberOfInputs: 0, outputChannelCount: [2], projectSampleRate: rate, onDiagnostic: () => {},
});
node.connect(context.destination);
let firstFrame = null;
node.port.addEventListener("message", (event) => {
  if (event.data?.type === "alpha2-first-read") firstFrame = event.data.frame;
});
// An underrun stalls the clock while the device keeps counting, so it is reported separately.
let underruns = 0;
const underrunsAtMs = [];
const created = performance.now();
node.port.addEventListener("message", (event) => {
  if (event.data?.type !== "alpha2-underrun") return;
  underruns += 1;
  underrunsAtMs.push(Math.round(performance.now() - created));
});
// Prefill one second so a loaded machine missing the 20 ms pump does not underrun; this checks the
// clock, not minimal buffering.
const ring = AudioRingBuffer.create(2, rate);
const silence = [new Float32Array(1024), new Float32Array(1024)];
const pump = new AudioPump(
  { sampleRate: rate, channels: 2, mix: async (_, frames) => ({ kind: "mixed", planes: silence.map((plane) => plane.subarray(0, frames)) }) },
  ring, 0, { blockFrames: 1024 },
);
await pump.fill();
await useAudioRing(node, ring.transfer);
await configureEngineMixedAudio(node);
const clock = new AudioPlaybackClock(pump, ring, context, { processing: Alpha2AudioWorkletHost.latency(node) });
const timer = setInterval(() => void pump.fill(), 20);
const errors = [];
const offsets = [];
const started = performance.now();
while (performance.now() - started < seconds * 1000) {
  await new Promise((resolve) => setTimeout(resolve, 250));
  const timestamp = context.getOutputTimestamp();
  const device = timestamp.contextTime * rate;
  if (device > 0) errors.push(clock.sample - device);
  if (device > 0 && firstFrame !== null) {
    const heard = ((timestamp.contextTime * context.sampleRate - firstFrame) * rate) / context.sampleRate;
    offsets.push(clock.sample - heard);
  }
}
clearInterval(timer);
const baseLatency = context.baseLatency ?? 0;
await context.close();
if (errors.length < 8) throw new Error("the audio device did not advance");
if (firstFrame === null) throw new Error("the worklet never reported its first ring read");
const settled = errors.slice(4);
const first = settled.slice(0, 4).reduce((a, b) => a + b) / 4;
const last = settled.slice(-4).reduce((a, b) => a + b) / 4;
const frame = rate / 30;
const spread = Math.max(...settled) - Math.min(...settled);
const settledOffsets = offsets.slice(4);
const meanOffsetMs = settledOffsets.length === 0
  ? null
  : (settledOffsets.reduce((a, b) => a + b) / settledOffsets.length / rate) * 1000;
const offsetBoundMs = baseLatency * 1000 + (128 / context.sampleRate) * 1000 + 2;
const details = {
  contextRate: context.sampleRate,
  seconds,
  underruns,
  underrunsAtMs,
  samples: settled.length,
  meanErrorMs: +((settled.reduce((a, b) => a + b) / settled.length / rate) * 1000).toFixed(2),
  spreadMs: +((spread / rate) * 1000).toFixed(2),
  driftMs: +(((last - first) / rate) * 1000).toFixed(2),
  firstFrame,
  meanOffsetMs: meanOffsetMs === null ? null : +meanOffsetMs.toFixed(2),
  offsetBoundMs: +offsetBoundMs.toFixed(2),
  latencySource: clock.latencySource,
  latencyMs: +(clock.latencySeconds * 1000).toFixed(2),
};
const tracked = underruns === 0 && spread <= frame && Math.abs(last - first) <= frame;
if (tracked && (meanOffsetMs === null || Math.abs(meanOffsetMs) > offsetBoundMs))
  skip(
    "audio-output-timestamp",
    "the output timestamp is " + details.meanOffsetMs + " ms from the common origin; allowed " + details.offsetBoundMs + " ms",
    details,
  );
report({ ok: tracked, ...details });
`;
}

// A 48 kHz project in a 44.1 or 96 kHz context keeps pitch, drain rate and the clock right.
export function audioWorkletOtherRate() {
  return `
const { AudioPump, AudioRingBuffer, AudioPlaybackClock } = await import("/packages/alpha2/dist/audio-timeline.js");
const { Alpha2AudioWorkletHost, loadAlpha2AudioWorkletWasm, useAudioRing, configureEngineMixedAudio } =
  await import("/packages/alpha2/dist/audio-worklet.js");
const wasm = await loadAlpha2AudioWorkletWasm();
const project = 48_000;
const results = {};
let ok = true;
for (const contextRate of [44_100, 96_000]) {
  const context = new OfflineAudioContext({ numberOfChannels: 1, length: contextRate, sampleRate: contextRate });
  const diagnostics = [];
  const node = await Alpha2AudioWorkletHost.create(context, wasm, {
    numberOfInputs: 0, outputChannelCount: [1], projectSampleRate: project,
    onDiagnostic: (diagnostic) => diagnostics.push(diagnostic),
  });
  node.connect(context.destination);
  const ring = AudioRingBuffer.create(1, 2 * project);
  // An offline context may render before a just-posted message reaches the processor.
  await useAudioRing(node, ring.transfer);
  await configureEngineMixedAudio(node);
  const pump = new AudioPump(
    { sampleRate: project, channels: 1, mix: async (start, frames) => ({ kind: "mixed", planes: [Float32Array.from({ length: frames }, (_, index) => 0.5 * Math.sin(2 * Math.PI * 1000 * (start + index) / project))] }) },
    ring, 0, { blockFrames: 4096, targetFrames: project + 8192 },
  );
  await pump.fill();
  const clock = new AudioPlaybackClock(pump, ring, { sampleRate: contextRate }, { processing: Alpha2AudioWorkletHost.latency(node) });
  const rendered = (await context.startRendering()).getChannelData(0);
  const crossings = [];
  for (let index = contextRate / 10; index < rendered.length; index += 1)
    if (rendered[index - 1] < 0 && rendered[index] >= 0)
      crossings.push(index - rendered[index] / (rendered[index] - rendered[index - 1]));
  const heardHz = ((crossings.length - 1) * contextRate) / (crossings.at(-1) - crossings[0]);
  const playedSeconds = (Math.ceil(contextRate / 128) * 128) / contextRate;
  const taken = pump.position - ring.available;
  const clockSeconds = Number(clock.timelineTicks) / 120_000;
  results[contextRate] = { heardHz: +heardHz.toFixed(3), taken, playedSeconds: +playedSeconds.toFixed(5), clockSeconds: +clockSeconds.toFixed(5), latencySamples: Alpha2AudioWorkletHost.latency(node).latencySamples, diagnostics };
  ok &&= Math.abs(heardHz - 1000) < 0.05 && Math.abs(taken - (playedSeconds * project + 18)) < 4 && Math.abs(clockSeconds - playedSeconds) < 0.0001
    && diagnostics.length === 1 && diagnostics[0].kind === "resampled";
}
report({ ok, results });
`;
}
