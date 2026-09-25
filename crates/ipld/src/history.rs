//! History formats (ADR 0009): the epoch index (the envelopes of one epoch, in order) and state
//! snapshots (the complete flat state at one block, chunked deterministically).
//!
//! Both are plain dag-cbor DAGs of at most 1 MiB blocks, so any IPFS node or trustless gateway can
//! serve them, and every honest node that builds them from the same data gets the same root CID.

use crate::{Cid, DAG_CBOR, IpldError, sha256_cid};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// Envelope CIDs per epoch-index group.
pub const EPOCH_GROUP: usize = 1024;

/// Snapshot chunks are cut once they pass this size (well under the 1 MiB block limit).
pub const SNAPSHOT_CHUNK_TARGET: usize = 512 * 1024;

/// Root of an epoch index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochIndex {
    /// Format version (1).
    pub v: u8,
    /// Epoch.
    pub epoch: u64,
    /// Height of its first block.
    pub first: u64,
    /// Number of blocks.
    pub count: u64,
    /// Groups of up to [`EPOCH_GROUP`] envelope CIDs, in height order.
    pub groups: Vec<Cid>,
}

fn cbor<T: Serialize>(v: &T) -> Vec<u8> {
    serde_ipld_dagcbor::to_vec(v).expect("dag-cbor encodes")
}

fn block<T: Serialize>(v: &T) -> (Cid, Vec<u8>) {
    let bytes = cbor(v);
    (sha256_cid(DAG_CBOR, &bytes), bytes)
}

/// Builds the index of an epoch whose blocks start at `first` and have `envelopes` (in order).
/// Returns the root CID and every block (root last).
pub fn epoch_index(epoch: u64, first: u64, envelopes: &[Cid]) -> (Cid, Vec<(Cid, Vec<u8>)>) {
    let mut blocks = Vec::new();
    let mut groups = Vec::new();
    for g in envelopes.chunks(EPOCH_GROUP) {
        let (cid, bytes) = block(&g.to_vec());
        groups.push(cid);
        blocks.push((cid, bytes));
    }
    let (root, bytes) =
        block(&EpochIndex { v: 1, epoch, first, count: envelopes.len() as u64, groups });
    blocks.push((root, bytes));
    (root, blocks)
}

impl EpochIndex {
    /// Decodes an index root.
    pub fn decode(bytes: &[u8]) -> Result<Self, IpldError> {
        let i: Self = serde_ipld_dagcbor::from_slice(bytes)
            .map_err(|_| IpldError::Malformed("epoch index"))?;
        if i.v != 1 {
            return Err(IpldError::Malformed("epoch index version"));
        }
        Ok(i)
    }
}

/// Decodes an epoch-index group (a list of envelope CIDs).
pub fn decode_group(bytes: &[u8]) -> Result<Vec<Cid>, IpldError> {
    serde_ipld_dagcbor::from_slice(bytes).map_err(|_| IpldError::Malformed("epoch index group"))
}

/// Root of a state snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRoot {
    /// Format version (1).
    pub v: u8,
    /// Chain id.
    pub chain_id: u64,
    /// Block whose post-state this is.
    pub number: u64,
    /// Its hash.
    pub hash: ByteBuf,
    /// Its state root (the import must rebuild exactly this).
    pub state_root: ByteBuf,
    /// Total difficulty up to that block (32 bytes, big-endian).
    pub td: ByteBuf,
    /// Its envelope.
    pub envelope: Cid,
    /// Account chunks, in address order.
    pub accounts: Vec<Cid>,
    /// Contract code chunks, in code-hash order.
    pub codes: Vec<Cid>,
}

impl SnapshotRoot {
    /// dag-cbor block of this root.
    pub fn to_block(&self) -> (Cid, Vec<u8>) {
        block(self)
    }

    /// Decodes a root.
    pub fn decode(bytes: &[u8]) -> Result<Self, IpldError> {
        let r: Self =
            serde_ipld_dagcbor::from_slice(bytes).map_err(|_| IpldError::Malformed("snapshot"))?;
        if r.v != 1 || r.hash.len() != 32 || r.state_root.len() != 32 || r.td.len() != 32 {
            return Err(IpldError::Malformed("snapshot root"));
        }
        Ok(r)
    }
}

