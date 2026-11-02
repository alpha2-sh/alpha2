// The renderer a preview test's bridge makes, recording what it loads and draws. Tests steer it
// through `globalThis.stubEngine` (see preview-stubs.mjs).
export class Engine {
  constructor(canvas, sources, library, runtimeAssets = {}, options = {}) {
    this.canvas = canvas;
    this.options = options;
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
    const renderer =
      globalThis.stubEngine.renderer?.(this) ??
      (this.options.renderer === "cpu" ? "cpu" : "gpu");
    this.rendererChoice = {
      renderer,
      rendererReason: renderer === "cpu" ? "stub CPU renderer" : null,
      backend: renderer === "cpu" ? "cpu" : "webgpu",
      adapter: null,
    };
    return {};
  }
  lossReport() {
    return this.deviceLost ? this.report : undefined;
  }
  capabilityReport() {
    const choice = this.rendererChoice;
    return {
      gpu: null,
      color: null,
      playback: null,
      renderer: choice && {
        ...choice,
        runtimeEffects: choice.renderer === "gpu",
      },
    };
  }
  loadProject(project) {
    this.project = project;
    this.loads.push(project);
  }
  /** The runtime effects the test's `drawnEffects` hook says the loaded project draws. */
  projectRuntimeEffects() {
    return globalThis.stubEngine.drawnEffects?.(this) ?? [];
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
   * A test's `registerEffects` hook may throw to refuse the whole call, as registration is all or
   * none.
   */
  async registerEffects(definitions, operation = "preview") {
    await globalThis.stubEngine.registerEffects?.(this, definitions, operation);
    this.effects.push(...definitions.map((d) => `${d.id}@${d.version}`));
  }
  setCaptionFonts(fonts) {
    this.fonts = fonts;
    return globalThis.stubEngine.setCaptionFonts?.(fonts) ?? [];
  }
  async render(seconds) {
    await globalThis.stubEngine.render?.(this, seconds);
    const unchanged = globalThis.stubEngine.unchanged?.(this, seconds) === true;
    this.renders.push({ seconds, project: this.project, unchanged });
    if (!unchanged && this.canvas.drawn !== undefined) this.canvas.drawn += 1;
    const degradedSources =
      globalThis.stubEngine.degraded?.(this, seconds) ?? [];
    return { rendered: true, unchanged, degradedSources };
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
