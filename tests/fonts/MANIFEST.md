# Font fixture manifest

| File                           | SHA-256                                                            | Coverage                                                                  |
| ------------------------------ | ------------------------------------------------------------------ | ------------------------------------------------------------------------- |
| `Alpha2Fixture.ttf`            | `690243adfefe0ce154b547db6205794bd30ac4277275179517a90994f4980648` | Latin, Hebrew, Arabic, combining marks, symbols; no Hangul                |
| `Alpha2FixtureHangul.ttf`      | `fb906f242aebdf798957e7b49cd5f90a4acc7a7e654efc8742a81a32380a8d8b` | 29 Hangul syllables (`font-hangul.sh`); nothing else, not even a space    |
| `Alpha2FixtureEmojiColr.ttf`   | `79579ebfc3f23b08b357affedb501a0eae1a75396417baa7a808317903da1ccb` | Noto Color Emoji COLRv1: a few emoji and sequences (`font-emoji.sh`)      |
| `Alpha2FixtureEmojiBitmap.ttf` | `19cbde7d85defc89ae97a9c529f28a1b4b2e4ad476aea9da84835af7bb7616ad` | Noto Color Emoji CBDT: the same emoji and sequences (`font-emoji.sh`)     |
| `Alpha2FixtureColor.ttf`       | `63f73302a126e400dd7dfead23f4491d879e75334d405f3b7e55294ad439d046` | COLR glyphs at U+E000 to U+E005, one a kind of painting (`font-color.py`) |
| `Alpha2FixtureVariable.ttf`    | `49db52dd4e76a4ac6f5e376de96cacd7d04b48bd9f4c29bf38df2ceb525b0259` | "H", "I", space; a `wght` axis 100 to 900, default 400 (`font-weights.py`) |
| `Alpha2FixturePair.ttc`        | `5bb0557d3d50fceee598ebdbe7887a2ac9a836eac69aacf51ccf09d4d3bf1d97` | Regular (400) and Bold (700) faces of "H", "I", space (`font-weights.py`)  |

The text golden (`tests/golden/text_raster.json`) registers them all in this order, the DejaVu fixture first, and pins exact layout metrics and a pixel hash for each case; `npm run test:wasm-parity` and the browser `text-raster-parity` page hold the wasm build to the same pins.
