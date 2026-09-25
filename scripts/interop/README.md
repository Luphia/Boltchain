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

## Whole chain over IPFS only (`helia-full-chain.mjs`)

Starting from one envelope CID (any recent block's `root=` in the node log), follows `parent`
links back to genesis and fetches every header and body chunk with Helia over Bitswap. Helia
checks every block against its CID; the script decodes the transactions and, with an RPC URL,
compares each block's hash and transaction list with the node's.

```sh
npm install ethers   # in addition to the packages above
node helia-full-chain.mjs /ip4/127.0.0.1/tcp/18017/p2p/<peer_id> <root> http://127.0.0.1:18545
```

Verified 2026-09-25 on a devnet with 60 transfers (12 of them carrying 2 KB calldata):

```
{"blocks":157,"txs":60,"chunks":12,"ipfsBytes":146609,"mismatches":0,"seconds":6.627}
```

Boltchain nodes keep connections from peers that announce no Boltchain fork id (plain IPFS
clients), so they can fetch blocks, but such peers are not treated as chain peers.
Discovery is not through the public IPFS DHT (Boltchain runs its own Kademlia under
`/bolt/<chain>/kad/1.0.0`): connect to a Boltchain node directly, or use a node's HTTP gateway
(`--gateway`).
