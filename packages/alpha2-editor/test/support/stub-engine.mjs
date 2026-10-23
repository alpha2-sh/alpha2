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
    this.effects = [];
    this.renders = [];
    this.catalog = null;
    this.lastDiagnostics = null;
    this.previewScale = 0.5;
    this.passTiming = false;
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
  resize(width, height) {
    this.canvas.width = width;
    this.canvas.height = height;
  }
  setPreviewScale(scale) {
    this.previewScale = scale;
    return this.previewSize;
  }
  get previewSize() {
    if (!this.project) return null;
    const { width, height } = this.project.settings;
    return [
      Math.round(width * this.previewScale),
      Math.round(height * this.previewScale),
    ];
  }
  setPassTiming(enabled) {
    this.passTiming = enabled;
  }
  invalidate() {
    this.generation += 1;
  }
  /**
   * Records each call's definitions; a test's `registerEffects` hook may refuse a call by throwing,
   * and then nothing of it is registered, as the renderer's registration is all or none.
   */
  async registerEffects(definitions, operation = "preview") {
    await globalThis.stubEngine.registerEffects?.(this, definitions, operation);
    this.effects.push(...definitions.map((d) => `${d.id}@${d.version}`));
  }
  /** What a test's `setCaptionFonts` hook refuses, as the renderer reports refused fonts. */
  setCaptionFonts(fonts) {
    this.fonts = fonts;
    return globalThis.stubEngine.setCaptionFonts?.(fonts) ?? [];
  }
  async render(seconds) {
    // A test may hold a render in flight (`stubEngine.render`), as a slow GPU frame would.
    await globalThis.stubEngine.render?.(this, seconds);
    // A test may say a frame equals the one shown (`stubEngine.unchanged`): nothing is drawn.
    const unchanged = globalThis.stubEngine.unchanged?.(this, seconds) === true;
    this.renders.push({ seconds, project: this.project, unchanged });
    if (!unchanged && this.canvas.drawn !== undefined) this.canvas.drawn += 1;
    return { rendered: true, unchanged };
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
