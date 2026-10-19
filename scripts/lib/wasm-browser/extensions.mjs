// Cases on the extension module: restart after a trap, SVG budgets, and the provider contract.

// A provider panic (a real out-of-bounds trap) restarts the module from the compiled module
// without a fetch, and the old instance's sources open again.
export function extensionModuleRestart() {
  return `
const { loadExtensions, ExtensionModuleError, StaleExtensionSourceError } = await import("/packages/alpha2/dist/extensions.js");
const glue = await import("alpha2/extensions-wasm");
let failedLoad = false;
try { await loadExtensions("/missing/alpha2_extensions_bg.wasm"); } catch { failedLoad = true; }
const extensions = await loadExtensions("/packages/alpha2/wasm/alpha2_extensions_bg.wasm");
const svg = new TextEncoder().encode('<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="#f00"/></svg>');
const fit = { width: 8, height: 8 };
const first = extensions.open({ format: "svg" }, svg);
const second = extensions.open({ format: "svg" }, svg);
const render = glue.ExtensionRegistry.prototype.render;
glue.ExtensionRegistry.prototype.render = function (...args) {
  glue.ExtensionRegistry.prototype.render = render;
  glue.initSync().extensionregistry_open(this.__wbg_ptr, 0xffffff00, 200, 0, 0);
  return render.apply(this, args);
};
let restarted = null;
try { first.render(0n, fit); } catch (error) { restarted = error; }
glue.ExtensionRegistry.prototype.render = render;
let stale = false;
try { second.render(0n, fit); } catch (error) { stale = error instanceof StaleExtensionSourceError; }
const pixel = [...extensions.open({ format: "svg" }, svg).render(0n, fit).pixels.subarray(0, 4)];
report({
  ok: failedLoad && restarted instanceof ExtensionModuleError && restarted.cause instanceof WebAssembly.RuntimeError && stale && pixel.join() === "255,0,0,255",
  failedLoad,
  restarted: String(restarted?.message),
  stale,
  pixel,
});
`;
}

// SVGs past a budget (deep nesting, hour-long filters, backtracking styles, gzip bombs) are refused
// within the module's 1 MiB stack without stopping it; one nested as deep as allowed is drawn.
export function extensionSvgBudgets() {
  return `
const { loadExtensions, ExtensionModuleError } = await import("/packages/alpha2/dist/extensions.js");
const extensions = await loadExtensions("/packages/alpha2/wasm/alpha2_extensions_bg.wasm");
const doc = (body, size = 64) => '<svg xmlns="http://www.w3.org/2000/svg" width="' + size + '" height="' + size + '">' + body + '</svg>';
const nested = (levels) => doc('<g opacity=".99">'.repeat(levels) + '<rect width="8" height="8"/>' + '</g>'.repeat(levels));
let entities = '<!ENTITY d0 ' + "'" + '<rect width="8" height="8"/>' + "'" + '>';
for (let level = 1; level <= 4; level++) entities += '<!ENTITY d' + level + ' "' + '<g>'.repeat(90) + '&d' + (level - 1) + ';' + '</g>'.repeat(90) + '">';
const packed = new Uint8Array(await new Response(new Blob([doc(' '.repeat(40 << 20), 8)]).stream().pipeThrough(new CompressionStream("gzip"))).arrayBuffer());
let binary = "";
for (let at = 0; at < packed.length; at += 0x8000) binary += String.fromCharCode(...packed.subarray(at, at + 0x8000));
const documents = {
  nestedAsDeepAsAllowed: nested(254),
  nestedTooDeep: nested(100000),
  entitiesTooDeep: '<!DOCTYPE svg [' + entities + ']>' + doc('&d4;'),
  morphology400: doc('<filter id="f" x="0" y="0" width="1" height="1"><feMorphology operator="dilate" radius="400"/></filter><rect width="1920" height="1080" fill="red" filter="url(#f)"/>', 1920),
  backtrackingRule: doc('<style>x g g g g g rect{fill:red}</style>' + '<g>'.repeat(100) + '<rect width="1" height="1"/>'.repeat(10) + '</g>'.repeat(100)),
  gzipBomb: doc('<image width="8" height="8" href="data:image/svg+xml;base64,' + btoa(binary) + '"/>', 8),
};
const outcomes = {};
for (const [name, text] of Object.entries(documents)) {
  const started = performance.now();
  let outcome;
  try {
    const source = extensions.open({ format: "svg" }, new TextEncoder().encode(text));
    source.render(0n, { width: 64, height: 64 });
    outcome = "drawn";
  } catch (error) {
    outcome = (error instanceof ExtensionModuleError ? "module stopped: " : "refused: ") + String(error?.message ?? error);
  }
  outcomes[name] = { outcome: outcome.slice(0, 160), ms: Math.round(performance.now() - started) };
}
const refused = (name) => outcomes[name].outcome.startsWith("refused: ") && outcomes[name].outcome.includes("too large");
report({
  ok: outcomes.nestedAsDeepAsAllowed.outcome === "drawn"
    && ["nestedTooDeep", "entitiesTooDeep", "morphology400", "backtrackingRule", "gzipBomb"].every(refused)
    && Object.values(outcomes).every((o) => o.ms < 20000),
  outcomes,
});
`;
}

