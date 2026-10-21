// RogueIM "send to anyone" (PRD HB-3): decrypts a RIMF file in the browser.
// The format is described in crates/rim-drop/src/lib.rs. Shared by the
// download page and its test.
"use strict";

const RIMF = (() => {
  const MAGIC = [0x52, 0x49, 0x4d, 0x46, 0x01];
  const CHUNK = 1 << 20;
  const TAG = 16;

  function nonce(kind, index) {
    const n = new Uint8Array(12);
    n[0] = kind;
    new DataView(n.buffer).setBigUint64(4, BigInt(index));
    return n;
  }

  function b64url(s) {
    const pad = s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4);
    return Uint8Array.from(atob(pad), (c) => c.charCodeAt(0));
  }

  // "#<file url>,<key>" -> {url, key}
  function parseHash(hash) {
    const h = decodeURIComponent((hash || "").replace(/^#/, ""));
    const i = h.lastIndexOf(",");
    if (i < 0) return null;
    const url = h.slice(0, i);
    let key;
    try { key = b64url(h.slice(i + 1)); } catch { return null; }
    if (key.length !== 32 || !/^https:\/\/|^http:\/\/(localhost|127\.0\.0\.1)[:/]/.test(url)) return null;
    return { url, key };
  }

  async function importKey(raw) {
    return crypto.subtle.importKey("raw", raw, "AES-GCM", false, ["decrypt"]);
  }

  async function open(key, data, kind, index) {
    try {
      return new Uint8Array(await crypto.subtle.decrypt({ name: "AES-GCM", iv: nonce(kind, index) }, key, data));
    } catch {
      throw new Error("wrong key or damaged file");
    }
  }

  // Reads a RIMF stream. onMeta(meta) is awaited before the first chunk (the
  // page asks where to save then); onChunk(bytes, done) gets the plain file.
  async function decryptStream(reader, rawKey, onMeta, onChunk) {
    const key = await importKey(rawKey);
    let buf = new Uint8Array(0);
    let ended = false;
    async function need(n) {
      while (buf.length < n && !ended) {
        const { value, done } = await reader.read();
        if (done) { ended = true; break; }
        const next = new Uint8Array(buf.length + value.length);
        next.set(buf); next.set(value, buf.length);
        buf = next;
      }
      if (buf.length < n) throw new Error("the file is incomplete");
    }
    function take(n) { const out = buf.slice(0, n); buf = buf.slice(n); return out; }

    await need(9);
    const head = take(9);
    if (!MAGIC.every((b, i) => head[i] === b)) throw new Error("not a RogueIM file");
    const metaLen = new DataView(head.buffer).getUint32(5);
    await need(metaLen);
    const meta = JSON.parse(new TextDecoder().decode(await open(key, take(metaLen), 2, 0)));
    await onMeta(meta);
    let left = meta.size;
    let index = 0;
    for (;;) {
      const plainLen = Math.min(left, CHUNK);
      const last = left <= CHUNK;
      await need(plainLen + TAG);
      const plain = await open(key, take(plainLen + TAG), last ? 1 : 0, index++);
      left -= plain.length;
      await onChunk(plain, last);
      if (last) return meta;
    }
  }

  return { parseHash, decryptStream, b64url };
})();

if (typeof module !== "undefined") module.exports = RIMF;
