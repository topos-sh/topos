import { crc32 } from "node:zlib";

/**
 * A minimal, streaming ZIP writer — STORE method only (no compression). It exists so a
 * workspace export can assemble one archive from many vault objects WITHOUT buffering the whole
 * archive in memory: each entry's bytes are read once, framed, and released, and the archive is
 * emitted as a `ReadableStream` the HTTP layer streams to the client.
 *
 * STORE (not DEFLATE) is deliberate — skill bundles are small text + scripts, so the byte saving
 * would be marginal, and STORE keeps the format the simplest correct thing there is: the CRC-32
 * (via `node:zlib` — a checksum, no crypto) and the size are known from the in-hand bytes, so
 * every local header carries them directly and no data-descriptor trailer is needed. The central
 * directory and end-of-central-directory record are written after the last entry.
 *
 * Unix modes are preserved: the central header declares a Unix host and carries the entry's
 * `st_mode` in its external attributes, so a `100755` script extracts executable — a faithful
 * bundle export, not a flattened one.
 *
 * Memory: at most ONE entry's bytes live in memory at a time (the stream pulls entries lazily).
 * The only per-file retention is a small central-directory record (a fixed header + the path),
 * because the ZIP format writes the central directory last — so memory scales with file COUNT,
 * never with byte size. Bounds (no ZIP64): a single entry over 4 GiB, a cumulative archive over
 * 4 GiB, or over 65535 entries throws a typed error rather than emit a corrupt archive. A
 * skill-catalog export is far below all three.
 */

export interface ZipEntry {
  /** The archive path (forward slashes); a leading slash is stripped, a `..` component or a
   * backslash is REFUSED (see `archiveName`) — the writer never emits a traversal path. */
  path: string;
  bytes: Uint8Array;
  /** The Unix `st_mode` recorded in the central directory (default `0o100644`, a regular file). */
  mode?: number;
}

const LOCAL_SIG = 0x0403_4b50; // "PK\x03\x04"
const CENTRAL_SIG = 0x0201_4b50; // "PK\x01\x02"
const EOCD_SIG = 0x0605_4b50; // "PK\x05\x06"
const UINT32_MAX = 0xffff_ffff;
const UINT16_MAX = 0xffff;
/** General-purpose bit 11: the file name is UTF-8. */
const UTF8_FLAG = 0x0800;
const STORE = 0;
/** Version needed to extract: 2.0 (STORE, no ZIP64). */
const VERSION_20 = 20;
/** Version made by: host 3 (Unix) in the high byte, so extractors honor the external-attr mode. */
const VERSION_MADE_BY = (3 << 8) | VERSION_20;
/** The default entry mode — a regular, non-executable file (`0o100644`). */
const DEFAULT_MODE = 0o100644;

