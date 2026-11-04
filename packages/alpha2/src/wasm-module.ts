/**
 * Loads a wasm-bindgen module once per realm and replaces its instance after a trap, always
 * asynchronously: browsers refuse to compile or instantiate a large module synchronously on the
 * main thread.
 */

/** The parts of wasm-bindgen `web` glue a {@link WasmModule} uses. */
export interface WasmBindgenGlue {
  /** The glue's asynchronous init; it reuses a finished instance. */
  default(input?: { module_or_path: string | URL }): Promise<unknown>;
  /**
   * Replaces the instance from the compiled module, invalidating objects made before. Present
   * when the glue is built to restart (the extension provider module).
   */
  __wbg_reset_state_async?(): Promise<void>;
}

/**
 * One wasm-bindgen module. `importGlue` imports its glue, so a bundler sees a static specifier.
 */
export class WasmModule<Glue extends WasmBindgenGlue> {
  readonly #importGlue: () => Promise<Glue>;
  #loading: Promise<Glue> | null = null;
  #glue: Glue | null = null;

  constructor(importGlue: () => Promise<Glue>) {
    this.#importGlue = importGlue;
  }

  /**
   * The loaded glue. `wasmUrl` locates the `.wasm` file when a bundler moved it; it applies to the
   * first load only. Concurrent calls share one load, and a failed load is tried again by the next
   * call.
   *
   * @throws When the module cannot be fetched, compiled or instantiated.
   */
  load(wasmUrl?: string | URL): Promise<Glue> {
    this.#loading ??= (async () => {
      const glue = await this.#importGlue();
      await glue.default(
        wasmUrl === undefined ? undefined : { module_or_path: wasmUrl },
      );
      this.#glue = glue;
      return glue;
    })().catch((error: unknown) => {
      this.#loading = null;
      throw error;
    });
    return this.#loading;
  }

  /**
   * Replaces the instance with a new one from the compiled module. Objects made from the old
   * instance throw when used.
   *
   * @throws When the module is not loaded, its glue cannot restart, or instantiation fails.
   */
  async restart(): Promise<void> {
    const glue = this.#glue;
    if (glue === null) throw new Error("the wasm module is not loaded");
    if (glue.__wbg_reset_state_async === undefined)
      throw new Error("the wasm module is not built to restart");
    await glue.__wbg_reset_state_async();
  }
}
