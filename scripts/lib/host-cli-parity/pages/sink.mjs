// The browser sink encodes a real export through mediabunny; each frame's bitrate or quantizer
// must be the engine's, never mediabunny's own level table.
export function sinkPage() {
  return `<!doctype html><title>pending</title>
<script type="importmap">{"imports":{"mediabunny":"/node_modules/mediabunny/dist/bundles/mediabunny.mjs"}}</script>
<script type="module">
try {
  const wasm = await import("/packages/alpha2/wasm/alpha2_wasm.js");
  await wasm.default();
  const { MediabunnyMp4Sink } = await import("/packages/alpha2/dist/mediabunny.js");
  const { resolveVideoRateControl } = await import("/packages/alpha2/dist/rate-control.js");
  // The quantizer each frame is encoded at, as the encoder receives it: mediabunny passes it in the
  // frame's encode options, not in the configuration the sink reads back.
  let encodedQuantizers = new Set();
  const encode = VideoEncoder.prototype.encode;
  VideoEncoder.prototype.encode = function (frame, options) {
    encodedQuantizers.add(options?.avc?.quantizer ?? null);
    return encode.call(this, frame, options);
  };
  const results = {};
  for (const level of ["low", "high"]) {
    encodedQuantizers = new Set();
    const videoRateControl = resolveVideoRateControl(wasm, {
      codec: "h264", width: 640, height: 360, frameRate: { num: 30, den: 1 }, quality: level,
    });
    const sink = new MediabunnyMp4Sink({ videoCodec: "avc", videoRateControl, frameRate: { num: 30, den: 1 }, destination: { kind: "memory", maxBytes: 64 * 1024 * 1024 } });
    const canvas = new OffscreenCanvas(640, 360);
    const context = canvas.getContext("2d");
    for (let index = 0; index < 60; index += 1) {
      const image = context.createImageData(640, 360);
      let seed = 1;
      for (let offset = 0; offset < image.data.length; offset += 4) {
        const x = (offset / 4) % 640;
        const y = Math.floor(offset / 4 / 640);
        seed = (seed * 1103515245 + 12345) & 0x7fffffff;
        image.data[offset] = (x + index * 4) & 255;
        image.data[offset + 1] = (y * 2 + index) & 255;
        image.data[offset + 2] = seed & 63;
        image.data[offset + 3] = 255;
      }
      context.putImageData(image, 0, 0);
      const frame = new VideoFrame(canvas, { timestamp: Math.round((index * 1e6) / 30) });
      await sink.writeVideo({ index: BigInt(index), timeTicks: BigInt(index * 4000), frame });
    }
    await sink.finish();
    results[level] = { videoRateControl, applied: sink.appliedVideoRateControl, encodedQuantizers: [...encodedQuantizers], bytes: sink.output.buffer.byteLength };
  }
  document.title = encodeURIComponent(JSON.stringify(results));
} catch (error) {
  document.title = encodeURIComponent(JSON.stringify({ error: String(error?.stack ?? error) }));
}
</script>`;
}
