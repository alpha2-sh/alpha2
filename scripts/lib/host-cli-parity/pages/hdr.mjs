// Renders the HDR project and compares each frame with the native export's. Each source is static,
// so every need is answered with its first frame, copied out planar with the frame's own color.
export function hdrPage() {
  return `<!doctype html>
<html><head><title>pending</title>
<script type="importmap">{"imports":{"mediabunny":"/node_modules/mediabunny/dist/bundles/mediabunny.mjs"}}</script>
</head><body><script type="module">
import init, { WebEffectRegistry } from "/packages/alpha2/wasm/alpha2_wasm.js";
import { chromaSitingForStream, copyPlanarYuvFrame, uploadPlanarYuvFrame } from "/packages/alpha2/dist/hdr-frames.js";
import { ALL_FORMATS, BlobSource, EncodedPacketSink, Input, VideoSampleSink } from "mediabunny";
const done = (value) => { document.title = encodeURIComponent(JSON.stringify(value)); };
const PROJECT_TICKS_PER_FRAME = 4_000n; // 30 fps
try {
  await init();
  const project = await fetch("/tests/cases/host-parity-hdr-project.json").then((response) => response.json());
  const { width, height } = project.settings;
  const native = parseY4m(new Uint8Array(await fetch("/native.y4m").then((response) => response.arrayBuffer())), width, height);
  const surface = document.createElement("canvas");
  surface.width = width;
  surface.height = height;
  document.body.append(surface);
  const engine = await WebEffectRegistry.createForHtmlCanvas(surface);
  engine.loadProject(JSON.stringify(project));
  const planar = {};
  const transfers = [];
  // What the container declares, as the editor's import reads it to record the source's color.
  const declaredTransfers = [];
  // Where each stream says its chroma sits, as the SDK's adapters read it from the parameter sets.
  const sitings = [];
  for (const source of project.sources) {
    const blob = await fetch(\`/tests/corpus/\${source.locator.slice("fixture://".length)}.mp4\`).then((response) => response.blob());
    const input = new Input({ source: new BlobSource(blob), formats: ALL_FORMATS });
    const track = await input.getPrimaryVideoTrack();
    declaredTransfers.push((await track.getColorSpace()).transfer ?? null);
    const config = await track.getDecoderConfig();
    const description = config.description ?? undefined;
    const first = description === undefined ? await new EncodedPacketSink(track).getFirstPacket() : null;
    const chromaSiting = chromaSitingForStream(config.codec, description, first?.data);
    sitings.push(chromaSiting);
    const sample = await new VideoSampleSink(track).getSample(0);
    const frame = sample.toVideoFrame();
    sample.close();
    transfers.push(frame.colorSpace.transfer ?? null);
    // The color the project declares wins over the frame's own, as the SDK's frame supply passes
    // it.
    const declared = engine.sourceColorMetadata(source.id);
    planar[source.id] = await copyPlanarYuvFrame(frame, declared === undefined ? null : JSON.parse(declared), { chromaSiting });
    input.dispose();
  }
  const totals = { y: [0, 0, 0], u: [0, 0, 0], v: [0, 0, 0] };
  // Where each plane is furthest apart, for a failure to point at.
  const worst = {};
  let lumaMin = 255;
  let lumaMax = 0;
  for (let index = 0; index < native.length; index += 1) {
    const time = BigInt(index) * PROJECT_TICKS_PER_FRAME;
    let rendered;
    for (let attempt = 0; attempt < 4 && rendered === undefined; attempt += 1) {
      const ticket = engine.beginExportAt(time, "mpeg4-part2");
      if (ticket.kind === "rendered") {
        rendered = await ticket.readback;
        break;
      }
      if (ticket.kind !== "waiting") throw new Error(\`frame \${index}: \${ticket.kind}\`);
      for (const need of JSON.parse(engine.frameNeeds(time))) {
        uploadPlanarYuvFrame(engine, need.sourceId, BigInt(need.mediaTimeTicks), planar[need.sourceId], width, height);
      }
    }
    if (rendered === undefined) throw new Error(\`frame \${index} never rendered\`);
    const frame = new VideoFrame(rendered.pixels, {
      format: "I420", codedWidth: rendered.width, codedHeight: rendered.height, timestamp: 0,
      layout: rendered.layout,
    });
    const wasm = new Uint8Array(frame.allocationSize());
    await frame.copyTo(wasm);
    frame.close();
    const planes = { y: [0, width * height, width], u: [width * height, (width * height) / 4, width / 2], v: [(width * height * 5) / 4, (width * height) / 4, width / 2] };
    for (const [name, [offset, length, stride]] of Object.entries(planes)) {
      for (let sample = offset; sample < offset + length; sample += 1) {
        const delta = Math.abs(wasm[sample] - native[index][sample]);
        if (delta > totals[name][0]) {
          const at = sample - offset;
          worst[name] = { frame: index, x: at % stride, y: Math.floor(at / stride), wasm: wasm[sample], native: native[index][sample] };
        }
        totals[name][0] = Math.max(totals[name][0], delta);
        totals[name][1] += delta;
        totals[name][2] += 1;
      }
    }
    // The ramp occupies the first row.
    for (let x = 0; x < width; x += 1) {
      lumaMin = Math.min(lumaMin, wasm[x]);
      lumaMax = Math.max(lumaMax, wasm[x]);
    }
  }
  const metric = ([max, sum, count]) => ({ max, mean: sum / count });
  done({ adapter: JSON.parse(engine.diagnostics()).adapter, frames: native.length, transfers, declaredTransfers, sitings, lumaSpread: lumaMax - lumaMin, worst, y: metric(totals.y), u: metric(totals.u), v: metric(totals.v) });
} catch (error) {
  done({ error: error?.stack ?? String(error) });
}
function parseY4m(bytes, width, height) {
  let at = bytes.indexOf(10) + 1;
  const size = (width * height * 3) / 2;
  const frames = [];
  while (at < bytes.length) {
    const end = bytes.indexOf(10, at);
    if (new TextDecoder().decode(bytes.subarray(at, end)) !== "FRAME") throw new Error("bad y4m frame header");
    frames.push(bytes.subarray(end + 1, end + 1 + size));
    at = end + 1 + size;
  }
  return frames;
}
</script></body></html>`;
}