/// One account entry of a snapshot chunk. An account with more storage than fits in one chunk
/// continues in the next ones (`x` set, account fields repeated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapAccount {
    /// Address (20 bytes).
    pub a: ByteBuf,
    /// Nonce.
    pub n: u64,
    /// Balance (32 bytes, big-endian).
    pub b: ByteBuf,
    /// Code hash (32 bytes).
    pub c: ByteBuf,
    /// Storage slots and values (32 bytes each), in slot order.
    pub s: Vec<(ByteBuf, ByteBuf)>,
    /// Continuation of the previous entry's storage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub x: bool,
}

/// Decodes an account chunk.
pub fn decode_accounts(bytes: &[u8]) -> Result<Vec<SnapAccount>, IpldError> {
    serde_ipld_dagcbor::from_slice(bytes).map_err(|_| IpldError::Malformed("snapshot chunk"))
}

/// Decodes a code chunk.
pub fn decode_codes(bytes: &[u8]) -> Result<Vec<ByteBuf>, IpldError> {
    serde_ipld_dagcbor::from_slice(bytes).map_err(|_| IpldError::Malformed("code chunk"))
}

/// Cuts accounts and codes into snapshot chunks as they stream in (accounts in address order,
/// codes in hash order). Deterministic: the same input gives the same chunks.
#[derive(Debug, Default)]
pub struct SnapshotBuilder {
    current: Vec<SnapAccount>,
    size: usize,
    codes: Vec<ByteBuf>,
    code_size: usize,
    /// Account chunk CIDs so far.
    pub accounts: Vec<Cid>,
    /// Code chunk CIDs so far.
    pub code_chunks: Vec<Cid>,
}

const ACCOUNT_OVERHEAD: usize = 20 + 8 + 32 + 32 + 16;
const SLOT_SIZE: usize = 64 + 6;

impl SnapshotBuilder {
    /// Adds an account with its storage; returns finished chunks (CID, bytes).
    pub fn account(
        &mut self,
        address: [u8; 20],
        nonce: u64,
        balance: [u8; 32],
        code_hash: [u8; 32],
        storage: impl IntoIterator<Item = ([u8; 32], [u8; 32])>,
    ) -> Vec<(Cid, Vec<u8>)> {
        let mut out = Vec::new();
        let entry = |x: bool| SnapAccount {
            a: ByteBuf::from(address.to_vec()),
            n: nonce,
            b: ByteBuf::from(balance.to_vec()),
            c: ByteBuf::from(code_hash.to_vec()),
            s: Vec::new(),
            x,
        };
        let mut cur = entry(false);
        self.size += ACCOUNT_OVERHEAD;
        for (k, v) in storage {
            if self.size + SLOT_SIZE > SNAPSHOT_CHUNK_TARGET {
                self.current.push(std::mem::replace(&mut cur, entry(true)));
                out.extend(self.flush_accounts());
                self.size = ACCOUNT_OVERHEAD;
            }
            cur.s.push((ByteBuf::from(k.to_vec()), ByteBuf::from(v.to_vec())));
            self.size += SLOT_SIZE;
        }
        self.current.push(cur);
        if self.size > SNAPSHOT_CHUNK_TARGET {
            out.extend(self.flush_accounts());
        }
        out
    }

    /// Adds a contract's code; returns finished chunks.
    pub fn code(&mut self, code: &[u8]) -> Vec<(Cid, Vec<u8>)> {
        let mut out = Vec::new();
        if self.code_size + code.len() + 8 > SNAPSHOT_CHUNK_TARGET && !self.codes.is_empty() {
            out.extend(self.flush_codes());
        }
        self.code_size += code.len() + 8;
        self.codes.push(ByteBuf::from(code.to_vec()));
        out
    }

    fn flush_accounts(&mut self) -> Option<(Cid, Vec<u8>)> {
        if self.current.is_empty() {
            return None;
        }
        let b = block(&std::mem::take(&mut self.current));
        self.size = 0;
        self.accounts.push(b.0);
        Some(b)
    }

    fn flush_codes(&mut self) -> Option<(Cid, Vec<u8>)> {
        if self.codes.is_empty() {
            return None;
        }
        let b = block(&std::mem::take(&mut self.codes));
        self.code_size = 0;
        self.code_chunks.push(b.0);
        Some(b)
    }

    /// Flushes the last chunks; returns them.
    pub fn finish(&mut self) -> Vec<(Cid, Vec<u8>)> {
        self.flush_accounts().into_iter().chain(self.flush_codes()).collect()
    }
}
