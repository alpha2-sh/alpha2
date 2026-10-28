// What a video container's import records comes from the file: its length when the media element
// does not know it, its kind when it holds only sound, and whether it imports at all when the
// media element refuses it but WebCodecs decodes it. Node has no media element; a stand-in
// reports what Chrome reports, and the container is read as the browser's import reads it.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { ALL_FORMATS, BufferSource, Input } from "mediabunny";

const corpus = new URL("../../../tests/corpus/", import.meta.url);

/**
 * What the stand-in media element reports for the next file: its duration and picture size, and an
 * `error` when its media pipeline refuses the file.
 */
let element = { duration: Infinity, videoWidth: 0, videoHeight: 0 };

class FakeVideo {
  constructor() {
    Object.assign(this, element);
  }
  set src(value) {
    this._src = value;
    // Chrome fires loadeddata for any media it can play, sound alone included, and error for
    // media its pipeline refuses.
    setTimeout(
      () => (this.error ? this.onerror?.() : this.onloadeddata?.()),
      0,
    );
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

/** Runs `body` with a WebCodecs stand-in that answers `supported` for every decoder config. */
async function withVideoDecoder(supported, body) {
  globalThis.VideoDecoder = {
    isConfigSupported: async (config) => ({ supported, config }),
  };
  try {
    return await body();
  } finally {
    delete globalThis.VideoDecoder;
  }
}

/** Chrome's own words for a file its media pipeline cannot decode. */
const pipelineError = {
  code: 3,
  message: "PipelineStatus::PIPELINE_ERROR_DECODE: VDA Error 4",
};

test("a video the media element refuses imports when WebCodecs decodes it", async () => {
  element = {
    duration: NaN,
    videoWidth: 0,
    videoHeight: 0,
    error: pipelineError,
  };
  const bytes = await readFile(new URL("h264-bt709-copy.mp4", corpus));
  const input = new Input({
    source: new BufferSource(bytes),
    formats: ALL_FORMATS,
  });
  const track = await input.getPrimaryVideoTrack();
  const expected = {
    width: track.displayWidth,
    height: track.displayHeight,
    duration: await input.computeDuration(),
  };
  input.dispose();
  const library = new AssetLibrary();
  const asset = await withVideoDecoder(true, async () =>
    library.importFile(new File([bytes], "trimmed.mp4", { type: "video/mp4" })),
  );
  assert.equal(asset.kind, "video");
  assert.equal(asset.video, null, "no media element to fall back on");
  assert.deepEqual(
    { width: asset.width, height: asset.height, duration: asset.duration },
    expected,
  );
  library.dispose();
});

test("a video neither the media element nor WebCodecs reads is refused with the element's reason", async () => {
  element = {
    duration: NaN,
    videoWidth: 0,
    videoHeight: 0,
    error: pipelineError,
  };
  const library = new AssetLibrary();
  const imported = await file("h264-bt709-copy.mp4", "broken.mp4", "video/mp4");
  await withVideoDecoder(false, () =>
    assert.rejects(
      library.importFile(imported),
      (error) =>
        error.code === "unreadable_media" &&
        /PIPELINE_ERROR_DECODE/.test(error.message),
    ),
  );
  library.dispose();
});
