// What a video container's import records comes from the file: its length when the media element
// does not know it, and its kind when it holds only sound. Node has no media element; a stand-in
// reports what Chrome reports, and the container is read as the browser's import reads it.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const corpus = new URL("../../../tests/corpus/", import.meta.url);

/** What the stand-in media element reports for the next file: its duration and picture size. */
let element = { duration: Infinity, videoWidth: 0, videoHeight: 0 };

class FakeVideo {
  constructor() {
    Object.assign(this, element);
  }
  set src(value) {
    this._src = value;
    // Chrome fires loadeddata for any media it can play, sound alone included.
    setTimeout(() => this.onloadeddata?.(), 0);
  }
  get src() {
    return this._src;
  }
  cloneNode() {
    // The thumbnail is not looked at here: a clone that never loads makes none.
    return {
      load() {
        this.onerror?.();
      },
      removeAttribute() {},
    };
  }
  removeAttribute() {}
  pause() {}
  load() {}
}
globalThis.document = {
  createElement: (kind) =>
    kind === "video"
      ? new FakeVideo()
      : { getContext: () => ({ drawImage() {} }), toDataURL: () => "data:" },
};

const { AssetLibrary } = await import("../dist/browser/runtime/assets.js");

const file = async (name, as, type) =>
  new File([await readFile(new URL(name, corpus))], as, { type });

test("a video whose element reports no end records the container's length", async () => {
  // A WebM written in chunks by MediaRecorder has no Duration: Chrome reports +Infinity.
  element = { duration: Infinity, videoWidth: 64, videoHeight: 36 };
  const library = new AssetLibrary();
  const asset = await library.importFile(
    await file("h264-bt709-copy.mp4", "recording.mp4", "video/mp4"),
  );
  assert.equal(asset.kind, "video");
  assert.ok(
    Number.isFinite(asset.duration) && asset.duration > 0.5,
    `duration ${asset.duration}`,
  );
  library.dispose();
});

test("sound alone in a video container imports as audio", async () => {
  // The browser names the type by the extension: an AAC voice-over saved as .mp4 is `video/mp4`.
  element = { duration: 1, videoWidth: 0, videoHeight: 0 };
  const library = new AssetLibrary();
  const asset = await library.importFile(
    await file("audio-only-aac-priming.m4a", "voiceover.mp4", "video/mp4"),
  );
  assert.equal(asset.kind, "audio");
  assert.equal(asset.hasAudio, true);
  assert.equal(asset.width, undefined, "no picture size is made up");
  assert.ok(asset.duration > 0, `duration ${asset.duration}`);
  library.dispose();
});
