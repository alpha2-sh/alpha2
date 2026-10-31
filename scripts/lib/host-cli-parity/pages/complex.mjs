import { PEAK_CODES } from "../../parity-tolerance.mjs";

// Renders every frame of a project with the packaged engine and compares it with the native
// export's encoded frame. Frame needs are answered as a host must: the last frame at or before
// each requested time, uploaded under the requested time. `fonts` are registered in order, as the
// native export's `--font`s are.
export function complexPage(projectUrl, fonts = []) {
  return `<!doctype html>
<html><head><title>pending</title></head><body><script type="module">
import init, { WebEffectRegistry } from "/packages/alpha2/wasm/alpha2_wasm.js";
const done = (value) => { document.title = encodeURIComponent(JSON.stringify(value)); };
const VIDEO_TICKS_PER_FRAME = 10_000n; // 12 fps
const VIDEO_FRAMES = 12n;
const PROJECT_TICKS_PER_FRAME = 5_000n; // 24 fps
const PEAK_CODES = ${PEAK_CODES};
try {
  await init();
  const project = await fetch(${JSON.stringify(projectUrl)}).then((response) => response.json());
  const { width, height } = project.settings;
  const native = parseY4m(new Uint8Array(await fetch("/native.y4m").then((response) => response.arrayBuffer())), width, height);
  const surface = document.createElement("canvas");
  surface.width = width;
  surface.height = height;
  document.body.append(surface);
  const engine = await WebEffectRegistry.createForHtmlCanvas(surface);
  for (const font of ${JSON.stringify(fonts)})
    engine.addFont(font, new Uint8Array(await fetch(font).then((response) => response.arrayBuffer())), 0);
  for (const resource of project.effect_resources ?? [])
    engine.registerLut3d(resource.id, await fetch(\`/tests/corpus/\${resource.locator.replace("fixture://", "")}.cube\`).then((response) => response.text()));
  engine.loadProject(JSON.stringify(project));
  // At its own size, as the native host decodes it: a project may be larger than the still.
  const still = await image("/tests/corpus/parity-image.png");
  engine.uploadRgbaImage("still", still.width, still.height, still.data);
  const catalog = JSON.parse(engine.builtinCatalog());
  const used = new Set([
    ...(project.compositions ?? []).flatMap((composition) => composition.nodes.map((node) => node.op.effect?.id)),
    ...project.timeline.tracks.flatMap((track) => (track.transitions ?? []).map((transition) => transition.effect.id)),
  ]);
  const unused = [...catalog.effects, ...catalog.transitions].map((entry) => entry.id).filter((id) => !used.has(id));
  const totals = { y: [0, 0, 0], u: [0, 0, 0], v: [0, 0, 0] };
  // Samples off by 0, 1, 2, and 3 or more codes, across every plane.
  const histogram = [0, 0, 0, 0];
  const worstFrames = [];
  const frameMeans = [];
  // Samples more than one code apart, per frame.
  const frameOver = [];
  // Samples more than PEAK_CODES apart (the cross-device outliers), per frame.
  const frameOutliers = [];
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
        if (need.sourceId !== "video") continue;
        const requested = BigInt(need.mediaTimeTicks);
        let frame = requested / VIDEO_TICKS_PER_FRAME;
        if (frame >= VIDEO_FRAMES) frame = VIDEO_FRAMES - 1n;
        const name = String(frame).padStart(2, "0");
        engine.uploadRgbaFrame("video", requested, width, height, await pixels(\`/tests/corpus/parity-frames/\${name}.png\`));
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
    let sum = 0;
    let over = 0;
    let outliers = 0;
    for (const [name, [offset, length]] of Object.entries(planes)) {
      for (let sample = offset; sample < offset + length; sample += 1) {
        const delta = Math.abs(wasm[sample] - native[index][sample]);
        totals[name][0] = Math.max(totals[name][0], delta);
        totals[name][1] += delta;
        totals[name][2] += 1;
        histogram[Math.min(delta, 3)] += 1;
        worst = Math.max(worst, delta);
        sum += delta;
        if (delta > 1) over += 1;
        if (delta > PEAK_CODES) outliers += 1;
      }
    }
    worstFrames.push(worst);
    frameMeans.push(sum / native[index].length);
    frameOver.push(over);
    frameOutliers.push(outliers);
  }
  const metric = ([max, sum, count]) => ({ max, mean: sum / count });
  const outliers = frameOutliers.reduce((total, count) => total + count, 0);
  done({ adapter: JSON.parse(engine.diagnostics()).adapter, frames: native.length, y: metric(totals.y), u: metric(totals.u), v: metric(totals.v), histogram, worstFrames, frameMeans, frameOver, frameOutliers, outliers, unused });
} catch (error) {
  done({ error: error?.stack ?? String(error) });
}
// Straight RGBA as stored in the PNG, with no colour management: the native
// side decodes the same bytes.
async function pixels(url) {
  return (await image(url)).data;
}
async function image(url) {
  const blob = await fetch(url).then((response) => response.blob());
  const bitmap = await createImageBitmap(blob, { colorSpaceConversion: "none", premultiplyAlpha: "none" });
  const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
  const context = canvas.getContext("2d", { colorSpace: "srgb" });
  context.drawImage(bitmap, 0, 0);
  bitmap.close();
  return context.getImageData(0, 0, canvas.width, canvas.height);
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
