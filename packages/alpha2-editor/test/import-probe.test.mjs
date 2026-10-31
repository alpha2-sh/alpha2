// Node has no media element; a stand-in reports what Chrome reports, and the container is read as
// the browser's import reads it.
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

let element = { duration: Infinity, videoWidth: 0, videoHeight: 0 };

class FakeVideo {
  constructor() {
    Object.assign(this, element);
  }
  set src(value) {
    this._src = value;
    // Chrome fires loadeddata for any playable media, sound alone included, and error for media its
    // pipeline refuses.
    setTimeout(
      () => (this.error ? this.onerror?.() : this.onloadeddata?.()),
      0,
    );
  }
  get src() {
    return this._src;
  }
  cloneNode() {
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
const drawn = [];

const { AssetLibrary } = await import("../dist/browser/runtime/assets.js");

const file = async (name, as, type) =>
  new File([await readFile(new URL(name, corpus))], as, { type });

test("a video whose element reports no end records the container's length", async () => {
  // A WebM written in chunks by MediaRecorder has no Duration, so Chrome reports +Infinity.
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
  // The browser types by extension: an AAC voice-over saved as .mp4 is `video/mp4`.
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

class TestVideoFrame {
  constructor(source, init = {}) {
    this.timestamp = init.timestamp ?? source.timestamp;
    this.duration = init.duration ?? source.duration ?? null;
    this.format =
      "format" in init
        ? init.format
        : "format" in source
          ? source.format
          : "I420";
    this.codedWidth = this.displayWidth = source.codedWidth ?? 64;
    this.codedHeight = this.displayHeight = source.codedHeight ?? 36;
    this.closed = false;
  }
  allocationSize() {
    if (this.format === null)
      throw new DOMException("opaque frame", "NotSupportedError");
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
 * The stand-in H.264 decoder: `"decodes"` every packet, `"opaque"` gives frames with no format,
 * `"fails"` on the first packet, or `"unsupported"` refuses the config.
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
            ...(webCodecs.mode === "opaque" ? { format: null } : {}),
          },
        ),
      ),
    );
  }
  flush() {}
  close() {}
}
registerDecoder(StandInDecoder);

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

test("media error text is not shown to the user", async () => {
  element = {
    duration: NaN,
    videoWidth: 0,
    videoHeight: 0,
    error: pipelineError,
  };
  const library = new AssetLibrary();
  const imported = await file("h264-bt709-copy.mp4", "broken.mp4", "video/mp4");
  await withVideoDecoder("unsupported", () =>
    assert.rejects(library.importFile(imported), (error) => {
      assert.equal(error.code, "unreadable_media");
      assert.match(
        error.message,
        /^Cannot read broken\.mp4 as video: this browser cannot decode H\.264 video/,
      );
      assert.doesNotMatch(error.message, /PIPELINE|VDA/);
      assert.equal(error.cause, pipelineError);
      return true;
    }),
  );
  library.dispose();
});

test("hdr source without planes is refused at import with a typed cause", async () => {
  // Chrome on a Mac decodes some HLG streams only to opaque hardware frames with no software
  // decoder, so the file is refused by name at import.
  element = { duration: 0.2, videoWidth: 64, videoHeight: 48 };
  const library = new AssetLibrary();
  const imported = await file("hdr-hlg.mp4", "clip.mp4", "video/mp4");
  await withVideoDecoder("opaque", () =>
    assert.rejects(library.importFile(imported), (error) => {
      assert.equal(error.code, "unsupported_media");
      assert.equal(error.cause?.code, "PLANAR_DECODE_UNAVAILABLE");
      assert.equal(error.cause.facts.transfer, "hlg");
      assert.equal(error.cause.facts.softwareDecoder, "none");
      assert.match(
        error.message,
        /^clip\.mp4 is HDR video \(H\.264, 10-bit, HLG\)/,
      );
      assert.equal(error.failureCode, "INVALID_INPUT");
      return true;
    }),
  );
  const asset = await withVideoDecoder("decodes", () =>
    library.importFile(imported),
  );
  assert.equal(asset.kind, "video");
  library.dispose();
});

test("an hdr source without planes imports for a degraded preview when the library takes one", async () => {
  element = { duration: 0.2, videoWidth: 64, videoHeight: 48 };
  const library = new AssetLibrary({ hdrWithoutPlanes: "preview-degraded" });
  const imported = await file("hdr-hlg.mp4", "clip.mp4", "video/mp4");
  const asset = await withVideoDecoder("opaque", () =>
    library.importFile(imported),
  );
  assert.equal(asset.kind, "video");
  assert.equal(asset.planarDecode, "unavailable");
  await withVideoDecoder("unsupported", () =>
    assert.rejects(library.importFile(imported), { code: "unsupported_media" }),
  );
  assert.throws(
    () => new AssetLibrary({ hdrWithoutPlanes: "export" }),
    RangeError,
  );
  library.dispose();
});

test("a video the media element refuses and WebCodecs claims but cannot decode is refused naming both", async () => {
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
        !/PIPELINE_ERROR_DECODE/.test(error.message) &&
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
