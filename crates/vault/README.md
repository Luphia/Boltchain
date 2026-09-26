# bolt-vault

Boltchain Vault (ADR 0011): files split, encrypted and erasure-coded for IPFS, readable only by their recipients.

- Padding to size buckets, chunks of 4 KiB to 256 KiB, each encrypted with XChaCha20-Poly1305 (nonce = random prefix ‖ chunk index, chunk count in the AAD).
- Reed-Solomon over the ciphertext: `k` data + `m` parity shards per stripe (default 4 + 2). Any `m` shards of a stripe may be lost; shards not matching their CID count as lost.
- Encrypted manifest (name, type, exact size, shard CIDs). Plain envelope: manifest CID plus the content key wrapped per recipient with HPKE (RFC 9180, X25519 / HKDF-SHA256 / ChaCha20-Poly1305). The envelope CID names the file.
- Every block is an IPFS raw block (CIDv1, raw, sha2-256).
- `add_recipient` lets a holder of the content key add a reader (e.g. the executor once a job is matched) without re-uploading.

## WASM

```sh
cargo build -p bolt-vault --release --target wasm32-unknown-unknown --features wasm
wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/bolt_vault.wasm   # CLI version = wasm-bindgen in Cargo.lock
node crates/vault/js/smoke.mjs <pkg built with --target nodejs>
```

`js/file_operator.ts` is a drop-in replacement for SwarmStorage's `src/lib/file_operator.ts` (uploadFile / downloadFile with recipients' keys).
