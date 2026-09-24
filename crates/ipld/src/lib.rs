//! IPLD encoding of Boltchain blocks.
//!
//! A block is published as three kinds of IPFS blocks:
//!
//! | Node | Codec / hash | Content |
//! | --- | --- | --- |
//! | Header | `eth-block` (0x90) / keccak-256 | Ethereum header RLP. The CID digest **is** the block hash. |
//! | Body chunk | `raw` (0x55) / sha2-256 | Slice (at most 1 MiB) of the RLP list of EIP-2718 transactions |
//! | Envelope | `dag-cbor` (0x71) / sha2-256 | `{v, height, header, parent, chunks, qc}`; links the chain |
//!
//! The 1 MiB limit keeps every block acceptable to public IPFS nodes (bitswap's hard limit is
//! 2 MiB). Validators rebuild the transaction trie from the body and compare it with the header,
//! so the body needs no trie structure of its own and can be fetched in one bitswap round trip.

pub mod car;

use alloy_consensus::{Header, TxEnvelope};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{B256, Bytes, keccak256};
use alloy_rlp::{Decodable, Encodable};
use bolt_primitives::params::MAX_IPLD_BLOCK_BYTES;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use cid::Cid;

/// `eth-block` multicodec.
pub const ETH_BLOCK: u64 = 0x90;
/// `raw` multicodec.
pub const RAW: u64 = 0x55;
/// `dag-cbor` multicodec.
pub const DAG_CBOR: u64 = 0x71;
/// keccak-256 multihash code.
pub const KECCAK_256: u64 = 0x1b;
/// sha2-256 multihash code.
pub const SHA2_256: u64 = 0x12;

/// Current envelope format version.
pub const ENVELOPE_VERSION: u8 = 1;

/// IPLD errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IpldError {
    /// Content does not hash to its CID.
    #[error("content does not match CID {0}")]
    HashMismatch(Cid),
    /// Codec or hash function not used by Boltchain.
    #[error("unsupported CID {0}")]
    Unsupported(Cid),
    /// Malformed content.
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// A required block was not supplied.
    #[error("missing block {0}")]
    Missing(Cid),
}

fn mh(code: u64, digest: &[u8]) -> multihash::Multihash<64> {
    multihash::Multihash::wrap(code, digest).expect("digest fits in 64 bytes")
}

/// CID of a header, from its block hash.
pub fn header_cid(block_hash: B256) -> Cid {
    Cid::new_v1(ETH_BLOCK, mh(KECCAK_256, block_hash.as_slice()))
}

/// Block hash carried by a header CID.
pub fn block_hash_of(cid: &Cid) -> Option<B256> {
    (cid.codec() == ETH_BLOCK && cid.hash().code() == KECCAK_256 && cid.hash().size() == 32)
        .then(|| B256::from_slice(cid.hash().digest()))
}

/// CID of `data` under `codec` with sha2-256.
pub fn sha256_cid(codec: u64, data: &[u8]) -> Cid {
    Cid::new_v1(codec, mh(SHA2_256, &Sha256::digest(data)))
}

/// Checks that `data` hashes to `cid`. Only the codecs and hashes Boltchain uses are accepted.
pub fn verify(cid: &Cid, data: &[u8]) -> Result<(), IpldError> {
    if data.len() > MAX_IPLD_BLOCK_BYTES {
        return Err(IpldError::Malformed("oversized block"));
    }
    let digest: Vec<u8> = match (cid.codec(), cid.hash().code()) {
        (ETH_BLOCK, KECCAK_256) => keccak256(data).to_vec(),
        (RAW | DAG_CBOR, SHA2_256) => Sha256::digest(data).to_vec(),
        _ => return Err(IpldError::Unsupported(*cid)),
    };
    if cid.hash().digest() == digest.as_slice() {
        Ok(())
    } else {
        Err(IpldError::HashMismatch(*cid))
    }
}

/// The dag-cbor node that links a block's parts and its parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Format version.
    pub v: u8,
    /// Block number.
    pub height: u64,
    /// Header CID (its digest is the block hash).
    pub header: Cid,
    /// Parent block's envelope, `None` for genesis.
    pub parent: Option<Cid>,
    /// Body chunks in order; empty for a block without transactions.
    pub chunks: Vec<Cid>,
    /// Quorum certificate for the parent (from M3; empty before).
    #[serde(with = "serde_bytes")]
    pub qc: Vec<u8>,
}

impl Envelope {
    /// dag-cbor encoding.
    pub fn encode(&self) -> Vec<u8> {
        serde_ipld_dagcbor::to_vec(self).expect("envelope serializes")
    }

