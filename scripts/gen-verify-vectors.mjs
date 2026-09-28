// Generates crates/node/tests/fixtures/verify-vectors.json, the reference vectors for the
// explorer's verify.js, with independent implementations: viem (keccak, ABI), node:crypto
// (sha256), @ethereumjs/trie (MPT), @openzeppelin/merkle-tree, and CO2Exchange's own tree code.
//
//   npm i viem @openzeppelin/merkle-tree @ethereumjs/trie@6 @ethereumjs/rlp
//   CO2X=../CO2Exchange node scripts/gen-verify-vectors.mjs crates/node/tests/fixtures/verify-vectors.json
//
// Needs Node 22.18+ (it imports CO2Exchange's TypeScript files directly).
import crypto from "node:crypto";
import fs from "node:fs";
import { keccak256, encodeAbiParameters, parseAbiParameters, encodePacked, toEventSelector, encodeEventTopics, parseAbiItem, toHex, bytesToHex } from "viem";
import { StandardMerkleTree } from "@openzeppelin/merkle-tree";
import { Trie } from "@ethereumjs/trie";
import { RLP } from "@ethereumjs/rlp";
import path from "node:path";
import { pathToFileURL } from "node:url";

const CO2X = process.env.CO2X || "../CO2Exchange";
const { buildBalanceTree } = await import(pathToFileURL(path.resolve(CO2X, "web/lib/bank/tree.ts")).href);
const { buildTree, leaf: ledgerLeaf } = await import(pathToFileURL(path.resolve(CO2X, "web/lib/ledger/merkle.ts")).href);

let seed = 1;
const rnd = () => { seed = (seed * 1103515245 + 12345) & 0x7fffffff; return seed; };
const bytes = (n) => Uint8Array.from({ length: n }, () => (rnd() >> 16) & 255);
const out = { hashes: [], tries: [], proofs: [], abi: [], packed: [], events: [], merkle: [] };

for (const n of [0, 1, 55, 56, 63, 64, 135, 136, 137, 271, 272, 1000, 5000]) {
  const b = bytes(n);
  out.hashes.push({ data: bytesToHex(b), keccak256: keccak256(b), sha256: "0x" + crypto.createHash("sha256").update(b).digest("hex") });
}

// Ordered tries (transactions / receipts shape), with short values for inline nodes.
for (const n of [0, 1, 2, 3, 15, 16, 17, 127, 128, 129, 260]) {
  const t = new Trie();
  const vals = [];
  for (let i = 0; i < n; i++) {
    const len = [1, 3, 10, 40, 120][rnd() % 5];
    const v = bytes(len);
    vals.push(bytesToHex(v));
    await t.put(RLP.encode(i), v);
  }
  out.tries.push({ values: vals, root: bytesToHex(t.root()) });
}

// State-trie style proofs (hashed 32-byte keys), presence and absence.
{
  const t = new Trie({ useKeyHashing: true });
  const keys = [];
  for (let i = 0; i < 200; i++) {
    const k = bytes(20);
    const v = RLP.encode(bytes([1, 5, 33, 70][i % 4]));
    keys.push({ k, v });
    await t.put(k, v);
  }
  const root = bytesToHex(t.root());
  for (let i = 0; i < 30; i++) {
    const { k, v } = keys[i];
    const proof = (await t.createProof(k)).map(bytesToHex);
    out.proofs.push({ root, key: keccak256(k), proof, value: bytesToHex(v) });
  }
  for (let i = 0; i < 10; i++) {
    const k = bytes(20);
    const proof = (await t.createProof(k)).map(bytesToHex);
    out.proofs.push({ root, key: keccak256(k), proof, value: null });
  }
}

// abi.encode
const abiCases = [
  ["uint256, address, bool, bytes32", [123456789n, "0x00000000000000000000000000000000000b0b00", true, keccak256("0x01")]],
  ["bytes1, bytes32", ["0x00", keccak256("0x02")]],
  ["uint8, int16, int256", [255, -300, -1n]],
  ["string, bytes, uint64[]", ["碳權 CO2", "0xdeadbeef", [1n, 2n, 3n]]],
  ["(bytes32 a, uint64 b, uint256[] c) t, string s", [{ a: keccak256("0x03"), b: 9n, c: [7n] }, "x"]],
  ["uint256[2][], (address,bool)[]", [[[1n, 2n], [3n, 4n]], [["0x1111111111111111111111111111111111111111", false]]]],
];
for (const [types, values] of abiCases) {
  const params = parseAbiParameters(types);
  out.abi.push({ types: params.map(canonType), values: jsonable(values), encoded: encodeAbiParameters(params, values) });
}
function canonType(p) {
  if (p.type.startsWith("tuple")) return `(${p.components.map((c) => canonType(c) + (c.name ? " " + c.name : "")).join(",")})${p.type.slice(5)}`;
  return p.type;
}
function jsonable(v) {
  if (typeof v === "bigint") return v.toString();
  if (Array.isArray(v)) return v.map(jsonable);
  if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, jsonable(x)]));
  return v;
}

