// The renderer a preview test's bridge makes: it records what it was asked to load and draw. A test
// steers it through `globalThis.stubEngine` (see preview-stubs.mjs).
export class Engine {
  constructor(canvas, sources, library, runtimeAssets = {}) {
    this.canvas = canvas;
    this.sources = sources;
    this.library = library;
    this.runtimeAssets = runtimeAssets;
    this.project = null;
    this.generation = 0;
    this.deviceLost = false;
    this.audioEngine = {};
    this.loads = [];
    this.renders = [];
    this.catalog = null;
    this.lastDiagnostics = null;
    globalThis.stubEngine.engines.push(this);
  }
  async start() {
    await globalThis.stubEngine.start?.(this);
    return {};
  }
  capabilityReport() {
    return { gpu: null, color: null, playback: null };
  }
  loadProject(project) {
    this.project = project;
    this.loads.push(project);
  }
  resize() {}
  invalidate() {
    this.generation += 1;
  }
  /** What a test's `setCaptionFonts` hook refuses, as the renderer reports refused fonts. */
  setCaptionFonts(fonts) {
    this.fonts = fonts;
    return globalThis.stubEngine.setCaptionFonts?.(fonts) ?? [];
  }
  async render(seconds) {
    // A test may hold a render in flight (`stubEngine.render`), as a slow GPU frame would.
    await globalThis.stubEngine.render?.(this, seconds);
    this.renders.push({ seconds, project: this.project });
    if (this.canvas.drawn !== undefined) this.canvas.drawn += 1;
    return true;
  }
  audible() {
    return false;
  }
  playbackClock(policy, frameRate, rate, tick, now) {
    const custom = globalThis.stubEngine.clock;
    if (custom) return custom(policy, frameRate, rate, tick, now);
    return {
      step: (at) => ({
        tick: Math.max(
          0,
          Math.round(tick + ((at - now) / 1000) * rate * 120000),
        ),
        waitForFrame: false,
        free() {},
      }),
      free() {},
    };
  }
  async dispose() {}
}
