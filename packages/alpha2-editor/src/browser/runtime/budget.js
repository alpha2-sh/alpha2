// The browser export's byte budget, readable without loading the runtime.

import { minimumExportWorkingSet } from "./staging.js";

/**
 * Bytes of rendered frames an export keeps in flight unless asked otherwise: the SDK's
 * `maxInFlightBytes` default.
 */
export const DEFAULT_IN_FLIGHT_BYTES = 128 * 1024 * 1024;

/**
 * The export options one attempt runs with: a retry that asks for less in flight scales the byte
 * budget by its profile, but never below one frame's working set, which would only turn the retry
 * into a refusal.
 */
export function attemptOptions(options, attempt, settings) {
  const scale = attempt?.profile?.memoryScale ?? 1;
  if (scale >= 1) return options;
  const least = minimumExportWorkingSet(settings.width, settings.height);
  const budget = options.memory?.inFlightBytes ?? DEFAULT_IN_FLIGHT_BYTES;
  return {
    ...options,
    memory: {
      ...options.memory,
      inFlightBytes: Math.max(least, Math.floor(budget * scale)),
    },
  };
}
