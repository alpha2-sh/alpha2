// The wasm binding the editor's renderer module sees in tests (registry-hook.mjs): a registry that
// records what it is asked, and refuses a LUT as the engine's parser does when the test says so
// (`globalThis.fakeRegistry.refuseLut(id, text)` returns a reason or nothing).
globalThis.fakeRegistry ??= {
  registries: [],
  refuseLut: () => undefined,
  refuseEffect: () => undefined,
};

class FakeRegistry {
  constructor(canvas) {
    this.canvas = canvas;
    this.calls = [];
    this.luts = new Map();
    this.effects = new Set();
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
  /**
   * Compiles an effect, refused as the engine refuses one when the test's `refuseEffect(definition)`
   * gives a reason: `before` throws as a schema refusal does, anything else rejects as the shader's
   * diagnostics do. The compiled effect records whether it was freed.
   */
  compileEffect(json) {
    const definition = JSON.parse(json);
    const refused = globalThis.fakeRegistry.refuseEffect(definition);
    if (refused === "before") throw new Error("schema refused");
    const compiled = {
      key: `${definition.id}@${definition.version}`,
      freed: false,
      free() {
        compiled.freed = true;
      },
    };
    this.compiled = [...(this.compiled ?? []), compiled];
    return refused
      ? Promise.reject(new Error(refused))
      : Promise.resolve(compiled);
  }
  registerCompiledEffect(compiled) {
    if (this.effectLimit !== undefined && this.effects.size >= this.effectLimit)
      throw new Error("effect registry is full");
    this.effects.add(compiled.key);
    this.calls.push(["registerCompiledEffect", compiled.key]);
    return compiled.key;
  }
  unregisterEffect(id, version) {
    this.calls.push(["unregisterEffect", `${id}@${version}`]);
    return this.effects.delete(`${id}@${version}`);
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
