// Smoke test of the WASM build (Node.js 18+):
//   cargo build -p bolt-vault --release --target wasm32-unknown-unknown --features wasm
//   wasm-bindgen --target nodejs --out-dir <pkg> target/wasm32-unknown-unknown/release/bolt_vault.wasm
//   node crates/vault/js/smoke.mjs <pkg>
import { createRequire } from "node:module";
import path from "node:path";
const require = createRequire(import.meta.url);
const v = require(path.resolve(process.argv[2] ?? "pkg", "bolt_vault.js"));
const assert = (c, m) => { if (!c) { throw new Error(m); } };

const requester = v.generateKeyPair();
const executor = v.generateKeyPair();
const outsider = v.generateKeyPair();
const data = new Uint8Array(1_500_000).map((_, i) => (i * 7) % 256);

const sealed = v.seal(data, "input.jsonl", "application/jsonl", [requester.publicKey]);
const store = new Map([sealed.envelope, sealed.manifest, ...sealed.shards].map((b) => [b.cid, b.bytes]));
assert(v.cidOf(sealed.envelope.bytes) === sealed.envelope.cid, "envelope CID");

function open(envelope, sk, lose = []) {
  const { manifestCid, contentKey } = v.openEnvelope(envelope, sk);
  const m = v.openManifest(envelope, store.get(manifestCid), contentKey);
  const shards = m.shards.map((cid, i) => (lose.includes(i) ? null : store.get(cid)));
  return { m, data: v.recover(m.json, contentKey, shards) };
}

const a = open(sealed.envelope.bytes, requester.secretKey, [0, 5]);
assert(a.m.name === "input.jsonl" && a.data.length === data.length && a.data.every((x, i) => x === data[i]), "requester round trip with two lost shards");

let refused = false;
try { v.openEnvelope(sealed.envelope.bytes, outsider.secretKey); } catch { refused = true; }
assert(refused, "outsider refused");

// Job matched: the requester adds the executor without re-uploading anything.
const env2 = v.addRecipient(sealed.envelope.bytes, sealed.contentKey, executor.publicKey);
store.set(env2.cid, env2.bytes);
const b = open(env2.bytes, executor.secretKey);
assert(b.data.every((x, i) => x === data[i]), "executor round trip");
console.log(`ok: ${sealed.shards.length} shards, envelope ${sealed.envelope.cid}`);
