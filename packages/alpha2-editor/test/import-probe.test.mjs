// What a video container's import records comes from the file: its length when the media element
// does not know it, its kind when it holds only sound, and whether it imports at all when the
// media element refuses it but WebCodecs decodes it. Node has no media element; a stand-in
// reports what Chrome reports, and the container is read as the browser's import reads it.
import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  ALL_FORMATS,
  BufferSource,
  CustomVideoDecoder,
  Input,
  VideoSample,
  registerDecoder,
} from "mediabunny";

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
      : {
          getContext: () => ({
            drawImage: (image) => drawn.push(image),
            translate() {},
            rotate() {},
            scale() {},
          }),
          toDataURL: () => "data:",
        },
};
/** What thumbnails were drawn from. */
const drawn = [];

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

/** Node has no WebCodecs: a frame with what the decoder adapter reads, and no pixels. */
class TestVideoFrame {
  constructor(source, init = {}) {
    this.timestamp = init.timestamp ?? source.timestamp;
    this.duration = init.duration ?? source.duration ?? null;
    this.format = "I420";
    this.codedWidth = this.displayWidth = source.codedWidth ?? 64;
    this.codedHeight = this.displayHeight = source.codedHeight ?? 36;
    this.closed = false;
  }
  allocationSize() {
    return this.codedWidth * this.codedHeight * 1.5;
  }
  clone() {
    return new TestVideoFrame(this);
  }
  close() {
    this.closed = true;
  }
}
globalThis.VideoFrame = TestVideoFrame;

/**
 * How the H.264 decoder WebCodecs stands in with behaves: `"decodes"` every packet, `"fails"` on
 * the first, as a decoder fails on a damaged stream, or `"unsupported"`, the config refused.
 */
const webCodecs = { mode: "unsupported" };

class StandInDecoder extends CustomVideoDecoder {
  static supports(codec) {
    return webCodecs.mode !== "unsupported" && codec === "avc";
  }
  init() {}
  decode(packet) {
    if (webCodecs.mode === "fails") {
      this.onError(new DOMException("Decoding error.", "EncodingError"));
      return;
    }
    this.onSample(
      new VideoSample(
        new TestVideoFrame(
          {},
          {
            timestamp: Math.round(packet.timestamp * 1e6),
            duration: Math.round(packet.duration * 1e6),
          },
        ),
      ),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(StandInDecoder);

/** Runs `body` with WebCodecs decoding H.264 as `mode` says. */
async function withVideoDecoder(mode, body) {
  webCodecs.mode = mode;
  globalThis.VideoDecoder = {
    isConfigSupported: async (config) => ({ supported: false, config }),
  };
  try {
    return await body();
  } finally {
    webCodecs.mode = "unsupported";
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
  drawn.length = 0;
  const asset = await withVideoDecoder("decodes", async () =>
    library.importFile(new File([bytes], "trimmed.mp4", { type: "video/mp4" })),
  );
  assert.equal(asset.kind, "video");
  assert.equal(asset.video, null, "no media element to fall back on");
  assert.equal(drawn.length, 1, "a thumbnail of the decoded picture");
  assert.ok(drawn[0] instanceof TestVideoFrame && drawn[0].closed);
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
  await withVideoDecoder("unsupported", () =>
    assert.rejects(
      library.importFile(imported),
      (error) =>
        error.code === "unreadable_media" &&
        /PIPELINE_ERROR_DECODE/.test(error.message),
    ),
  );
  library.dispose();
});

test("a video the media element refuses and WebCodecs claims but cannot decode is refused with both reasons", async () => {
  element = {
    duration: NaN,
    videoWidth: 0,
    videoHeight: 0,
    error: pipelineError,
  };
  const library = new AssetLibrary();
  const imported = await file(
    "h264-bt709-copy.mp4",
    "damaged.mp4",
    "video/mp4",
  );
  await withVideoDecoder("fails", () =>
    assert.rejects(
      library.importFile(imported),
      (error) =>
        error.code === "unreadable_media" &&
        /PIPELINE_ERROR_DECODE/.test(error.message) &&
        /WebCodecs failed too/.test(error.message),
    ),
  );
  library.dispose();
});

test("an import whose decode is aborted rejects with the abort's reason", async () => {
  element = {
    duration: NaN,
    videoWidth: 0,
    videoHeight: 0,
    error: pipelineError,
  };
  const library = new AssetLibrary();
  const imported = await file(
    "h264-bt709-copy.mp4",
    "trimmed.mp4",
    "video/mp4",
  );
  const reason = new Error("import cancelled");
  await withVideoDecoder("decodes", () =>
    assert.rejects(
      library.importFile(imported, { signal: AbortSignal.abort(reason) }),
      (error) => error === reason,
    ),
  );
  library.dispose();
});
