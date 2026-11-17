# alpha2-extensions

Extension providers that supply frames of formats the host decoders do not read: SVG, animated images, Lottie, OpenEXR, image sequences, LaTeX math, PDF pages.

Part of [alpha2](https://github.com/alpha2-sh/alpha2), a media engine that renders timeline projects on the GPU. `alpha2-host` reads these formats with it, and the browser with the same code (`alpha2-extensions-web`).

- [Documentation](https://docs.rs/alpha2-extensions)
- [Repository](https://github.com/alpha2-sh/alpha2)

## Source limits

Every provider refuses a source past a limit with a typed error before it does the work: `Budget` (shown as `too large: ...`, naming the limit) or `Malformed`. Native hosts and the browser module run the same code, so they refuse the same files. The limits model the exact converter versions this crate requires.

| Applies to | Limit | Refusal |
| --- | --- | --- |
| Every source | canvas at most 16,384 a side and 7680x4320 in all | `Budget` |
| Every source | drawing one frame costs at most 256 canvases of blended fill (a canvas counts as at least 1920x1080), counting area, path edges, edges that share rows and dashes | `Budget`, naming the costliest part |
| Every source | images one source decodes: 4 canvases of pixels and 256 MiB of image bytes | `Budget` |
| Every source | pixels held at once (filter results, offscreen layers): 2^28 | `Budget` |
| Animated images | 10,000 frames and 2^31 pixels in all | `Budget` |
| SVG | 32 MiB of text with its nested SVG images and expanded entities; entities 10 deep | `Budget`; an entity loop is `Malformed` |
| SVG | 4,000,000 parsed nodes, 1,000,000 converted elements, 256 levels deep (a paint server, mask, clip path or filter reference counts 4) | `Budget` |
| SVG | 2^27 reading steps (ancestry, `use` copies, markers, arcs, text layout, style matching), a selector reaching at most 512 elements | `Budget` |
| SVG | a paint server, mask, clip path, filter or template that refers to itself | `Malformed` |
| SVG | 4,000,000 drawn nodes; coordinates within 262,144 px of what they are drawn onto (2^25 along an axis, 2^30 for layers and filter regions) | `Budget` |
| Lottie | 16 MiB animation; a dotLottie package of 256 MiB, 1,024 files and a 1 MiB directory | `Budget` |
| Lottie | 100,000 draws, 512 offscreen layers, 64 levels, 1,000 repeater copies and polystar points; nested layers holding at most 2^28 pixels at the size drawn | `Budget` |
| Lottie | a shape whose type is not a name | `Malformed` |
| PDF | 262,144 paths, glyphs and images per page, every form, pattern, soft mask and Type 3 glyph invocation counted; content streams inflating to 32 MiB together; streams 256 deep | `Budget`; a form that invokes itself is `Budget` |
| PDF | image size from the codec's own header (JPEG, JPEG 2000, JBIG2, CCITT); 256 MiB of image data per draw | `Budget`; a CCITT image without a row count is `Budget` |
| OpenEXR | the largest block of a part at most 256 MiB | `Budget` |
| OpenEXR | a second channel list | `Malformed` |
| LaTeX | 4,096 bytes of formula | `Budget` |
| LaTeX | a font size not above 0 or past 1,024 | `Malformed` |

## License

MIT, copyright Alpha2 contributors.
