// The wasm binding the editor's renderer module sees in tests (registry-hook.mjs): a registry that
// records what it is asked, and refuses a LUT as the engine's parser does when the test says so
// (`globalThis.fakeRegistry.refuseLut(id, text)` returns a reason or nothing).
globalThis.fakeRegistry ??= { registries: [], refuseLut: () => undefined };

class FakeRegistry {
  constructor(canvas) {
    this.canvas = canvas;
    this.calls = [];
    this.luts = new Map();
    this.budgets = [];
    globalThis.fakeRegistry.registries.push(this);
  }
  static async createForCanvas(canvas) {
    return new FakeRegistry(canvas);
  }
  static async createForHtmlCanvas(canvas) {
    return new FakeRegistry(canvas);
  }
  builtinCatalog() {
    return JSON.stringify({ effects: [], transitions: [] });
  }
  setResultCacheBudget(bytes) {
    this.budgets.push(bytes);
  }
  resizeSurface(width, height) {
    this.calls.push(["resizeSurface", width, height]);
  }
  registerLut3d(id, text) {
    const refused = globalThis.fakeRegistry.refuseLut(id, text);
    if (refused) throw new Error(refused);
    this.luts.set(id, text);
    this.calls.push(["registerLut3d", id]);
  }
  unregisterEffectResource(id) {
    this.calls.push(["unregisterEffectResource", id]);
    return this.luts.delete(id);
  }
  loadProject(json) {
    this.calls.push(["loadProject", JSON.parse(json)]);
  }
  resetPlayer() {}
  free() {}
}

export const WebEffectRegistry = FakeRegistry;
export class PlaybackClock {}
export async function loadEngineWasm() {}
