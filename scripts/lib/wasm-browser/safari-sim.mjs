// Safari's WebCodecs and VideoFrame behaviours that the engine handles by feature detection,
// simulated in Chrome so those branches run where Safari cannot be driven. A page loads this
// module and calls `installSimulations` before it imports the binding (`?sim=<list>` on any case
// page, or a case that names its simulations). Each wrapper changes only what Safari was measured
// to do differently; everything else is Chrome's own.

/** The simulations, by name. */
export const SIMULATIONS = Object.freeze([
  // An H.264 VideoEncoder in `latencyMode: "quality"` puts out nothing and its flush resolves.
  "quality-silent",
  // An AAC encoder configured above 48 kHz reports half the rate in its decoder configuration.
  "aac-rate",
  // An AAC encoder drops the first frame (1024 samples) of its input.
  "aac-first-frame-drop",
  // An AAC encoder puts 2112 frames of priming before the audio.
  "aac-priming-2112",
  // The VideoFrame buffer constructor ignores `layout` and reads the planes packed from 0.
  "frame-layout-ignored",
  // A VideoDecoder puts out H.264 frames in decode order, not presentation order.
  "decode-order",
]);

const AAC_FRAME = 1024;
const AAC_PRIMING = 2112;

/** Installs the simulations `names` lists; throws on an unknown name. */
export function installSimulations(names) {
  for (const name of names)
    if (!SIMULATIONS.includes(name))
      throw new Error(
        `unknown simulation ${JSON.stringify(name)}: expected one of ${SIMULATIONS.join(", ")}`,
      );
  const on = new Set(names);
  if (on.has("quality-silent")) qualitySilent();
  if (
    on.has("aac-rate") ||
    on.has("aac-first-frame-drop") ||
    on.has("aac-priming-2112")
  )
    aacEncoder({
      rate: on.has("aac-rate"),
      dropFirst: on.has("aac-first-frame-drop"),
      priming: on.has("aac-priming-2112"),
    });
  if (on.has("frame-layout-ignored")) frameLayoutIgnored();
  if (on.has("decode-order")) decodeOrder();
}

/**
 * Replaces the global constructor `name` with one that builds the real object and lets `adapt`
 * change its init and wrap its methods; `instanceof` and the static methods keep working.
 */
function wrapConstructor(name, adapt) {
  const Original = globalThis[name];
  globalThis[name] = new Proxy(Original, {
    construct(target, args) {
      return adapt(target, args);
    },
  });
}

function qualitySilent() {
  wrapConstructor("VideoEncoder", (Encoder, [init]) => {
    let quality = false;
    const encoder = new Encoder({
      ...init,
      output: (...args) => {
        if (!quality) init.output(...args);
      },
    });
    const configure = encoder.configure.bind(encoder);
    encoder.configure = (config) => {
      quality =
        /^(avc1|avc3)/.test(config.codec) &&
        (config.latencyMode ?? "quality") === "quality";
      configure(config);
    };
    return encoder;
  });
}

function aacEncoder({ rate, dropFirst, priming }) {
  wrapConstructor("AudioEncoder", (Encoder, [init]) => {
    let config = null;
    let first = true;
    const isAac = () => config?.codec?.startsWith("mp4a") === true;
    const encoder = new Encoder({
      ...init,
      output: (chunk, metadata) => {
        const decoderConfig = metadata?.decoderConfig;
        if (rate && isAac() && decoderConfig && config.sampleRate > 48_000)
          metadata = {
            ...metadata,
            decoderConfig: {
              ...decoderConfig,
              sampleRate: decoderConfig.sampleRate / 2,
            },
          };
        init.output(chunk, metadata);
      },
    });
    const configure = encoder.configure.bind(encoder);
    encoder.configure = (next) => {
      config = next;
      first = true;
      configure(next);
    };
    const encode = encoder.encode.bind(encoder);
    encoder.encode = (data) => {
      if (!isAac() || !first || !(dropFirst || priming)) return encode(data);
      first = false;
      // The first input: its first frame dropped, priming silence before the rest, stamped with
      // the input's own time, as Safari's encoder delivers it.
      const channels = data.numberOfChannels;
      const frames = data.numberOfFrames;
      const kept = dropFirst ? Math.max(0, frames - AAC_FRAME) : frames;
      const lead = priming ? AAC_PRIMING : 0;
      const planar = new Float32Array((lead + kept) * channels);
      const plane = new Float32Array(frames);
      for (let channel = 0; channel < channels; channel += 1) {
        data.copyTo(plane, { planeIndex: channel, format: "f32-planar" });
        planar.set(
          plane.subarray(frames - kept),
          channel * (lead + kept) + lead,
        );
      }
      const changed = new AudioData({
        format: "f32-planar",
        sampleRate: data.sampleRate,
        numberOfFrames: lead + kept,
        numberOfChannels: channels,
        timestamp: data.timestamp,
        data: planar,
      });
      try {
        encode(changed);
      } finally {
        changed.close();
      }
    };
    return encoder;
  });
}

function frameLayoutIgnored() {
  wrapConstructor("VideoFrame", (Frame, args) => {
    const [data, init] = args;
    const buffer =
      data instanceof ArrayBuffer ||
      ArrayBuffer.isView(data) ||
      (typeof SharedArrayBuffer !== "undefined" &&
        data instanceof SharedArrayBuffer);
    if (!buffer || init?.layout === undefined) return new Frame(...args);
    const { layout: _, ...rest } = init;
    return new Frame(data, rest);
  });
}

function decodeOrder() {
  wrapConstructor("VideoDecoder", (Decoder, [init]) => {
    let h264 = false;
    // Chunk times in decode order, and frames put out but not yet handed on.
    const order = [];
    const held = new Map();
    // Frames handed on before one with an earlier time, counted on the page
    // (`decodeOrderOutputs`) so a case can tell the simulation changed the order.
    let latest = -Infinity;
    const release = () => {
      while (order.length > 0 && held.has(order[0])) {
        const timestamp = order.shift();
        const frame = held.get(timestamp);
        held.delete(timestamp);
        if (timestamp < latest)
          globalThis.decodeOrderOutputs =
            (globalThis.decodeOrderOutputs ?? 0) + 1;
        latest = Math.max(latest, timestamp);
        init.output(frame);
      }
    };
    const decoder = new Decoder({
      ...init,
      output: (frame) => {
        if (!h264 || !order.includes(frame.timestamp)) {
          init.output(frame);
          return;
        }
        held.set(frame.timestamp, frame);
        release();
      },
    });
    const configure = decoder.configure.bind(decoder);
    decoder.configure = (config) => {
      h264 = /^(avc1|avc3)/.test(config.codec);
      configure(config);
    };
    const decode = decoder.decode.bind(decoder);
    decoder.decode = (chunk) => {
      if (h264) order.push(chunk.timestamp);
      decode(chunk);
    };
    const flush = decoder.flush.bind(decoder);
    decoder.flush = async () => {
      await flush();
      // Whatever the decoder dropped is skipped; the rest goes out in decode order.
      for (const timestamp of [...order]) {
        const frame = held.get(timestamp);
        if (frame !== undefined) init.output(frame);
      }
      order.length = 0;
      held.clear();
    };
    const reset = decoder.reset.bind(decoder);
    decoder.reset = () => {
      for (const frame of held.values()) frame.close();
      order.length = 0;
      held.clear();
      reset();
    };
    const close = decoder.close.bind(decoder);
    decoder.close = () => {
      for (const frame of held.values()) frame.close();
      held.clear();
      close();
    };
    return decoder;
  });
}
