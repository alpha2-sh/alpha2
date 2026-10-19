// Renders the extensions project with the packaged engine, its sources' frames from the packaged
// extensions module, and compares each frame with the native export's, as encoded.
export function extensionsPage() {
  return `<!doctype html>
<html><head><title>pending</title></head><body><script type="module">
import init, { WebEffectRegistry } from "/packages/alpha2/wasm/alpha2_wasm.js";
import initExtensions, { ExtensionRegistry } from "/packages/alpha2/wasm/alpha2_extensions.js";
const done = (value) => { document.title = encodeURIComponent(JSON.stringify(value)); };
const PROJECT_TICKS_PER_FRAME = 12_000n; // 10 fps
const bytes = async (name) => new Uint8Array(await fetch(\`/tests/corpus/extensions/\${name}\`).then((response) => response.arrayBuffer()));
try {
  await init();
  await initExtensions();
  const project = await fetch("/tests/cases/host-parity-extensions-project.json").then((response) => response.json());
  const { width, height } = project.settings;
  const native = parseY4m(new Uint8Array(await fetch("/native.y4m").then((response) => response.arrayBuffer())), width, height);
  const surface = document.createElement("canvas");
  surface.width = width;
  surface.height = height;
  document.body.append(surface);
  const engine = await WebEffectRegistry.createForHtmlCanvas(surface);
  engine.loadProject(JSON.stringify(project));
  const extensions = new ExtensionRegistry();
  extensions.registerFont(new Uint8Array(await fetch("/tests/fonts/Alpha2Fixture.ttf").then((response) => response.arrayBuffer())));
  for (const [index, locator] of ["f7", "f8", "f9", "f10"].entries())
    extensions.registerResource(locator, await bytes(\`sequence_\${String(index + 7).padStart(4, "0")}.png\`));
  for (const [index, locator] of ["x1", "x2"].entries())
    extensions.registerResource(locator, await bytes(\`sequence_\${String(index + 1).padStart(4, "0")}.exr\`));
  const files = { gif: "animation.gif", webp: "animation.webp", apng: "animation.png", logo: "shapes.svg", label: "label.svg", lottie: "moving.json", dot: "pair.lottie", exr: "window.exr" };
  const handles = {};
  const kinds = {};
  for (const source of project.sources) {
    handles[source.id] = extensions.open(JSON.stringify(source.provider), files[source.id] ? await bytes(files[source.id]) : undefined);
    kinds[source.id] = source.kind;
  }
  const totals = { y: [0, 0, 0], u: [0, 0, 0], v: [0, 0, 0] };
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
        const frame = extensions.render(handles[need.sourceId], BigInt(need.mediaTimeTicks), width, height);
        const image = kinds[need.sourceId] === "image";
        if (frame.precision === "linear_f16") {
          if (image) engine.uploadPresentedLinearImage(need.sourceId, frame.width, frame.height, frame.halfPixels, width, height);
          else engine.uploadPresentedLinearFrame(need.sourceId, BigInt(need.mediaTimeTicks), frame.width, frame.height, frame.halfPixels, width, height);
        } else if (image)
          engine.uploadPresentedRgbaImage(need.sourceId, frame.width, frame.height, frame.pixels, true, width, height);
        else
          engine.uploadPresentedRgbaFrame(need.sourceId, BigInt(need.mediaTimeTicks), frame.width, frame.height, frame.pixels, true, width, height);
        frame.free();
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
    for (const [name, [offset, length]] of Object.entries(planes)) {
      for (let sample = offset; sample < offset + length; sample += 1) {
        const delta = Math.abs(wasm[sample] - native[index][sample]);
        totals[name][0] = Math.max(totals[name][0], delta);
        totals[name][1] += delta;
        totals[name][2] += 1;
      }
    }
  }
  const metric = ([max, sum, count]) => ({ max, mean: sum / count });
  done({ frames: native.length, y: metric(totals.y), u: metric(totals.u), v: metric(totals.v) });
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