    /// Decodes dag-cbor.
    pub fn decode(bytes: &[u8]) -> Result<Self, IpldError> {
        let e: Self =
            serde_ipld_dagcbor::from_slice(bytes).map_err(|_| IpldError::Malformed("envelope"))?;
        if e.v != ENVELOPE_VERSION {
            return Err(IpldError::Malformed("envelope version"));
        }
        Ok(e)
    }

    /// Block hash this envelope commits to.
    pub fn block_hash(&self) -> Option<B256> {
        block_hash_of(&self.header)
    }
}

/// All IPFS blocks of one chain block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockBundle {
    /// Envelope CID: the block's root in IPFS.
    pub root: Cid,
    /// The decoded envelope.
    pub envelope: Envelope,
    /// Header, chunks and envelope as (CID, bytes).
    pub blocks: Vec<(Cid, Vec<u8>)>,
}

impl BlockBundle {
    /// Total bytes of the body chunks.
    pub fn body_len(&self) -> usize {
        let chunks: std::collections::HashSet<_> = self.envelope.chunks.iter().collect();
        self.blocks.iter().filter(|(c, _)| chunks.contains(c)).map(|(_, d)| d.len()).sum()
    }
}

/// RLP list of EIP-2718 transactions (the same bytes the store keeps).
pub fn encode_body(txs: &[TxEnvelope]) -> Vec<u8> {
    let items: Vec<Bytes> = txs.iter().map(|t| t.encoded_2718().into()).collect();
    alloy_rlp::encode(items)
}

/// Decodes a body produced by [`encode_body`].
pub fn decode_body(body: &[u8]) -> Result<Vec<TxEnvelope>, IpldError> {
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut buf = body;
    let items: Vec<Bytes> =
        Decodable::decode(&mut buf).map_err(|_| IpldError::Malformed("body"))?;
    if !buf.is_empty() {
        return Err(IpldError::Malformed("trailing body bytes"));
    }
    items
        .iter()
        .map(|b| {
            let mut s = b.as_ref();
            let tx =
                TxEnvelope::decode_2718(&mut s).map_err(|_| IpldError::Malformed("transaction"))?;
            if s.is_empty() { Ok(tx) } else { Err(IpldError::Malformed("trailing tx bytes")) }
        })
        .collect()
}

/// Builds the IPFS representation of a block.
pub fn bundle(
    header: &Header,
    txs: &[TxEnvelope],
    parent: Option<Cid>,
    qc: Vec<u8>,
) -> BlockBundle {
    let mut header_rlp = Vec::new();
    header.encode(&mut header_rlp);
    let hcid = header_cid(keccak256(&header_rlp));
    let mut blocks = vec![(hcid, header_rlp)];

    let mut chunks = Vec::new();
    if !txs.is_empty() {
        let body = encode_body(txs);
        for piece in body.chunks(MAX_IPLD_BLOCK_BYTES) {
            let c = sha256_cid(RAW, piece);
            chunks.push(c);
            blocks.push((c, piece.to_vec()));
        }
    }
    let envelope =
        Envelope { v: ENVELOPE_VERSION, height: header.number, header: hcid, parent, chunks, qc };
    let bytes = envelope.encode();
    let root = sha256_cid(DAG_CBOR, &bytes);
    blocks.push((root, bytes));
    BlockBundle { root, envelope, blocks }
}

/// A block reassembled from IPFS and checked against its CIDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBlock {
    /// Envelope.
    pub envelope: Envelope,
    /// Header (its hash equals the envelope's header CID digest).
    pub header: Header,
    /// Transactions.
    pub transactions: Vec<TxEnvelope>,
}

/// Reassembles a block from its envelope and a lookup for the other parts. Every part is
/// verified against its CID; the header number must match the envelope height.
pub fn decode_block(
    envelope: Envelope,
    mut get: impl FnMut(&Cid) -> Option<Vec<u8>>,
) -> Result<DecodedBlock, IpldError> {
    let hbytes = get(&envelope.header).ok_or(IpldError::Missing(envelope.header))?;
    verify(&envelope.header, &hbytes)?;
    let header =
        Header::decode(&mut hbytes.as_slice()).map_err(|_| IpldError::Malformed("header"))?;
    if header.number != envelope.height {
        return Err(IpldError::Malformed("height mismatch"));
    }
    let mut body = Vec::new();
    for c in &envelope.chunks {
        if c.codec() != RAW {
            return Err(IpldError::Unsupported(*c));
        }
        let piece = get(c).ok_or(IpldError::Missing(*c))?;
        verify(c, &piece)?;
        body.extend_from_slice(&piece);
    }
    let transactions = decode_body(&body)?;
    Ok(DecodedBlock { envelope, header, transactions })
}

#[cfg(test)]
mod tests;
