/**
 * A bzip2 decompressor. Cisco publishes its OpenH264 binaries as `.bz2` files, and Node's
 * standard library reads gzip, deflate and Brotli but not bzip2.
 *
 * It reads the format bzip2 1.0 writes: a stream header `BZh1` to `BZh9`, then blocks of
 * Huffman-coded move-to-front symbols with runs of zeros (RUNA, RUNB), each undone through the
 * inverse Burrows-Wheeler transform and the run-length coding of four or more equal bytes, then
 * the end-of-stream marker. Every block's CRC and the stream's combined CRC are checked, and
 * streams may follow one another. Blocks in the randomised form bzip2 0.9.0 could write are
 * refused: no bzip2 since 0.9.5 writes them.
 */

const BLOCK_MAGIC = [0x314159, 0x265359] as const;
const END_MAGIC = [0x177245, 0x385090] as const;
/** Symbols coded with one table before the selectors pick the next. */
const GROUP_SIZE = 50;
const MIN_GROUPS = 2;
const MAX_GROUPS = 6;
const MAX_CODE_LENGTH = 20;
/** The selectors bzip2 uses at most; a stream may list more, which bzip2 1.0.8 reads and ignores. */
const MAX_SELECTORS = 18_002;
/** A zero run's next weight may not reach this, as in bzip2. */
const MAX_RUN_WEIGHT = 2 * 1024 * 1024;

/** The CRC-32 bzip2 uses: polynomial 0x04c11db7, most significant bit first. */
const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let byte = 0; byte < 256; byte++) {
    let crc = byte << 24;
    for (let bit = 0; bit < 8; bit++)
      crc = crc & 0x80000000 ? (crc << 1) ^ 0x04c11db7 : crc << 1;
    table[byte] = crc >>> 0;
  }
  return table;
})();

function corrupt(reason: string): never {
  throw new Error(`invalid bzip2 data: ${reason}`);
}

/** Reads `data` most significant bit first, as bzip2 writes it. */
class BitReader {
  private position = 0;
  /** The last bytes read; its low `live` bits are the ones not consumed yet. */
  private buffer = 0;
  private live = 0;

  constructor(private readonly data: Uint8Array) {}

  /** The next `count` bits, 1 to 24, as a number. */
  bits(count: number): number {
    while (this.live < count) {
      if (this.position >= this.data.length) corrupt("it ends early");
      this.buffer = (this.buffer << 8) | this.data[this.position++];
      this.live += 8;
    }
    this.live -= count;
    return (this.buffer >>> this.live) & ((1 << count) - 1);
  }

  bit(): number {
    if (this.live === 0) {
      if (this.position >= this.data.length) corrupt("it ends early");
      this.buffer = this.data[this.position++];
      this.live = 8;
    }
    this.live--;
    return (this.buffer >>> this.live) & 1;
  }

  /** The next 32 bits, as an unsigned number. */
  uint32(): number {
    return this.bits(16) * 0x10000 + this.bits(16);
  }

  /** Skips to the next byte: a stream ends padded to a whole byte. */
  alignToByte(): void {
    this.live = 0;
  }

  /** Whether every byte has been read; only meaningful after {@link alignToByte}. */
  atEnd(): boolean {
    return this.position >= this.data.length;
  }
}

/** A growing byte buffer. */
class Output {
  bytes: Uint8Array;
  length = 0;

  constructor(capacity: number) {
    this.bytes = new Uint8Array(Math.max(capacity, 1024));
  }

  /** Makes room for `count` more bytes. */
  reserve(count: number): void {
    const needed = this.length + count;
    if (needed <= this.bytes.length) return;
    const grown = new Uint8Array(Math.max(needed, this.bytes.length * 2));
    grown.set(this.bytes.subarray(0, this.length));
    this.bytes = grown;
  }

  result(): Uint8Array {
    return this.bytes.slice(0, this.length);
  }
}

/** A canonical Huffman code, decoded a bit at a time from its shortest length. */
interface HuffmanTable {
  readonly minLength: number;
  readonly maxLength: number;
  /** Per length, the first code of that length and the largest (one less when there is none). */
  readonly firstCode: Int32Array;
  readonly lastCode: Int32Array;
  /** Per length, where its symbols start in `symbols`. */
  readonly firstIndex: Int32Array;
  /** The symbols, by code length and then by value. */
  readonly symbols: Uint16Array;
}

