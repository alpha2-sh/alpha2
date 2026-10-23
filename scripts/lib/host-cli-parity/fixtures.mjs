// The projects, media and host binaries test-host-cli-parity.mjs runs.

import { join, resolve } from "node:path";

export const root = resolve(import.meta.dirname, "..", "..", "..");
export const executable = resolve(
  process.env.ALPHA2_HOST_BINARY ??
    join(
      root,
      "target",
      "debug",
      `alpha2-host${process.platform === "win32" ? ".exe" : ""}`,
    ),
);
export const mediaExecutable = join(
  root,
  "target",
  "debug",
  `alpha2${process.platform === "win32" ? ".exe" : ""}`,
);
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
// A project using most of what the engine draws, compared frame by frame before encoding. Both
// sides receive the same RGB pixels: PNG frames in a QuickTime file natively, the PNGs in the page.
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

// Shapes generators (animated paths, trims, dashes, compound fills, group layers, spatial
// motion), a shapes generator node and a media node reading one later, and path masks.
export const shapesProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-shapes-project.json",
);
export const shapesAssets = { still: complexAssets.still };

// Every built-in effect and transition. The page fails a project that leaves a catalog entry out,
// so a new body is compared as soon as it ships.
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

// PQ and HLG sources: the browser copies the VideoFrame's planes out through the SDK (never the
// browser's own conversion), so both sides must land on the same codes.
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

// Sources read through extension providers (image sequences, GIF, WebP, APNG, Lottie, dotLottie,
// OpenEXR, SVG): natively in the host, in the browser from the same code built as wasm.
export const extensionsProjectPath = join(
  root,
  "tests",
  "cases",
  "host-parity-extensions-project.json",
);
export const extensionCorpus = (name) =>
  join(root, "tests", "corpus", "extensions", name);

// Exif orientations, a Display P3 ICC still, mirrored H.264 by MP4 matrix and Matroska pose, and
// 4:3 pixels. The page reads them as the editor and the SDK's mediabunny adapter do.
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
  // 4:3 pixels declared by MP4 `pasp`, and only in the SPS VUI for MPEG-TS and an MP4 without
  // `pasp`: each 128x96 shown 16:9.
  pasp: join(root, "tests", "corpus", "h264-bt709-pasp.mp4"),
  "vui-sar-ts": join(root, "tests", "corpus", "h264-bt709-vui-sar.ts"),
  "vui-sar-mp4": join(root, "tests", "corpus", "h264-bt709-vui-sar.mp4"),
};
