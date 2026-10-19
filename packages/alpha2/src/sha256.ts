// Incremental SHA-256: Web Crypto hashes only a whole buffer, and a media file can be gigabytes, so
// memory stays bounded by one chunk.

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
  0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
  0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
  0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
  0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
  0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
  0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

/** An incremental SHA-256. */
export class Sha256 {
  readonly #state = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
    0x1f83d9ab, 0x5be0cd19,
  ]);
  readonly #block = new Uint8Array(64);
  readonly #words = new Uint32Array(64);
  #buffered = 0;
  #length = 0;
  #done = false;

  /** Adds `bytes` to the message. Throws once {@link Sha256.hex} has been called. */
  update(bytes: Uint8Array): this {
    if (this.#done) throw new Error("the hash is already finished");
    let offset = 0;
    this.#length += bytes.length;
    if (this.#buffered > 0) {
      const take = Math.min(64 - this.#buffered, bytes.length);
      this.#block.set(bytes.subarray(0, take), this.#buffered);
      this.#buffered += take;
      offset = take;
      if (this.#buffered < 64) return this;
      this.#compress(this.#block, 0);
      this.#buffered = 0;
    }
    for (; offset + 64 <= bytes.length; offset += 64)
      this.#compress(bytes, offset);
    this.#block.set(bytes.subarray(offset));
    this.#buffered = bytes.length - offset;
    return this;
  }

  /**
   * Finishes the message and returns its hash as 64 lowercase hex digits.
   * The hash can be finished once; a second call throws.
   */
  hex(): string {
    if (this.#done) throw new Error("the hash is already finished");
    this.#done = true;
    const bits = this.#length * 8;
    this.#block[this.#buffered++] = 0x80;
    if (this.#buffered > 56) {
      this.#block.fill(0, this.#buffered);
      this.#compress(this.#block, 0);
      this.#buffered = 0;
    }
    this.#block.fill(0, this.#buffered);
    const view = new DataView(this.#block.buffer);
    view.setUint32(56, Math.floor(bits / 0x1_0000_0000));
    view.setUint32(60, bits >>> 0);
    this.#compress(this.#block, 0);
    return Array.from(this.#state, (word) =>
      word.toString(16).padStart(8, "0"),
    ).join("");
  }

  #compress(bytes: Uint8Array, offset: number): void {
    const w = this.#words;
    for (let i = 0; i < 16; i++) {
      const at = offset + i * 4;
      w[i] =
        (bytes[at] << 24) |
        (bytes[at + 1] << 16) |
        (bytes[at + 2] << 8) |
        bytes[at + 3];
    }
    for (let i = 16; i < 64; i++) {
      const a = w[i - 15];
      const b = w[i - 2];
      const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
      const s1 =
        ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
      w[i] = (w[i - 16] + s0 + w[i - 7] + s1) | 0;
    }
    const s = this.#state;
    let a = s[0];
    let b = s[1];
    let c = s[2];
    let d = s[3];
    let e = s[4];
    let f = s[5];
    let g = s[6];
    let h = s[7];
    for (let i = 0; i < 64; i++) {
      const s1 =
        ((e >>> 6) | (e << 26)) ^
        ((e >>> 11) | (e << 21)) ^
        ((e >>> 25) | (e << 7));
      const choice = (e & f) ^ (~e & g);
      const t1 = (h + s1 + choice + K[i] + w[i]) | 0;
      const s0 =
        ((a >>> 2) | (a << 30)) ^
        ((a >>> 13) | (a << 19)) ^
        ((a >>> 22) | (a << 10));
      const majority = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (s0 + majority) | 0;
      h = g;
      g = f;
      f = e;
      e = (d + t1) | 0;
      d = c;
      c = b;
      b = a;
      a = (t1 + t2) | 0;
    }
    s[0] = (s[0] + a) | 0;
    s[1] = (s[1] + b) | 0;
    s[2] = (s[2] + c) | 0;
    s[3] = (s[3] + d) | 0;
    s[4] = (s[4] + e) | 0;
    s[5] = (s[5] + f) | 0;
    s[6] = (s[6] + g) | 0;
    s[7] = (s[7] + h) | 0;
  }
}

/** How long hashing holds the thread before it lets other tasks run, in milliseconds. */
const HASH_SLICE_MS = 8;
/** The most bytes hashed between two looks at the clock: about 5 ms of hashing. */
const HASH_PIECE_BYTES = 1 << 20;

/**
 * Resolves in a task of its own, once the event loop has had its turn: a frame, input, timers, an
 * abort. Posts a channel message, which a timer's minimum delay does not slow down.
 *
 * Not `scheduler.yield()`: Chrome runs its continuation ahead of timers and of the next frame, so a
 * hash yielding through it draws about 10 frames a second and runs no timer until it ends.
 */
function yieldToEventLoop(): Promise<void> {
  if (typeof MessageChannel === "function") {
    return new Promise((resolve) => {
      const channel = new MessageChannel();
      channel.port1.onmessage = () => {
        channel.port1.close();
        resolve();
      };
      channel.port2.postMessage(undefined);
    });
  }
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/**
 * The SHA-256 of a blob's bytes, read as a stream, as 64 lowercase hex digits.
 *
 * `onProgress` receives the bytes hashed so far after each piece of at most 1 MiB. Memory stays
 * bounded by one chunk of the blob's stream.
 *
 * A blob whose chunks are already read (a cached local file) resolves each read without a task in
 * between, so hashing yields to the event loop about every 8 ms: the page keeps painting and
 * handling input, and `signal` can abort it partway.
 *
 * Rejects with `signal.reason` when `signal` aborts, and with the stream's error when the blob
 * cannot be read.
 */
export async function sha256Blob(
  blob: Blob,
  options: {
    readonly signal?: AbortSignal;
    readonly onProgress?: (bytes: number) => void;
  } = {},
): Promise<string> {
  const hash = new Sha256();
  const reader = blob.stream().getReader();
  let read = 0;
  let sliceStart = performance.now();
  try {
    for (;;) {
      options.signal?.throwIfAborted();
      const { done, value } = await reader.read();
      if (done) break;
      for (let at = 0; at < value.length; at += HASH_PIECE_BYTES) {
        if (performance.now() - sliceStart >= HASH_SLICE_MS) {
          await yieldToEventLoop();
          options.signal?.throwIfAborted();
          sliceStart = performance.now();
        }
        const piece = value.subarray(at, at + HASH_PIECE_BYTES);
        hash.update(piece);
        read += piece.length;
        options.onProgress?.(read);
      }
    }
  } finally {
    reader.releaseLock();
  }
  return hash.hex();
}
