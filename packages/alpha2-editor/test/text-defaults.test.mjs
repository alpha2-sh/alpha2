import test from "node:test";
import assert from "node:assert/strict";
import { TEXT_DEFAULTS } from "../dist/index.js";

const { AssetLibrary } = await import("../dist/browser/runtime/assets.js");

test("a title without a line height is drawn at TEXT_DEFAULTS.lineHeight", () => {
  const drawn = [];
  class Library extends AssetLibrary {
    rasterizeText(asset, text) {
      asset.text = { ...asset.text, ...text };
      drawn.push(asset.text);
    }
  }
  const library = new Library();
  const asset = library.createText({
    content: "Two\nlines",
    fontSize: 72,
    color: "#ffffff",
    fontFamily: "sans-serif",
    align: "center",
  });
  assert.equal(asset.text.lineHeight, TEXT_DEFAULTS.lineHeight);
  assert.equal(drawn.at(-1).lineHeight, TEXT_DEFAULTS.lineHeight);
  assert.equal(asset.text.maxWidth, TEXT_DEFAULTS.maxWidth);
  assert.equal(
    library.createText({ content: "x", lineHeight: 2 }).text.lineHeight,
    2,
  );
  assert.ok(Object.isFrozen(TEXT_DEFAULTS));
});
