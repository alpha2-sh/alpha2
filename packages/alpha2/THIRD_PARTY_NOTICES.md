# Third-party notices

alpha2 is under the MIT License (`LICENSE.md`). It also contains third-party work, under the terms below.

## Rust crates

The `alpha2` npm package and the native archives each list the third-party Rust crates compiled into them, with their license texts: `wasm/rust-crate-licenses.md` in the package, `licenses/rust-crate-licenses.md` in each archive.

## FFmpeg

The native archives load FFmpeg 8, built as shared libraries under the LGPL 2.1 or later with its GPL and nonfree components disabled. Each archive carries the license texts of FFmpeg and the libraries it is built with in `licenses/`, and says where to get the corresponding source in `compliance/SOURCE.md`.

## gl-transitions

The transitions in `crates/alpha2-shaders/shaders/runtime/transitions/` are WGSL ports of shaders from [gl-transitions](https://github.com/gl-transitions/gl-transitions) at commit `902218a1b63773ac0d0d9f491951da3392365bfe`. Each file names the shader it ports and its author.

Copyright (c) 2017-present gl-transitions contributors.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
