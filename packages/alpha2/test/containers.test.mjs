import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";

import {
  EXPORT_CONTAINERS,
  ExportFormatError,
  containerFromExtension,
  containerPolicy,
  defaultVideoCodec,
  offeredAudioCodecs,
  resolveEngineExportFormat,
  resolveExportFormat,
  resolvePixels,
} from "../dist/containers.js";

const corpus = JSON.parse(
  readFileSync(
    resolve(
      import.meta.dirname,
      "../../../tests/cases/export-container-corpus.json",
    ),
    "utf8",
  ),
);

const browserCodec = (codec) => (codec === "h264" ? "avc" : codec);
const engineCodec = (codec) => (codec === "avc" ? "h264" : codec);

test("the container policy matches the shared corpus", () => {
  assert.deepEqual(
    EXPORT_CONTAINERS.map((policy) => policy.id),
    corpus.containers.map((entry) => entry.id),
  );
  for (const entry of corpus.containers) {
    const policy = containerPolicy(entry.id);
    assert.deepEqual(policy.extensions, entry.extensions);
    assert.equal(policy.mimeType, entry.mime);
    assert.deepEqual(policy.video, entry.video);
    assert.deepEqual(policy.audio, entry.audio);
    assert.equal(policy.video[0], entry.defaultVideoCodec);
    assert.equal(policy.audio[0], entry.defaultAudioCodec);
    assert.equal(
      engineCodec(defaultVideoCodec(entry.id)),
      entry.defaultVideoCodec,
    );
    assert.deepEqual(offeredAudioCodecs(entry.id), entry.audio);
  }
});

test("every resolve case matches the shared corpus", () => {
  for (const entry of corpus.resolve) {
    const request = {
      ...(entry.container === null ? {} : { container: entry.container }),
      ...(entry.extension === null ? {} : { extension: entry.extension }),
      ...(entry.video === null
        ? {}
        : { videoCodec: browserCodec(entry.video) }),
      ...(entry.audio === null ? {} : { audioCodec: entry.audio }),
    };
    if (entry.ok) {
      const format = resolveExportFormat(request);
      assert.deepEqual(
        [format.container, engineCodec(format.videoCodec), format.audioCodec],
        entry.ok,
        JSON.stringify(entry),
      );
    } else {
      assert.throws(
        () => resolveExportFormat(request),
        (error) =>
          error instanceof ExportFormatError &&
          error instanceof RangeError &&
          error.reason === entry.error,
        JSON.stringify(entry),
      );
    }
  }
});

test("every resolve case matches the shared corpus in the engine's spelling", () => {
  for (const entry of corpus.resolve) {
    const request = Object.fromEntries(
      [
        ["container", entry.container],
        ["extension", entry.extension],
        ["videoCodec", entry.video],
        ["audioCodec", entry.audio],
      ].filter(([, value]) => value !== null),
    );
    if (entry.ok) {
      const format = resolveEngineExportFormat(request);
      assert.deepEqual(
        [format.container, format.videoCodec, format.audioCodec],
        entry.ok,
        JSON.stringify(entry),
      );
    } else {
      assert.throws(
        () => resolveEngineExportFormat(request),
        (error) =>
          error instanceof ExportFormatError && error.reason === entry.error,
        JSON.stringify(entry),
      );
    }
  }
  // MPEG-4 Part 2 has no browser encoder but is the engine's to offer.
  assert.equal(
    resolveEngineExportFormat({ container: "mov", videoCodec: "mpeg4-part2" })
      .videoCodec,
    "mpeg4-part2",
  );
});

test("extensions name containers case-insensitively", () => {
  assert.equal(containerFromExtension("MOV"), "mov");
  assert.equal(containerFromExtension("Webm"), "webm");
  assert.equal(containerFromExtension(""), undefined);
  assert.equal(containerFromExtension("mp4.partial"), undefined);
  assert.throws(() => containerPolicy("matroska"), {
    name: "ExportFormatError",
    message:
      'unknown container "matroska"; expected one of mp4, mov, mkv, webm',
  });
});

test("refusals name what the container offers", () => {
  assert.throws(
    () => resolveExportFormat({ container: "webm", videoCodec: "avc" }),
    { message: "webm does not offer video codec h264; it offers vp9, av1" },
  );
  assert.throws(
    () => resolveExportFormat({ container: "mov", audioCodec: "opus" }),
    { message: "mov does not offer audio codec opus; it offers aac" },
  );
});

test("the pixel policy matches the shared corpus", () => {
  for (const entry of corpus.pixels) {
    const request = {
      ...(entry.bitDepth === null ? {} : { bitDepth: entry.bitDepth }),
      alpha: entry.alpha,
    };
    if (entry.ok !== undefined) {
      assert.equal(
        resolvePixels(entry.codec, entry.container, request).format,
        entry.ok,
        JSON.stringify(entry),
      );
    } else {
      assert.throws(
        () => resolvePixels(entry.codec, entry.container, request),
        (error) =>
          error instanceof ExportFormatError && error.reason === entry.error,
        JSON.stringify(entry),
      );
    }
  }
});
