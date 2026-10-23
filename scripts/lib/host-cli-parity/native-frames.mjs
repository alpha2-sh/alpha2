// The native side of the browser comparison: the parity export decoded by the media CLI, and each
// project's frames as the encoder receives them (ALPHA2_EXPORT_FRAMES_Y4M).

import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import {
  complexAssets,
  complexProjectPath,
  effectsAssets,
  effectsProjectPath,
  effectsResources,
  executable,
  extensionCorpus,
  extensionsProjectPath,
  hdrAssets,
  hdrProjectPath,
  mediaExecutable,
  orientationAssets,
  orientationProjectPath,
  PINNED_RATE_ARGUMENTS,
  root,
  shapesAssets,
  shapesProjectPath,
} from "./fixtures.mjs";
import { run } from "./processes.mjs";

/** Writes the native frames under `scratch`, from `cliOutput` (the parity export), and returns their paths. */
export async function nativeFrames(scratch, cliOutput) {
  const nativeFrame = join(scratch, "native.png");
  await run(mediaExecutable, ["render", cliOutput, nativeFrame]);
  const complexFrames = join(scratch, "complex.y4m");
  await run(
    executable,
    [
      "export",
      "--project",
      complexProjectPath,
      ...Object.entries(complexAssets).flatMap(([id, path]) => [
        "--asset",
        `${id}=${path}`,
      ]),
      "--out",
      join(scratch, "complex.mp4"),
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
    ],
    { ALPHA2_EXPORT_FRAMES_Y4M: complexFrames },
  );
  const shapesFrames = join(scratch, "shapes.y4m");
  await run(
    executable,
    [
      "export",
      "--project",
      shapesProjectPath,
      ...Object.entries(shapesAssets).flatMap(([id, path]) => [
        "--asset",
        `${id}=${path}`,
      ]),
      "--out",
      join(scratch, "shapes.mp4"),
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
    ],
    { ALPHA2_EXPORT_FRAMES_Y4M: shapesFrames },
  );
  const effectsFrames = join(scratch, "effects.y4m");
  await run(
    executable,
    [
      "export",
      "--project",
      effectsProjectPath,
      ...Object.entries(effectsAssets).flatMap(([id, path]) => [
        "--asset",
        `${id}=${path}`,
      ]),
      ...Object.entries(effectsResources).flatMap(([id, path]) => [
        "--effect-asset",
        `${id}=${path}`,
      ]),
      "--out",
      join(scratch, "effects.mp4"),
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
    ],
    { ALPHA2_EXPORT_FRAMES_Y4M: effectsFrames },
  );
  // Without the video's own clip, the video is read only through the composition source.
  const complexProject = JSON.parse(await readFile(complexProjectPath, "utf8"));
  const nestedProject = structuredClone(complexProject);
  for (const track of nestedProject.timeline.tracks) {
    track.clips = track.clips.filter((clip) => clip.id !== "video-clip");
    track.transitions = (track.transitions ?? []).filter(
      (transition) =>
        transition.from_clip !== "video-clip" &&
        transition.to_clip !== "video-clip",
    );
  }
  const nestedProjectPath = join(scratch, "nested-project.json");
  await writeFile(nestedProjectPath, JSON.stringify(nestedProject));
  for (const [name, project] of [
    ["complex", complexProjectPath],
    ["nested", nestedProjectPath],
  ]) {
    const frames = {};
    for (const [mode, pipeline] of [
      ["pipelined", "on"],
      ["synchronous", "off"],
    ]) {
      frames[mode] = join(scratch, `${name}-${mode}.y4m`);
      await run(
        executable,
        [
          "export",
          "--project",
          project,
          ...Object.entries(complexAssets).flatMap(([id, path]) => [
            "--asset",
            `${id}=${path}`,
          ]),
          "--out",
          join(scratch, `${name}-${mode}.mp4`),
          "--video-codec",
          "mpeg4-part2",
          ...PINNED_RATE_ARGUMENTS,
          "--no-audio",
        ],
        {
          ALPHA2_EXPORT_FRAMES_Y4M: frames[mode],
          ALPHA2_EXPORT_PIPELINE: pipeline,
        },
      );
    }
    assert.deepEqual(
      await readFile(frames.synchronous),
      await readFile(frames.pipelined),
      `${name}: pipelined and synchronous frames must be identical`,
    );
  }
  console.log(
    "verified identical pipelined/synchronous frames for the complex project and media read only through a composition source",
  );
  const hdrFrames = join(scratch, "hdr.y4m");
  await run(
    executable,
    [
      "export",
      "--project",
      hdrProjectPath,
      ...Object.entries(hdrAssets).flatMap(([id, path]) => [
        "--asset",
        `${id}=${path}`,
      ]),
      "--out",
      join(scratch, "hdr.mp4"),
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
    ],
    { ALPHA2_EXPORT_FRAMES_Y4M: hdrFrames },
  );
  const orientationFrames = join(scratch, "orientation.y4m");
  await run(
    executable,
    [
      "export",
      "--project",
      orientationProjectPath,
      ...Object.entries(orientationAssets).flatMap(([id, path]) => [
        "--asset",
        `${id}=${path}`,
      ]),
      "--out",
      join(scratch, "orientation.mp4"),
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
      // The browser decodes these in software; so does this, on any GPU.
      "--video-decode",
      "software",
    ],
    { ALPHA2_EXPORT_FRAMES_Y4M: orientationFrames },
  );
  const extensionFrames = join(scratch, "extensions.y4m");
  await run(
    executable,
    [
      "export",
      "--project",
      extensionsProjectPath,
      "--asset",
      `gif=${extensionCorpus("animation.gif")}`,
      "--asset",
      `webp=${extensionCorpus("animation.webp")}`,
      "--asset",
      `apng=${extensionCorpus("animation.png")}`,
      "--asset",
      `logo=${extensionCorpus("shapes.svg")}`,
      "--asset",
      `label=${extensionCorpus("label.svg")}`,
      // An SVG's text draws with the caption fonts natively; the page registers the same font
      // with the provider module.
      "--font",
      join(root, "tests", "fonts", "Alpha2Fixture.ttf"),
      "--asset",
      `lottie=${extensionCorpus("moving.json")}`,
      "--asset",
      `dot=${extensionCorpus("pair.lottie")}`,
      "--asset",
      `exr=${extensionCorpus("window.exr")}`,
      "--frames",
      `seq=${[7, 8, 9, 10]
        .map((n) =>
          extensionCorpus(`sequence_${String(n).padStart(4, "0")}.png`),
        )
        .join(",")}`,
      "--frames",
      `exrseq=${[1, 2]
        .map((n) =>
          extensionCorpus(`sequence_${String(n).padStart(4, "0")}.exr`),
        )
        .join(",")}`,
      "--out",
      join(scratch, "extensions.mp4"),
      "--video-codec",
      "mpeg4-part2",
      ...PINNED_RATE_ARGUMENTS,
      "--no-audio",
    ],
    { ALPHA2_EXPORT_FRAMES_Y4M: extensionFrames },
  );
  return {
    nativeFrame,
    complexFrames,
    shapesFrames,
    effectsFrames,
    hdrFrames,
    orientationFrames,
    extensionFrames,
  };
}
