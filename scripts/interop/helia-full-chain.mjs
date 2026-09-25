// Downloads a Boltchain chain with a stock IPFS client (Helia, Bitswap 1.2.0) and nothing else:
// starting from one envelope CID it follows `parent` links back to genesis, fetches every header
// and body chunk, verifies them against their CIDs, decodes the transactions, and (optionally)
// compares each block with a node's JSON-RPC.
//
//   node helia-full-chain.mjs <multiaddr-with-/p2p/> <tip-envelope-cid | latest> [rpc-url]

import { createHelia } from 'helia'
import { tcp } from '@libp2p/tcp'
import { noise } from '@chainsafe/libp2p-noise'
import { yamux } from '@chainsafe/libp2p-yamux'
import { identify } from '@libp2p/identify'
import { multiaddr } from '@multiformats/multiaddr'
import { CID } from 'multiformats/cid'
import { keccak256 } from '@multiformats/sha3'
import * as dagCbor from '@ipld/dag-cbor'
import { decodeRlp, keccak256 as keccakHex, hexlify } from 'ethers'

const [addr, tip, rpc] = process.argv.slice(2)
const helia = await createHelia({
  hashers: [keccak256],
  libp2p: {
    transports: [tcp()],
    connectionEncrypters: [noise()],
    streamMuxers: [yamux()],
    services: { identify: identify() },
  },
})
await helia.start?.()
await helia.libp2p.dial(multiaddr(addr))

const bytes = async (x) => {
  x = await x
  if (x instanceof Uint8Array) return x
  const parts = []
  for await (const p of x) parts.push(p)
  return new Uint8Array(Buffer.concat(parts))
}
// Helia verifies every block against its CID (sha2-256 or keccak-256) before returning it.
const get = (cid) => bytes(helia.blockstore.get(cid, { signal: AbortSignal.timeout(15000) }))

const call = async (method, params) => {
  const r = await fetch(rpc, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  })
  return (await r.json()).result
}

// `latest` (with an RPC URL): start from the head block's IPFS root (`ipfsRoot`, a Boltchain
// extension of eth_getBlockByNumber).
let cid = CID.parse(tip === 'latest' ? (await call('eth_getBlockByNumber', ['latest', false])).ipfsRoot : tip)
let blocks = 0, txs = 0, chunks = 0, ipfsBytes = 0, mismatches = 0
const started = Date.now()
while (cid) {
  const envBytes = await get(cid)
  const env = dagCbor.decode(envBytes)
  const header = await get(env.header)
  const blockHash = '0x' + Buffer.from(env.header.multihash.digest).toString('hex')
  let body = new Uint8Array()
  for (const c of env.chunks) {
    const piece = await get(c)
    body = new Uint8Array([...body, ...piece])
    chunks++
    ipfsBytes += piece.length
  }
  const hashes = body.length ? decodeRlp(body).map((t) => keccakHex(t)) : []
  ipfsBytes += envBytes.length + header.length
  if (rpc) {
    const b = await call('eth_getBlockByNumber', ['0x' + env.height.toString(16), false])
    const ok = b && b.hash === blockHash && JSON.stringify(b.transactions) === JSON.stringify(hashes)
    if (!ok) {
      mismatches++
      console.log('MISMATCH at', env.height, { ipfs: { blockHash, hashes }, rpc: b && { hash: b.hash, txs: b.transactions } })
    }
  }
  blocks++
  txs += hashes.length
  if (env.height % 20 === 0 || env.height < 2) {
    console.log(`block ${env.height} ${blockHash.slice(0, 18)}… txs ${hashes.length} chunks ${env.chunks.length}`)
  }
  cid = env.parent ?? null
}
console.log(JSON.stringify({ blocks, txs, chunks, ipfsBytes, mismatches, seconds: (Date.now() - started) / 1000 }))
await helia.stop()
process.exit(mismatches ? 1 : 0)
