// Encoder surface pool budget cases: the pool stays inside it, the smallest workable budget
// degrades to a rendezvous channel, and a smaller one is refused when the path is required.

import { existsSync } from "node:fs";
import { join } from "node:path";

import { align, assertEqual, FRAMES, HEIGHT, WIDTH } from "./harness.mjs";

export function budgetChecks(harness, { gpu, projectPath, assets, baseline }) {
  const { check, runExport, scratch } = harness;
  check("the encoder surface pool stays inside its budget", () => {
    const performance = baseline.result.performance;
    const budget = performance.gpuEncodeBudget;
    if (!budget) throw new Error("gpuEncodeBudget was never reported");
    assertEqual(
      budget.leaseBound,
      performance.encodeChannelDepth + 2,
      "gpuEncodeBudget.leaseBound",
    );
    // The host's leases are capped by its bound; the pool may allocate more for what the handoff
    // still holds, and no more than the budget's ceiling.
    const leases = performance.encoderSurfaceLeasePeak;
    const allocated = performance.encoderFramesAllocated;
    if (!(leases >= 1 && leases <= budget.leaseBound)) {
      throw new Error(
        `encoderSurfaceLeasePeak ${leases} is outside 1..${budget.leaseBound}`,
      );
    }
    if (!(allocated >= leases && allocated <= budget.surfaceCeiling)) {
      throw new Error(
        `encoderFramesAllocated ${allocated} is outside ${leases}..${budget.surfaceCeiling}`,
      );
    }
    if (budget.reservedBytes > performance.inFlightByteBudget) {
      throw new Error(
        `the GPU path reserved ${budget.reservedBytes} B of a ${performance.inFlightByteBudget} B budget`,
      );
    }
    assertEqual(
      performance.encoderSurfaceAcquires,
      FRAMES,
      "encoderSurfaceAcquires",
    );
  });

  // The ring, a rendezvous channel's two surfaces, and what the handoff and the encoder retain: the
  // smallest budget the GPU path runs in.
  const codedBytes =
    align(WIDTH, gpu.widthAlignment) * align(HEIGHT, gpu.heightAlignment) * 1.5;
  const minimumBudget =
    Math.ceil(WIDTH * HEIGHT * 1.5) * gpu.planeRingSlots +
    Math.ceil(codedBytes) * (2 + gpu.retainedSurfaces);
  check("the smallest budget degrades to a rendezvous channel", () => {
    const degraded = runExport({
      project: projectPath,
      assets,
      output: join(scratch, "degraded.mp4"),
      env: { ALPHA2_EXPORT_IN_FLIGHT_BYTES: String(minimumBudget) },
      extra: ["--input-policy", "gpu-required"],
    });
    if (degraded.status !== 0) {
      throw new Error(
        `the degraded export failed (${degraded.status}): ${degraded.stderr.slice(-2000)}`,
      );
    }
    const performance = degraded.result.performance;
    assertEqual(performance.encodeChannelDepth, 0, "encodeChannelDepth");
    assertEqual(
      performance.gpuEncodeBudget.reservedBytes,
      minimumBudget,
      "gpuEncodeBudget.reservedBytes",
    );
    if (performance.encoderSurfaceLeasePeak > 2) {
      throw new Error(
        `a rendezvous channel held ${performance.encoderSurfaceLeasePeak} surfaces; at most two can be leased`,
      );
    }
    if (
      performance.encoderFramesAllocated >
      performance.gpuEncodeBudget.surfaceCeiling
    ) {
      throw new Error(
        `a rendezvous channel allocated ${performance.encoderFramesAllocated} surfaces, over its ceiling`,
      );
    }
  });

  check(
    "a budget below that minimum refuses when the GPU path is required",
    () => {
      const output = join(scratch, "short.mp4");
      const refused = runExport({
        project: projectPath,
        assets,
        output,
        env: { ALPHA2_EXPORT_IN_FLIGHT_BYTES: String(minimumBudget - 1) },
        extra: ["--input-policy", "gpu-required"],
      });
      if (refused.status === 0) {
        throw new Error(
          "an export whose budget cannot hold the GPU path succeeded",
        );
      }
      const reported = `${refused.stdout}${refused.stderr}`;
      // The CLI prints the message; the protocol carries it as GPU_OUTPUT_UNAVAILABLE.
      if (
        !reported.includes("GPU-resident output is required but unavailable") ||
        !reported.includes("cannot hold the GPU path's plane ring")
      ) {
        throw new Error(
          `the refusal did not name the budget: ${reported.slice(-1000)}`,
        );
      }
      if (existsSync(output))
        throw new Error("the refused export left an output");
    },
  );
}
