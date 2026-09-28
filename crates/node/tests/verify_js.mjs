// Checks crates/node/src/explorer/verify.js against reference vectors produced by independent
// implementations (viem, node:crypto, @ethereumjs/trie, @openzeppelin/merkle-tree and
// CO2Exchange's tree code; see fixtures/verify-vectors.json). Plain Node, no packages:
//   node crates/node/tests/verify_js.mjs
// Run from `cargo test` by verify_js.rs when node is installed.
import { createRequire } from "node:module";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const V = createRequire(import.meta.url)(path.join(here, "../src/explorer/verify.js"));
const vec = JSON.parse(fs.readFileSync(path.join(here, "fixtures/verify-vectors.json"), "utf8"));
let n = 0;
const eq = (a, b, what) => {
  n++;
  const sa = JSON.stringify(a), sb = JSON.stringify(b);
  if (sa !== sb) throw new Error(`${what}:\n  got      ${sa}\n  expected ${sb}`);
};
const throws = (f, what) => { n++; try { f(); } catch { return; } throw new Error(`${what}: expected an error`); };

// Hashes, one-shot and streamed in odd chunks.
for (const h of vec.hashes) {
  const d = V.fromHex(h.data);
  eq(V.toHex(V.keccak256(d)), h.keccak256, `keccak256 of ${d.length} bytes`);
  eq(V.toHex(V.sha256(d)), h.sha256, `sha256 of ${d.length} bytes`);
  for (const step of [1, 7, 135, 137, 1000]) {
    const k = new V.Keccak256(), s = new V.Sha256();
    for (let o = 0; o < d.length; o += step) { k.update(d.subarray(o, o + step)); s.update(d.subarray(o, o + step)); }
    eq(V.toHex(k.digest()), h.keccak256, `streamed keccak256 (${d.length}/${step})`);
    eq(V.toHex(s.digest()), h.sha256, `streamed sha256 (${d.length}/${step})`);
  }
}
{
  const big = crypto.randomBytes(3 * 1024 * 1024 + 11);
  const t0 = Date.now();
  const k = V.toHex(V.keccak256(big));
  const ms = Date.now() - t0;
  const s = new V.Keccak256();
  for (let o = 0; o < big.length; o += 65536) s.update(big.subarray(o, o + 65536));
  eq(V.toHex(s.digest()), k, "streamed keccak of 3 MiB");
  eq(V.toHex(V.sha256(big)), "0x" + crypto.createHash("sha256").update(big).digest("hex"), "sha256 of 3 MiB");
  console.log(`keccak256: ${(3 / (ms / 1000)).toFixed(0)} MiB/s`);
}

// Tries and proofs.
for (const t of vec.tries) eq(V.orderedTrieRoot(t.values), t.root, `ordered trie of ${t.values.length}`);
for (const p of vec.proofs) {
  const v = V.verifyProof(p.root, p.key, p.proof);
  eq(v === null ? null : V.toHex(v), p.value, `proof of ${p.key}`);
  if (p.proof.length > 1) {
    const bad = [...p.proof];
    bad[1] = bad[1].slice(0, -2) + (bad[1].endsWith("00") ? "01" : "00");
    throws(() => V.verifyProof(p.root, p.key, bad), "tampered proof");
  }
}

// ABI.
for (const a of vec.abi) {
  eq(V.toHex(V.abiEncode(a.types, a.values)), a.encoded, `abi.encode(${a.types})`);
  eq(V.plain(V.abiDecode(a.types, a.encoded)), a.values.map((x) => JSON.parse(JSON.stringify(x, (_k, v) => (typeof v === "number" ? String(v) : v)))), `abi.decode(${a.types})`);
}
for (const p of vec.packed) eq(V.toHex(V.encodePacked(p.types, p.values)), p.encoded, `encodePacked(${p.types})`);
for (const e of vec.events) {
  const sig = V.parseSignature(e.signature);
  eq(sig.topic, e.topic, "event topic");
  const d = V.decodeLog(sig, e.log);
  for (const [f, v] of Object.entries(e.expect)) eq(V.plain(V.getField(d.args, f)), v, `event field ${f}`);
  throws(() => V.decodeLog("Committed(uint64 indexed epoch, bytes32 anchor)", e.log), "wrong signature");
}
eq(V.parseSignature("commitmentOf(uint64) view returns ((bytes32 logRoot, uint256 total) c)").returns[0].type.components[1].name, "total", "returns");
eq(V.parseSignature("function balanceOf(address a) returns (uint256)").selector, "0x70a08231", "selector");

// Merkle schemes.
for (const m of vec.merkle) {
  const content = m.leaf.types ? { types: m.leaf.types, values: m.leaf.values } : { hash: m.leaf.contentHash };
  const r = V.merkleRoot(m.scheme, content, m.leaf, m.siblings, m.path);
  eq(r.root.hash, m.root, m.name);
  if (m.sums) eq(r.root.sums.map(String), m.sums, `${m.name} sums`);
  if (m.siblings.length) {
    const bad = JSON.parse(JSON.stringify(m.siblings));
    if (typeof bad[0] === "string") bad[0] = bad[0].replace(/.$/, (c) => (c === "0" ? "1" : "0"));
    else bad[0].sums[0] = String(BigInt(bad[0].sums[0]) + 1n);
    n++;
    if (V.merkleRoot(m.scheme, content, m.leaf, bad, m.path).root.hash === m.root) throw new Error(`${m.name}: tampered sibling still verifies`);
  }
}
throws(() => V.merkleRoot("sum-packed-v1", { types: ["uint256"], values: ["1"] }, { sums: [0] }, [{ hash: "0x" + "00".repeat(32), sums: ["-1"] }], 0), "negative sum");
throws(() => V.pathBits("0x4", 2), "path bits beyond siblings");

// Report hash covers everything but itself and ignores key order.
const r1 = { b: 1n, a: [{ y: "x", z: 2 }], reportHash: "0x" };
const r2 = { a: [{ z: 2, y: "x" }], b: "1" };
eq(V.reportHash(r1), V.reportHash(r2), "report hash is canonical");

console.log(`verify.js: ${n} checks passed`);
