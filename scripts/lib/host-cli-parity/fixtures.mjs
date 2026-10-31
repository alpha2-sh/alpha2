import { join, resolve } from "node:path";

import { executable as cargoExecutable } from "../cargo-target.mjs";

export const root = resolve(import.meta.dirname, "..", "..", "..");
export const executable = resolve(
  process.env.ALPHA2_HOST_BINARY ?? cargoExecutable("alpha2-host", "debug"),
);
export const mediaExecutable = cargoExecutable("alpha2", "debug");
export const projectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-project.json",
);
export const assetPath = join(
  root,
  "tests",
  "corpus",
  "cross_topology_parity.png",
);
export const sharedSourceProjectPath = join(
  root,
  "tests",
  "cases",
  "host-shared-source-export-project.json",
);
// Pin 12 Mbps for effectively lossless 64x48 parity, independent of quality defaults.
export const PINNED_RATE = { bitrate: 12_000_000 };
export const PINNED_RATE_ARGUMENTS = [
  "--video-quality",
  String(PINNED_RATE.bitrate),
];
export const sharedSourceAssetPath = join(
  root,
  "tests",
  "corpus",
  "long-gop.mp4",
);
// Compared frame by frame before encoding; both sides receive the same RGB pixels.
export const complexProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-complex-project.json",
);
export const complexAssets = {
  video: join(root, "tests", "corpus", "parity-rgb.mov"),
  still: join(root, "tests", "corpus", "parity-image.png"),
};

export const shapesProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-shapes-project.json",
);
export const shapesAssets = { still: complexAssets.still };

export const layoutProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-layout-project.json",
);
export const layoutAssets = { still: complexAssets.still };
export const layoutFont = join(root, "tests", "fonts", "Alpha2Fixture.ttf");

// The page fails if any catalog effect or transition is left out.
export const effectsProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-effects-project.json",
);
export const effectsAssets = complexAssets;
export const effectsResources = {
  grade: join(root, "tests", "corpus", "parity-grade.cube"),
};

// The browser copies HDR planes out through the SDK, so both sides must land on the same codes.
export const hdrProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-hdr-project.json",
);
export const hdrAssets = {
  pq: join(root, "tests", "corpus", "hdr-pq.mp4"),
  hlg: join(root, "tests", "corpus", "hdr-hlg.mp4"),
  "pq-topleft": join(root, "tests", "corpus", "hdr-pq-topleft.mp4"),
  "pq-declared": join(root, "tests", "corpus", "hdr-pq-untagged.mp4"),
};

export const extensionsProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-extensions-project.json",
);
export const extensionCorpus = (name) =>
  join(root, "tests", "corpus", "extensions", name);

export const orientationProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-orientation-project.json",
);
export const orientationAssets = {
  ...Object.fromEntries(
    [1, 2, 3, 4, 5, 6, 7, 8].map((value) => [
      `exif${value}`,
      join(root, "tests", "corpus", `exif-orientation-${value}.jpg`),
    ]),
  ),
  p3: join(root, "tests", "corpus", "icc-display-p3.jpg"),
  hflip: join(root, "tests", "corpus", "h264-bt709-hflip.mp4"),
  vflip: join(root, "tests", "corpus", "h264-bt709-vflip.mp4"),
  transverse: join(root, "tests", "corpus", "h264-bt709-transverse.mp4"),
  "transverse-mkv": join(root, "tests", "corpus", "h264-bt709-transverse.mkv"),
  // 4:3 pixels from MP4 `pasp`, or only from the SPS VUI (MPEG-TS, MP4 without `pasp`).
  pasp: join(root, "tests", "corpus", "h264-bt709-pasp.mp4"),
  "vui-sar-ts": join(root, "tests", "corpus", "h264-bt709-vui-sar.ts"),
  "vui-sar-mp4": join(root, "tests", "corpus", "h264-bt709-vui-sar.mp4"),
};