function huffmanTable(lengths: Uint8Array): HuffmanTable {
  let minLength = MAX_CODE_LENGTH;
  let maxLength = 1;
  const counts = new Int32Array(MAX_CODE_LENGTH + 1);
  for (const length of lengths) {
    counts[length]++;
    if (length < minLength) minLength = length;
    if (length > maxLength) maxLength = length;
  }
  const firstCode = new Int32Array(MAX_CODE_LENGTH + 2);
  const lastCode = new Int32Array(MAX_CODE_LENGTH + 2);
  const firstIndex = new Int32Array(MAX_CODE_LENGTH + 2);
  const symbols = new Uint16Array(lengths.length);
  let code = 0;
  let index = 0;
  for (let length = minLength; length <= maxLength; length++) {
    firstCode[length] = code;
    firstIndex[length] = index;
    for (let symbol = 0; symbol < lengths.length; symbol++)
      if (lengths[symbol] === length) symbols[index++] = symbol;
    code += counts[length];
    lastCode[length] = code - 1;
    code <<= 1;
  }
  return { minLength, maxLength, firstCode, lastCode, firstIndex, symbols };
}

function decodeSymbol(reader: BitReader, table: HuffmanTable): number {
  let length = table.minLength;
  let code = reader.bits(length);
  while (code > table.lastCode[length]) {
    if (++length > table.maxLength)
      corrupt("a Huffman code is not in its table");
    code = (code << 1) | reader.bit();
  }
  if (code < table.firstCode[length])
    corrupt("a Huffman code is not in its table");
  return table.symbols[
    table.firstIndex[length] + code - table.firstCode[length]
  ];
}

/**
 * Decompresses `data`, one or more bzip2 streams one after another, and throws on anything that
 * is not exactly that.
 */
export function bunzip2(data: Uint8Array): Uint8Array {
  const reader = new BitReader(data);
  const output = new Output(data.length * 3);
  do {
    readStream(reader, output);
    reader.alignToByte();
  } while (!reader.atEnd());
  return output.result();
}

function readStream(reader: BitReader, output: Output): void {
  if (reader.bits(24) !== 0x425a68) corrupt("it has no stream header");
  const level = reader.bits(8) - 0x30;
  if (level < 1 || level > 9) corrupt("the block size is not 1 to 9");
  const blockSize = level * 100_000;
  const block = new Uint32Array(blockSize);
  let combinedCrc = 0;
  for (;;) {
    const high = reader.bits(24);
    const low = reader.bits(24);
    if (high === END_MAGIC[0] && low === END_MAGIC[1]) {
      if (reader.uint32() !== combinedCrc)
        corrupt("the stream's CRC does not match");
      return;
    }
    if (high !== BLOCK_MAGIC[0] || low !== BLOCK_MAGIC[1])
      corrupt("a block has no block header");
    const storedCrc = reader.uint32();
    const crc = readBlock(reader, block, blockSize, output);
    if (crc !== storedCrc) corrupt("a block's CRC does not match");
    combinedCrc = (((combinedCrc << 1) | (combinedCrc >>> 31)) ^ crc) >>> 0;
  }
}

