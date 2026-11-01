// Scaling checks that hold on any machine: a step is run at one size and at eight times it, and its
// CPU work must grow less than 8^1.5 (22x), which linear and n log n work meet and quadratic work
// does not. Mirrors `tests/scaling_support` in Rust.

export const GROWTH = 8;

export const BOUND = 22;

// Windows counts CPU time in 15.6 ms ticks, so readings there are longer.
const LEAST_RUN_MS = process.platform === "win32" ? 100 : 20;

const READINGS = 3;

function cpuMicros() {
  const { user, system } = process.cpuUsage();
  return user + system;
}

async function measure(size, repeats, setup, run, count) {
  const inputs = [];
  for (let index = 0; index < count; index += 1) inputs.push(await setup(size));
  const wall = performance.now();
  const started = cpuMicros();
  for (let index = 0; index < repeats; index += 1)
    await run(inputs[index % count]);
  return { work: cpuMicros() - started, ms: performance.now() - wall };
}

/**
 * Asserts that `run` on an input `growth` times `small` takes less than `bound` times its CPU work
 * on `small`. The small size cycles through `growth` inputs so it does not run from a cache the
 * large one overflows. `fresh` gives each run its own input, defeating per-input caches.
 *
 * @param {string} what
 * @param {number} small
 * @param {(size: number) => unknown} setup Not measured; may be async.
 * @param {(input: any) => unknown} run Measured; may be async.
 * @param {{ growth?: number, bound?: number, fresh?: boolean }} [options]
 */
export async function assertScalesBelow(
  what,
  small,
  setup,
  run,
  { growth = GROWTH, bound = BOUND, fresh = false } = {},
) {
  const large = small * growth;
  const count = (size, repeats) =>
    fresh ? repeats : size === small ? growth : 1;
  let repeats = 1;
  let leastSmall;
  for (;;) {
    const reading = await measure(
      small,
      repeats,
      setup,
      run,
      count(small, repeats),
    );
    if (reading.ms >= LEAST_RUN_MS || repeats >= 1 << 16) {
      leastSmall = reading.work;
      break;
    }
    repeats *= 2;
  }
  let leastLarge = Infinity;
  for (let reading = 0; reading < READINGS; reading += 1) {
    leastLarge = Math.min(
      leastLarge,
      (await measure(large, repeats, setup, run, count(large, repeats))).work,
    );
    if (reading + 1 < READINGS)
      leastSmall = Math.min(
        leastSmall,
        (await measure(small, repeats, setup, run, count(small, repeats))).work,
      );
  }
  const ratio = leastLarge / Math.max(leastSmall, 1);
  if (!(leastLarge < Math.max(leastSmall, 1) * bound))
    throw new Error(
      `${what}: ${growth}x the input (${small} -> ${large}, ${repeats} runs each) took ` +
        `${ratio.toFixed(1)}x the CPU work (${leastSmall} -> ${leastLarge} us); the bound is ` +
        `${bound}x (linear work takes about ${growth}x, quadratic ${growth * growth}x)`,
    );
  return ratio;
}

/**
 * The least wall time of `count` runs, for comparison with a baseline measured the same way. Wall
 * time because one run is often shorter than Windows' CPU-time tick.
 *
 * @param {number} count
 * @param {() => unknown} run
 */
export function minOf(count, run) {
  let least = Infinity;
  for (let index = 0; index < count; index += 1) {
    const started = performance.now();
    run();
    least = Math.min(least, performance.now() - started);
  }
  return least;
}
