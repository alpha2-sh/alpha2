#!/usr/bin/env node
// Protocol fixture for the GPU export harness's own tests; no media or GPU work. It claims a
// GPU-resident path and, when asked to cancel (ALPHA2_EXPORT_CANCEL_AT_FRAME), ends the way
// GPU_EXPORT_FIXTURE names. The `unavailable` scenarios claim no GPU path, for
// `--expect-unavailable`; `unavailable-broken` misbehaves the ways that check must catch.
import { readFileSync, writeFileSync } from "node:fs";

const scenario = process.env.GPU_EXPORT_FIXTURE ?? "clean";
const hang = () => setInterval(() => {}, 1 << 30);
const reason = "Fixture Adapter reports no D3D12 video encoder for h264";

if (process.argv[2] === "serve" && scenario.startsWith("unavailable")) {
  for (const line of readFileSync(0, "utf8").split("\n").filter(Boolean)) {
    const request = JSON.parse(line);
    if (request.operation === "capabilities") {
      console.log(
        JSON.stringify({
          requestId: request.requestId,
          ok: true,
          result: {
            capabilities: {
              encode: {
                gpuOutputPath: { available: false, reason },
                video: [{ id: "h264" }],
              },
            },
          },
        }),
      );
    } else {
      if (scenario === "unavailable-broken")
        writeFileSync(request.output, "fixture");
      console.log(
        JSON.stringify({
          requestId: request.requestId,
          ok: false,
          error: {
            code: "GPU_OUTPUT_UNAVAILABLE",
            message: `GPU-resident output is required but unavailable: ${reason}`,
          },
        }),
      );
    }
  }
} else if (scenario.startsWith("unavailable")) {
  const args = process.argv.slice(2);
  writeFileSync(args[args.indexOf("--out") + 1], "fixture");
  console.log(
    JSON.stringify({
      ok: true,
      result: {
        videoOutputPath: "readback",
        performance: { videoOutputPath: "readback" },
        gpuOutputPathUnavailable: scenario === "unavailable" ? reason : null,
        frames: 100,
      },
    }),
  );
} else if (process.argv[2] === "serve") {
  if (scenario === "hang-capabilities") hang();
  else
    console.log(
      JSON.stringify({
        requestId: "gpu-export-capabilities",
        ok: true,
        result: {
          capabilities: {
            encode: {
              gpuOutputPath: {
                available: true,
                encoder: "h264_fixture",
                videoCodec: "h264",
              },
              video: [{ id: "h264" }],
            },
          },
        },
      }),
    );
} else {
  const args = process.argv.slice(2);
  const output = args[args.indexOf("--out") + 1];
  if (process.env.ALPHA2_EXPORT_CANCEL_AT_FRAME === undefined) {
    writeFileSync(output, "fixture");
    console.log(
      JSON.stringify({ ok: true, result: { videoOutputPath: "gpu-resident" } }),
    );
  } else {
    const clean = [
      "alpha2-host export: frames 10/100",
      "alpha2 native export: 12 parked decoder surfaces released",
      "alpha2-host: native export was cancelled at frame 12",
    ];
    const lines = {
      clean,
      // The host's teardown diagnostics, each exiting with the same status 1 as a clean cancel.
      "decode-left-running": [
        clean[0],
        clean[1],
        "alpha2 native decode worker left running: a decode shard is stuck in the decoder",
        clean[2],
      ],
      "encode-panicked": [
        clean[0],
        "thread 'alpha2-encode' panicked at bindings/alpha2-host/src/encode.rs:1:1:",
        "alpha2 native encode worker panicked during cleanup",
        clean[1],
        clean[2],
      ],
      "cleanup-failed": [
        clean[0],
        "alpha2 native export cleanup failed: the muxer could not abort",
        clean[1],
        clean[2],
      ],
      "unknown-parked": [
        clean[0],
        "alpha2 native export: unknown parked decoder surfaces released",
        clean[2],
      ],
      crash: clean,
      hang: [clean[0]],
    }[scenario];
    process.stderr.write(`${lines.join("\n")}\n`);
    if (scenario === "crash") {
      // Windows has no SIGSEGV to raise: a crashing host exits with STATUS_ACCESS_VIOLATION.
      if (process.platform === "win32") process.exit(0xc0000005);
      process.kill(process.pid, "SIGSEGV");
    } else if (scenario === "hang") hang();
    else process.exitCode = 1;
  }
}
