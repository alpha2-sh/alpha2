// A stand-in for `alpha2/audio-worklet` that accepts what the mixer sends and plays nothing.
export async function loadAlpha2AudioWorkletWasm() {
  return {};
}
export class Alpha2AudioWorkletHost {
  static latency() {
    return { latencySamples: 0 };
  }
  static async create() {
    return {
      port: { addEventListener() {}, postMessage() {} },
      connect() {},
      disconnect() {},
    };
  }
}
export async function useAudioRing() {}
export async function configureEngineMixedAudio() {}
export async function resetAudioWorklet() {}
export function disposeAudioWorklet() {}
