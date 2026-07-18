import { crc32 } from "node:zlib";

/**
 * A STORE-only ZIP reader for the export tests — the round-trip counterpart to
 * `app/lib/export/zip.server.ts`. It walks the End-Of-Central-Directory record, then each central
 * directory entry, follows the local-header offset to the stored bytes, and VERIFIES the CRC-32
 * the writer recorded. That makes it a real structural check (signatures, offsets, sizes, CRC),
 * not just a name lister — a misplaced byte or a wrong offset throws here.
 */

const LOCAL_SIG = 0x0403_4b50;
const CENTRAL_SIG = 0x0201_4b50;
const EOCD_SIG = 0x0605_4b50;

export interface UnzipEntry {
  bytes: Buffer;
  /** The Unix `st_mode` decoded from the central header's external attributes (0 if none). */
  mode: number;
}

export function unzipStore(archive: Uint8Array): Map<string, UnzipEntry> {
  const buf = Buffer.from(archive.buffer, archive.byteOffset, archive.byteLength);
  let eocd = -1;
  for (let i = buf.length - 22; i >= 0; i--) {
    if (buf.readUInt32LE(i) === EOCD_SIG) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) {
    throw new Error("unzip: no end-of-central-directory record");
  }
  const count = buf.readUInt16LE(eocd + 10);
  let ptr = buf.readUInt32LE(eocd + 16); // central directory offset
  const out = new Map<string, UnzipEntry>();
  for (let n = 0; n < count; n++) {
    if (buf.readUInt32LE(ptr) !== CENTRAL_SIG) {
      throw new Error("unzip: bad central directory signature");
    }
    const method = buf.readUInt16LE(ptr + 10);
    const crc = buf.readUInt32LE(ptr + 16);
    const size = buf.readUInt32LE(ptr + 24); // uncompressed size
    const nameLen = buf.readUInt16LE(ptr + 28);
    const extraLen = buf.readUInt16LE(ptr + 30);
    const commentLen = buf.readUInt16LE(ptr + 32);
    const externalAttrs = buf.readUInt32LE(ptr + 38);
    const localOff = buf.readUInt32LE(ptr + 42);
    const name = buf.toString("utf8", ptr + 46, ptr + 46 + nameLen);
    if (method !== 0) {
      throw new Error(`unzip: entry ${name} is not STORE`);
    }
    if (buf.readUInt32LE(localOff) !== LOCAL_SIG) {
      throw new Error(`unzip: bad local header signature for ${name}`);
    }
    const localNameLen = buf.readUInt16LE(localOff + 26);
    const localExtraLen = buf.readUInt16LE(localOff + 28);
    const dataStart = localOff + 30 + localNameLen + localExtraLen;
    const data = Buffer.from(buf.subarray(dataStart, dataStart + size));
    if (crc32(data) >>> 0 !== crc) {
      throw new Error(`unzip: CRC mismatch for ${name}`);
    }
    // The Unix st_mode lives in the high 16 bits of the external attributes.
    out.set(name, { bytes: data, mode: externalAttrs >>> 16 });
    ptr += 46 + nameLen + extraLen + commentLen;
  }
  return out;
}
