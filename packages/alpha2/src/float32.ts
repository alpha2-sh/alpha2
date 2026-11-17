/** The shortest decimal that reads back as the same 32-bit float, as the engine writes it. */
export function shortestFloat32(value: number): number {
  const target = Math.fround(value);
  for (let digits = 1; digits < 9; digits++) {
    const short = Number(target.toPrecision(digits));
    if (Math.fround(short) === target) return short;
  }
  return Number(target.toPrecision(9));
}
