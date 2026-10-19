# Font fixture manifest

| File                      | SHA-256                                                            | Coverage                                                                   |
| ------------------------- | ------------------------------------------------------------------ | -------------------------------------------------------------------------- |
| `Alpha2Fixture.ttf`       | `690243adfefe0ce154b547db6205794bd30ac4277275179517a90994f4980648` | Latin, Hebrew, Arabic, combining marks, symbols; no Hangul                 |
| `Alpha2FixtureHangul.ttf` | `fb906f242aebdf798957e7b49cd5f90a4acc7a7e654efc8742a81a32380a8d8b` | 29 Hangul syllables (`font-hangul.sh`); nothing else, not even a space    |

The text golden (`tests/golden/text_raster.json`) registers both, the DejaVu fixture first, and pins exact layout metrics and a pixel hash for each case; `npm run test:wasm-parity` and the browser `text-raster-parity` page hold the wasm build to the same pins.
