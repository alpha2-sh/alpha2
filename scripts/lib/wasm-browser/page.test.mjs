import assert from "node:assert/strict";
import test from "node:test";

import { DECODE_REFUSAL_SKIP, LUMA_AT } from "./page.mjs";

const lumaAt = new Function(`${LUMA_AT}; return lumaAt;`)();

function frame(format, bytes, stride) {
  return {
    format,
    allocationSize: () => bytes.length,
    copyTo: async (target) => {
      target.set(bytes);
      return [{ offset: 0, stride }];
    },
  };
}

test("the luma helper reads BT.709 limited-range luma from RGB and YUV layouts", async () => {
  const rgba = Uint8Array.of(0, 0, 0, 255, 230, 120, 40, 255);
  const bgra = Uint8Array.of(0, 0, 0, 255, 40, 120, 230, 255);
  // 16 + (65.481 * 230 + 128.553 * 120 + 24.966 * 40) / 255 = 139.47
  const orange = 139;
  for (const format of ["RGBA", "RGBX"])
    assert.equal(await lumaAt(frame(format, rgba, 8), 1, 0), orange, format);
  for (const format of ["BGRA", "BGRX"])
    assert.equal(await lumaAt(frame(format, bgra, 8), 1, 0), orange, format);
  assert.equal(await lumaAt(frame("RGBA", rgba, 8), 0, 0), 16);
  assert.equal(
    await lumaAt(frame("RGBX", Uint8Array.of(255, 255, 255, 255), 4), 0, 0),
    235,
  );
  const plane = Uint8Array.of(10, 20, 30, 0, 40, 50, 60, 0);
  for (const format of ["I420", "I420A", "NV12", "I444"])
    assert.equal(await lumaAt(frame(format, plane, 4), 2, 1), 60, format);
  await assert.rejects(
    lumaAt(frame("RGBAF16", plane, 4), 0, 0),
    /no luma for a RGBAF16 frame/,
  );
});

test("the engine's DECODE_UNAVAILABLE skips only when WebCodecs refuses the codec too", async () => {
  const asked = [];
  const decoders = (supported) => {
    const decoder = {
      isConfigSupported: async (config) => {
        asked.push(config);
        return { supported: supported(config.codec), config };
      },
    };
    return { VideoDecoder: decoder, AudioDecoder: decoder };
  };
  const skipWith = (globals) =>
    new Function(
      "globalThis",
      `${DECODE_REFUSAL_SKIP}; return decodeRefusalSkip;`,
    )(globals);
  const refusal = (kind, codec, channels) =>
    Object.assign(new Error(`cannot decode ${codec}`), {
      code: "DECODE_UNAVAILABLE",
      facts: { kind, codec, ...(channels ? { channels } : {}) },
    });
  const only = skipWith(decoders((codec) => codec === "avc1.64001f"));
  assert.equal(await only(refusal("video", "avc1.64001f")), null);
  // The browser's own decoder threw on a codec it claims: only a failing plain decode of the
  // case's source skips.
  const trialFailed = Object.assign(refusal("audio", "avc1.64001f"), {
    cause: new Error("EncodingError"),
  });
  assert.equal(await only(trialFailed), null, "no source was tried");
  const trying = (decodes) =>
    skipWith({
      ...decoders((codec) => codec === "avc1.64001f"),
      trialDecodes: async () => decodes,
    });
  assert.equal(
    (await trying(false)(trialFailed)).capability,
    "webcodecs-audio",
    "the source fails a plain decode too",
  );
  assert.equal(await trying(true)(trialFailed), null, "the source decodes");
  assert.equal(await trying(undefined)(trialFailed), null, "no source matched");
  assert.deepEqual(await only(refusal("video", "hvc1.1.6.L93.B0")), {
    skipped:
      "this browser does not decode the case's source (WebCodecs refuses hvc1.1.6.L93.B0): cannot decode hvc1.1.6.L93.B0",
    capability: "webcodecs-video",
  });
  assert.equal(
    (await only(refusal("audio", "ac-3", 6))).capability,
    "webcodecs-audio",
  );
  assert.deepEqual(asked.at(-1), {
    codec: "ac-3",
    sampleRate: 48000,
    numberOfChannels: 6,
  });
  // No codec to ask about, another error, or no WebCodecs at all.
  assert.equal(await only(refusal("video", null)), null);
  assert.equal(await only(new Error("device lost")), null);
  assert.equal(
    (await skipWith({})(refusal("audio", "opus"))).capability,
    "webcodecs-audio",
  );
  const throwing = skipWith({
    VideoDecoder: {
      isConfigSupported: async () => Promise.reject(new TypeError("bad")),
    },
  });
  assert.equal(
    (await throwing(refusal("video", "vp8"))).capability,
    "webcodecs-video",
  );
});
