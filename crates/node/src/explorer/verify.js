// Boltchain evidence verification (issue #1): hashing, RLP, Merkle-Patricia tries, ABI, Merkle
// proof schemes and the verification steps behind the explorer's /verify page.
//
// Runs in three places, with no dependencies and no build step:
//   - the page (`<script src="/verify.js">`, exposes `BoltVerify`),
//   - a Worker (`new Worker("/verify.js")`), which hashes files off the main thread,
//   - Node, for tests (`require` or `import` it; it also sets `globalThis.BoltVerify`).
//
// Nothing here uploads anything: files are hashed locally and only their hashes are used.
// Every chain value the verdict depends on is checked against block hashes where the chain
// allows it (header RLP -> block hash, transaction and receipt tries -> header roots, state
// proofs -> stateRoot); what cannot be checked that way is marked as reported by the node.
(function (G) {
  "use strict";

  // ---------------------------------------------------------------------------------------
  // bytes and hex
  // ---------------------------------------------------------------------------------------
  const HEX = Array.from({ length: 256 }, (_, i) => i.toString(16).padStart(2, "0"));
  function toHex(b) {
    let s = "0x";
    for (let i = 0; i < b.length; i++) s += HEX[b[i]];
    return s;
  }
  function fromHex(h) {
    if (h instanceof Uint8Array) return h;
    if (typeof h !== "string" || !/^0x[0-9a-fA-F]*$/.test(h)) throw new Error(`not hex: ${String(h).slice(0, 80)}`);
    let s = h.slice(2);
    if (s.length % 2) s = "0" + s;
    const out = new Uint8Array(s.length / 2);
    for (let i = 0; i < out.length; i++) out[i] = parseInt(s.substr(i * 2, 2), 16);
    return out;
  }
  function concat(...parts) {
    let n = 0;
    for (const p of parts) n += p.length;
    const out = new Uint8Array(n);
    let o = 0;
    for (const p of parts) { out.set(p, o); o += p.length; }
    return out;
  }
  function eqBytes(a, b) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
  }
  const utf8 = (s) => new TextEncoder().encode(s);
  function bigToBytes(v, len) {
    const out = new Uint8Array(len);
    let x = v;
    for (let i = len - 1; i >= 0; i--) { out[i] = Number(x & 0xffn); x >>= 8n; }
    return out;
  }
  function bytesToBig(b) {
    let v = 0n;
    for (const x of b) v = (v << 8n) | BigInt(x);
    return v;
  }
  /// Parses an integer given as a JSON number, decimal string, "0x" hex string or bigint.
  function toBig(v) {
    if (typeof v === "bigint") return v;
    if (typeof v === "number") {
      if (!Number.isSafeInteger(v)) throw new Error(`unsafe integer ${v}; give it as a string`);
      return BigInt(v);
    }
    if (typeof v === "string" && /^-?(0x[0-9a-fA-F]+|[0-9]+)$/.test(v.trim())) {
      const s = v.trim();
      return s.startsWith("-") ? -BigInt(s.slice(1)) : BigInt(s);
    }
    throw new Error(`not an integer: ${JSON.stringify(v)}`);
  }

  // ---------------------------------------------------------------------------------------
  // keccak-256 (32-bit lanes, streaming)
  // ---------------------------------------------------------------------------------------
  const RC = [
    0x00000001, 0x00000000, 0x00008082, 0x00000000, 0x0000808a, 0x80000000, 0x80008000, 0x80000000,
    0x0000808b, 0x00000000, 0x80000001, 0x00000000, 0x80008081, 0x80000000, 0x00008009, 0x80000000,
    0x0000008a, 0x00000000, 0x00000088, 0x00000000, 0x80008009, 0x00000000, 0x8000000a, 0x00000000,
    0x8000808b, 0x00000000, 0x0000008b, 0x80000000, 0x00008089, 0x80000000, 0x00008003, 0x80000000,
    0x00008002, 0x80000000, 0x00000080, 0x80000000, 0x0000800a, 0x00000000, 0x8000000a, 0x80000000,
    0x80008081, 0x80000000, 0x00008080, 0x80000000, 0x80000001, 0x00000000, 0x80008008, 0x80000000,
  ]; // [lo, hi] per round
  const ROT = [0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61, 56, 14];
  // pi: lane i = x + 5y moves to y + 5 * ((2x + 3y) % 5)
  const PI = Array.from({ length: 25 }, (_, i) => { const x = i % 5, y = (i / 5) | 0; return y + 5 * ((2 * x + 3 * y) % 5); });

  // Lane tables for keccak-f (indices into the [lo, hi] state array).
  const RHO_SRC = new Int32Array(25), RHO_DST = new Int32Array(25), RHO_N = new Int32Array(25);
  for (let i = 0; i < 25; i++) { RHO_SRC[i] = 2 * i; RHO_DST[i] = 2 * PI[i]; RHO_N[i] = ROT[i]; }
  const KB = new Int32Array(50);
  function keccakF(s) {
    const b = KB;
    for (let r = 0; r < 48; r += 2) {
      // theta
      const c0 = s[0] ^ s[10] ^ s[20] ^ s[30] ^ s[40], c1 = s[1] ^ s[11] ^ s[21] ^ s[31] ^ s[41];
      const c2 = s[2] ^ s[12] ^ s[22] ^ s[32] ^ s[42], c3 = s[3] ^ s[13] ^ s[23] ^ s[33] ^ s[43];
      const c4 = s[4] ^ s[14] ^ s[24] ^ s[34] ^ s[44], c5 = s[5] ^ s[15] ^ s[25] ^ s[35] ^ s[45];
      const c6 = s[6] ^ s[16] ^ s[26] ^ s[36] ^ s[46], c7 = s[7] ^ s[17] ^ s[27] ^ s[37] ^ s[47];
      const c8 = s[8] ^ s[18] ^ s[28] ^ s[38] ^ s[48], c9 = s[9] ^ s[19] ^ s[29] ^ s[39] ^ s[49];
      const d0 = c8 ^ ((c2 << 1) | (c3 >>> 31)), d1 = c9 ^ ((c3 << 1) | (c2 >>> 31));
      const d2 = c0 ^ ((c4 << 1) | (c5 >>> 31)), d3 = c1 ^ ((c5 << 1) | (c4 >>> 31));
      const d4 = c2 ^ ((c6 << 1) | (c7 >>> 31)), d5 = c3 ^ ((c7 << 1) | (c6 >>> 31));
      const d6 = c4 ^ ((c8 << 1) | (c9 >>> 31)), d7 = c5 ^ ((c9 << 1) | (c8 >>> 31));
      const d8 = c6 ^ ((c0 << 1) | (c1 >>> 31)), d9 = c7 ^ ((c1 << 1) | (c0 >>> 31));
      for (let y = 0; y < 50; y += 10) {
        s[y] ^= d0; s[y + 1] ^= d1; s[y + 2] ^= d2; s[y + 3] ^= d3; s[y + 4] ^= d4;
        s[y + 5] ^= d5; s[y + 6] ^= d6; s[y + 7] ^= d7; s[y + 8] ^= d8; s[y + 9] ^= d9;
      }
      // rho + pi
      for (let i = 0; i < 25; i++) {
        const si = RHO_SRC[i], di = RHO_DST[i], n = RHO_N[i];
        const lo = s[si], hi = s[si + 1];
        if (n === 0) { b[di] = lo; b[di + 1] = hi; }
        else if (n < 32) { b[di + 1] = (hi << n) | (lo >>> (32 - n)); b[di] = (lo << n) | (hi >>> (32 - n)); }
        else if (n === 32) { b[di] = hi; b[di + 1] = lo; }
        else { const m = n - 32; b[di + 1] = (lo << m) | (hi >>> (32 - m)); b[di] = (hi << m) | (lo >>> (32 - m)); }
      }
      // chi
      for (let y = 0; y < 50; y += 10) {
        const b0 = b[y], b1 = b[y + 1], b2 = b[y + 2], b3 = b[y + 3], b4 = b[y + 4];
        const b5 = b[y + 5], b6 = b[y + 6], b7 = b[y + 7], b8 = b[y + 8], b9 = b[y + 9];
        s[y] = b0 ^ (~b2 & b4); s[y + 1] = b1 ^ (~b3 & b5);
        s[y + 2] = b2 ^ (~b4 & b6); s[y + 3] = b3 ^ (~b5 & b7);
        s[y + 4] = b4 ^ (~b6 & b8); s[y + 5] = b5 ^ (~b7 & b9);
        s[y + 6] = b6 ^ (~b8 & b0); s[y + 7] = b7 ^ (~b9 & b1);
        s[y + 8] = b8 ^ (~b0 & b2); s[y + 9] = b9 ^ (~b1 & b3);
      }
      // iota
      s[0] ^= RC[r]; s[1] ^= RC[r + 1];
    }
  }

  class Keccak256 {
    constructor() { this.s = new Int32Array(50); this.buf = new Uint8Array(136); this.pos = 0; }
    _absorb(d, o) {
      const s = this.s;
      for (let i = 0; i < 34; i++, o += 4) s[i] ^= d[o] | (d[o + 1] << 8) | (d[o + 2] << 16) | (d[o + 3] << 24);
      keccakF(s);
    }
    update(data) {
      let o = 0;
      const n = data.length;
      if (this.pos > 0) {
        const take = Math.min(136 - this.pos, n);
        this.buf.set(data.subarray(0, take), this.pos);
        this.pos += take; o = take;
        if (this.pos < 136) return this;
        this._absorb(this.buf, 0); this.pos = 0;
      }
      for (; o + 136 <= n; o += 136) this._absorb(data, o);
      if (o < n) { this.buf.set(data.subarray(o), 0); this.pos = n - o; }
      return this;
    }
    digest() {
      const blk = new Uint8Array(136);
      blk.set(this.buf.subarray(0, this.pos));
      blk[this.pos] ^= 0x01; blk[135] ^= 0x80;
      this._absorb(blk, 0);
      const out = new Uint8Array(32);
      for (let i = 0; i < 8; i++) { const w = this.s[i]; out[4 * i] = w; out[4 * i + 1] = w >>> 8; out[4 * i + 2] = w >>> 16; out[4 * i + 3] = w >>> 24; }
      return out;
    }
  }
  const keccak256 = (b) => new Keccak256().update(b instanceof Uint8Array ? b : fromHex(b)).digest();

  // ---------------------------------------------------------------------------------------
  // sha-256 (streaming)
  // ---------------------------------------------------------------------------------------
  const K256 = new Uint32Array([
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2]);
  class Sha256 {
    constructor() {
      this.h = new Uint32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
      this.w = new Uint32Array(64); this.buf = new Uint8Array(64); this.pos = 0; this.len = 0;
    }
    _block(d, o) {
      const w = this.w, h = this.h;
      for (let i = 0; i < 16; i++, o += 4) w[i] = (d[o] << 24) | (d[o + 1] << 16) | (d[o + 2] << 8) | d[o + 3];
      for (let i = 16; i < 64; i++) {
        const a = w[i - 15], b = w[i - 2];
        const s0 = ((a >>> 7) | (a << 25)) ^ ((a >>> 18) | (a << 14)) ^ (a >>> 3);
        const s1 = ((b >>> 17) | (b << 15)) ^ ((b >>> 19) | (b << 13)) ^ (b >>> 10);
        w[i] = (w[i - 16] + s0 + w[i - 7] + s1) | 0;
      }
      let A = h[0], B = h[1], C = h[2], D = h[3], E = h[4], F = h[5], Gg = h[6], H = h[7];
      for (let i = 0; i < 64; i++) {
        const S1 = ((E >>> 6) | (E << 26)) ^ ((E >>> 11) | (E << 21)) ^ ((E >>> 25) | (E << 7));
        const t1 = (H + S1 + ((E & F) ^ (~E & Gg)) + K256[i] + w[i]) | 0;
        const S0 = ((A >>> 2) | (A << 30)) ^ ((A >>> 13) | (A << 19)) ^ ((A >>> 22) | (A << 10));
        const t2 = (S0 + ((A & B) ^ (A & C) ^ (B & C))) | 0;
        H = Gg; Gg = F; F = E; E = (D + t1) | 0; D = C; C = B; B = A; A = (t1 + t2) | 0;
      }
      h[0] += A; h[1] += B; h[2] += C; h[3] += D; h[4] += E; h[5] += F; h[6] += Gg; h[7] += H;
    }
    update(data) {
      let o = 0; const n = data.length; this.len += n;
      if (this.pos > 0) {
        const take = Math.min(64 - this.pos, n);
        this.buf.set(data.subarray(0, take), this.pos); this.pos += take; o = take;
        if (this.pos < 64) return this;
        this._block(this.buf, 0); this.pos = 0;
      }
      for (; o + 64 <= n; o += 64) this._block(data, o);
      if (o < n) { this.buf.set(data.subarray(o), 0); this.pos = n - o; }
      return this;
    }
    digest() {
      const bits = this.len * 8;
      const padLen = this.pos < 56 ? 56 - this.pos : 120 - this.pos;
      const pad = new Uint8Array(padLen + 8); pad[0] = 0x80;
      const hi = Math.floor(bits / 2 ** 32), lo = bits >>> 0;
      pad.set([hi >>> 24, hi >>> 16, hi >>> 8, hi, lo >>> 24, lo >>> 16, lo >>> 8, lo].map((x) => x & 255), padLen);
      this.len -= 0; this.update(pad);
      const out = new Uint8Array(32);
      for (let i = 0; i < 8; i++) { const x = this.h[i]; out[4 * i] = x >>> 24; out[4 * i + 1] = x >>> 16; out[4 * i + 2] = x >>> 8; out[4 * i + 3] = x; }
      return out;
    }
  }
  const sha256 = (b) => new Sha256().update(b instanceof Uint8Array ? b : fromHex(b)).digest();

  // ---------------------------------------------------------------------------------------
  // RLP
  // ---------------------------------------------------------------------------------------
  function rlpLen(len, offset) {
    if (len < 56) return Uint8Array.of(offset + len);
    const lb = bigToBytes(BigInt(len), Math.ceil(len.toString(16).length / 2));
    return concat(Uint8Array.of(offset + 55 + lb.length), lb);
  }
  function rlpEncode(x) {
    if (Array.isArray(x)) {
      const payload = concat(...x.map(rlpEncode));
      return concat(rlpLen(payload.length, 0xc0), payload);
    }
    const b = x instanceof Uint8Array ? x : fromHex(x);
    if (b.length === 1 && b[0] < 0x80) return b;
    return concat(rlpLen(b.length, 0x80), b);
  }
  /// Decodes one RLP item. Strings come back as Uint8Array, lists as arrays; `raw` keeps the
  /// exact encoding of every list (needed to hash inline trie nodes).
  function rlpDecode(input) {
    const buf = input instanceof Uint8Array ? input : fromHex(input);
    const [item, end] = rlpItem(buf, 0);
    if (end !== buf.length) throw new Error("rlp: trailing bytes");
    return item;
  }
  function rlpItem(b, o) {
    if (o >= b.length) throw new Error("rlp: truncated");
    const p = b[o];
    const num = (s, n) => { if (n > 6) throw new Error("rlp: length too long"); let v = 0; for (let i = 0; i < n; i++) v = v * 256 + b[s + i]; return v; };
    let start, len, list;
    if (p < 0x80) return [b.subarray(o, o + 1), o + 1];
    if (p <= 0xb7) { start = o + 1; len = p - 0x80; list = false; }
    else if (p < 0xc0) { const n = p - 0xb7; start = o + 1 + n; len = num(o + 1, n); list = false; }
    else if (p <= 0xf7) { start = o + 1; len = p - 0xc0; list = true; }
    else { const n = p - 0xf7; start = o + 1 + n; len = num(o + 1, n); list = true; }
    const end = start + len;
    if (end > b.length) throw new Error("rlp: truncated");
    if (!list) return [b.subarray(start, end), end];
    const items = [];
    let q = start;
    while (q < end) { const [it, e] = rlpItem(b, q); items.push(it); q = e; }
    if (q !== end) throw new Error("rlp: bad list length");
    items.raw = b.subarray(o, end);
    return [items, end];
  }
  /// Minimal big-endian bytes of a non-negative integer (RLP's integer encoding).
  function rlpBig(v) {
    const x = toBig(v);
    if (x === 0n) return new Uint8Array(0);
    return bigToBytes(x, Math.ceil(x.toString(16).length / 2));
  }
  const rlpInt = (i) => (i === 0 ? new Uint8Array(0) : bigToBytes(BigInt(i), Math.ceil(i.toString(16).length / 2)));

  // ---------------------------------------------------------------------------------------
  // Merkle-Patricia trie: root of an ordered list, and proof verification
  // ---------------------------------------------------------------------------------------
  const EMPTY_TRIE = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421";
  const nibbles = (b) => { const n = new Uint8Array(b.length * 2); for (let i = 0; i < b.length; i++) { n[2 * i] = b[i] >> 4; n[2 * i + 1] = b[i] & 15; } return n; };
  function hexPrefix(nib, leaf) {
    const odd = nib.length % 2;
    const out = new Uint8Array(1 + (nib.length >> 1));
    out[0] = ((leaf ? 2 : 0) + odd) << 4 | (odd ? nib[0] : 0);
    for (let i = odd, j = 1; i < nib.length; i += 2, j++) out[j] = (nib[i] << 4) | nib[i + 1];
    return out;
  }
  function hexPrefixDecode(b) {
    const flag = b[0] >> 4;
    if (flag > 3) throw new Error("trie: bad hex-prefix");
    const n = [];
    if (flag & 1) n.push(b[0] & 15);
    for (let i = 1; i < b.length; i++) n.push(b[i] >> 4, b[i] & 15);
    return { path: Uint8Array.from(n), leaf: (flag & 2) === 2 };
  }
  /// Encoded node -> the reference its parent embeds (inline if shorter than 32 bytes).
  const nodeRef = (enc) => (enc.length < 32 ? { inline: enc } : keccak256(enc));
  function encodeNode(entries, depth) {
    // entries: [{ k: nibbles, v: bytes }], all sharing the first `depth` nibbles, keys unique.
    if (entries.length === 1) {
      const e = entries[0];
      return rlpEncode([hexPrefix(e.k.subarray(depth), true), e.v]);
    }
    let common = 0;
    for (;;) {
      const i = depth + common;
      if (entries.some((e) => e.k.length <= i)) break;
      const n0 = entries[0].k[i];
      if (entries.some((e) => e.k[i] !== n0)) break;
      common++;
    }
    const child = (enc) => { const r = nodeRef(enc); return r.inline ? rlpDecode(r.inline) : r; };
    if (common > 0) {
      const sub = encodeNode(entries, depth + common);
      return rlpEncode([hexPrefix(entries[0].k.subarray(depth, depth + common), false), child(sub)]);
    }
    const slots = new Array(17).fill(new Uint8Array(0));
    for (let n = 0; n < 16; n++) {
      const group = entries.filter((e) => e.k.length > depth && e.k[depth] === n);
      if (group.length) slots[n] = child(encodeNode(group, depth + 1));
    }
    const here = entries.find((e) => e.k.length === depth);
    if (here) slots[16] = here.v;
    return rlpEncode(slots);
  }
  /// Root of the trie of an ordered list (transactions, receipts): key i = rlp(i).
  function orderedTrieRoot(values) {
    if (!values.length) return EMPTY_TRIE;
    const entries = values.map((v, i) => ({ k: nibbles(rlpEncode(rlpInt(i))), v: v instanceof Uint8Array ? v : fromHex(v) }));
    return toHex(keccak256(encodeNode(entries, 0)));
  }
  /// Verifies an EIP-1186 proof of `key` (32-byte trie key, already hashed) against `root`.
  /// Returns the value (Uint8Array) when present, `null` when the proof shows absence, and
  /// throws when the proof does not match the root.
  function verifyProof(root, key, proof) {
    const want = nibbles(fromHex(key));
    const nodes = proof.map(fromHex);
    if (toHex(fromHex(root)) === EMPTY_TRIE && nodes.length === 0) return null;
    let expect = fromHex(root);
    let node = null;
    let k = 0, pi = 0;
    for (;;) {
      if (node === null) {
        if (pi >= nodes.length) throw new Error("proof: ran out of nodes");
        const raw = nodes[pi++];
        if (!eqBytes(keccak256(raw), expect)) throw new Error(`proof: node ${pi - 1} does not hash to its reference`);
        node = rlpDecode(raw);
      }
      if (!Array.isArray(node)) throw new Error("proof: node is not a list");
      let next;
      if (node.length === 17) {
        if (k === want.length) return node[16].length ? node[16] : null;
        next = node[want[k++]];
      } else if (node.length === 2) {
        const { path, leaf } = hexPrefixDecode(node[0]);
        const rest = want.subarray(k);
        if (leaf) return eqBytes(path, rest) ? node[1] : null;
        if (rest.length < path.length || !eqBytes(path, rest.subarray(0, path.length))) return null;
        k += path.length;
        next = node[1];
      } else throw new Error("proof: bad node");
      if (Array.isArray(next)) { node = next; continue; } // inline child
      if (next.length === 0) return null;
      if (next.length !== 32) throw new Error("proof: bad child reference");
      expect = next; node = null;
    }
  }

  // ---------------------------------------------------------------------------------------
  // headers, transactions, receipts
  // ---------------------------------------------------------------------------------------
  const HEADER_FIELDS = ["parentHash", "ommersHash", "beneficiary", "stateRoot", "transactionsRoot", "receiptsRoot", "logsBloom",
    "difficulty", "number", "gasLimit", "gasUsed", "timestamp", "extraData", "mixHash", "nonce", "baseFeePerGas",
    "withdrawalsRoot", "blobGasUsed", "excessBlobGas", "parentBeaconBlockRoot", "requestsHash"];
  const HEADER_INTS = new Set(["difficulty", "number", "gasLimit", "gasUsed", "timestamp", "baseFeePerGas", "blobGasUsed", "excessBlobGas"]);
  function decodeHeader(raw) {
    const items = rlpDecode(raw);
    const h = { hash: toHex(keccak256(fromHex(raw))) };
    items.forEach((v, i) => { const f = HEADER_FIELDS[i] || `field${i}`; h[f] = HEADER_INTS.has(f) ? bytesToBig(v) : toHex(v); });
    return h;
  }
  /// EIP-2718 receipt -> { status, logs: [{ address, topics, data }] }.
  function decodeReceipt(raw) {
    let b = fromHex(raw);
    let type = 0;
    if (b[0] < 0x7f) { type = b[0]; b = b.subarray(1); }
    const r = rlpDecode(b);
    return {
      type,
      status: bytesToBig(r[0]) === 1n,
      cumulativeGasUsed: bytesToBig(r[1]),
      logs: r[3].map((l) => ({ address: toHex(l[0]), topics: l[1].map(toHex), data: toHex(l[2]) })),
    };
  }

  // ---------------------------------------------------------------------------------------
  // ABI: human-readable signatures, abi.encode / decode, abi.encodePacked
  // ---------------------------------------------------------------------------------------
  /// Parses a parameter list like "uint64 indexed epoch, (bytes32 a, uint256[] b) c".
  function parseParams(src) {
    let i = 0;
    const s = src.trim();
    const ws = () => { while (i < s.length && /\s/.test(s[i])) i++; };
    function parseType() {
      ws();
      let t;
      if (s[i] === "(" || s.startsWith("tuple(", i)) {
        if (s[i] !== "(") i += 5;
        i++; // (
        const comps = [];
        ws();
        if (s[i] === ")") i++;
        else for (;;) {
          const p = parseParam(false);
          comps.push(p);
          ws();
          if (s[i] === ",") { i++; continue; }
          if (s[i] === ")") { i++; break; }
          throw new Error(`abi: expected , or ) at ${i} in ${s}`);
        }
        t = { kind: "tuple", components: comps };
      } else {
        const m = /^[a-z]+[0-9]*/.exec(s.slice(i));
        if (!m) throw new Error(`abi: expected a type at ${i} in ${s}`);
        i += m[0].length;
        t = basicType(m[0]);
      }
      for (;;) {
        const m = /^\[([0-9]*)\]/.exec(s.slice(i));
        if (!m) break;
        i += m[0].length;
        t = { kind: "array", inner: t, length: m[1] === "" ? null : Number(m[1]) };
      }
      return t;
    }
    function parseParam(allowIndexed) {
      const type = parseType();
      ws();
      let indexed = false, name = "";
      let m = /^[A-Za-z_$][A-Za-z0-9_$]*/.exec(s.slice(i));
      if (m && m[0] === "indexed" && allowIndexed) { indexed = true; i += 7; ws(); m = /^[A-Za-z_$][A-Za-z0-9_$]*/.exec(s.slice(i)); }
      if (m && !["indexed"].includes(m[0])) { name = m[0]; i += m[0].length; }
      return { type, name, indexed };
    }
    const out = [];
    ws();
    if (i >= s.length) return out;
    for (;;) {
      out.push(parseParam(true));
      ws();
      if (i >= s.length) break;
      if (s[i] !== ",") throw new Error(`abi: expected , at ${i} in ${s}`);
      i++;
    }
    return out;
  }
  function basicType(n) {
    if (n === "address" || n === "bool" || n === "string" || n === "bytes") return { kind: n };
    let m = /^(u?int)([0-9]*)$/.exec(n);
    if (m) {
      const bits = m[2] ? Number(m[2]) : 256;
      if (bits < 8 || bits > 256 || bits % 8) throw new Error(`abi: bad type ${n}`);
      return { kind: m[1], bits };
    }
    m = /^bytes([0-9]+)$/.exec(n);
    if (m && Number(m[1]) >= 1 && Number(m[1]) <= 32) return { kind: "fixedbytes", size: Number(m[1]) };
    throw new Error(`abi: unsupported type ${n}`);
  }
  /// "Name(params)" -> { name, params, canonical, selector (4 bytes hex), topic (32 bytes hex) }.
  function parseSignature(sig) {
    const s = sig.trim().replace(/^(event|function)\s+/, "");
    const open = s.indexOf("(");
    if (open <= 0 || !s.endsWith(")")) throw new Error(`abi: bad signature ${sig}`);
    const name = s.slice(0, open).trim();
    // Split off a trailing "returns (...)" if present.
    let body = s.slice(open + 1);
    let returns = null;
    const r = /\)\s*(?:external\s+|public\s+|view\s+|pure\s+)*returns\s*\((.*)\)$/.exec(body);
    if (r) { returns = parseParams(r[1]); body = body.slice(0, r.index); } else body = body.slice(0, -1);
    const params = parseParams(body);
    const canonical = `${name}(${params.map((p) => canon(p.type)).join(",")})`;
    const h = keccak256(utf8(canonical));
    return { name, params, returns, canonical, selector: toHex(h.subarray(0, 4)), topic: toHex(h) };
  }
  function canon(t) {
    switch (t.kind) {
      case "uint": case "int": return `${t.kind}${t.bits}`;
      case "fixedbytes": return `bytes${t.size}`;
      case "tuple": return `(${t.components.map((c) => canon(c.type)).join(",")})`;
      case "array": return `${canon(t.inner)}[${t.length ?? ""}]`;
      default: return t.kind;
    }
  }
  const isDynamic = (t) => t.kind === "bytes" || t.kind === "string" || (t.kind === "array" && (t.length === null || isDynamic(t.inner))) || (t.kind === "tuple" && t.components.some((c) => isDynamic(c.type)));
  const headSize = (t) => (isDynamic(t) ? 32 : t.kind === "tuple" ? t.components.reduce((a, c) => a + headSize(c.type), 0) : t.kind === "array" ? t.length * headSize(t.inner) : 32);
  const pad32 = (b) => { const out = new Uint8Array(Math.ceil(b.length / 32) * 32); out.set(b); return out; };
  function encodeWord(t, v) {
    switch (t.kind) {
      case "uint": {
        const x = toBig(v);
        if (x < 0n || x >> BigInt(t.bits) !== 0n) throw new Error(`abi: ${x} out of range for uint${t.bits}`);
        return bigToBytes(x, 32);
      }
      case "int": {
        const x = toBig(v);
        const lim = 1n << BigInt(t.bits - 1);
        if (x < -lim || x >= lim) throw new Error(`abi: ${x} out of range for int${t.bits}`);
        return bigToBytes(BigInt.asUintN(256, x), 32);
      }
      case "address": {
        const b = fromHex(v);
        if (b.length !== 20) throw new Error(`abi: bad address ${v}`);
        return concat(new Uint8Array(12), b);
      }
      case "bool": return bigToBytes(v === true || v === "true" || v === 1 || v === "1" ? 1n : 0n, 32);
      case "fixedbytes": {
        const b = fromHex(v);
        if (b.length !== t.size) throw new Error(`abi: expected ${t.size} bytes, got ${b.length}`);
        return pad32(b);
      }
      default: throw new Error(`abi: ${t.kind} is not a word`);
    }
  }
  function tupleValues(comps, v) {
    if (Array.isArray(v)) return v;
    if (v && typeof v === "object") return comps.map((c, i) => (c.name && c.name in v ? v[c.name] : v[i]));
    throw new Error("abi: expected an array or object for a tuple");
  }
  function abiEncodeTuple(types, values) {
    if (values.length !== types.length) throw new Error(`abi: expected ${types.length} values, got ${values.length}`);
    const heads = [], tails = [];
    let tailLen = 0;
    const headLen = types.reduce((a, t) => a + headSize(t), 0);
    types.forEach((t, i) => {
      const enc = abiEncodeOne(t, values[i]);
      if (isDynamic(t)) { heads.push(bigToBytes(BigInt(headLen + tailLen), 32)); tails.push(enc); tailLen += enc.length; }
      else heads.push(enc);
    });
    return concat(...heads, ...tails);
  }
  function abiEncodeOne(t, v) {
    switch (t.kind) {
      case "bytes": case "string": {
        const b = t.kind === "string" ? utf8(String(v)) : fromHex(v);
        return concat(bigToBytes(BigInt(b.length), 32), pad32(b));
      }
      case "tuple": return abiEncodeTuple(t.components.map((c) => c.type), tupleValues(t.components, v));
      case "array": {
        if (!Array.isArray(v)) throw new Error("abi: expected an array");
        if (t.length !== null && v.length !== t.length) throw new Error(`abi: expected ${t.length} elements`);
        const body = abiEncodeTuple(v.map(() => t.inner), v);
        return t.length === null ? concat(bigToBytes(BigInt(v.length), 32), body) : body;
      }
      default: return encodeWord(t, v);
    }
  }
  const typeList = (types) => types.map((t) => (typeof t === "string" ? parseParams(t)[0].type : t.type || t));
  /// abi.encode(values) for types given as strings ("uint256", "(bytes32,uint64)") or parsed.
  const abiEncode = (types, values) => abiEncodeTuple(typeList(types), values);
  /// abi.encodePacked: words at their natural width, bytes/string raw, array elements padded.
  function encodePacked(types, values) {
    const ts = typeList(types);
    if (ts.length !== values.length) throw new Error(`packed: expected ${ts.length} values, got ${values.length}`);
    return concat(...ts.map((t, i) => packedOne(t, values[i], false)));
  }
  function packedOne(t, v, inArray) {
    switch (t.kind) {
      case "bytes": return fromHex(v);
      case "string": return utf8(String(v));
      case "array": return concat(...v.map((x) => packedOne(t.inner, x, true)));
      case "tuple": throw new Error("packed: tuples are not supported by abi.encodePacked");
      default: {
        const w = encodeWord(t, v);
        if (inArray) return w;
        if (t.kind === "uint" || t.kind === "int") return w.subarray(32 - t.bits / 8);
        if (t.kind === "address") return w.subarray(12);
        if (t.kind === "bool") return w.subarray(31);
        return w.subarray(0, t.size); // fixedbytes
      }
    }
  }
  function abiDecodeTuple(comps, data, base) {
    const out = [];
    let o = base;
    for (const c of comps) {
      if (isDynamic(c.type)) {
        const off = Number(bytesToBig(word(data, o)));
        out.push(abiDecodeOne(c.type, data, base + off));
      } else out.push(abiDecodeOne(c.type, data, o));
      o += headSize(c.type);
    }
    const named = {};
    comps.forEach((c, i) => { named[i] = out[i]; if (c.name) named[c.name] = out[i]; });
    Object.defineProperty(named, "length", { value: out.length, enumerable: false });
    return named;
  }
  function word(d, o) {
    if (o + 32 > d.length) throw new Error("abi: data too short");
    return d.subarray(o, o + 32);
  }
  function abiDecodeOne(t, d, o) {
    switch (t.kind) {
      case "uint": return bytesToBig(word(d, o));
      case "int": return BigInt.asIntN(t.bits, bytesToBig(word(d, o)));
      case "address": return toHex(word(d, o).subarray(12));
      case "bool": return bytesToBig(word(d, o)) !== 0n;
      case "fixedbytes": return toHex(word(d, o).subarray(0, t.size));
      case "bytes": case "string": {
        const n = Number(bytesToBig(word(d, o)));
        if (o + 32 + n > d.length) throw new Error("abi: data too short");
        const b = d.subarray(o + 32, o + 32 + n);
        return t.kind === "string" ? new TextDecoder().decode(b) : toHex(b);
      }
      case "tuple": return abiDecodeTuple(t.components, d, o);
      case "array": {
        let n = t.length, start = o;
        if (n === null) { n = Number(bytesToBig(word(d, o))); start = o + 32; }
        const arr = abiDecodeTuple(Array.from({ length: n }, () => ({ type: t.inner, name: "" })), d, start);
        return Array.from({ length: n }, (_, i) => arr[i]);
      }
    }
    throw new Error(`abi: cannot decode ${t.kind}`);
  }
  const abiDecode = (types, data) => abiDecodeTuple(typeList(types).map((type) => ({ type, name: "" })), fromHex(data), 0);

  /// Decodes an event log against a signature. Returns { name, args } where indexed dynamic
  /// parameters are their topic (a hash), as the chain only stores that.
  function decodeLog(sig, log) {
    const ev = typeof sig === "string" ? parseSignature(sig) : sig;
    if (!log.topics.length || log.topics[0].toLowerCase() !== ev.topic) {
      throw new Error(`log topic0 ${log.topics[0] || "(none)"} is not ${ev.canonical} (${ev.topic})`);
    }
    const indexed = ev.params.filter((p) => p.indexed);
    if (log.topics.length !== indexed.length + 1) throw new Error(`log has ${log.topics.length - 1} indexed topics, the signature ${indexed.length}`);
    const data = abiDecodeTuple(ev.params.filter((p) => !p.indexed), fromHex(log.data), 0);
    const args = {};
    let ti = 1, di = 0;
    ev.params.forEach((p, i) => {
      let v;
      if (p.indexed) {
        const topic = fromHex(log.topics[ti++]);
        v = isDynamic(p.type) || p.type.kind === "tuple" || p.type.kind === "array" ? toHex(topic) : abiDecodeOne(p.type, topic, 0);
      } else v = data[di++];
      args[i] = v;
      if (p.name) args[p.name] = v;
    });
    return { name: ev.name, canonical: ev.canonical, args };
  }
  /// Resolves "commitment.balanceRoot" / "2.3" in decoded values.
  function getField(values, path) {
    let v = values;
    for (const part of String(path).split(".")) {
      if (v === null || typeof v !== "object" || !(part in v)) throw new Error(`field "${path}": no "${part}"`);
      v = v[part];
    }
    return v;
  }

  // ---------------------------------------------------------------------------------------
  // Merkle proof schemes
  // ---------------------------------------------------------------------------------------
  const B1_00 = pad32(Uint8Array.of(0x00)), B1_01 = pad32(Uint8Array.of(0x01)); // abi.encode(bytes1(..))
  /// Parses `path`: a bitmap (number, decimal or 0x string; bit i = 1 means sibling i is on the
  /// left) or an array of 0/1 in the same sense.
  function pathBits(path, n) {
    if (Array.isArray(path)) {
      if (path.length !== n) throw new Error(`path has ${path.length} entries, there are ${n} siblings`);
      return path.map((x) => { if (x !== 0 && x !== 1 && x !== "0" && x !== "1") throw new Error("path entries must be 0 or 1"); return Number(x) === 1; });
    }
    const bm = toBig(path ?? 0);
    if (bm < 0n || bm >> BigInt(n) !== 0n) throw new Error(`path bitmap has bits beyond the ${n} siblings`);
    return Array.from({ length: n }, (_, i) => ((bm >> BigInt(i)) & 1n) === 1n);
  }
  const SCHEMES = {
    "prefixed-abi-v1": {
      title: "keccak256(abi.encode(bytes1(0x00), contentHash)) / keccak256(abi.encode(bytes1(0x01), left, right))",
      leaf(content) {
        const h = content.hash ? fromHex(content.hash) : keccak256(abiEncode(content.types, content.values));
        if (h.length !== 32) throw new Error("contentHash must be 32 bytes");
        return { hash: keccak256(concat(B1_00, h)), contentHash: toHex(h) };
      },
      node: (l, r) => ({ hash: keccak256(concat(B1_01, l.hash, r.hash)) }),
    },
    "prefixed-packed-v1": {
      title: "keccak256(0x00 ‖ abi.encodePacked(leaf)) / keccak256(0x01 ‖ left ‖ right)",
      leaf(content) {
        const body = content.hash ? fromHex(content.hash) : encodePacked(content.types, content.values);
        return { hash: keccak256(concat(Uint8Array.of(0), body)) };
      },
      node: (l, r) => ({ hash: keccak256(concat(Uint8Array.of(1), l.hash, r.hash)) }),
    },
    "sum-packed-v1": {
      title: "Merkle sum tree: keccak256(0x00 ‖ abi.encodePacked(leaf)) / keccak256(0x01 ‖ L.hash ‖ L.sums ‖ R.hash ‖ R.sums), sums add up",
      sums: true,
      leaf(content, leaf) {
        if (!content.types) throw new Error("sum-packed-v1 needs leaf.types and leaf.values");
        if (!Array.isArray(leaf.sums) || !leaf.sums.length) throw new Error("sum-packed-v1 needs leaf.sums (indexes of the amounts in leaf.values)");
        const sums = leaf.sums.map((i) => {
          const t = typeList([content.types[i]])[0];
          if (t.kind !== "uint" || t.bits !== 256) throw new Error(`leaf.sums: value ${i} must be a uint256`);
          return toBig(content.values[i]);
        });
        return { hash: keccak256(concat(Uint8Array.of(0), encodePacked(content.types, content.values))), sums };
      },
      node(l, r) {
        if (l.sums.length !== r.sums.length) throw new Error("siblings carry a different number of sums");
        const sums = l.sums.map((x, i) => {
          if (x < 0n || r.sums[i] < 0n) throw new Error("negative sum");
          const s = x + r.sums[i];
          if (s >> 256n) throw new Error("sum overflows uint256");
          return s;
        });
        const w = (n) => concat(n.hash, ...n.sums.map((s) => bigToBytes(s, 32)));
        return { hash: keccak256(concat(Uint8Array.of(1), w(l), w(r))), sums };
      },
    },
    "oz-standard-v1": {
      title: "OpenZeppelin StandardMerkleTree: keccak256(keccak256(abi.encode(leaf))), sorted pairs",
      sorted: true,
      leaf(content) {
        if (!content.types) throw new Error("oz-standard-v1 needs leaf.types and leaf.values");
        return { hash: keccak256(keccak256(abiEncode(content.types, content.values))) };
      },
      node(a, b) {
        const [x, y] = toHex(a.hash) <= toHex(b.hash) ? [a, b] : [b, a];
        return { hash: keccak256(concat(x.hash, y.hash)) };
      },
    },
  };
  /// Recomputes the root of a Merkle proof. `content` is the leaf content resolved from the
  /// proof file ({ hash } or { types, values }). Returns { leaf, root, steps }.
  function merkleRoot(scheme, content, leafSpec, siblings, path) {
    const S = SCHEMES[scheme];
    if (!S) throw new Error(`unknown scheme "${scheme}" (supported: ${Object.keys(SCHEMES).join(", ")})`);
    if (!Array.isArray(siblings)) throw new Error("siblings must be an array");
    const leaf = S.leaf(content, leafSpec);
    const sib = siblings.map((s, i) => {
      if (S.sums) {
        if (!s || typeof s !== "object" || !Array.isArray(s.sums)) throw new Error(`sibling ${i}: expected { hash, sums }`);
        return { hash: fromHex(s.hash), sums: s.sums.map(toBig) };
      }
      return { hash: fromHex(typeof s === "object" && s ? s.hash : s) };
    });
    sib.forEach((s, i) => { if (s.hash.length !== 32) throw new Error(`sibling ${i}: hash must be 32 bytes`); });
    const left = S.sorted ? null : pathBits(path, sib.length);
    let cur = leaf;
    for (let i = 0; i < sib.length; i++) cur = S.sorted ? S.node(cur, sib[i]) : left[i] ? S.node(sib[i], cur) : S.node(cur, sib[i]);
    return { leaf: { hash: toHex(leaf.hash), sums: leaf.sums, contentHash: leaf.contentHash }, root: { hash: toHex(cur.hash), sums: cur.sums } };
  }

  // ---------------------------------------------------------------------------------------
  // canonical JSON and report hash
  // ---------------------------------------------------------------------------------------
  /// JSON with sorted keys and bigints as decimal strings: the bytes the report hash covers.
  function canonicalJson(v) {
    if (typeof v === "bigint") return JSON.stringify(v.toString());
    if (v instanceof Uint8Array) return JSON.stringify(toHex(v));
    if (Array.isArray(v)) return `[${v.map(canonicalJson).join(",")}]`;
    if (v && typeof v === "object") {
      return `{${Object.keys(v).filter((k) => v[k] !== undefined).sort().map((k) => `${JSON.stringify(k)}:${canonicalJson(v[k])}`).join(",")}}`;
    }
    return JSON.stringify(v ?? null);
  }
  /// Plain JSON form of decoded values (bigints -> strings, named tuple members only).
  function plain(v) {
    if (typeof v === "bigint") return v.toString();
    if (v instanceof Uint8Array) return toHex(v);
    if (Array.isArray(v)) return v.map(plain);
    if (v && typeof v === "object") {
      const keys = Object.keys(v);
      const named = keys.filter((k) => !/^[0-9]+$/.test(k));
      if (!named.length) return Array.from({ length: v.length ?? keys.length }, (_, i) => plain(v[i]));
      const o = {};
      for (const k of named) o[k] = plain(v[k]);
      return o;
    }
    return v;
  }
  const reportHash = (report) => { const r = { ...report }; delete r.reportHash; return toHex(keccak256(utf8(canonicalJson(r)))); };

  // ---------------------------------------------------------------------------------------
  // file hashing (in a Worker when available)
  // ---------------------------------------------------------------------------------------
  const CHUNK = 4 << 20;
  async function hashBlob(blob, onProgress) {
    const k = new Keccak256(), s = new Sha256();
    for (let o = 0; o < blob.size; o += CHUNK) {
      const part = new Uint8Array(await blob.slice(o, o + CHUNK).arrayBuffer());
      k.update(part); s.update(part);
      if (onProgress) onProgress(Math.min(o + CHUNK, blob.size) / blob.size);
    }
    return { size: blob.size, keccak256: toHex(k.digest()), sha256: toHex(s.digest()) };
  }
  /// Hashes a File/Blob with keccak-256 and sha-256. Uses a Worker running this same script
  /// when one can be started, so large files do not freeze the page.
  function hashFile(file, onProgress, scriptUrl) {
    if (typeof Worker === "undefined" || !scriptUrl) return hashBlob(file, onProgress);
    return new Promise((resolve, reject) => {
      let w;
      try { w = new Worker(scriptUrl); } catch (_) { hashBlob(file, onProgress).then(resolve, reject); return; }
      w.onmessage = (e) => {
        const m = e.data;
        if (m.progress !== undefined) { if (onProgress) onProgress(m.progress); return; }
        w.terminate();
        if (m.error) reject(new Error(m.error)); else resolve(m.result);
      };
      w.onerror = (e) => { w.terminate(); hashBlob(file, onProgress).then(resolve, reject); e.preventDefault && e.preventDefault(); };
      w.postMessage({ file });
    });
  }

  // ---------------------------------------------------------------------------------------
  // evidence verification
  // ---------------------------------------------------------------------------------------
  const lc = (x) => String(x).toLowerCase();
  const isBytes32 = (x) => typeof x === "string" && /^0x[0-9a-fA-F]{64}$/.test(x);
  const addrOk = (x) => typeof x === "string" && /^0x[0-9a-fA-F]{40}$/.test(x);
  // Candidate base slots of an ERC-20 `balances` mapping: plain layouts put it in one of the
  // first slots; OpenZeppelin's upgradeable ERC20 keeps it at its ERC-7201 namespace.
  const OZ_ERC20_NAMESPACE = "0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00";
  const mappingSlot = (holder, base) => toHex(keccak256(concat(abiEncode(["address"], [holder]), bigToBytes(toBig(base), 32))));

  /// Verifies the header a node served: its keccak must be the block hash it claims.
  function checkHeader(check, block, label) {
    const h = decodeHeader(block.header);
    check(`${label}.header`, lc(h.hash) === lc(block.hash) && h.number === BigInt(block.number),
      `keccak256(header) = ${h.hash}, block ${h.number}`, "verified");
    return h;
  }
  /// Verifies an account proof against a header's stateRoot; returns the account.
  function verifiedAccount(check, header, p, label) {
    const want = rlpEncode([rlpBig(p.nonce), rlpBig(p.balance), fromHex(p.storageHash), fromHex(p.codeHash)]);
    let got;
    try { got = verifyProof(header.stateRoot, toHex(keccak256(fromHex(p.address))), p.accountProof); }
    catch (e) { check(`${label}.account`, false, e.message, "verified"); return null; }
    const ok = got !== null && eqBytes(got, want);
    check(`${label}.account`, ok, ok ? `account ${p.address} proven against stateRoot ${header.stateRoot}` : `account proof of ${p.address} does not match`, "verified");
    return ok ? p : null;
  }

  async function resolveLogAnchor(A, fetchJson, check, chain) {
    if (!isBytes32(A.txHash)) throw new Error("anchor.txHash: expected a 32-byte transaction hash");
    const ev = A.event ? parseSignature(A.event) : null;
    const tx = await fetchJson(`tx/${A.txHash}`);
    if (tx.pending) throw new Error("the transaction is still pending");
    const raw = await fetchJson(`raw/block/${tx.block}`);
    const H = checkHeader(check, raw, "block");
    check("block.transactions", lc(orderedTrieRoot(raw.transactions)) === lc(H.transactionsRoot), `transactionsRoot ${H.transactionsRoot}`, "verified");
    const txi = Number(tx.index);
    check("tx.inBlock", !!raw.transactions[txi] && lc(toHex(keccak256(fromHex(raw.transactions[txi])))) === lc(A.txHash), `transaction ${txi} of block ${raw.number} hashes to ${A.txHash}`, "verified");
    check("block.receipts", lc(orderedTrieRoot(raw.receipts)) === lc(H.receiptsRoot), `receiptsRoot ${H.receiptsRoot}`, "verified");
    const receipts = raw.receipts.map(decodeReceipt);
    const rc = receipts[txi];
    if (!rc) throw new Error("receipt missing");
    check("tx.success", rc.status, rc.status ? "the transaction succeeded" : "the transaction reverted", "verified");
    let first = 0;
    for (let i = 0; i < txi; i++) first += receipts[i].logs.length;
    let li;
    if (A.logIndex !== undefined && A.logIndex !== null) {
      li = Number(A.logIndex) - first;
      if (li < 0 || li >= rc.logs.length) throw new Error(`log ${A.logIndex} is not in this transaction (its logs are ${first}..${first + rc.logs.length - 1} in the block)`);
    } else {
      if (!ev) throw new Error("anchor.logIndex is required when anchor.event is not given");
      li = rc.logs.findIndex((l) => l.topics[0] && lc(l.topics[0]) === ev.topic && (!A.contract || lc(l.address) === lc(A.contract)));
      if (li < 0) throw new Error(`the transaction has no ${ev.canonical} log`);
    }
    const log = rc.logs[li];
    if (A.contract) check("log.contract", lc(log.address) === lc(A.contract), `log emitted by ${log.address}`, "verified");
    // Without a signature the log is read as raw 32-byte words: topics.N and data.N.
    const d = ev ? decodeLog(ev, log) : { args: { topics: log.topics, data: dataWords(log.data) } };
    if (ev) check("log.event", true, `${ev.canonical}, topic ${ev.topic}`, "verified");
    chain.anchor = {
      type: "log",
      block: { number: raw.number, hash: raw.hash, timestamp: H.timestamp.toString(), final: raw.final, header: raw.header },
      transaction: { hash: A.txHash, index: txi, raw: raw.transactions[txi] },
      receipt: raw.receipts[txi],
      log: { index: first + li, address: log.address, topics: log.topics, data: log.data },
      event: ev ? ev.canonical : null,
      decoded: plain(d.args),
    };
    check("block.final", !!raw.final, raw.final ? "the block is final" : "the block is not final yet", "node", false);
    return { values: d.args, contract: log.address };
  }

  const dataWords = (data) => { const b = fromHex(data); const w = []; for (let o = 0; o + 32 <= b.length; o += 32) w.push(toHex(b.subarray(o, o + 32))); return w; };

  /// Finds where any of `hashes` appears as a 32-byte word in the logs of a transaction.
  /// Only locates candidates (from the node's data); verifyEvidence then checks them properly.
  async function findInTransaction(txHash, hashes, fetchJson) {
    const tx = await fetchJson(`tx/${txHash}`);
    if (tx.pending) throw new Error("the transaction is still pending");
    const raw = await fetchJson(`raw/block/${tx.block}`);
    const receipts = raw.receipts.map(decodeReceipt);
    const txi = Number(tx.index);
    let first = 0;
    for (let i = 0; i < txi; i++) first += receipts[i].logs.length;
    const want = new Map(Object.entries(hashes).filter(([, h]) => h).map(([alg, h]) => [lc(h), alg]));
    const found = [];
    (receipts[txi] ? receipts[txi].logs : []).forEach((l, i) => {
      l.topics.forEach((tp, j) => { if (j > 0 && want.has(lc(tp))) found.push({ logIndex: first + i, address: l.address, field: `topics.${j}`, alg: want.get(lc(tp)), topic0: l.topics[0] || null }); });
      dataWords(l.data).forEach((wd, j) => { if (want.has(lc(wd))) found.push({ logIndex: first + i, address: l.address, field: `data.${j}`, alg: want.get(lc(wd)), topic0: l.topics[0] || null }); });
    });
    return { block: tx.block, logs: receipts[txi] ? receipts[txi].logs.length : 0, found };
  }

  async function resolveCallAnchor(A, fetchJson, check, chain) {
    if (!addrOk(A.to || A.contract)) throw new Error("anchor.to: expected the contract address");
    const to = A.to || A.contract;
    const sig = parseSignature(A.function || A.signature || "");
    const returns = sig.returns || (A.returns ? parseParams(String(A.returns).replace(/^\s*\(([\s\S]*)\)\s*$/, "$1")) : null);
    if (!returns) throw new Error("anchor.returns: give the return types (or \"… returns (…)\" in the signature)");
    const data = toHex(concat(fromHex(sig.selector), abiEncode(sig.params.map((p) => p.type), A.args || [])));
    const res = await fetchJson(`call?to=${to}&data=${data}`);
    checkHeader(check, res.block, "call.block");
    const values = abiDecodeTuple(returns, fromHex(res.output), 0);
    check("call.output", true, `${sig.canonical} at block ${res.block.number} (latest state; reported by the node, not provable)`, "node");
    chain.anchor = { type: "call", to, function: sig.canonical, data, output: res.output, block: { number: res.block.number, hash: res.block.hash, header: res.block.header }, decoded: plain(values) };
    return { values, contract: to };
  }

  async function checkCustody(C, rootSums, fetchJson, check, out) {
    if (!addrOk(C.holder) || !addrOk(C.token)) throw new Error("custody: holder and token must be addresses");
    const erc20 = (sig, args, ret) => fetchJson(`call?to=${C.token}&data=${toHex(concat(fromHex(parseSignature(sig).selector), abiEncode(parseSignature(sig).params.map((p) => p.type), args)))}`)
      .then((r) => ({ block: r.block, value: abiDecode([ret], r.output)[0] }));
    const [bal, dec, sym] = await Promise.all([
      erc20("balanceOf(address)", [C.holder], "uint256"),
      erc20("decimals()", [], "uint8").catch(() => null),
      erc20("symbol()", [], "string").catch(() => null),
    ]);
    const bases = C.slot !== undefined && C.slot !== null ? [C.slot] : [...Array.from({ length: 15 }, (_, i) => i), OZ_ERC20_NAMESPACE];
    const keys = bases.map((b) => mappingSlot(C.holder, b));
    const pr = await fetchJson(`proof/${C.token}?slots=${keys.join(",")}`);
    const H = checkHeader(check, pr.block, "custody.block");
    const acct = verifiedAccount(check, H, pr.proof, "custody.token");
    let proven = null;
    if (acct) {
      for (let i = 0; i < pr.proof.storageProof.length; i++) {
        const sp = pr.proof.storageProof[i];
        let v;
        try { v = verifyProof(acct.storageHash, toHex(keccak256(fromHex(sp.key))), sp.proof); } catch (_) { continue; }
        const value = v === null ? 0n : bytesToBig(rlpDecode(v));
        if (value === toBig(sp.value) && value === bal.value && (value !== 0n || C.slot !== undefined)) { proven = { base: String(bases[i]), key: sp.key, value }; break; }
      }
    }
    const held = proven ? proven.value : bal.value;
    check("custody.balance", true, proven
      ? `balances[${C.holder}] = ${held} proven against stateRoot of block ${pr.block.number} (mapping at slot ${proven.base})`
      : `balanceOf(${C.holder}) = ${held} at block ${bal.block.number} (reported by the node; no storage proof matched)`, proven ? "verified" : "node");
    const idx = C.sum === undefined ? null : Number(C.sum);
    const claimed = idx !== null && rootSums ? rootSums[idx] : null;
    if (claimed !== null && claimed !== undefined) {
      check("custody.solvent", held >= claimed, `held now ${held} ${held >= claimed ? "≥" : "<"} root total ${claimed}`, "latest", false);
    }
    out.custody = {
      holder: C.holder, token: C.token, symbol: sym ? sym.value : null, decimals: dec ? Number(dec.value) : null,
      held: held.toString(), claimed: claimed === null || claimed === undefined ? null : claimed.toString(),
      block: { number: pr.block.number, hash: pr.block.hash, header: pr.block.header },
      proof: proven ? { slotBase: proven.base, key: proven.key, accountProof: pr.proof.accountProof, storageProof: pr.proof.storageProof.find((x) => x.key === proven.key).proof, storageHash: pr.proof.storageHash, nonce: pr.proof.nonce, balance: pr.proof.balance, codeHash: pr.proof.codeHash } : null,
      balanceOf: { block: bal.block.number, value: bal.value.toString() },
    };
  }

  /// Runs a verification. `proof` is the proof file (object); `file` the uploaded file's
  /// hashes ({ size, keccak256, sha256 }) or null; `fetchJson(path)` reads the explorer API
  /// (`/api/<path>`). Never throws: errors become a failed check. Returns the report.
  async function verifyEvidence({ proof, file, fetchJson, origin, now }) {
    const checks = [];
    // trust: "verified" (checked against block hashes), "local" (computed here), "node"
    // (reported by the node), "latest" (latest state, not the anchor's block). Checks with
    // decisive = false are shown but do not decide the result.
    const check = (id, ok, detail, trust, decisive = true) => { checks.push({ id, ok: !!ok, detail, trust: trust || "verified", decisive }); return ok; };
    const report = {
      format: "boltchain-evidence-report/1",
      generatedAt: (now || new Date()).toISOString(),
      explorer: { origin: origin || null },
      proof,
      file: file ? { size: file.size, keccak256: file.keccak256, sha256: file.sha256 } : null,
      chain: {},
      checks,
    };
    try {
      if (!proof || typeof proof !== "object") throw new Error("no proof file");
      if (proof.version !== undefined && Number(proof.version) !== 1) throw new Error(`unsupported proof file version ${proof.version}`);
      const A = proof.anchor;
      if (!A || typeof A !== "object") throw new Error("proof.anchor is missing");
      const st = await fetchJson("status");
      Object.assign(report.explorer, { version: st.version || null, chainId: st.chainId, genesis: st.genesis });
      if (proof.chainId !== undefined) check("chain.id", Number(proof.chainId) === Number(st.chainId), `chain ${st.chainId}${Number(proof.chainId) === Number(st.chainId) ? "" : `, the proof is for chain ${proof.chainId}`}`, "node");
      const type = A.type || (A.txHash ? "log" : "call");
      const { values, contract } = type === "log"
        ? await resolveLogAnchor(A, fetchJson, check, report.chain)
        : type === "call" ? await resolveCallAnchor(A, fetchJson, check, report.chain)
          : (() => { throw new Error(`anchor.type "${type}" (expected log or call)`); })();
      // The contract's code hash, proven against the latest state (so people can compare it
      // with the published source).
      try {
        const cp = await fetchJson(`proof/${contract}`);
        const ch = checkHeader(() => {}, cp.block, "code");
        const acct = verifiedAccount(() => {}, ch, cp.proof, "code");
        if (acct) report.chain.contract = { address: contract, codeHash: acct.codeHash, block: cp.block.number };
      } catch (_) { /* informational only */ }

      const onChain = plain(getField(values, A.field || "0"));
      const leafSpec = proof.leaf || (file ? { file: "keccak256" } : null);
      if (!leafSpec) throw new Error("proof.leaf is missing (or upload the file to compare its hash)");
      let content;
      if (leafSpec.file) {
        if (!file) throw new Error("this proof needs the file: upload it to compute its hash");
        const alg = leafSpec.file === true ? "keccak256" : leafSpec.file;
        if (alg !== "keccak256" && alg !== "sha256") throw new Error(`leaf.file: unknown hash ${alg}`);
        content = { hash: file[alg] };
        check("file.hash", true, `${alg}(file) = ${file[alg]}`, "local");
      } else if (leafSpec.contentHash) content = { hash: leafSpec.contentHash };
      else if (leafSpec.types) content = { types: leafSpec.types, values: leafSpec.values || [] };
      else throw new Error("proof.leaf: give file, contentHash, or types and values");
      if (file && !leafSpec.file) check("file.hash", true, `keccak256(file) = ${file.keccak256} (not part of this proof)`, "local");

      if (!proof.scheme || proof.scheme === "none") {
        const mine = content.hash || toHex(keccak256(abiEncode(content.types, content.values)));
        report.leaf = { hash: mine };
        check("match", lc(mine) === lc(onChain), `${A.field || "value"} on chain ${onChain} ${lc(mine) === lc(onChain) ? "=" : "≠"} ${mine}`, "verified");
      } else {
        const r = merkleRoot(proof.scheme, content, leafSpec, proof.siblings || [], proof.path);
        report.leaf = { hash: r.leaf.hash, contentHash: r.leaf.contentHash, sums: r.leaf.sums && r.leaf.sums.map(String) };
        report.root = { hash: r.root.hash, sums: r.root.sums && r.root.sums.map(String) };
        check("match", lc(r.root.hash) === lc(onChain), `${A.field || "root"} on chain ${onChain} ${lc(r.root.hash) === lc(onChain) ? "=" : "≠"} recomputed ${r.root.hash} (${proof.scheme}, ${(proof.siblings || []).length} siblings)`, "verified");
        if (r.root.sums) {
          const fields = A.sums || [];
          if (fields.length !== r.root.sums.length) check("sums", false, `anchor.sums lists ${fields.length} fields, the tree carries ${r.root.sums.length} sums`, "verified");
          else fields.forEach((f, i) => {
            const v = toBig(plain(getField(values, f)));
            check(`sums.${i}`, v === r.root.sums[i], `${f} on chain ${v} ${v === r.root.sums[i] ? "=" : "≠"} root sum ${r.root.sums[i]}`, "verified");
          });
        }
        if (proof.custody) await checkCustody(proof.custody, r.root.sums, fetchJson, check, report);
      }
    } catch (e) {
      check("error", false, e && e.message ? e.message : String(e), "error");
    }
    report.result = checks.some((c) => c.trust === "error") ? "error" : checks.filter((c) => c.decisive).every((c) => c.ok) ? "match" : "mismatch";
    report.reportHash = reportHash(report);
    return report;
  }

  // ---------------------------------------------------------------------------------------
  // evidence certificates (ADR 0016)
  // ---------------------------------------------------------------------------------------
  const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
  /// 10 bytes (hex or bytes) -> 16 Crockford base-32 characters.
  function encodeCode(b) {
    const x = bytesToBig(fromHex(b));
    let s = "";
    for (let i = 15; i >= 0; i--) s += CROCKFORD[Number((x >> BigInt(5 * i)) & 31n)];
    return s;
  }
  /// Parses a code (any case, dashes and spaces ignored, I/L = 1, O = 0) -> "0x" + 10 bytes, or null.
  function decodeCode(str) {
    const t = String(str || "").toUpperCase().replace(/[\s-]/g, "").replace(/[IL]/g, "1").replace(/O/g, "0");
    if (t.length !== 16) return null;
    let x = 0n;
    for (const ch of t) { const d = CROCKFORD.indexOf(ch); if (d < 0) return null; x = (x << 5n) | BigInt(d); }
    return toHex(bigToBytes(x, 10));
  }
  const formatCode = (c) => c.replace(/(.{4})(?=.)/g, "$1-");

  const B32 = "abcdefghijklmnopqrstuvwxyz234567";
  function base32(bytes) {
    let bits = 0, val = 0, out = "";
    for (const b of bytes) { val = (val << 8) | b; bits += 8; while (bits >= 5) { out += B32[(val >>> (bits - 5)) & 31]; bits -= 5; } }
    if (bits > 0) out += B32[(val << (5 - bits)) & 31];
    return out;
  }
  function unbase32(s) {
    let bits = 0, val = 0; const out = [];
    for (const ch of s) { const d = B32.indexOf(ch); if (d < 0) throw new Error("bad base32"); val = (val << 5) | d; bits += 5; if (bits >= 8) { out.push((val >>> (bits - 8)) & 255); bits -= 8; } }
    return Uint8Array.from(out);
  }
  /// CIDv1 string of a raw (0x55) sha-256 block.
  const rawCid = (data) => "b" + base32(concat(Uint8Array.of(0x01, 0x55, 0x12, 0x20), sha256(data)));
  /// CID string <-> binary CID bytes (base32 CIDv1 only).
  const cidBytes = (cid) => { if (cid[0] !== "b") throw new Error("unsupported CID"); return unbase32(cid.slice(1)); };
  const cidString = (bytes) => "b" + base32(fromHex(bytes));
  /// Checks a block against its CID (sha-256 or keccak-256 multihash).
  function checkCid(cid, data) {
    const d = cidDigest(cid);
    const got = d.code === 0x12 ? sha256(data) : d.code === 0x1b ? keccak256(data) : null;
    if (!got || toHex(got).slice(2) !== d.digest) throw new Error(`block ${cid} does not match its CID`);
  }
  function cidDigest(cid) {
    const out = cidBytes(cid);
    let i = 0;
    const varint = () => { let x = 0, sh = 0, b; do { b = out[i++]; x |= (b & 0x7f) << sh; sh += 7; } while (b & 0x80); return x; };
    varint(); varint(); const code = varint(); const len = varint();
    return { code, digest: toHex(out.subarray(i, i + len)).slice(2) };
  }

  const CERT_EVENT = "Issued(bytes10 indexed code, address indexed issuer, bytes32 root, uint256 deal, bool publicFiles, bytes32[] keccak, bytes32[] sha, uint64[] sizes, bytes manifest, bytes dealRoot)";
  const CERT_ISSUE = "issue(bytes32 salt, bytes32[] k, bytes32[] s, uint64[] sizes, bool publicFiles, bytes manifest, bytes dealRoot, uint64 blocks, uint64 size, uint8 replicas, uint64 epochs, uint128 price)";
  const CHUNK_SIZE = 256 * 1024;
  /// Leaf of a file in a certificate (the contract's leafOf).
  const certLeafContent = (f) => keccak256(abiEncode(["bytes32", "bytes32", "uint64"], [f.keccak256, f.sha256, String(f.size)]));
  /// Merkle root of a certificate's files and the proof of each (prefixed-abi-v1, lone node up).
  function certTree(files) {
    if (!files.length) throw new Error("no files");
    const S = SCHEMES["prefixed-abi-v1"];
    const layers = [files.map((f) => S.leaf({ hash: toHex(certLeafContent(f)) }).hash)];
    while (layers[layers.length - 1].length > 1) {
      const cur = layers[layers.length - 1], next = [];
      for (let i = 0; i < cur.length; i += 2) next.push(i + 1 < cur.length ? S.node({ hash: cur[i] }, { hash: cur[i + 1] }).hash : cur[i]);
      layers.push(next);
    }
    const proof = (index) => {
      const siblings = []; let path = 0n, idx = index;
      for (let l = 0; l < layers.length - 1; l++) {
        const isRight = idx % 2 === 1, sib = isRight ? idx - 1 : idx + 1;
        if (sib < layers[l].length) { if (isRight) path |= 1n << BigInt(siblings.length); siblings.push(toHex(layers[l][sib])); }
        idx = Math.floor(idx / 2);
      }
      return { siblings, path: "0x" + path.toString(16) };
    };
    return { root: toHex(layers[layers.length - 1][0]), proof };
  }
  const certCode = (root, issuer, salt) => toHex(keccak256(abiEncode(["bytes32", "address", "bytes32"], [root, issuer, salt])).subarray(0, 10));

  /// Loads a certificate and checks its Issued event against the block hash (header -> receipt
  /// trie). Returns { cert (API view), event (decoded), files, checks, ok }.
  async function loadCertificate(code, fetchJson) {
    const cert = await fetchJson(`cert/${encodeURIComponent(code)}`);
    const checks = [];
    const check = (id, ok, detail, trust) => checks.push({ id, ok: !!ok, detail, trust: trust || "verified", decisive: true });
    const raw = await fetchJson(`raw/block/${cert.block}`);
    const H = decodeHeader(raw.header);
    check("block.header", lc(H.hash) === lc(raw.hash) && H.number === BigInt(raw.number), `keccak256(header) = ${H.hash}`);
    check("block.receipts", lc(orderedTrieRoot(raw.receipts)) === lc(H.receiptsRoot), `receiptsRoot ${H.receiptsRoot}`);
    const ev = parseSignature(CERT_EVENT);
    const topic1 = toHex(concat(fromHex(cert.codeHex), new Uint8Array(22)));
    let found = null;
    for (const r of raw.receipts.map(decodeReceipt)) {
      if (!r.status) continue;
      for (const l of r.logs) {
        if (lc(l.address) === lc(cert.contract) && lc(l.topics[0]) === ev.topic && lc(l.topics[1] || "") === lc(topic1)) found = l;
      }
    }
    if (!found) {
      check("cert.event", false, `no Issued event for ${cert.code} in block ${cert.block}`);
      return { cert, event: null, files: [], checks, ok: false, timestamp: H.timestamp };
    }
    const d = decodeLog(ev, found).args;
    const files = Array.from({ length: d.keccak.length }, (_, i) => ({ keccak256: d.keccak[i], sha256: d.sha[i], size: Number(d.sizes[i]) }));
    check("cert.event", true, `Issued event of ${found.address} in block ${cert.block}`);
    const root = certTree(files).root;
    check("cert.root", lc(root) === lc(d.root) && lc(root) === lc(cert.root), `Merkle root of ${files.length} file(s) = ${root}`);
    check("cert.issuer", lc(d.issuer) === lc(cert.issuer), `issuer ${d.issuer}`);
    const manifest = d.manifest && d.manifest !== "0x" ? cidString(d.manifest) : null;
    return {
      cert, files, checks, ok: checks.every((c) => c.ok), timestamp: H.timestamp, blockHash: raw.hash,
      event: { root: d.root, issuer: d.issuer, deal: String(d.deal), publicFiles: d.publicFiles, manifest, dealRoot: d.dealRoot },
    };
  }
  /// Index of each file (by its hashes and size) in the certificate, or -1.
  const matchFile = (files, h) => files.findIndex((f) => lc(f.keccak256) === lc(h.keccak256) && lc(f.sha256) === lc(h.sha256) && f.size === h.size);
  /// Downloads file `i` of a public certificate from the gateway, checking every block against
  /// its CID and the file against the certificate's hashes. Returns a Blob.
  async function downloadFile(loaded, i, fetchRaw, onProgress) {
    const m = loaded.event && loaded.event.manifest;
    if (!m) throw new Error("this certificate has no manifest");
    const mb = await fetchRaw(m);
    checkCid(m, mb);
    const man = JSON.parse(new TextDecoder().decode(mb));
    const entry = man.files && man.files[i];
    const want = loaded.files[i];
    if (!entry || !entry.chunks || !entry.chunks.length) throw new Error("this file is not public");
    if (lc(entry.sha256) !== lc(want.sha256)) throw new Error("the manifest does not match the certificate");
    const parts = [];
    const k = new Keccak256(), s = new Sha256();
    for (let j = 0; j < entry.chunks.length; j++) {
      const b = await fetchRaw(entry.chunks[j]);
      checkCid(entry.chunks[j], b);
      k.update(b); s.update(b); parts.push(b);
      if (onProgress) onProgress((j + 1) / entry.chunks.length);
    }
    if (lc(toHex(k.digest())) !== lc(want.keccak256) || lc(toHex(s.digest())) !== lc(want.sha256)) throw new Error("the downloaded file does not match the certificate");
    return new Blob(parts);
  }
  /// Cuts a file into CHUNK_SIZE blocks: [{ cid, data }], calling `each` as it goes.
  async function chunkFile(blob, each) {
    const out = [];
    for (let o = 0; o < blob.size || (o === 0 && blob.size === 0); o += CHUNK_SIZE) {
      const data = new Uint8Array(await blob.slice(o, o + CHUNK_SIZE).arrayBuffer());
      const c = { cid: rawCid(data), data };
      out.push(c.cid);
      if (each) await each(c);
      if (blob.size === 0) break;
    }
    return out;
  }
  /// Body of POST /api/upload: [u32 length][bytes]…
  function uploadBody(blocks) {
    const n = blocks.reduce((a, b) => a + 4 + b.length, 0);
    const out = new Uint8Array(n); const dv = new DataView(out.buffer); let o = 0;
    for (const b of blocks) { dv.setUint32(o, b.length); out.set(b, o + 4); o += 4 + b.length; }
    return out;
  }
  /// The manifest kept in the storage deal: hashes and sizes (and the blocks, when public); no names.
  const certManifest = (files, chunks) => utf8(canonicalJson({ v: 1, type: "boltchain-certificate", files: files.map((f, i) => ({ keccak256: f.keccak256, sha256: f.sha256, size: f.size, ...(chunks ? { chunks: chunks[i] } : {}) })) }));
  /// Call data of Certificates.issue.
  function issueCalldata(o) {
    const sig = parseSignature(CERT_ISSUE);
    return toHex(concat(fromHex(sig.selector), abiEncode(sig.params.map((p) => p.type), [
      o.salt, o.files.map((f) => f.keccak256), o.files.map((f) => f.sha256), o.files.map((f) => String(f.size)), !!o.publicFiles,
      o.manifest, o.dealRoot, String(o.blocks), String(o.size), String(o.replicas), String(o.epochs), String(o.price),
    ])));
  }
  /// A proof file (docs/evidence-proof-format.md) for file `i` of a certificate.
  function certProofFile(loaded, i, chainId) {
    const f = loaded.files[i];
    const p = certTree(loaded.files).proof(i);
    return {
      version: 1, chainId,
      anchor: { type: "call", to: loaded.cert.contract, function: "certificate(bytes10 code) view returns ((address issuer, uint64 issuedAt, uint64 blockNumber, uint32 files, bool publicFiles, bytes32 root, uint256 deal, bytes manifest) c)", args: [loaded.cert.codeHex], field: "c.root" },
      scheme: "prefixed-abi-v1",
      leaf: { types: ["bytes32", "bytes32", "uint64"], values: [f.keccak256, f.sha256, String(f.size)], names: ["keccak256", "sha256", "size"] },
      siblings: p.siblings, path: p.path,
    };
  }

  const api = {
    toHex, fromHex, concat, eqBytes, utf8, toBig, bigToBytes, bytesToBig,
    Keccak256, keccak256, Sha256, sha256,
    rlpEncode, rlpDecode, orderedTrieRoot, verifyProof, EMPTY_TRIE,
    decodeHeader, decodeReceipt,
    parseSignature, parseParams, abiEncode, abiDecode, encodePacked, decodeLog, getField, canon,
    SCHEMES, merkleRoot, pathBits,
    canonicalJson, plain, reportHash,
    hashFile, hashBlob, verifyEvidence, findInTransaction, mappingSlot,
    encodeCode, decodeCode, formatCode, rawCid, cidBytes, cidString, checkCid, CHUNK_SIZE, CERT_EVENT, CERT_ISSUE,
    certTree, certCode, certLeafContent, loadCertificate, matchFile, downloadFile, chunkFile, uploadBody, certManifest, issueCalldata, certProofFile,
  };
  // As a Worker: hash the posted file.
  if (typeof WorkerGlobalScope !== "undefined" && G instanceof WorkerGlobalScope) {
    G.onmessage = (e) => {
      hashBlob(e.data.file, (p) => G.postMessage({ progress: p }))
        .then((result) => G.postMessage({ result }), (err) => G.postMessage({ error: String(err && err.message || err) }));
    };
  }
  G.BoltVerify = api;
  if (typeof module === "object" && module.exports) module.exports = api;
})(typeof globalThis !== "undefined" ? globalThis : this);
