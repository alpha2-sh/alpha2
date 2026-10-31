// The editor reads a user's text files (subtitles, LUTs) by the rule the host and the Python client
// use: UTF-8 with or without a byte order mark, UTF-16 behind its mark, nothing else.
import test from "node:test";
import assert from "node:assert/strict";

const { readUserText } = await import("../dist/browser/runtime/text.js");

const file = (bytes, name = "captions.srt") =>
  new File([new Uint8Array(bytes)], name);
const utf8 = (text) => [...new TextEncoder().encode(text)];
const utf16 = (text, littleEndian) =>
  [...text].flatMap((character) => {
    const unit = character.charCodeAt(0);
    return littleEndian ? [unit & 0xff, unit >> 8] : [unit >> 8, unit & 0xff];
  });

test("UTF-8 with or without a byte order mark and UTF-16 behind its mark are read", async () => {
  // Hangul, as the test data of a caption.
  const text = "1\n\uc790\ub9c9\n";
  assert.equal(await readUserText(file(utf8(text))), text);
  assert.equal(
    await readUserText(file([0xef, 0xbb, 0xbf, ...utf8(text)])),
    text,
  );
  assert.equal(
    await readUserText(file([0xff, 0xfe, ...utf16(text, true)])),
    text,
  );
  assert.equal(
    await readUserText(file([0xfe, 0xff, ...utf16(text, false)])),
    text,
  );
});

test("a legacy code page or a broken UTF-16 file is refused naming the file", async () => {
  // The Hangul syllable U+D55C in cp949, which is no UTF-8.
  for (const bytes of [
    [0x31, 0x0a, 0xc7, 0xd1, 0x0a],
    [0xff, 0xfe, 0x00, 0xd8],
  ]) {
    await assert.rejects(readUserText(file(bytes, "old.srt")), (error) => {
      assert.equal(error.name, "EditorError");
      assert.equal(error.code, "invalid_text");
      assert.match(error.message, /old\.srt is not UTF-8 text/);
      return true;
    });
  }
});
