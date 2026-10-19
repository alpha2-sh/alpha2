// A stand-in for `alpha2/audio-worklet`: a node that accepts what the mixer sends it and plays
// nothing.
export async function loadAlpha2AudioWorkletWasm() {
  return {};
}
export class Alpha2AudioWorkletHost {
  static async create() {
    return {
      port: { addEventListener() {}, postMessage() {} },
      connect() {},
      disconnect() {},
    };
  }
}
// The configuration functions resolve once the processor has applied them, as the real ones do.
export async function useAudioRing() {}
export async function configureEngineMixedAudio() {}
export async function resetAudioWorklet() {}
export function disposeAudioWorklet() {}
