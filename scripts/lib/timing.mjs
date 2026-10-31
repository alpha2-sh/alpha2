// Timing checks for tests that must hold on any machine.
//
// A bound on elapsed milliseconds fails on a slow or busy machine without any regression and
// passes a quadratic step on a fast one. A step meant to take time in proportion to its input is
// instead run at one size and at eight times it, and its work must grow less than 22 times: slower
// than the size to the power 1.5, as linear and n log n work does (8 and about 11 to 15 times) and
// quadratic work (64 times) does not. The work is the process's CPU time
// (`process.cpuUsage()`, user and system), not the time it waited for a CPU. The Rust tests check
// the same way (`tests/scaling_support`).

/** How much larger the large input is than the small one. */
export const GROWTH = 8;

/** The large input's work must be less than this multiple of the small one's: 8^1.5 rounded down. */
export const BOUND = 22;

/**
 * How long the small input's runs take together at least, in milliseconds. Windows counts process
 * CPU time in clock ticks of 15.6 ms, so a reading there is longer to keep the tick a small part.
 */
const LEAST_RUN_MS = process.platform === "win32" ? 100 : 20;

/** Readings of each size; the least is kept, as the one least disturbed. */
const READINGS = 3;

/** The process's CPU time so far, in microseconds. */
function cpuMicros() {
  const { user, system } = process.cpuUsage();
  return user + system;
}

/**
 * The CPU work and wall time of `repeats` runs of `run` over inputs of `size` made by `setup`
 * before the reading starts: one input for all of them, or with `fresh` one input per run.
 */
async function measure(size, repeats, setup, run, fresh) {
  const inputs = [];
  for (let index = 0; index < (fresh ? repeats : 1); index += 1)
    inputs.push(await setup(size));
  const wall = performance.now();
  const started = cpuMicros();
  for (let index = 0; index < repeats; index += 1)
    await run(inputs[fresh ? index : 0]);
  return { work: cpuMicros() - started, ms: performance.now() - wall };
}

/**
 * Asserts that `run` over an input `setup` makes of `growth` times `small` takes less than `bound`
 * times the CPU work it takes over one of `small`. `setup` (which may be async) is not measured;
 * `run` (which may be async) is. Each reading runs over one input; with `fresh`, each run gets an
 * input of its own, so a step that caches what it computed per input is measured computing it.
 * The small input runs as many times as take 20 ms together (100 ms on Windows), and the large one
 * as many times; the sizes alternate three times and the least reading of each is kept.
 *
 * @param {string} what named in the message
 * @param {number} small the small size
 * @param {(size: number) => unknown} setup makes an input of a size
 * @param {(input: any) => unknown} run the measured step
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
  let repeats = 1;
  let leastSmall;
  for (;;) {
    const reading = await measure(small, repeats, setup, run, fresh);
    if (reading.ms >= LEAST_RUN_MS || repeats >= 1 << 16) {
      leastSmall = reading.work;
      break;
    }
    repeats *= 2;
  }
  let leastLarge = Infinity;
  // Alternated, so a slow stretch of the machine falls on both sizes.
  for (let reading = 0; reading < READINGS; reading += 1) {
    leastLarge = Math.min(
      leastLarge,
      (await measure(large, repeats, setup, run, fresh)).work,
    );
    if (reading + 1 < READINGS)
      leastSmall = Math.min(
        leastSmall,
        (await measure(small, repeats, setup, run, fresh)).work,
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
 * The least wall time, in milliseconds, of `count` runs of the synchronous `run`: compared with a
 * baseline measured the same way in the same test, it tells a step's cost apart from the machine's
 * speed. Wall time, as one run is often shorter than Windows' CPU-time tick; the least reading is
 * the one least disturbed by the rest of the machine.
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
