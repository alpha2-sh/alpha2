// Scaling checks that hold on any machine: a step is run at one size and at eight times it, and its
// CPU work must grow less than 8^1.5 (22x), which linear and n log n work meet and quadratic work
// does not. Mirrors `tests/scaling_support` in Rust.

export const GROWTH = 8;

export const BOUND = 22;

// The small reading's least CPU work: 20 ms, and at least ten ticks of the CPU clock, which
// Windows advances in 15.6 ms steps.
const LEAST_WORK_US = 20_000;
const LEAST_TICKS = 10;

const READINGS = 3;

function cpuMicros() {
  const { user, system } = process.cpuUsage();
  return user + system;
}

// The smallest step of the CPU clock seen over a few steps.
function cpuTickMicros() {
  let least = Infinity;
  let last = cpuMicros();
  for (let steps = 0; steps < 3;) {
    const now = cpuMicros();
    if (now === last) continue;
    least = Math.min(least, now - last);
    last = now;
    steps += 1;
  }
  return least;
}

async function measure(size, repeats, setup, run, count) {
  const inputs = [];
  for (let index = 0; index < count; index += 1) inputs.push(await setup(size));
  const started = cpuMicros();
  for (let index = 0; index < repeats; index += 1)
    await run(inputs[index % count]);
  return cpuMicros() - started;
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
  // The JIT recompiles code on background threads when the large input first reaches it, and how
  // soon depends on the machine's load. Running the large input once first and taking both sizes
  // only from the readings after it compares them in the same compiled state.
  await measure(large, 1, setup, run, 1);
  const leastWork = Math.max(LEAST_WORK_US, LEAST_TICKS * cpuTickMicros());
  let repeats = 1;
  while (
    (await measure(small, repeats, setup, run, count(small, repeats))) <
      leastWork &&
    repeats < 1 << 16
  )
    repeats *= 2;
  let leastSmall = Infinity;
  let leastLarge = Infinity;
  for (let reading = 0; reading < READINGS; reading += 1) {
    leastSmall = Math.min(
      leastSmall,
      await measure(small, repeats, setup, run, count(small, repeats)),
    );
    leastLarge = Math.min(
      leastLarge,
      await measure(large, repeats, setup, run, count(large, repeats)),
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