/** DOS date/time (two-second granularity) — the format ZIP's mod-time fields use. */
function dosDateTime(d: Date): { time: number; date: number } {
  const year = d.getFullYear();
  if (year < 1980) {
    // The epoch of the DOS date format: 1980-01-01 00:00:00.
    return { time: 0, date: (1 << 5) | 1 };
  }
  const time = (d.getHours() << 11) | (d.getMinutes() << 5) | (d.getSeconds() >> 1);
  const date = ((year - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate();
  return { time, date };
}

/**
 * The archive path, made relative and REFUSED if it could escape an extraction root. A leading
 * slash is stripped (absolute → relative). Then two traversal vectors are rejected outright rather
 * than rewritten — rewriting is what turns a safe name into a dangerous one:
 *  - a `..` path component (classic zip-slip);
 *  - a BACKSLASH anywhere. A backslash is a legal, ordinary character in a Unix filename (the
 *    trust kernel permits it), so a name like `..\\..\\x` is ONE safe component on Unix — but it
 *    is a SEPARATOR to Windows extractors, where it would traverse. Refusing it keeps the archive
 *    safe on every platform without silently reinterpreting the bytes.
 * A legitimate bundle path (a git tree entry) carries neither, so this never fires in practice; it
 * is a hard safety floor, not a transform.
 */
function archiveName(path: string): string {
  const relative = path.replace(/^\/+/, "");
  if (relative.length === 0) {
    throw new Error("zip: empty archive path");
  }
  if (relative.includes("\\")) {
    throw new Error(`zip: refusing a backslash in an archive path: ${path}`);
  }
  if (relative.split("/").some((segment) => segment === "..")) {
    throw new Error(`zip: refusing a traversal (..) archive path: ${path}`);
  }
  return relative;
}

function localHeader(name: Buffer, crc: number, size: number, time: number, date: number): Buffer {
  const h = Buffer.alloc(30 + name.length);
  h.writeUInt32LE(LOCAL_SIG, 0);
  h.writeUInt16LE(VERSION_20, 4);
  h.writeUInt16LE(UTF8_FLAG, 6);
  h.writeUInt16LE(STORE, 8);
  h.writeUInt16LE(time, 10);
  h.writeUInt16LE(date, 12);
  h.writeUInt32LE(crc, 14);
  h.writeUInt32LE(size, 18); // compressed size (STORE ⇒ == uncompressed)
  h.writeUInt32LE(size, 22); // uncompressed size
  h.writeUInt16LE(name.length, 26);
  h.writeUInt16LE(0, 28); // extra field length
  name.copy(h, 30);
  return h;
}

function centralHeader(
  name: Buffer,
  crc: number,
  size: number,
  offset: number,
  time: number,
  date: number,
  mode: number,
): Buffer {
  const h = Buffer.alloc(46 + name.length);
  h.writeUInt32LE(CENTRAL_SIG, 0);
  h.writeUInt16LE(VERSION_MADE_BY, 4); // version made by (host 3 = Unix)
  h.writeUInt16LE(VERSION_20, 6); // version needed to extract
  h.writeUInt16LE(UTF8_FLAG, 8);
  h.writeUInt16LE(STORE, 10);
  h.writeUInt16LE(time, 12);
  h.writeUInt16LE(date, 14);
  h.writeUInt32LE(crc, 16);
  h.writeUInt32LE(size, 20);
  h.writeUInt32LE(size, 24);
  h.writeUInt16LE(name.length, 28);
  h.writeUInt16LE(0, 30); // extra field length
  h.writeUInt16LE(0, 32); // file comment length
  h.writeUInt16LE(0, 34); // disk number start
  h.writeUInt16LE(0, 36); // internal file attributes
  // External attributes: the Unix st_mode in the high 16 bits (host = Unix), so the executable
  // bit and file type survive extraction.
  h.writeUInt32LE((mode << 16) >>> 0, 38);
  h.writeUInt32LE(offset, 42); // relative offset of the local header
  name.copy(h, 46);
  return h;
}

function endOfCentralDirectory(count: number, size: number, offset: number): Buffer {
  const h = Buffer.alloc(22);
  h.writeUInt32LE(EOCD_SIG, 0);
  h.writeUInt16LE(0, 4); // number of this disk
  h.writeUInt16LE(0, 6); // disk with the central directory
  h.writeUInt16LE(count, 8); // entries on this disk
  h.writeUInt16LE(count, 10); // total entries
  h.writeUInt32LE(size, 12); // central directory size
  h.writeUInt32LE(offset, 16); // central directory offset
  h.writeUInt16LE(0, 20); // comment length
  return h;
}

/** Emit the archive as an ordered chunk sequence, pulling entries lazily from `entries`. */
async function* zipChunks(
  entries: AsyncIterable<ZipEntry>,
  modified: Date,
): AsyncGenerator<Uint8Array> {
  const { time, date } = dosDateTime(modified);
  const central: Buffer[] = [];
  let offset = 0;
  let count = 0;
  for await (const entry of entries) {
    const bytes = entry.bytes;
    if (bytes.length > UINT32_MAX) {
      throw new Error("zip: entry exceeds the 4 GiB store limit (zip64 unsupported)");
    }
    const nameBytes = Buffer.from(archiveName(entry.path), "utf8");
    // CRC-32 is a checksum over the in-hand bytes — a data-integrity value, not a digest.
    const crc = crc32(bytes) >>> 0;
    const header = localHeader(nameBytes, crc, bytes.length, time, date);
    yield header;
    yield bytes;
    central.push(
      centralHeader(nameBytes, crc, bytes.length, offset, time, date, entry.mode ?? DEFAULT_MODE),
    );
    offset += header.length + bytes.length;
    count += 1;
    // A 32-bit offset field cannot address past 4 GiB — refuse before an overflowed offset would
    // land in the next central-directory record (a corrupt archive). Skill catalogs never approach
    // this; the guard keeps the failure a clean, typed one.
    if (offset > UINT32_MAX) {
      throw new Error("zip: archive exceeds 4 GiB (zip64 unsupported)");
    }
    if (count > UINT16_MAX) {
      throw new Error("zip: more than 65535 entries (zip64 unsupported)");
    }
  }
  const centralStart = offset;
  let centralSize = 0;
  for (const record of central) {
    yield record;
    centralSize += record.length;
  }
  yield endOfCentralDirectory(count, centralSize, centralStart);
}

/**
 * A `ReadableStream` of the STORE archive built from `entries`. The stream pulls one entry at a
 * time (backpressure-friendly) so the producer never materializes more than the current entry's
 * bytes plus the (small) central-directory metadata. `modified` stamps every entry's mod-time.
 */
export function zipStream(
  entries: AsyncIterable<ZipEntry>,
  modified: Date = new Date(),
): ReadableStream<Uint8Array> {
  const chunks = zipChunks(entries, modified);
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      try {
        const { value, done } = await chunks.next();
        if (done) {
          controller.close();
        } else {
          controller.enqueue(value);
        }
      } catch (err) {
        controller.error(err);
      }
    },
    async cancel() {
      await chunks.return(undefined);
    },
  });
}
