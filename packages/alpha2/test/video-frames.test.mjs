import test from "node:test";
import assert from "node:assert/strict";

// `honoursLayout: false` reads planes packed from offset 0, ignoring `layout`, as Safari 26 does.
function fakeVideoFrame({ honoursLayout }) {
  return class FakeVideoFrame {
    constructor(pixels, init) {
      this.codedWidth = init.codedWidth;
      this.codedHeight = init.codedHeight;
      this.init = init;
      this.source = pixels;
      this.layout = honoursLayout ? init.layout : undefined;
      if (init.transfer) {
        this.source = pixels.slice();
        structuredClone(pixels.buffer, { transfer: init.transfer });
      }
    }
    allocationSize() {
      return (this.codedWidth * this.codedHeight * 3) / 2;
    }
    async copyTo(out) {
      const planes = sizes(this.codedWidth, this.codedHeight);
      const tight = [];
      let written = 0;
      planes.forEach(([width, height], index) => {
        const { offset, stride } = this.layout?.[index] ?? {
          offset: written,
          stride: width,
        };
        tight.push({ offset: written, stride: width });
        for (let row = 0; row < height; row += 1) {
          const start = offset + row * stride;
          out.set(this.source.subarray(start, start + width), written);
          written += width;
        }
      });
      return tight;
    }
    close() {}
  };
}

function sizes(width, height) {
  return [
    [width, height],
    [width / 2, height / 2],
    [width / 2, height / 2],
  ];
}

// Rows padded to 16 bytes with 0xff, as the engine readback pads them.
function paddedFrame() {
  const [width, height, stride, chromaStride] = [6, 2, 16, 16];
  const pixels = new Uint8Array(stride * height + chromaStride * 2).fill(0xff);
  const expected = [];
  for (let row = 0; row < height; row += 1) {
    for (let column = 0; column < width; column += 1) {
      pixels[row * stride + column] = row * 10 + column;
      expected.push(row * 10 + column);
    }
  }
  for (let plane = 0; plane < 2; plane += 1) {
    for (let column = 0; column < width / 2; column += 1) {
      const value = 100 + plane * 10 + column;
      pixels[stride * height + plane * chromaStride + column] = value;
      expected.push(value);
    }
  }
  const layout = [
    { offset: 0, stride },
    { offset: stride * height, stride: chromaStride },
    { offset: stride * height + chromaStride, stride: chromaStride },
  ];
  return { pixels, layout, width, height, expected: new Uint8Array(expected) };
}

async function load(behaviour, honoursLayout) {
  globalThis.VideoFrame = fakeVideoFrame({ honoursLayout });
  // Fresh module per behaviour because the probe result is cached.
  return import(`../dist/video-frames.js?${behaviour}`);
}

async function picture(frame) {
  const out = new Uint8Array(frame.allocationSize());
  await frame.copyTo(out);
  return out;
}

test("a browser that honours layout reads the padded planes in place", async () => {
  const frames = await load("honours", true);
  assert.equal(await frames.videoFrameHonoursLayout(), true);
  const { pixels, layout, width, height, expected } = paddedFrame();
  const frame = await frames.videoFrameFromI420(pixels, {
    format: "I420",
    codedWidth: width,
    codedHeight: height,
    timestamp: 0,
    layout,
  });
  assert.deepEqual(frame.init.layout, layout);
  assert.equal(frames.videoFrameRepackedBytes(frame), 0);
  assert.deepEqual(await picture(frame), expected);
});

test("a browser that ignores layout gets the planes packed tight first", async () => {
  const frames = await load("ignores", false);
  assert.equal(await frames.videoFrameHonoursLayout(), false);
  const { pixels, layout, width, height, expected } = paddedFrame();
  const frame = await frames.videoFrameFromI420(pixels, {
    format: "I420",
    codedWidth: width,
    codedHeight: height,
    timestamp: 0,
    layout,
    colorSpace: { matrix: "bt709" },
  });
  assert.equal(frame.init.layout, undefined);
  assert.deepEqual(frame.init.colorSpace, { matrix: "bt709" });
  assert.equal(frames.videoFrameRepackedBytes(frame), expected.byteLength);
  assert.deepEqual(await picture(frame), expected);

  const tight = new Uint8Array(expected);
  const same = await frames.videoFrameFromI420(tight, {
    format: "I420",
    codedWidth: width,
    codedHeight: height,
    timestamp: 0,
    layout: [
      { offset: 0, stride: width },
      { offset: width * height, stride: width / 2 },
      { offset: width * height + (width * height) / 4, stride: width / 2 },
    ],
  });
  assert.equal(frames.videoFrameRepackedBytes(same), 0);
  assert.deepEqual(await picture(same), expected);
});
