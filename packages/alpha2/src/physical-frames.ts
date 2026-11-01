const MAX_PHYSICAL_FRAME_ALIASES = 512;

/** Fields that make two decoded frames the same pixels once uploaded. */
export interface PhysicalFrameIdentity {
  readonly sourceId: string;
  readonly contentGeneration: number;
  readonly timestamp: string;
  readonly duration: number | null;
  readonly sourceWidth: number;
  readonly sourceHeight: number;
  readonly outputWidth: number;
  readonly outputHeight: number;
  readonly rotation: 0 | 90 | 180 | 270;
  readonly mirror: boolean;
}

export interface ExactFrameKey {
  readonly sourceId: string;
  readonly mediaTimeTicks: bigint;
}

/**
 * Maps recently uploaded frames to their exact key, so a request that decodes to the same frame
 * aliases the existing upload. LRU of 512 entries; an exact key belongs to at most one identity.
 */
export class PhysicalFrameAliases {
  readonly #entries = new Map<string, ExactFrameKey>();

  /**
   * Serves `requested` from the upload recorded for `identity`. On `false` (nothing recorded or
   * the engine refused) the caller uploads and calls `record`.
   */
  alias(
    identity: PhysicalFrameIdentity,
    requested: ExactFrameKey,
    aliasExact: (existing: ExactFrameKey, requested: ExactFrameKey) => boolean,
  ): boolean {
    const key = physicalKey(identity);
    const existing = this.#entries.get(key);
    if (existing === undefined) return false;
    this.#entries.delete(key);
    if (!aliasExact(existing, requested)) return false;
    this.#forgetExact(requested, key);
    this.#entries.set(key, requested);
    return true;
  }

  record(identity: PhysicalFrameIdentity, exact: ExactFrameKey): void {
    const key = physicalKey(identity);
    this.#forgetExact(exact, key);
    this.#entries.delete(key);
    this.#entries.set(key, exact);
    while (this.#entries.size > MAX_PHYSICAL_FRAME_ALIASES) {
      const oldest = this.#entries.keys().next().value;
      if (oldest === undefined) break;
      this.#entries.delete(oldest);
    }
  }

  clear(): void {
    this.#entries.clear();
  }

  #forgetExact(exact: ExactFrameKey, except: string): void {
    for (const [key, candidate] of this.#entries) {
      if (
        key !== except &&
        candidate.sourceId === exact.sourceId &&
        candidate.mediaTimeTicks === exact.mediaTimeTicks
      ) {
        this.#entries.delete(key);
      }
    }
  }
}

function physicalKey(identity: PhysicalFrameIdentity): string {
  return JSON.stringify([
    identity.sourceId,
    identity.contentGeneration,
    identity.timestamp,
    identity.duration,
    identity.sourceWidth,
    identity.sourceHeight,
    identity.outputWidth,
    identity.outputHeight,
    identity.rotation,
    identity.mirror,
  ]);
}