/** Decodes one block into `output` and returns its CRC. */
function readBlock(
  reader: BitReader,
  block: Uint32Array,
  blockSize: number,
  output: Output,
): number {
  if (reader.bit()) corrupt("randomised blocks are not supported");
  const origin = reader.bits(24);

  // The bytes the block uses, in order: the move-to-front list starts as them.
  const used = reader.bits(16);
  const moveToFront = new Uint8Array(256);
  let inUse = 0;
  for (let high = 0; high < 16; high++) {
    if (!(used & (0x8000 >>> high))) continue;
    const lows = reader.bits(16);
    for (let low = 0; low < 16; low++)
      if (lows & (0x8000 >>> low)) moveToFront[inUse++] = high * 16 + low;
  }
  if (inUse === 0) corrupt("a block uses no bytes");
  const alphabetSize = inUse + 2;
  const endOfBlock = inUse + 1;

  const groups = reader.bits(3);
  if (groups < MIN_GROUPS || groups > MAX_GROUPS)
    corrupt("a block has a Huffman table count other than 2 to 6");
  const selectorCount = reader.bits(15);
  if (selectorCount === 0) corrupt("a block has no selectors");
  const selectors = new Uint8Array(Math.min(selectorCount, MAX_SELECTORS));
  const tableOrder = [0, 1, 2, 3, 4, 5];
  for (let i = 0; i < selectorCount; i++) {
    let position = 0;
    while (reader.bit())
      if (++position >= groups) corrupt("a selector names no table");
    const table = tableOrder[position];
    for (let j = position; j > 0; j--) tableOrder[j] = tableOrder[j - 1];
    tableOrder[0] = table;
    if (i < MAX_SELECTORS) selectors[i] = table;
  }

  const tables: HuffmanTable[] = [];
  for (let t = 0; t < groups; t++) {
    const lengths = new Uint8Array(alphabetSize);
    let length = reader.bits(5);
    for (let symbol = 0; symbol < alphabetSize; symbol++) {
      for (;;) {
        if (length < 1 || length > MAX_CODE_LENGTH)
          corrupt("a code length is not 1 to 20");
        if (!reader.bit()) break;
        length += reader.bit() ? -1 : 1;
      }
      lengths[symbol] = length;
    }
    tables.push(huffmanTable(lengths));
  }

  // The symbols: runs of the front byte (RUNA, RUNB), move-to-front positions, end of block.
  const byteCounts = new Int32Array(256);
  let count = 0;
  let selector = 0;
  let groupLeft = 0;
  let table = tables[0];
  let run = 0;
  let runWeight = 1;
  for (;;) {
    if (groupLeft === 0) {
      if (selector >= selectors.length)
        corrupt("a block runs out of selectors");
      table = tables[selectors[selector++]];
      groupLeft = GROUP_SIZE;
    }
    groupLeft--;
    const symbol = decodeSymbol(reader, table);
    if (symbol <= 1) {
      if (runWeight >= MAX_RUN_WEIGHT) corrupt("a run is too long");
      run += runWeight << symbol;
      runWeight <<= 1;
      continue;
    }
    if (run > 0) {
      if (run > blockSize - count) corrupt("a block is longer than its size");
      const byte = moveToFront[0];
      byteCounts[byte] += run;
      block.fill(byte, count, count + run);
      count += run;
      run = 0;
      runWeight = 1;
    }
    if (symbol === endOfBlock) break;
    if (count >= blockSize) corrupt("a block is longer than its size");
    const position = symbol - 1;
    const byte = moveToFront[position];
    moveToFront.copyWithin(1, 0, position);
    moveToFront[0] = byte;
    byteCounts[byte]++;
    block[count++] = byte;
  }
  if (origin >= count) corrupt("a block's origin is outside it");

  // The inverse Burrows-Wheeler transform: each entry keeps its byte in the low 8 bits and gains
  // the index of the entry that follows it above them.
  const next = new Int32Array(256);
  for (let byte = 0, sum = 0; byte < 256; byte++) {
    next[byte] = sum;
    sum += byteCounts[byte];
  }
  for (let i = 0; i < count; i++) block[next[block[i] & 0xff]++] |= i << 8;

  // Undoing the run-length coding: four equal bytes are followed by a count of more.
  let crc = 0xffffffff;
  let last = -1;
  let equal = 0;
  let position = block[origin] >>> 8;
  output.reserve(count);
  for (let i = 0; i < count; i++) {
    const entry = block[position];
    const byte = entry & 0xff;
    position = entry >>> 8;
    if (equal === 4) {
      output.reserve(byte + count - i);
      const bytes = output.bytes;
      bytes.fill(last, output.length, output.length + byte);
      output.length += byte;
      for (let k = 0; k < byte; k++)
        crc = (crc << 8) ^ CRC_TABLE[((crc >>> 24) ^ last) & 0xff];
      equal = 0;
      continue;
    }
    if (byte === last) equal++;
    else {
      last = byte;
      equal = 1;
    }
    output.bytes[output.length++] = byte;
    crc = (crc << 8) ^ CRC_TABLE[((crc >>> 24) ^ byte) & 0xff];
  }
  return ~crc >>> 0;
}
