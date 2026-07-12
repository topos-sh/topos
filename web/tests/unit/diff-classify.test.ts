import { describe, expect, it } from "vitest";
import { classifyBytes, decodeTextVerbatim } from "@/lib/diff/classify";

const utf8 = (s: string) => new TextEncoder().encode(s);

describe("classifyBytes", () => {
  it("valid UTF-8 is text", () => {
    expect(classifyBytes(utf8("hello\nworld\n"))).toBe("text");
  });

  it("empty bytes are text", () => {
    expect(classifyBytes(new Uint8Array(0))).toBe("text");
  });

  it("invalid UTF-8 is binary", () => {
    expect(classifyBytes(new Uint8Array([0xff, 0xfe, 0x41]))).toBe("binary");
  });

  it("a NUL byte in the first 8000 bytes is binary even when it decodes", () => {
    // U+0000 is VALID UTF-8 — the NUL gate is deliberately stricter than decodability.
    expect(classifyBytes(new Uint8Array([0x41, 0x00, 0x42]))).toBe("binary");
  });

  it("UTF-16LE text (NUL-interleaved) is binary", () => {
    const utf16 = new Uint8Array([0x68, 0x00, 0x69, 0x00]); // "hi" in UTF-16LE
    expect(classifyBytes(utf16)).toBe("binary");
  });

  it("a NUL exactly AT the scan cap (index 8000) is outside the NUL window", () => {
    const bytes = new Uint8Array(8001).fill(0x61);
    bytes[8000] = 0x00;
    // Past the window the NUL gate doesn't fire; U+0000 decodes, so this stays text.
    expect(classifyBytes(bytes)).toBe("text");
  });

  it("a NUL at index 7999 (the last scanned byte) is binary", () => {
    const bytes = new Uint8Array(8000).fill(0x61);
    bytes[7999] = 0x00;
    expect(classifyBytes(bytes)).toBe("binary");
  });

  it("a UTF-8 BOM is text", () => {
    expect(classifyBytes(new Uint8Array([0xef, 0xbb, 0xbf, 0x41]))).toBe("text");
  });
});

describe("decodeTextVerbatim", () => {
  it("preserves a UTF-8 BOM byte-for-byte", () => {
    const decoded = decodeTextVerbatim(new Uint8Array([0xef, 0xbb, 0xbf, 0x41]));
    expect(decoded).toBe("﻿A");
  });

  it("preserves CRLF verbatim", () => {
    const decoded = decodeTextVerbatim(utf8("a\r\nb\r\n"));
    expect(decoded).toBe("a\r\nb\r\n");
  });

  it("throws on invalid UTF-8 (fatal mode)", () => {
    expect(() => decodeTextVerbatim(new Uint8Array([0xc3]))).toThrow();
  });
});
