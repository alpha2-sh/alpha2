import { basename } from "node:path";

import { orientationAssets } from "../fixtures.mjs";

// Renders the orientation project with the packaged engine and compares each frame with the native
// export's.
export function orientationPage() {
  return `<!doctype html>
<html><head><title>pending</title>
<script type="importmap">{"imports":{"mediabunny":"/node_modules/mediabunny/dist/bundles/mediabunny.mjs"}}</script>
</head><body><script type="module">
import init, { WebEffectRegistry } from "/packages/alpha2/wasm/alpha2_wasm.js";
import { chromaSitingForCodec, copyPlanarYuvFrame, uploadPlanarYuvFrame } from "/packages/alpha2/dist/hdr-frames.js";
import { readVideoOrientation } from "/packages/alpha2/dist/media.js";
import { decodeStill, stillRaster } from "/packages/alpha2-editor/dist/browser/runtime/still.js";
import { ALL_FORMATS, BlobSource, Input, VideoSampleSink } from "mediabunny";
const done = (value) => { document.title = encodeURIComponent(JSON.stringify(value)); };
const PROJECT_TICKS_PER_FRAME = 4_000n; // 30 fps
const files = ${JSON.stringify(Object.fromEntries(Object.entries(orientationAssets).map(([id, path]) => [id, basename(path)])))};
try {
  await init();
  const project = await fetch("/tests/cases/host-parity-orientation-project.json").then((response) => response.json());
  const { width, height } = project.settings;
  const native = parseY4m(new Uint8Array(await fetch("/native.y4m").then((response) => response.arrayBuffer())), width, height);
  const surface = document.createElement("canvas");
  surface.width = width;
  surface.height = height;
  document.body.append(surface);
  const engine = await WebEffectRegistry.createForHtmlCanvas(surface);
  engine.loadProject(JSON.stringify(project));
  const stills = {};
  const planar = {};
  const orientations = {};
  const displaySizes = {};
  for (const source of project.sources) {
    const blob = await fetch(\`/tests/corpus/\${files[source.id]}\`).then((response) => response.blob());
    if (source.kind === "image") {
      const bitmap = await decodeStill(blob);
      stills[source.id] = stillRaster(bitmap);
      bitmap.close();
      continue;
    }
    const input = new Input({ source: new BlobSource(blob), formats: ALL_FORMATS });
    const track = await input.getPrimaryVideoTrack();
    // A container without a display matrix (MPEG-TS) states no orientation: the track's own, as the
    // SDK's mediabunny adapter reads it.
    const orientation = (await readVideoOrientation(blob, track.id)) ?? { rotationDegrees: track.rotation, mirror: false };
    orientations[source.id] = orientation;
    // MPEG-TS starts at its mux delay: the first frame, not time zero.
    const sample = await new VideoSampleSink(track).getSample(await track.getFirstTimestamp());
    const frame = sample.toVideoFrame();
    sample.close();
    planar[source.id] = await copyPlanarYuvFrame(frame, null, {
      chromaSiting: chromaSitingForCodec(track.codec),
      rotation: orientation.rotationDegrees,
      mirror: orientation.mirror,
    });
    displaySizes[source.id] = [planar[source.id].displayWidth, planar[source.id].displayHeight];
    input.dispose();
  }
  const totals = { y: [0, 0, 0], u: [0, 0, 0], v: [0, 0, 0] };
  const worstFrames = [];
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
        const still = stills[need.sourceId];
        if (still) engine.uploadPresentedRgbaImage(need.sourceId, still.width, still.height, still.pixels, still.alpha !== "straight", width, height);
        else uploadPlanarYuvFrame(engine, need.sourceId, BigInt(need.mediaTimeTicks), planar[need.sourceId], width, height);
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
    const planes = { y: [0, width * height], u: [width * height, (width * height) / 4], v: [(width * height * 5) / 4, (width * height) / 4] };
    let worst = 0;
    for (const [name, [offset, length]] of Object.entries(planes)) {
      for (let sample = offset; sample < offset + length; sample += 1) {
        const delta = Math.abs(wasm[sample] - native[index][sample]);
        totals[name][0] = Math.max(totals[name][0], delta);
        totals[name][1] += delta;
        totals[name][2] += 1;
        worst = Math.max(worst, delta);
      }
    }
    worstFrames.push(worst);
  }
  const metric = ([max, sum, count]) => ({ max, mean: sum / count });
  done({ frames: native.length, orientations, displaySizes, worstFrames, y: metric(totals.y), u: metric(totals.u), v: metric(totals.v) });
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
