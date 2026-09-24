import { createHelia } from 'helia'
import { tcp } from '@libp2p/tcp'
import { noise } from '@chainsafe/libp2p-noise'
import { yamux } from '@chainsafe/libp2p-yamux'
import { identify } from '@libp2p/identify'
import { multiaddr } from '@multiformats/multiaddr'
import { CID } from 'multiformats/cid'
import { keccak256 } from '@multiformats/sha3'
import * as dagCbor from '@ipld/dag-cbor'

const [peer, root, blockHash] = process.argv.slice(2)
const helia = await createHelia({
  hashers: [keccak256],
  libp2p: { transports: [tcp()], connectionEncrypters: [noise()], streamMuxers: [yamux()], services: { identify: identify() } },
})
await helia.start?.()
const libp2p = helia.libp2p
await libp2p.dial(multiaddr(`/ip4/127.0.0.1/tcp/18017/p2p/${peer}`))
const signal = AbortSignal.timeout(10000)
const bytes = async (x) => { x = await x; if (x instanceof Uint8Array) return x; const parts = []; for await (const p of x) parts.push(p); return new Uint8Array(Buffer.concat(parts)) }
const envBytes = await bytes(helia.blockstore.get(CID.parse(root), { signal }))
const env = dagCbor.decode(envBytes)
console.log('envelope', JSON.stringify({ v: env.v, height: env.height, header: env.header.toString(), parent: env.parent?.toString(), chunks: env.chunks.length }))
const hdr = await bytes(helia.blockstore.get(env.header, { signal }))
console.log('header bytes', hdr.length, 'cid digest', Buffer.from(env.header.multihash.digest).toString('hex'))
console.log('matches block hash', '0x' + Buffer.from(env.header.multihash.digest).toString('hex') === blockHash)
await helia.stop()