// abi.encodePacked
const packedCases = [
  [["bytes1", "address", "uint64", "bytes32", "uint256", "uint256"], ["0x00", "0xcccccccccccccccccccccccccccccccccccccccc", 7n, keccak256("0x04"), 251003n, 1234567n]],
  [["bytes1", "uint256", "uint256"], ["0x00", 7n, 1000n]],
  [["string", "bool", "int8", "bytes2"], ["hi", true, -2, "0xabcd"]],
  [["uint256[]", "uint256[]", "uint256"], [[7n, 42n], [1500n, 250012n], 99n]],
];
for (const [types, values] of packedCases) out.packed.push({ types, values: jsonable(values), encoded: encodePacked(types, values) });

// Events: CO2Exchange Ledger's Committed.
{
  const sig = "event Committed(uint64 indexed epoch, bytes32 anchor, (bytes32 prev, uint64 epoch, bytes32 logRoot, bytes32 balanceRoot, bytes32 registryRoot, bytes32 identityRoot, uint256 totalKg, uint256 totalCash, bytes32 totalsHash, uint64 upToBlock, uint64 lastSeq, uint16 rulesVersion) commitment)";
  const item = parseAbiItem(sig);
  const c = { prev: keccak256("0x10"), epoch: 5n, logRoot: keccak256("0x11"), balanceRoot: keccak256("0x12"), registryRoot: keccak256("0x13"), identityRoot: keccak256("0x14"), totalKg: 70000n, totalCash: 1100000000000n, totalsHash: keccak256("0x15"), upToBlock: 112n, lastSeq: 12n, rulesVersion: 2 };
  const topics = encodeEventTopics({ abi: [item], eventName: "Committed", args: { epoch: 5n } });
  const data = encodeAbiParameters(item.inputs.filter((i) => !i.indexed), [keccak256("0x16"), c]);
  out.events.push({ signature: sig.replace(/^event /, ""), topic: toEventSelector(item), log: { topics, data }, expect: { "commitment.balanceRoot": c.balanceRoot, "commitment.totalCash": "1100000000000", epoch: "5", "anchor": keccak256("0x16") } });
}

// Merkle: CO2Exchange's balance tree (sum tree) and asset trees.
{
  const accounts = Array.from({ length: 7 }, (_, i) => ({
    account: `0x${String(i + 1).repeat(40).slice(0, 40)}`,
    assets: i % 3 === 0 ? [] : [{ batchId: 7n, kg: BigInt(1000 + i) }, { batchId: 42n, kg: BigInt(50 * i) }, ...(i % 2 ? [{ batchId: 900n, kg: 3n }] : [])],
    cash: BigInt(1_000_000 * i + 17),
  }));
  const epoch = 5n;
  const tree = buildBalanceTree(accounts, epoch);
  for (const a of accounts) {
    const p = tree.proofOf(a.account);
    out.merkle.push({
      name: `co2x balance ${a.account}`,
      scheme: "sum-packed-v1",
      leaf: { types: ["address", "uint64", "bytes32", "uint256", "uint256"], values: [p.account, epoch.toString(), p.assetsRoot, p.leafKg.toString(), p.leafCash.toString()], sums: [3, 4] },
      siblings: p.siblings.map((s) => ({ hash: s.hash, sums: [s.kg.toString(), s.cash.toString()] })),
      path: toHex(p.path),
      root: tree.root.hash,
      sums: [tree.root.kg.toString(), tree.root.cash.toString()],
    });
    for (const as of a.assets) {
      if (as.kg === 0n) continue;
      const ap = tree.assetProofOf(a.account, as.batchId);
      out.merkle.push({
        name: `co2x asset ${a.account} batch ${as.batchId}`,
        scheme: "prefixed-packed-v1",
        leaf: { types: ["uint256", "uint256"], values: [as.batchId.toString(), ap.kg.toString()] },
        siblings: ap.siblings,
        path: ap.path.toString(),
        root: p.assetsRoot,
      });
    }
  }
  // Ledger v2 general tree (registry / identity / log roots).
  const contents = Array.from({ length: 11 }, (_, i) => keccak256(encodeAbiParameters(parseAbiParameters("uint8, uint256, bytes32"), [3, BigInt(i), keccak256(toHex(i))])));
  const lt = buildTree(contents.map(ledgerLeaf));
  contents.forEach((c, i) => {
    const pr = lt.proof(i);
    out.merkle.push({
      name: `co2x ledger leaf ${i}`,
      scheme: "prefixed-abi-v1",
      leaf: i % 2 ? { contentHash: c } : { types: ["uint8", "uint256", "bytes32"], values: ["3", String(i), keccak256(toHex(i))] },
      siblings: pr.siblings,
      path: pr.path.toString(2).split("").reverse().map(Number).concat(Array(Math.max(0, pr.siblings.length - pr.path.toString(2).length)).fill(0)).slice(0, pr.siblings.length),
      root: lt.root,
    });
  });
  // OpenZeppelin StandardMerkleTree.
  const values = Array.from({ length: 9 }, (_, i) => [`0x${String(9 - i).repeat(40).slice(0, 40)}`, String(1000 * (i + 1))]);
  const oz = StandardMerkleTree.of(values, ["address", "uint256"]);
  values.forEach((v, i) => out.merkle.push({ name: `oz ${i}`, scheme: "oz-standard-v1", leaf: { types: ["address", "uint256"], values: v }, siblings: oz.getProof(i), root: oz.root }));
}

fs.writeFileSync(process.argv[2], JSON.stringify(out, null, 1) + "\n");
console.log("vectors:", Object.fromEntries(Object.entries(out).map(([k, v]) => [k, v.length])));
