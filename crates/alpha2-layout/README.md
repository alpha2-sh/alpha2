# alpha2-layout

Where alpha2 text and layout boxes lie, measured without a GPU.

Part of [alpha2](https://github.com/alpha2-sh/alpha2), a media engine that renders timeline projects on the GPU. It turns a text style or a layout generator's box tree into the requests the text host draws and the rectangles they fill, laid out by CSS flexbox and grid with the fonts the engine draws with, so a measurement and a frame place text alike on every target. Most programs use it through the [`alpha2`](https://crates.io/crates/alpha2) crate.

- [Documentation](https://docs.rs/alpha2-layout)
- [Repository](https://github.com/alpha2-sh/alpha2)

## License

MIT, copyright Alpha2 contributors.