// The browser module must reach the native providers' conclusions over
// tests/cases/visual-provider-contract.json; a module stopped by a panic is a failure.
export function extensionProviderContract() {
  return `
const { loadExtensions, ExtensionModuleError } = await import("/packages/alpha2/dist/extensions.js");
const extensions = await loadExtensions("/packages/alpha2/wasm/alpha2_extensions_bg.wasm");
const contract = await (await fetch("/tests/cases/visual-provider-contract.json")).json();
const kind = (error) =>
  error instanceof ExtensionModuleError ? "module stopped: " + error.message : String(error?.message ?? error).split(":")[0];
const sourceFor = (provider, report) =>
  provider === "svg" ? { format: "svg" }
  : provider === "lottie" ? (report?.item ? { format: "lottie", animation: report.item } : { format: "lottie" })
  : provider === "exr" ? { format: "exr" }
  : { format: "animated_image" };
const frameCount = (timing) => timing.kind === "static" ? 1 : timing.kind === "frames" ? timing.durations.length : timing.frames;
// The tick a frame starts in: FrameRate::frame_start, at 120,000 ticks a second.
const frameTicks = (timing, index) =>
  timing.kind === "frames" ? BigInt(timing.durations.slice(0, index).reduce((sum, d) => sum + d, 0))
  : timing.kind === "rate" ? (BigInt(index) * 120000n * BigInt(timing.rate.den)) / BigInt(timing.rate.num)
  : 0n;
const bytesOf = async (c) => {
  if (c.file !== undefined) return new Uint8Array(await (await fetch("/tests/" + c.file)).arrayBuffer());
  if (c.text !== undefined) return new TextEncoder().encode(c.text);
  return Uint8Array.from(atob(c.base64), (ch) => ch.charCodeAt(0));
};
const wrong = [];
for (const c of contract.cases) {
  const bytes = await bytesOf(c);
  const reached = { probe: "none" };
  let probed = null;
  try {
    probed = extensions.probe(bytes);
    if (probed !== null) Object.assign(reached, { probe: "ok", provider: probed.provider });
  } catch (error) {
    reached.probe = kind(error);
    // The module's refusal does not name the provider that claimed the bytes.
    reached.provider = c.provider;
  }
  if (probed !== null) {
    const { provider, report } = probed;
    Object.assign(reached, { size: [report.width, report.height], frames: frameCount(report.timing) });
    let source = null;
    try {
      source = extensions.open(sourceFor(provider, report), bytes);
      reached.open = "ok";
    } catch (error) {
      reached.open = kind(error);
    }
    if (source !== null) {
      reached.renders = [];
      for (const { frame, fit } of c.renders ?? []) {
        const index = Math.min(frame, frameCount(report.timing) - 1);
        try {
          const rendered = source.render(frameTicks(report.timing, index), { width: fit[0], height: fit[1] });
          reached.renders.push({ frame, fit, size: [rendered.width, rendered.height] });
        } catch (error) {
          reached.renders.push({ frame, fit, refused: kind(error) });
        }
      }
      source.release?.();
    }
  }
  const conclusion = (o) =>
    JSON.stringify([
      ...["probe", "provider", "open", "size", "frames"].map((key) => o[key] ?? null),
      (o.renders ?? null)?.map((r) => [r.frame, r.fit, r.size ?? null, r.refused ?? null]) ?? null,
    ]);
  if (conclusion(reached) !== conclusion(c)) wrong.push({ name: c.name, listed: conclusion(c), reached: conclusion(reached) });
}
report({ ok: wrong.length === 0, cases: contract.cases.length, wrong });
`;
}
