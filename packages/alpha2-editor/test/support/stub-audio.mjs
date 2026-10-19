// The bridge's audio module for preview tests: the real one, with each mixer the bridge makes
// recorded in `globalThis.stubEngine.mixers`.
import { EngineAudioMixer as RealMixer } from "../../dist/browser/runtime/audio.js";

export * from "../../dist/browser/runtime/audio.js";

export class EngineAudioMixer extends RealMixer {
  constructor(...args) {
    super(...args);
    globalThis.stubEngine.mixers.push(this);
  }
}
