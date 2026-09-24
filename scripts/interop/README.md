# Interoperability check with Helia (JavaScript IPFS)

Checks that a stock IPFS implementation can fetch Boltchain blocks from a Boltchain node over
Bitswap 1.2.0, decode the dag-cbor envelope, and verify the `eth-block` header whose CID digest is
the block hash.

```sh
# 1. start a devnet node listening on TCP 18017
cargo run --release -p boltchain -- devnet --datadir /tmp/hdev --block-time 2 \
  --rpc 127.0.0.1:18545 --p2p-host 127.0.0.1 --p2p-port 18017
# note the `p2p identity peer_id=...` line and a `produced block ... root=... hash=...` line

# 2. fetch that block with Helia
cd scripts/interop
npm install helia @libp2p/tcp @chainsafe/libp2p-noise @chainsafe/libp2p-yamux @libp2p/identify \
  @multiformats/multiaddr multiformats @multiformats/sha3 @ipld/dag-cbor
node helia-fetch.mjs <peer_id> <root> <hash>
```

Expected output (verified 2026-09-25 with helia 7.1.15 / @helia/bitswap 4.0.19):

```
envelope {"v":1,"height":2,"header":"bagiacgza...","parent":"bafyrei...","chunks":0}
header bytes 609 cid digest 1951444406fd...
matches block hash true
```
